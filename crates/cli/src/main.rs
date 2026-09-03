//! serena-cli —— single-binary LSP CLI（PLAN Task 10/16 / ARCHITECTURE §2）。
//!
//! 三种模式：
//! - `--direct`：进程内直连 LS（M0 冒烟/单测路径）。
//! - `--daemon`：本进程作为常驻 daemon（lock 仲裁 + HTTP + reaper）。
//! - 默认（无 flag）：转发模式——读 lock → TCP 探活 → 活着转发 / 死了 lazy-spawn
//!   自身 `--daemon`（I5：CREATE_NO_WINDOW + 新进程组 + 句柄不继承 + stdin/stdout→NULL）。
//!
//! 管理命令：`status` / `stop-all`。
//!
//! Windows 启动即 `SetConsoleOutputCP(65001)` —— 中文 Windows conhost 默认 GBK 码页。
//!
//! Exit code 协议（ARCH §6.3）：
//! 0 成功 / 1 工具失败（含 LS 未装）/ 2 用法错 / 3 daemon 或传输故障。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use serde_json::json;
use lsp_core::framing::{JsonRpc, decode, encode};
use supervisor::{Supervisor, SupervisorTrait, ToolError};


/// 转发超时（工具请求 300s；管理命令 5s）。
const FORWARD_TIMEOUT: Duration = Duration::from_secs(300);
const MGMT_TIMEOUT: Duration = Duration::from_secs(5);
/// lazy-spawn 后等 daemon 就绪的总窗口。
const SPAWN_WAIT: Duration = Duration::from_secs(10);

#[derive(Parser, Debug)]
#[command(name = "serena-cli", version, about = "serena-rust LSP CLI")]
struct Cli {
    /// 直连模式：单进程拉 LS 直调（M0 路径）。
    #[arg(long, conflicts_with = "daemon")]
    direct: bool,

    /// daemon 模式：本进程作为常驻 daemon。
    #[arg(long)]
    daemon: bool,

    /// 项目根（--direct 模式必填；转发模式透传给 daemon）。
    #[arg(long, value_name = "ROOT")]
    project: Option<PathBuf>,

    /// JSON 输出：所有子命令输出可被 jq 解析的 JSON（默认人类可读文本）。
    #[arg(long, global = true)]
    json: bool,
    /// 子命令；`--daemon` 模式下可省略。
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出文件顶层符号。
    Overview { file: String },
    /// 跳转到符号定义（textDocument/definition）。
    Def { file: String, line: u32, col: u32 },
    /// 列出引用（textDocument/references）。line/col 0-based。
    Refs { file: String, line: u32, col: u32 },
    /// 鼠标位置符号的 type / doc（textDocument/hover）。
    Hover { file: String, line: u32, col: u32 },
    /// 当前文件错误/警告（textDocument/diagnostic）。
    Diagnostics { file: String },
    /// 全 workspace 跨文件符号查找（workspace/symbol）。
    FindSymbol {
        /// 子串或正则（取决于 LSP server 行为，clangd 默认子串）。
        query: String,
        /// 上限。
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// 符号的所有实现位置（textDocument/implementation）。
    FindImplementations { file: String, line: u32, col: u32 },
    /// 跨文件 rename（textDocument/rename）。
    RenameSymbol {
        file: String,
        line: u32,
        col: u32,
        /// 新名。
        #[arg(long = "to")]
        new_name: String,
    },
    /// workspace/search：跨文件正则搜索。
    Search {
        pattern: String,
        /// glob 过滤文件路径（如 **/*.cpp）。
        #[arg(long)]
        path_glob: Option<String>,
        /// 最大结果数。
        #[arg(long, default_value_t = 100)]
        max_results: u32,
        /// 大小写敏感（默认不敏感）。
        #[arg(long, default_value_t = false)]
        case_sensitive: bool,
    },
    /// 按行范围读文件（1-based 含端）。
    ReadFile {
        file: String,
        /// 起始行（1-based，默认 1）。
        #[arg(long)]
        start_line: Option<u32>,
        /// 结束行（1-based 含端，默认 EOF）。
        #[arg(long)]
        end_line: Option<u32>,
    },
    /// 列出目录项（不递归）。
    ListDir { path: String },
    /// 按文件名 glob 查找文件（限深 5）。
    FindFile {
        /// glob 模式（如 main.cpp）。
        name_pattern: String,
    },
    /// 所有引用 + 每个 ref 落在哪个外层符号里。
    FindReferencingSymbols { file: String, line: u32, col: u32 },
    /// 所有引用 + 每个 ref 前后 N 行。
    FindReferencingCodeSnippets {
        file: String,
        line: u32,
        col: u32,
        /// 每个 ref 上下文行数（前后对称）。
        #[arg(long, default_value_t = 3)]
        context_lines: u32,
        /// 上限。
        #[arg(long, default_value_t = 20)]
        max_results: u32,
    },
    /// 取符号体切片（position-free；documentSymbol 定位）。
    SymbolBody { file: String, symbol: String },
    /// 替换符号体（写门 + hash 对账 + 原子写）。
    ReplaceBody {
        file: String,
        symbol: String,
        /// 新符号体完整文本。
        #[arg(long = "with")]
        new_body: String,
    },
    /// 在 symbol 体内替换 old → new（行级字节切片）。
    ReplaceTextInSymbol {
        file: String,
        symbol: String,
        /// 待替换原文。
        old_text: String,
        /// 新文。
        new_text: String,
    },
    /// 在 symbol 开头插入 text。
    InsertTextBeforeSymbol {
        file: String,
        symbol: String,
        text: String,
    },
    /// 在 symbol 末尾插入 text。
    InsertTextAfterSymbol {
        file: String,
        symbol: String,
        text: String,
    },
    /// 在 symbol 体内删除 [start_line, end_line] 切片（1-based 含端）。
    DeleteTextInSymbol {
        file: String,
        symbol: String,
        start_line: u32,
        end_line: u32,
    },
    /// daemon 状态（uptime / pid / loaded LS）。
    Status,
    /// 停掉 daemon（draining + 删 lock）。
    StopAll,
    /// MCP stdio server（Claude Desktop / MCP 客户端连通用）。
    Mcp {
        /// 项目根（必填）。
        #[arg(long, value_name = "ROOT")]
        project: PathBuf,
    },
    /// 长连接 shell（stdin/stdout JSONL）。Task 18。
    Shell,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    #[cfg(windows)]
    unsafe {
        let r = windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
        if r == 0 {
            eprintln!("warning: SetConsoleOutputCP(65001) failed");
        }
    }

    let cli = Cli::parse();
    let lock_path = daemon::serve::default_lock_path();

    // ---- daemon 模式：本进程做 daemon，阻塞至 shutdown ----
    if cli.daemon {
        let cfg = daemon::serve::ServeConfig {
            lock_path,
            ..Default::default()
        };
        return match daemon::serve::serve(cfg).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("daemon exited with error: {e:#}");
                ExitCode::from(3)
            }
        };
    }

    // ---- 管理命令：只走 lock/HTTP，不需要 project ----
    match &cli.cmd {
        Some(Cmd::Status) => return cmd_status(&lock_path).await,
        Some(Cmd::StopAll) => return cmd_stop_all(&lock_path).await,
        Some(Cmd::Shell) | Some(Cmd::Mcp { .. }) => {}
        _ => {}
    }

    // ---- --direct：进程内直调（原 M0 路径）----
    if cli.direct {
        return run_direct(&cli).await;
    }

    // ---- shell：长连接 stdin/stdout JSONL ----
    if matches!(&cli.cmd, Some(Cmd::Shell)) {
        return cmd_shell(&cli).await;
    }

    if let Some(Cmd::Mcp { project }) = &cli.cmd {
        return cmd_mcp(project).await;
    }

    // ---- 默认：转发模式（lazy-spawn）----
    match forward_or_spawn(&cli, &lock_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(3)
        }
    }
}

/// M0 --direct 路径（行为不变）。
async fn run_direct(cli: &Cli) -> ExitCode {
    let Some(root) = cli.project.clone() else {
        eprintln!("--direct requires --project <ROOT>");
        return ExitCode::from(2);
    };
    let root = dunce::canonicalize(&root).unwrap_or(root);
    let sup = match Supervisor::direct().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor init failed: {e:#}");
            return ExitCode::from(1);
        }
    };
    let res: Result<(), ToolError> = match &cli.cmd {
        Some(Cmd::Overview { file }) => sup
            .tool_overview(&root, file)
            .await
            .and_then(|hits| print_json(&json!(hits))),
        Some(Cmd::Def { file, line, col }) => sup
            .tool_def(&root, file, *line, *col)
            .await
            .and_then(|opt| print_json(&json!(opt))),
        Some(Cmd::Refs { file, line, col }) => sup
            .tool_refs(&root, file, *line, *col)
            .await
            .and_then(|vec| print_json(&json!(vec))),
        other => {
            let _ = other;
            eprintln!("this subcommand is daemon-mode only in M1");
            return ExitCode::from(2);
        }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let exit = tool_error_exit(&e);
            eprintln!("{e}");
            ExitCode::from(exit)
        }
    }
}

fn tool_error_exit(e: &ToolError) -> u8 {
    match e {
        ToolError::BadArgs { .. } => 2,
        ToolError::WriteConflict { .. } | ToolError::NotInstalled { .. } => 1,
        ToolError::Core(_) | ToolError::Launch(_) => 3,
    }
}

/// 转发模式：探活 → 转发；死 lock → lazy-spawn --daemon → 轮询就绪 → 转发。
async fn forward_or_spawn(cli: &Cli, lock_path: &Path) -> Result<(), String> {
    let entry = daemon::lockfile::read(lock_path).map_err(|e| format!("read lock: {e}"))?;

    let base = match entry {
        Some(e) if probe(e.port) => format!("http://127.0.0.1:{}", e.port),
        _ => {
            // 死 lock（或无 lock）：清残留 + lazy-spawn。
            let _ = daemon::lockfile::remove(lock_path);
            let port = spawn_daemon_child()?;
            wait_ready(port, SPAWN_WAIT).await?;
            format!("http://127.0.0.1:{port}")
        }
    };

    let token = daemon::lockfile::read(lock_path)
        .map_err(|e| format!("read lock: {e}"))?
        .map(|e| e.token)
        .unwrap_or_default();

    forward(cli, &base, &token).await
}

/// Windows：CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP + 句柄不继承 + stdio→NULL。
fn spawn_daemon_child() -> Result<u16, String> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok(7860) // M1 固定端口；M2 起 OS 分配 + lock 回填
}

/// TCP 探活。
fn probe(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 轮询 /status 直到就绪。
async fn wait_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{port}/status");
    let client = reqwest::Client::new();
    let mut token: Option<String> = None;
    while Instant::now() < deadline {
        // 抓 token：spawn 后 daemon 写 lock 通常几 ms 内完成。
        if token.is_none()
            && let Some(e) = daemon::lockfile::read(&daemon::serve::default_lock_path())
                .ok()
                .flatten()
        {
            token = Some(e.token);
        }
        let mut req = client.get(&url).timeout(Duration::from_millis(500));
        if let Some(t) = &token {
            req = req.header("X-Serena-Token", t);
        }
        if let Ok(resp) = req.send().await
            && resp.status().is_success()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("daemon on :{port} not ready within {timeout:?}"))
}

/// 按子命令转发 HTTP。
async fn forward(cli: &Cli, base: &str, token: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    // 工具名与 args 组装。
    let (tool, args): (&str, serde_json::Value) = match &cli.cmd {
        Some(Cmd::Overview { file }) => ("overview", json!({"file": file})),
        Some(Cmd::Def { file, line, col }) => {
            ("def", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Refs { file, line, col }) => {
            ("refs", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Hover { file, line, col }) => {
            ("hover", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Diagnostics { file }) => ("diagnostics", json!({"file": file})),
        Some(Cmd::FindSymbol { query, limit }) => (
            "find-symbol",
            json!({"query": query, "limit": limit}),
        ),
        Some(Cmd::FindImplementations { file, line, col }) => (
            "find-implementations",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::RenameSymbol { file, line, col, new_name }) => (
            "rename-symbol",
            json!({"file": file, "line": line, "col": col, "new_name": new_name}),
        ),
        Some(Cmd::Search { pattern, path_glob, max_results, case_sensitive }) => (
            "search",
            json!({
                "pattern": pattern,
                "path_glob": path_glob,
                "max_results": max_results,
                "case_sensitive": case_sensitive,
            }),
        ),
        Some(Cmd::ReadFile { file, start_line, end_line }) => (
            "read-file",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
            }),
        ),
        Some(Cmd::ListDir { path }) => ("list-dir", json!({"path": path})),
        Some(Cmd::FindFile { name_pattern }) => {
            ("find-file", json!({"name_pattern": name_pattern}))
        }
        Some(Cmd::FindReferencingSymbols { file, line, col }) => (
            "find-referencing-symbols",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::FindReferencingCodeSnippets { file, line, col, context_lines, max_results }) => (
            "find-referencing-code-snippets",
            json!({
                "file": file,
                "line": line,
                "col": col,
                "context_lines": context_lines,
                "max_results": max_results,
            }),
        ),
        Some(Cmd::SymbolBody { file, symbol }) => {
            ("symbol-body", json!({"file": file, "symbol": symbol}))
        }
        Some(Cmd::ReplaceBody { file, symbol, new_body }) => (
            "replace-body",
            json!({"file": file, "symbol": symbol, "new_body": new_body}),
        ),
        Some(Cmd::ReplaceTextInSymbol { file, symbol, old_text, new_text }) => (
            "replace-text-in-symbol",
            json!({"file": file, "symbol": symbol, "old_text": old_text, "new_text": new_text}),
        ),
        Some(Cmd::InsertTextBeforeSymbol { file, symbol, text }) => (
            "insert-text-before-symbol",
            json!({"file": file, "symbol": symbol, "text": text}),
        ),
        Some(Cmd::InsertTextAfterSymbol { file, symbol, text }) => (
            "insert-text-after-symbol",
            json!({"file": file, "symbol": symbol, "text": text}),
        ),
        Some(Cmd::DeleteTextInSymbol { file, symbol, start_line, end_line }) => (
            "delete-text-in-symbol",
            json!({"file": file, "symbol": symbol, "start_line": start_line, "end_line": end_line}),
        ),
        Some(Cmd::Status) | Some(Cmd::StopAll) | Some(Cmd::Shell) | Some(Cmd::Mcp { .. }) | None => {
            unreachable!("handled earlier")
        }
    };
    let project_root = resolve_project_root(cli.project.clone());
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
    });

    let resp = client
        .post(format!("{base}/tools/{tool}"))
        .header("X-Serena-Token", token)
        .json(&body)
        .timeout(FORWARD_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("forward {tool}: {e}"))?;

    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;

    if !status.is_success() {
        // 403/503 等传输层错。
        return Err(format!("daemon transport error {status}: {payload}"));
    }
    match payload.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => {
            print_json(payload.get("data").unwrap_or(&serde_json::Value::Null))
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        _ => {
            let err = payload.get("error").cloned().unwrap_or(payload);
            eprintln!("tool error: {err}");
            std::process::exit(1);
        }
    }
}

/// `status` 子命令。
async fn cmd_status(lock_path: &Path) -> ExitCode {
    let entry = match daemon::lockfile::read(lock_path) {
        Ok(Some(e)) if probe(e.port) => e,
        _ => {
            println!("daemon: not running");
            return ExitCode::from(1);
        }
    };
    let client = reqwest::Client::new();
    match client
        .get(format!("http://127.0.0.1:{}/status", entry.port))
        .header("X-Serena-Token", &entry.token)
        .timeout(MGMT_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or(json!(null));
            print_json(&body).expect("print status");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("status probe failed: {other:?}");
            ExitCode::from(3)
        }
    }
}

/// `stop-all` 子命令：POST /shutdown + 删 lock。
async fn cmd_stop_all(lock_path: &Path) -> ExitCode {
    let Some(entry) = daemon::lockfile::read(lock_path).unwrap_or(None) else {
        println!("daemon: not running");
        return ExitCode::SUCCESS;
    };
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://127.0.0.1:{}/shutdown", entry.port))
        .header("X-Serena-Token", &entry.token)
        .timeout(MGMT_TIMEOUT)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {
            println!(
                "daemon draining (pid {}); lock will be removed by reaper",
                entry.pid
            );
            ExitCode::SUCCESS
        }
        other => {
            // 端口死但 lock 残留：直接清。
            let _ = daemon::lockfile::remove(lock_path);
            eprintln!("shutdown probe failed: {other:?}; stale lock removed");
            ExitCode::from(3)
        }
    }
}

/// Print JSON pretty; map serde_json errors to ToolError::Launch (uniform exit 3 path).
fn print_json(v: &serde_json::Value) -> Result<(), ToolError> {
    println!(
        "{}",
        serde_json::to_string_pretty(v)
            .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))?
    );
    Ok(())
}
// ============== shell 模式 (Task 18) ==============

/// 长连接 shell：stdin/stdout JSONL。
///
/// 输入（一行 JSON）：
///   `{"id":<n>,"cmd":"<tool>","args":{...}}`  —— 调用 LSP 工具
///   `{"id":<n>,"cmd":"status"}`                —— daemon 状态
///   `{"id":<n>,"cmd":"exit"}`                  —— 退出 shell
///
/// 输出（一行 JSON）：
///   `{"id":<n>,"ok":true,"data":<v>}`
///   `{"id":<n>,"ok":false,"error":<msg>}`
///   EOF / `exit` 后退出 0。
async fn cmd_shell(cli: &Cli) -> ExitCode {
    let lock_path = daemon::serve::default_lock_path();
    let base_token = match ensure_daemon(&lock_path).await {
        Ok(b) => b,
        Err(e) => {
            // shell 启动失败也要在 stdout 留 JSON，便于 agent 解析。
            println!(r#"{{"id":null,"ok":false,"error":"{}"}}"#, json_escape(&e));
            return ExitCode::from(3);
        }
    };

    let client = reqwest::Client::new();
    use tokio::io::{AsyncBufReadExt, BufReader};
    let project_root = resolve_project_root(cli.project.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // 解析 input。
        let input: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                println!(
                    r#"{{"id":null,"ok":false,"error":"bad json: {}"}}"#,
                    json_escape(&e.to_string())
                );
                continue;
            }
        };

        let id = input.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let cmd = input.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

        // exit: 退出。
        if cmd == "exit" {
            println!(
                r#"{{"id":{},"ok":true,"data":null,"bye":true}}"#,
                serde_json::to_string(&id).unwrap_or("null".into())
            );
            break;
        }

        // 处理单条命令。
        let resp = dispatch_shell_cmd(
            &client,
            &base_token,
            &project_root,
            cmd,
            input.get("args").cloned().unwrap_or(json!({})),
        )
        .await;
        println!("{}", resp_with_id(&id, resp));
    }

    ExitCode::SUCCESS
}

/// 探活 + lazy-spawn，返回 (base_url, token)。
async fn ensure_daemon(lock_path: &Path) -> Result<(String, String), String> {
    let entry = daemon::lockfile::read(lock_path).map_err(|e| format!("read lock: {e}"))?;
    let base = match entry {
        Some(e) if probe(e.port) => format!("http://127.0.0.1:{}", e.port),
        _ => {
            let _ = daemon::lockfile::remove(lock_path);
            let port = spawn_daemon_child()?;
            wait_ready(port, SPAWN_WAIT).await?;
            format!("http://127.0.0.1:{port}")
        }
    };
    let token = daemon::lockfile::read(lock_path)
        .map_err(|e| format!("read lock: {e}"))?
        .map(|e| e.token)
        .unwrap_or_default();
    Ok((base, token))
}

/// 单条 shell 命令：HTTP 转发到 daemon。
async fn dispatch_shell_cmd(
    client: &reqwest::Client,
    base_token: &(String, String),
    project_root: &Path,
    cmd: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    // 管理命令。
    if cmd == "status" {
        let resp = client
            .get(format!("{}/status", base_token.0))
            .header("X-Serena-Token", &base_token.1)
            .timeout(MGMT_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("status: {e}"))?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await.unwrap_or(json!(null));
        if !status.is_success() {
            return Err(format!("daemon transport {status}: {data}"));
        }
        return Ok(data);
    }

    // LSP 工具：透传到 /tools/{name}。
    let tool = match cmd {
        "overview"
        | "hover"
        | "diagnostics"
        | "def"
        | "refs"
        | "symbol-body"
        | "replace-body"
        | "completion"
        | "search"
        | "find-symbol"
        | "find-implementations"
        | "rename-symbol"
        | "read-file"
        | "list-dir"
        | "find-file"
        | "find-referencing-symbols"
        | "find-referencing-code-snippets"
        | "replace-text-in-symbol"
        | "insert-text-after-symbol"
        | "insert-text-before-symbol"
        | "delete-text-in-symbol" => cmd,

        other => return Err(format!("unknown cmd: {other}")),
    };
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
    });
    let resp = client
        .post(format!("{}/tools/{tool}", base_token.0))
        .header("X-Serena-Token", &base_token.1)
        .json(&body)
        .timeout(FORWARD_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("forward {tool}: {e}"))?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    if !status.is_success() {
        return Err(format!("daemon transport {status}: {payload}"));
    }
    match payload.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(payload
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        _ => Err(payload.get("error").cloned().unwrap_or(payload).to_string()),
    }
}

/// 把 (id, result) 序列化成一行 JSON 输出。
fn resp_with_id(id: &serde_json::Value, r: Result<serde_json::Value, String>) -> String {
    let id_str = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
    match r {
        Ok(data) => format!(r#"{{"id":{},"ok":true,"data":{}}}"#, id_str, data),
        Err(e) => format!(
            r#"{{"id":{},"ok":false,"error":"{}"}}"#,
            id_str,
            json_escape(&e)
        ),
    }
}

/// JSON string 转义（仅控制 + 引号 + 反斜杠 —— 不全但够错误消息用）。
/// 解析 --project：相对 → CWD 拼接 → canonicalize。失败回退原值。
fn resolve_project_root(raw: Option<PathBuf>) -> PathBuf {
    let p = raw.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    dunce::canonicalize(&p).unwrap_or(p)
}
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!(r"\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ============== MCP stdio server (Claude Desktop / MCP 客户端通用) ==============

/// MCP 协议工具列表（每个工具 + inputSchema）。MCP spec 2025-06-18 版本。
fn mcp_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "overview",
            "description": "列出文件顶层符号（递归 children）。position-free；只传 file。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]},
        },
        {
            "name": "find-symbol",
            "description": "workspace/symbol：全 workspace 跨文件符号查找。",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["query"]},
        },
        {
            "name": "def",
            "description": "textDocument/definition：跳转到符号定义。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "refs",
            "description": "textDocument/references：列出所有引用。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "hover",
            "description": "textDocument/hover：鼠标位置符号的 type / doc 注释。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "find-implementations",
            "description": "textDocument/implementation：符号的所有实现位置。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "search",
            "description": "workspace/search：跨文件正则搜索。",
            "inputSchema": {"type": "object", "properties": {"pattern": {"type": "string"}, "path_glob": {"type": "string"}, "max_results": {"type": "integer"}, "case_sensitive": {"type": "boolean"}}, "required": ["pattern"]},
        },
        {
            "name": "diagnostics",
            "description": "textDocument/diagnostic：当前文件错误/警告列表。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]},
        },
        {
            "name": "symbol-body",
            "description": "按符号名取函数/类体切片（position-free）。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}}, "required": ["file", "symbol"]},
        },
        {
            "name": "replace-body",
            "description": "替换符号体（写门 + hash 对账 + 原子写 + didChange）。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}, "new_body": {"type": "string"}}, "required": ["file", "symbol", "new_body"]},
        },
        {
            "name": "rename-symbol",
            "description": "textDocument/rename：rename symbol 跨文件变更。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}, "new_name": {"type": "string"}}, "required": ["file", "line", "col", "new_name"]},
        },
        {
            "name": "read-file",
            "description": "按行范围读文件（1-based 含端）。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"}}, "required": ["file"]},
        },
        {
            "name": "list-dir",
            "description": "列出目录项（不递归）。",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]},
        },
        {
            "name": "find-file",
            "description": "按文件名 glob 查找文件（限深 5）。",
            "inputSchema": {"type": "object", "properties": {"name_pattern": {"type": "string"}}, "required": ["name_pattern"]},
        },
        {
            "name": "find-referencing-symbols",
            "description": "所有引用 + 每个 ref 落在哪个外层符号里。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "find-referencing-code-snippets",
            "description": "所有引用 + 每个 ref 前后 N 行。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}, "context_lines": {"type": "integer"}, "max_results": {"type": "integer"}}, "required": ["file", "line", "col"]},
        },
        {
            "name": "replace-text-in-symbol",
            "description": "在 symbol 体内替换 old → new（行级字节切片）。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}, "old_text": {"type": "string"}, "new_text": {"type": "string"}}, "required": ["file", "symbol", "old_text", "new_text"]},
        },
        {
            "name": "insert-text-before-symbol",
            "description": "在 symbol 开头插入 text。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}, "text": {"type": "string"}}, "required": ["file", "symbol", "text"]},
        },
        {
            "name": "insert-text-after-symbol",
            "description": "在 symbol 末尾插入 text。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}, "text": {"type": "string"}}, "required": ["file", "symbol", "text"]},
        },
        {
            "name": "delete-text-in-symbol",
            "description": "在 symbol 体内删除 [start_line, end_line] 切片（1-based 含端）。",
            "inputSchema": {"type": "object", "properties": {"file": {"type": "string"}, "symbol": {"type": "string"}, "start_line": {"type": "integer"}, "end_line": {"type": "integer"}}, "required": ["file", "symbol", "start_line", "end_line"]},
        },
    ])
}

/// MCP stdio server：长连接 stdin/stdout Content-Length 帧。
async fn cmd_mcp(project_root: &PathBuf) -> ExitCode {
    let root = dunce::canonicalize(project_root).unwrap_or_else(|_| project_root.clone());
    let sup = match Supervisor::direct().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor init failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = BytesMut::new();
    let mut chunk = [0u8; 4096];

    loop {
        let n = match stdin.read(&mut chunk).await {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(n) => n,
            Err(_) => return ExitCode::from(3),
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Ok(Some(msg)) = decode(&mut buf) {
            let resp = handle_mcp(&sup, &root, msg).await;
            let frame = encode(&resp);
            if stdout.write_all(&frame).await.is_err() {
                return ExitCode::from(3);
            }
            if stdout.flush().await.is_err() {
                return ExitCode::from(3);
            }
        }
    }
}

/// 单帧 MCP 请求 → 响应帧。
async fn handle_mcp(sup: &Supervisor, root: &Path, msg: JsonRpc) -> JsonRpc {
    let id = msg.id.clone();
    let method = msg.method.clone().unwrap_or_default();
    let params = msg.params.clone().unwrap_or(serde_json::Value::Null);

    let result = match method.as_str() {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2025-06-18",
            "serverInfo": {"name": "serena-rust", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"tools": {}},
        })),
        "tools/list" => Ok(serde_json::json!({"tools": mcp_tools()})),
        "tools/call" => match call_mcp_tool(sup, root, &params).await {
            Ok(v) => Ok(v),
            Err(e) => Err(e),
        },
        _ => Err(format!("unknown method: {method}")),
    };

    match result {
        Ok(value) => JsonRpc {
            jsonrpc: "2.0".into(),
            id,
            method: None,
            params: None,
            result: Some(value),
            error: None,
        },
        Err(e) => JsonRpc {
            jsonrpc: "2.0".into(),
            id,
            method: None,
            params: None,
            result: None,
            error: Some(lsp_core::framing::RpcError {
                code: -32000,
                message: e,
                data: None,
            }),
        },
    }
}

/// 单个 tools/call → MCP content 数组。
async fn call_mcp_tool(sup: &Supervisor, root: &Path, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    let name = params.get("name").and_then(|v| v.as_str()).ok_or("missing 'name'")?;
    let args = params.get("arguments").cloned().unwrap_or(serde_json::Value::Object(Default::default()));
    let resp = sup.execute_tool(name, &root.to_string_lossy(), args)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(serde_json::json!({
        "content": [{"type": "text", "text": serde_json::to_string(&resp).unwrap_or_default()}],
        "isError": false,
    }))
}

// 未用占位避免警告
