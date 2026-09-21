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
use supervisor::{Supervisor, ToolError};

/// 转发超时（工具请求 300s；管理命令 3s，daemon 卡死时 stop-all 快速失败）。
const FORWARD_TIMEOUT: Duration = Duration::from_secs(300);
const MGMT_TIMEOUT: Duration = Duration::from_secs(3);
/// lazy-spawn 后等 daemon 就绪的总窗口。
const SPAWN_WAIT: Duration = Duration::from_secs(10);

/// CLI 侧共享 HTTP client：本机回环，建连 2s 封顶（管理面失败即报，daemon 侧自愈）。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client build")
}

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

    /// 覆盖文件扩展名探测：多语言项目用 (如 --lang typescript 在 .ts 项目里用 TS LS)。
    /// find-symbol 不指定时也用它过滤到单 LS。
    #[arg(long, global = true, value_name = "LANG")]
    lang: Option<String>,

    /// 全局工具请求超时（毫秒）。Phase 4 基建 Task 22b。优先级最高：CLI > servers.toml
    /// `[defaults].timeout_ms` > 30s 默认。`--index-timeout` 单独覆盖 workspace/symbol
    /// 等长操作（默认 120s）。
    #[arg(long, global = true, value_name = "MS")]
    request_timeout: Option<u32>,

    /// 索引型工具（workspace/symbol、workspace/diagnostic 等）超时（毫秒）。默认 120s。
    #[arg(long, global = true, value_name = "MS")]
    index_timeout: Option<u32>,

    /// 子命令；`--daemon` 模式下可省略。
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出文件顶层符号。
    Overview { file: String },
    /// 聚合目录下源码文件符号（跨文件符号树；依赖 LS，逐文件可复用缓存）。
    SymbolTree {
        dir: String,
        /// 保险丝：最多扫描文件数（超出截断并标 truncated）。
        #[arg(long, value_name = "N", default_value_t = 200)]
        max_files: usize,
    },
    /// 跳转到符号定义（textDocument/definition）。line/col 为 0-based。
    Def { file: String, line: u32, col: u32 },
    /// 列出引用（textDocument/references）。line/col 0-based。
    Refs { file: String, line: u32, col: u32 },
    /// 鼠标位置符号的 type / doc（textDocument/hover）。line/col 为 0-based。
    Hover { file: String, line: u32, col: u32 },
    Diagnostics {
        file: String,
        /// 等 diagnostics generation >= N（替代盲轮询 5s）；0=立即返回当前；仍受 5s 上限。
        #[arg(long, value_name = "GEN")]
        wait_gen: Option<u64>,
    },
    /// 全 workspace 跨文件符号查找（workspace/symbol）。
    FindSymbol {
        /// 子串或正则（取决于 LSP server 行为，clangd 默认子串）。
        query: String,
        /// 上限。
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// 符号的所有实现位置（textDocument/implementation）。line/col 为 0-based。
    FindImplementations { file: String, line: u32, col: u32 },
    /// 跨文件 rename（textDocument/rename）。line/col 为 0-based。
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
    /// 所有引用 + 每个 ref 落在哪个外层符号里。line/col 为 0-based。
    FindReferencingSymbols { file: String, line: u32, col: u32 },
    /// 所有引用 + 每个 ref 前后 N 行。line/col 为 0-based。
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
    /// AI 编辑主路径聚合：单次返回 body + callers + doc + tests（ai-token §10-B）。
    EditContext {
        /// 目标文件（相对 root）。
        file: String,
        /// 符号名。
        symbol: String,
    },
    /// 全 workspace 符号地图（按调用热度 top N）。ai-token §10-E。
    RepoMap {
        /// top N 符号（默认 20；超过会按 direct_refs 降序截断）。
        #[arg(long, default_value_t = 20)]
        top_n: u32,
    },
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
    /// 安全删除符号：无引用才删；有引用拒删并列出引用位置。
    SafeDeleteSymbol { file: String, symbol: String },
    /// 在 line（1-based）前插入内容，原行下移；line = 总行数+1 即追加。
    InsertAtLine {
        file: String,
        line: u32,
        text: String,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 用新内容替换 [start_line, end_line]（1-based 含端）。
    ReplaceLines {
        file: String,
        start_line: u32,
        end_line: u32,
        text: String,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 删除 [start_line, end_line]（1-based 含端）。
    DeleteLines {
        file: String,
        start_line: u32,
        end_line: u32,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 代码补全（textDocument/completion）—— AI-friendly 字段裁剪 + 自动推断 trigger。
    /// line/col 为 0-based（与 def/refs 同基线，均直传 LSP Position）。
    Completion {
        file: String,
        line: u32,
        col: u32,
        /// 上限（0 = 不限，默认 5）。
        #[arg(long, default_value_t = 5)]
        limit: u32,
        /// 显式 trigger char（如 "." / "::"）；省略时按 file 后缀自动推断。
        #[arg(long)]
        trigger: Option<String>,
    },
    /// 按位置反查最深层包含符号（documentSymbol walk）。无命中返空数组（合法）。
    /// line/col 为 0-based。
    ContainingSymbol { file: String, line: u32, col: u32 },
    /// 跳到定义并取完整符号信息（def + documentSymbol walk + body 切片）。
    /// def 返空 → null；定义无符号覆盖 → 空数组；C++ 重载等多定义 → 多元素。
    /// line/col 为 0-based。
    DefiningSymbol { file: String, line: u32, col: u32 },

    /// 函数调用位置的参数签名提示（textDocument/signatureHelp）。无调用位置返 null。
    /// line/col 为 0-based。
    SignatureHelp { file: String, line: u32, col: u32 },
    /// 列出可用 codeAction（textDocument/codeAction）。可选 `--kind` 过滤（如 quickfix / refactor）。
    /// line/col 为 0-based。
    CodeAction {
        file: String,
        line: u32,
        col: u32,
        #[arg(long)]
        kind: Option<String>,
    },
    /// 整文件格式化（textDocument/formatting）。返 edits[]；非编辑端用 `jq` 应用。
    Format {
        file: String,
        #[arg(long)]
        tab_size: Option<u32>,
        #[arg(long)]
        insert_spaces: Option<bool>,
    },
    /// range 内格式化（textDocument/rangeFormatting）。返 edits[]。行/列为 0-based。
    FormatRange {
        file: String,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
        #[arg(long)]
        tab_size: Option<u32>,
        #[arg(long)]
        insert_spaces: Option<bool>,
    },
    /// 行范围内类型提示（textDocument/inlayHint）。行号为 0-based。
    InlayHint {
        file: String,
        start_line: u32,
        end_line: u32,
    },
    /// 光标位置的同符号高亮（textDocument/documentHighlight）。line/col 为 0-based。
    DocumentHighlight {
        file: String,
        line: u32,
        col: u32,
    },
    /// 折叠区（textDocument/foldingRange）。
    FoldingRange { file: String },
    /// 语义 token（textDocument/semanticTokens/full）。
    SemanticTokens { file: String },
    /// 代码透镜（textDocument/codeLens）。
    CodeLens { file: String },
    /// 文档链接（textDocument/documentLink）。
    DocumentLink { file: String },
    /// 调用层级：prepare / incoming / outgoing 三件套（callHierarchy/*）。
    /// prepare 的 line/col 为 0-based。
    CallHierarchy {
        /// "prepare" 用 file/line/col；"incoming"/"outgoing" 用 --item。
        op: String,
        file: Option<String>,
        line: Option<u32>,
        col: Option<u32>,
        /// prepare 返的 CallHierarchyItem JSON（incoming/outgoing 必填）。
        #[arg(long)]
        item: Option<String>,
    },
    /// 类型层级：prepare / supertypes / subtypes 三件套（typeHierarchy/*）。
    /// prepare 的 line/col 为 0-based。
    TypeHierarchy {
        op: String,
        file: Option<String>,
        line: Option<u32>,
        col: Option<u32>,
        #[arg(long)]
        item: Option<String>,
    },
    /// 全局符号标识（textDocument/moniker）。line/col 为 0-based。
    Moniker {
        file: String,
        line: u32,
        col: u32,
    },
    /// workspace 级 pull diagnostics（workspace/diagnostic）。
    WorkspaceDiagnostic,
    /// daemon 状态（uptime / pid / loaded LS）。
    Status,
    /// 停掉 daemon（draining + 删 lock）。
    StopAll,
    /// 安装 servers.toml 配置驱动 LS（PATH 探测 → 下载 → sha256 校验 → 落地缓存）。
    /// 手写 T2 语言（rust/python/...）不在此列——按各 LS 官方方式安装。
    /// `--all` 装全表（幂等——已装即跳过，未装按 ensure_launch 路径装）。
    Install {
        /// 单条 lang 装；`--all` 模式忽略。
        #[arg(default_value = "")]
        lang: String,
        /// 装 servers.toml 全部条目。
        #[arg(long)]
        all: bool,
    },
    /// 长连接 shell（stdin/stdout JSONL）。Task 18。
    Shell,
    /// 环境体检（5 类：运行时 / PATH / 本机 LS / daemon / 网络）。
    Doctor {
        /// JSON 输出（默认人类可读）。
        #[arg(long)]
        json: bool,
        /// 尝试自动安装 MISS 的 LS（仅对 servers.toml 已收录的条目）。
        #[arg(long)]
        fix: bool,
    },
}

fn main() -> ExitCode {
    // Windows 主线程默认栈 1MB：clap derive 的大命令枚举在解析期递归构造/
    // 析构会打爆浅栈（Phase 5 stack overflow）。整个入口搬进 16MB 栈线程；
    // tokio multi_thread runtime 语义不变（属性宏挂在内层 async fn）。
    match std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(cli_main)
        .expect("spawn cli main thread")
        .join()
    {
        Ok(code) => code,
        // panic hook 已输出信息；重抛保持原 panic 退出语义（rc=101）。
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn cli_main() -> ExitCode {
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
        // install 内部自建 blocking runtime（下载），必须在阻塞线程跑。
        Some(Cmd::Install { lang, all }) => {
            if *all {
                return tokio::task::spawn_blocking(cmd_install_all)
                    .await
                    .unwrap_or(ExitCode::from(3));
            }
            if lang.is_empty() {
                eprintln!("install: --lang <ID> or --all required");
                return ExitCode::from(2);
            }
            let lang = lang.clone();
            return tokio::task::spawn_blocking(move || cmd_install(&lang))
                .await
                .unwrap_or(ExitCode::from(3));
        }
        Some(Cmd::Doctor { json, fix }) => {
            return cmd_doctor(*json, *fix, &lock_path).await;
        }
        Some(Cmd::Shell) => {}
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
    // 用户未传 --lang 时按 file 后缀/shebang/文件名推断；显式 --lang 优先。
    let effective_lang: Option<String> = cli.lang.clone().or_else(|| autodetect_lang(cli));
    let lang_ref = effective_lang.as_deref();
    let sup = match Supervisor::direct().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor init failed: {e:#}");
            return ExitCode::from(1);
        }
    };
    let res: Result<(), ToolError> = match &cli.cmd {
        Some(Cmd::Overview { file }) => sup
            .tool_overview(&root, file, lang_ref)
            .await
            .and_then(|hits| print_json(&json!(hits))),
        Some(Cmd::SymbolTree { dir, max_files }) => sup
            .tool_symbol_tree(&root, dir, lang_ref, *max_files)
            .await
            .and_then(|tree| print_json(&tree)),
        Some(Cmd::Def { file, line, col }) => sup
            .tool_def(&root, file, *line, *col, lang_ref)
            .await
            .and_then(|opt| print_json(&json!(opt))),
        Some(Cmd::Refs { file, line, col }) => sup
            .tool_refs(&root, file, *line, *col, lang_ref)
            .await
            .and_then(|vec| print_json(&json!(vec))),
        Some(Cmd::Completion {
            file,
            line,
            col,
            limit,
            trigger,
        }) => {
            let trigger = trigger
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| infer_trigger_char(file));
            sup.tool_completion(
                &root,
                file,
                *line,
                *col,
                *limit as usize,
                trigger.as_deref(),
                lang_ref,
            )
            .await
            .and_then(|resp| print_json(&json!(resp)))
        }
        // ==== Phase 1 · 上游 wrapper 缺口（13 个 --direct 路径）====
        Some(Cmd::CodeAction { file, line, col, kind }) => sup
            .tool_code_action(
                &root,
                file,
                *line,
                *col,
                kind.as_deref(),
                lang_ref,
            )
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::Format { file, tab_size, insert_spaces }) => sup
            .tool_format(&root, file, *tab_size, *insert_spaces, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::FormatRange {
            file,
            start_line,
            start_col,
            end_line,
            end_col,
            tab_size,
            insert_spaces,
        }) => sup
            .tool_format_range(
                &root,
                file,
                *start_line,
                *start_col,
                *end_line,
                *end_col,
                *tab_size,
                *insert_spaces,
                lang_ref,
            )
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::InlayHint { file, start_line, end_line }) => sup
            .tool_inlay_hint(&root, file, *start_line, *end_line, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::DocumentHighlight { file, line, col }) => sup
            .tool_document_highlight(&root, file, *line, *col, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::FoldingRange { file }) => sup
            .tool_folding_range(&root, file, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::SemanticTokens { file }) => sup
            .tool_semantic_tokens(&root, file, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::CodeLens { file }) => sup
            .tool_code_lens(&root, file, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::DocumentLink { file }) => sup
            .tool_document_link(&root, file, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::CallHierarchy { op, file, line, col, item }) => handle_call_hierarchy(
            &sup,
            &root,
            op,
            file.as_deref(),
            *line,
            *col,
            item.as_deref(),
            lang_ref,
        )
        .await,
        Some(Cmd::TypeHierarchy { op, file, line, col, item }) => handle_type_hierarchy(
            &sup,
            &root,
            op,
            file.as_deref(),
            *line,
            *col,
            item.as_deref(),
            lang_ref,
        )
        .await,
        Some(Cmd::Moniker { file, line, col }) => sup
            .tool_moniker(&root, file, *line, *col, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
        Some(Cmd::WorkspaceDiagnostic) => sup
            .tool_workspace_diagnostic(&root, lang_ref)
            .await
            .and_then(|v| print_json(&json!(v))),
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
        ToolError::Protocol { .. } => 1,
        ToolError::Serialize(_) => 3,
        ToolError::Core(_) | ToolError::Launch(_) => 3,
    }
}

/// Phase 1 · call-hierarchy 三件套的 --direct 调度（op=prepare|incoming|outgoing）。
///
/// prepare 必填 file/line/col；incoming/outgoing 必填 --item（CallHierarchyItem JSON）。
#[allow(clippy::too_many_arguments)]
async fn handle_call_hierarchy(
    sup: &Supervisor,
    root: &Path,
    op: &str,
    file: Option<&str>,
    line: Option<u32>,
    col: Option<u32>,
    item: Option<&str>,
    lang: Option<&str>,
) -> Result<(), ToolError> {
    match op {
        "prepare" => {
            let f = file.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <file> <line> <col>".into(),
            })?;
            let l = line.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <line>".into(),
            })?;
            let c = col.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <col>".into(),
            })?;
            let items = sup.tool_call_hierarchy_prepare(root, f, l, c, lang).await?;
            print_json(&json!(items))
        }
        "incoming" => {
            let item_str = item.ok_or_else(|| ToolError::BadArgs {
                detail: "incoming requires --item".into(),
            })?;
            let item_val: serde_json::Value =
                serde_json::from_str(item_str).map_err(|e| ToolError::BadArgs {
                    detail: format!("bad --item JSON: {e}"),
                })?;
            let v = sup
                .tool_call_hierarchy_incoming(root, item_val, lang)
                .await?;
            print_json(&json!(v))
        }
        "outgoing" => {
            let item_str = item.ok_or_else(|| ToolError::BadArgs {
                detail: "outgoing requires --item".into(),
            })?;
            let item_val: serde_json::Value =
                serde_json::from_str(item_str).map_err(|e| ToolError::BadArgs {
                    detail: format!("bad --item JSON: {e}"),
                })?;
            let v = sup
                .tool_call_hierarchy_outgoing(root, item_val, lang)
                .await?;
            print_json(&json!(v))
        }
        other => Err(ToolError::BadArgs {
            detail: format!("unknown call-hierarchy op: {other}"),
        }),
    }
}

/// Phase 1 · type-hierarchy 三件套的 --direct 调度（op=prepare|supertypes|subtypes）。
#[allow(clippy::too_many_arguments)]
async fn handle_type_hierarchy(
    sup: &Supervisor,
    root: &Path,
    op: &str,
    file: Option<&str>,
    line: Option<u32>,
    col: Option<u32>,
    item: Option<&str>,
    lang: Option<&str>,
) -> Result<(), ToolError> {
    match op {
        "prepare" => {
            let f = file.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <file> <line> <col>".into(),
            })?;
            let l = line.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <line>".into(),
            })?;
            let c = col.ok_or_else(|| ToolError::BadArgs {
                detail: "prepare requires <col>".into(),
            })?;
            let items = sup.tool_type_hierarchy_prepare(root, f, l, c, lang).await?;
            print_json(&json!(items))
        }
        "supertypes" => {
            let item_str = item.ok_or_else(|| ToolError::BadArgs {
                detail: "supertypes requires --item".into(),
            })?;
            let item_val: serde_json::Value =
                serde_json::from_str(item_str).map_err(|e| ToolError::BadArgs {
                    detail: format!("bad --item JSON: {e}"),
                })?;
            let v = sup
                .tool_type_hierarchy_supertypes(root, item_val, lang)
                .await?;
            print_json(&json!(v))
        }
        "subtypes" => {
            let item_str = item.ok_or_else(|| ToolError::BadArgs {
                detail: "subtypes requires --item".into(),
            })?;
            let item_val: serde_json::Value =
                serde_json::from_str(item_str).map_err(|e| ToolError::BadArgs {
                    detail: format!("bad --item JSON: {e}"),
                })?;
            let v = sup
                .tool_type_hierarchy_subtypes(root, item_val, lang)
                .await?;
            print_json(&json!(v))
        }
        other => Err(ToolError::BadArgs {
            detail: format!("unknown type-hierarchy op: {other}"),
        }),
    }
}

/// 转发模式：探活 → 转发；死 lock → lazy-spawn --daemon → 轮询就绪 → 转发。
async fn forward_or_spawn(cli: &Cli, lock_path: &Path) -> Result<(), String> {
    let entry = daemon::lockfile::read(lock_path).map_err(|e| format!("read lock: {e}"))?;

    let base = match entry {
        Some(e) if daemon::lockfile::is_alive_graceful(e.port) => {
            format!("http://127.0.0.1:{}", e.port)
        }
        _ => {
            // 死 lock（或无 lock）：lazy-spawn。不在这里删 lock——daemon 子进程
            // 的 lock 仲裁会带宽限接管，CLI 无归属凭据先删会误伤启动中/易主 lock。
            let port = spawn_daemon_child()?;
            wait_ready(port, SPAWN_WAIT).await?;
            format!("http://127.0.0.1:{port}")
        }
    };

    let mut token = read_token_with_retry(lock_path).await?;

    // 用户未传 --lang 时按 file 后缀/shebang/文件名推断；显式 --lang 优先。
    let effective_lang = cli.lang.clone().or_else(|| autodetect_lang(cli));
    forward(cli, &base, &mut token, lock_path, effective_lang.as_deref()).await
}

/// 读 lock token；lock 瞬时缺失（bind 赢家尚未写回）时短暂重试，
/// 仍缺失则报错——绝不带空 token 转发（必 403 且掩盖真实状态，bd y2y）。
async fn read_token_with_retry(lock_path: &Path) -> Result<String, String> {
    for _ in 0..10 {
        match daemon::lockfile::read(lock_path) {
            Ok(Some(e)) => return Ok(e.token),
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => return Err(format!("read lock: {e}")),
        }
    }
    Err("daemon is serving but lock is missing; retry the command".into())
}

/// 403 自愈：stop-all × lazy-spawn 交叉时 daemon 会换代，缓存 token 随旧
/// daemon 一起失效——重读 lock 取新 token。token 确实变了返回 `Some(新token)`
/// （调用方应更新缓存并重发一次），否则 `None`（403 另有原因，如实上报）。
async fn refresh_token_if_stale(lock_path: &Path, current: &str) -> Option<String> {
    let fresh = read_token_with_retry(lock_path).await.ok()?;
    if fresh != current {
        Some(fresh)
    } else {
        None
    }
}

/// 从子命令的第一个 file 形参（Pos 0）推断 LanguageId（仅当用户未传 --lang）。
/// 复用 ls-registry::file_detect::detect_language（ext → shebang → filename 三层）。
/// 推断失败 → None（保留现状：缺 lang 时 supervisor 走默认 / 报错）。
fn autodetect_lang(cli: &Cli) -> Option<String> {
    if cli.lang.is_some() {
        return None; // 显式 --lang 优先
    }
    // 取第一个 file 形参：overview / def / refs / hover / diagnostics / read-file /
    // find-implementations / rename-symbol / find-referencing-symbols /
    // find-referencing-code-snippets / symbol-body / completion / containing-symbol /
    // defining-symbol / signature-help / code-action / format / format-range /
    // inlay-hint / document-highlight / folding-range / semantic-tokens / code-lens /
    // document-link / call-hierarchy(prepare) / type-hierarchy(prepare) / moniker /
    // search(pattern but uses pattern, not file) / insert-text-*-symbol / safe-delete-symbol /
    // replace-body / replace-text-in-symbol / delete-text-in-symbol / replace-lines /
    // delete-lines / insert-at-line
    let file_arg: Option<&str> = match &cli.cmd {
        Some(Cmd::Overview { file }) => Some(file),
        Some(Cmd::Def { file, .. }) => Some(file),
        Some(Cmd::Refs { file, .. }) => Some(file),
        Some(Cmd::Hover { file, .. }) => Some(file),
        Some(Cmd::Diagnostics { file, .. }) => Some(file),
        Some(Cmd::ReadFile { file, .. }) => Some(file),
        Some(Cmd::FindImplementations { file, .. }) => Some(file),
        Some(Cmd::RenameSymbol { file, .. }) => Some(file),
        Some(Cmd::FindReferencingSymbols { file, .. }) => Some(file),
        Some(Cmd::FindReferencingCodeSnippets { file, .. }) => Some(file),
        Some(Cmd::SymbolBody { file, .. }) => Some(file),
        Some(Cmd::EditContext { file, .. }) => Some(file),
        Some(Cmd::Completion { file, .. }) => Some(file),
        Some(Cmd::ContainingSymbol { file, .. }) => Some(file),
        Some(Cmd::DefiningSymbol { file, .. }) => Some(file),
        Some(Cmd::SignatureHelp { file, .. }) => Some(file),
        Some(Cmd::CodeAction { file, .. }) => Some(file),
        Some(Cmd::Format { file, .. }) => Some(file),
        Some(Cmd::FormatRange { file, .. }) => Some(file),
        Some(Cmd::InlayHint { file, .. }) => Some(file),
        Some(Cmd::DocumentHighlight { file, .. }) => Some(file),
        Some(Cmd::FoldingRange { file }) => Some(file),
        Some(Cmd::SemanticTokens { file }) => Some(file),
        Some(Cmd::CodeLens { file }) => Some(file),
        Some(Cmd::DocumentLink { file }) => Some(file),
        Some(Cmd::Moniker { file, .. }) => Some(file),
        Some(Cmd::SafeDeleteSymbol { file, .. }) => Some(file),
        Some(Cmd::ReplaceBody { file, .. }) => Some(file),
        Some(Cmd::ReplaceTextInSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertTextBeforeSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertTextAfterSymbol { file, .. }) => Some(file),
        Some(Cmd::DeleteTextInSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertAtLine { file, .. }) => Some(file),
        Some(Cmd::ReplaceLines { file, .. }) => Some(file),
        Some(Cmd::DeleteLines { file, .. }) => Some(file),
        // call-hierarchy/type-hierarchy 的 prepare 模式才有 file
        Some(Cmd::CallHierarchy { op, file, .. }) if op == "prepare" => file.as_deref(),
        Some(Cmd::TypeHierarchy { op, file, .. }) if op == "prepare" => file.as_deref(),
        _ => None,
    };
    let file = file_arg?;
    let path = std::path::Path::new(file);
    // 内置三层探测（扩展名/shebang/文件名）→ external-servers.toml extensions 兜底。
    ls_registry::file_detect::detect_language(path)
        .map(|l| l.as_str().to_string())
        .or_else(|| ls_registry::resolve_lang_name(path).map(str::to_string))
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
        // ponytail: DETACHED_PROCESS 让父进程退出不影响子进程 —— 缺这个 daemon 退随父 CLI。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
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
    let client = http_client();
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
async fn forward(
    cli: &Cli,
    base: &str,
    token: &mut String,
    lock_path: &Path,
    lang: Option<&str>,
) -> Result<(), String> {
    let client = http_client();
    // 工具名与 args 组装。
    let (tool, args): (&str, serde_json::Value) = match &cli.cmd {
        // 本地管理命令已在 main 提前 return；到达此处即编程错误。
        Some(Cmd::Overview { file }) => ("overview", json!({"file": file})),
        Some(Cmd::SymbolTree { dir, max_files }) => (
            "symbol-tree",
            json!({"dir": dir, "max_files": max_files}),
        ),
        Some(Cmd::Def { file, line, col }) => {
            ("def", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Refs { file, line, col }) => {
            ("refs", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Hover { file, line, col }) => {
            ("hover", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Diagnostics { file, wait_gen }) => {
            ("diagnostics", json!({"file": file, "wait_gen": wait_gen}))
        }
        Some(Cmd::FindSymbol { query, limit }) => {
            ("find-symbol", json!({"query": query, "limit": limit}))
        }
        Some(Cmd::FindImplementations { file, line, col }) => (
            "find-implementations",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::RenameSymbol {
            file,
            line,
            col,
            new_name,
        }) => (
            "rename-symbol",
            json!({"file": file, "line": line, "col": col, "new_name": new_name}),
        ),
        Some(Cmd::Search {
            pattern,
            path_glob,
            max_results,
            case_sensitive,
        }) => (
            "search",
            json!({
                "pattern": pattern,
                "path_glob": path_glob,
                "max_results": max_results,
                "case_sensitive": case_sensitive,
            }),
        ),
        Some(Cmd::ReadFile {
            file,
            start_line,
            end_line,
        }) => (
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
        Some(Cmd::FindReferencingCodeSnippets {
            file,
            line,
            col,
            context_lines,
            max_results,
        }) => (
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
        Some(Cmd::EditContext { file, symbol }) => {
            ("edit-context", json!({"file": file, "symbol": symbol}))
        }
        Some(Cmd::RepoMap { top_n }) => ("repo-map", json!({"top_n": top_n})),
        Some(Cmd::ReplaceBody {
            file,
            symbol,
            new_body,
        }) => (
            "replace-body",
            json!({"file": file, "symbol": symbol, "new_body": new_body}),
        ),
        Some(Cmd::ReplaceTextInSymbol {
            file,
            symbol,
            old_text,
            new_text,
        }) => (
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
        Some(Cmd::DeleteTextInSymbol {
            file,
            symbol,
            start_line,
            end_line,
        }) => (
            "delete-text-in-symbol",
            json!({"file": file, "symbol": symbol, "start_line": start_line, "end_line": end_line}),
        ),
        Some(Cmd::SafeDeleteSymbol { file, symbol }) => (
            "safe-delete-symbol",
            json!({"file": file, "symbol": symbol}),
        ),
        Some(Cmd::InsertAtLine {
            file,
            line,
            text,
            expected_hash,
        }) => (
            "insert-at-line",
            json!({"file": file, "line": line, "content": text, "expected_hash": expected_hash}),
        ),
        Some(Cmd::ReplaceLines {
            file,
            start_line,
            end_line,
            text,
            expected_hash,
        }) => (
            "replace-lines",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
                "content": text,
                "expected_hash": expected_hash,
            }),
        ),
        Some(Cmd::DeleteLines {
            file,
            start_line,
            end_line,
            expected_hash,
        }) => (
            "delete-lines",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
                "expected_hash": expected_hash,
            }),
        ),
        Some(Cmd::Completion {
            file,
            line,
            col,
            limit,
            trigger,
        }) => {
            // trigger=None 时按 file 后缀自动推断（C++=., Rust=::, TS/JS/Py=.）；
            // 显式传 "" 也视为 None（agent 端不需要感知 7 种扩展名）。
            let trigger = trigger
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| infer_trigger_char(file));
            (
                "completion",
                json!({
                    "file": file,
                    "line": line,
                    "col": col,
                    "limit": limit,
                    "trigger": trigger,
                }),
            )
        }
        Some(Cmd::ContainingSymbol { file, line, col }) => (
            "containing-symbol",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),
        Some(Cmd::DefiningSymbol { file, line, col }) => (
            "defining-symbol",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),

        Some(Cmd::SignatureHelp { file, line, col }) => (
            "signature-help",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),
        // ==== Phase 1 · 上游 wrapper 缺口（13 个）====
        Some(Cmd::CodeAction { file, line, col, kind }) => (
            "code-action",
            json!({"file": file, "line": line, "col": col, "kind": kind}),
        ),
        Some(Cmd::Format { file, tab_size, insert_spaces }) => (
            "format",
            json!({
                "file": file,
                "tab_size": tab_size,
                "insert_spaces": insert_spaces,
            }),
        ),
        Some(Cmd::FormatRange {
            file,
            start_line,
            start_col,
            end_line,
            end_col,
            tab_size,
            insert_spaces,
        }) => (
            "format-range",
            json!({
                "file": file,
                "start_line": start_line,
                "start_col": start_col,
                "end_line": end_line,
                "end_col": end_col,
                "tab_size": tab_size,
                "insert_spaces": insert_spaces,
            }),
        ),
        Some(Cmd::InlayHint {
            file,
            start_line,
            end_line,
        }) => (
            "inlay-hint",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
            }),
        ),
        Some(Cmd::DocumentHighlight { file, line, col }) => (
            "document-highlight",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::FoldingRange { file }) => ("folding-range", json!({"file": file})),
        Some(Cmd::SemanticTokens { file }) => {
            ("semantic-tokens", json!({"file": file}))
        }
        Some(Cmd::CodeLens { file }) => ("code-lens", json!({"file": file})),
        Some(Cmd::DocumentLink { file }) => {
            ("document-link", json!({"file": file}))
        }
        Some(Cmd::CallHierarchy { op, file, line, col, item }) => {
            let mut args = json!({"op": op});
            if let (Some(f), Some(l), Some(c)) = (file, line, col) {
                args["file"] = json!(f);
                args["line"] = json!(l);
                args["col"] = json!(c);
            }
            if let Some(it) = item {
                // 解析 agent 传入的 JSON item 字符串；解析失败留原串（supervisor 端会拒）。
                args["item"] = serde_json::from_str(it)
                    .unwrap_or_else(|_| serde_json::Value::String(it.clone()));
            }
            ("call-hierarchy", args)
        }
        Some(Cmd::TypeHierarchy { op, file, line, col, item }) => {
            let mut args = json!({"op": op});
            if let (Some(f), Some(l), Some(c)) = (file, line, col) {
                args["file"] = json!(f);
                args["line"] = json!(l);
                args["col"] = json!(c);
            }
            if let Some(it) = item {
                args["item"] = serde_json::from_str(it)
                    .unwrap_or_else(|_| serde_json::Value::String(it.clone()));
            }
            ("type-hierarchy", args)
        }
        Some(Cmd::Moniker { file, line, col }) => (
            "moniker",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::WorkspaceDiagnostic) => ("workspace-diagnostic", json!({})),
        Some(Cmd::Status)
        | Some(Cmd::StopAll)
        | Some(Cmd::Install { .. })
        | Some(Cmd::Shell)
        | Some(Cmd::Doctor { .. })
        | None => {
            unreachable!("handled earlier")
        }
    };
    let project_root = resolve_project_root(cli.project.clone());
    // Phase 4 基建 Task 22b：CLI flag → args 私有字段 → supervisor 三层合并。
    let mut args = args;
    inject_timeout_args(&mut args, cli.request_timeout, cli.index_timeout);
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
        "lang": lang,
    });

    let url = format!("{base}/tools/{tool}");
    let mut resp = client
        .post(&url)
        .header("X-Serena-Token", &*token)
        .json(&body)
        .timeout(FORWARD_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("forward {tool}: {e}"))?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        && let Some(fresh) = refresh_token_if_stale(lock_path, token).await
    {
        // daemon 换代后缓存 token 过期：已刷新，用新 token 重发一次。
        *token = fresh;
        resp = client
            .post(&url)
            .header("X-Serena-Token", &*token)
            .json(&body)
            .timeout(FORWARD_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("forward {tool}: {e}"))?;
    }

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
            // Δ 43ae021：exit 码按 wire code 取（ARCH §6.3 / dto::wire_error_code_to_exit），
            // 不再一律 1 —— Internal→3、BadArgs→2，agent 据此免重试确定性失败。
            let code = err
                .get("code")
                .and_then(|c| serde_json::from_value::<daemon::dto::WireErrorCode>(c.clone()).ok());
            std::process::exit(i32::from(
                code.map_or(1u8, daemon::dto::wire_error_code_to_exit),
            ));
        }
    }
}

/// `install` 子命令（Task 21）：配置驱动 LS 安装（幂等——已装即返回路径）。
/// `source` 字段标注条目来源（external-servers.toml / 内置 servers.toml）。
fn cmd_install(lang: &str) -> ExitCode {
    match ls_registry::config::ensure_launch(lang, None, true, false) {
        Ok((exe, args)) => {
            // args = expand_exec 完整 argv（首元素即 exe）。
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "lang": lang,
                    "source": ls_registry::config::spec_source(lang).unwrap_or("builtin"),
                    "exe": exe.display().to_string(),
                    "cmd": args,
                }))
                .unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("install failed: {msg}");
            ExitCode::from(3)
        }
    }
}

/// `install --all`：遍历 servers.toml 全部条目（内置 + external-servers.toml）逐个
/// ensure_launch。幂等——已装返 Ready（无下载），未装走 ensure_launch 完整下载路径。
/// 累计统计 ok / skipped / failed，最后输出 JSON。
fn cmd_install_all() -> ExitCode {
    let ids: Vec<&str> = ls_registry::config::all_server_ids().collect();
    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for id in ids {
        match ls_registry::config::ensure_launch(id, None, true, false) {
            Ok((exe, _args)) => {
                ok += 1;
                let src = ls_registry::config::spec_source(id).unwrap_or("builtin");
                eprintln!("[OK]    {id:<28} -> {}/{}", exe.display(), src);
            }
            Err(msg) => {
                failed.push((id.to_string(), msg.clone()));
                eprintln!("[FAIL]  {id:<28} {msg}");
            }
        }
    }
    let total = ok + failed.len();
    let payload = json!({
        "ok": failed.is_empty(),
        "total": total,
        "installed": ok,
        "failed": failed.iter().map(|(id, m)| json!({"id": id, "msg": m})).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `doctor` 子命令：5 类体检 + 可选 --fix 自动装 MISS 的 LS。
async fn cmd_doctor(json: bool, fix: bool, lock_path: &Path) -> ExitCode {
    let report = supervisor::doctor::run_all(lock_path);
    // 可选：--fix 尝试装 MISS 的 server 类别条目
    if fix {
        for c in &report.checks {
            if c.status == supervisor::doctor::Status::Miss
                && c.category == "ls"
            {
                // `id` 是 server name（如 rust-analyzer）—— 不一定在 servers.toml
                // （如 csharp-ls 是 dotnet tool）；只对 spec_for 能命中的跑 ensure_launch。
                if ls_registry::config::spec_for(c.id).is_some() {
                    let _ = ls_registry::config::ensure_launch(c.id, None, true, false);
                }
            }
        }
    }
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => {
                println!("{s}");
            }
            Err(e) => {
                eprintln!("json serialize failed: {e}");
                return ExitCode::from(3);
            }
        }
    } else {
        print!("{}", supervisor::doctor::format_text(&report));
    }
    ExitCode::from(report.exit_code())
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
    let client = http_client();
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
    let client = http_client();
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
            // 端口死/请求失败：lock 留给 daemon 仲裁路径接管清理。
            // CLI 无归属凭据，此处的 daemon 可能正在 draining 收尾或 lock 已易主——
            // 无条件删会制造孤儿（bd y2y）。
            eprintln!("shutdown probe failed: {other:?}; lock left for lazy-spawn arbitration");
            ExitCode::from(3)
        }
    }
}

/// Print JSON pretty; map serde_json errors to ToolError::Serialize (uniform exit 3 path).
fn print_json(v: &serde_json::Value) -> Result<(), ToolError> {
    println!(
        "{}",
        serde_json::to_string_pretty(v).map_err(|e| ToolError::Serialize(e.into()))?
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
    let mut base_token = match ensure_daemon(&lock_path).await {
        Ok(b) => b,
        Err(e) => {
            // shell 启动失败也要在 stdout 留 JSON，便于 agent 解析。
            println!(r#"{{"id":null,"ok":false,"error":"{}"}}"#, json_escape(&e));
            return ExitCode::from(3);
        }
    };

    let client = http_client();
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
            &mut base_token,
            &lock_path,
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
        Some(e) if daemon::lockfile::is_alive_graceful(e.port) => {
            format!("http://127.0.0.1:{}", e.port)
        }
        _ => {
            // 同 forward_or_spawn：不删 lock，交 daemon 子进程仲裁接管。
            let port = spawn_daemon_child()?;
            wait_ready(port, SPAWN_WAIT).await?;
            format!("http://127.0.0.1:{port}")
        }
    };
    let token = read_token_with_retry(lock_path).await?;
    Ok((base, token))
}

/// 单条 shell 命令：HTTP 转发到 daemon。
async fn dispatch_shell_cmd(
    client: &reqwest::Client,
    base_token: &mut (String, String),
    lock_path: &Path,
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
        | "symbol-tree"
        | "hover"
        | "diagnostics"
        | "def"
        | "refs"
        | "containing-symbol"
        | "defining-symbol"
        | "signature-help"
        | "symbol-body"
        | "replace-body"
        | "completion"
        | "search"
        | "find-symbol"
        | "find-implementations"
        | "rename-symbol"
        | "code-action"
        | "format"
        | "format-range"
        | "inlay-hint"
        | "document-highlight"
        | "folding-range"
        | "semantic-tokens"
        | "code-lens"
        | "document-link"
        | "call-hierarchy"
        | "type-hierarchy"
        | "moniker"
        | "workspace-diagnostic"
        | "read-file"
        | "list-dir"
        | "find-file"
        | "find-referencing-symbols"
        | "find-referencing-code-snippets"
        | "replace-text-in-symbol"
        | "insert-text-after-symbol"
        | "insert-text-before-symbol"
        | "delete-text-in-symbol"
        | "safe-delete-symbol"
        | "insert-at-line"
        | "replace-lines"
        | "delete-lines" => cmd,
        other => return Err(format!("unknown cmd: {other}")),
    };
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
    });
    let url = format!("{}/tools/{tool}", base_token.0);
    let send_tool = |token: &String| {
        client
            .post(&url)
            .header("X-Serena-Token", token)
            .json(&body)
            .timeout(FORWARD_TIMEOUT)
            .send()
    };
    let mut resp = send_tool(&base_token.1)
        .await
        .map_err(|e| format!("forward {tool}: {e}"))?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        && let Some(fresh) = refresh_token_if_stale(lock_path, &base_token.1).await
    {
        // daemon 换代后旧 token 过期：已刷新，用新 token 重发一次。
        base_token.1 = fresh;
        resp = send_tool(&base_token.1)
            .await
            .map_err(|e| format!("forward {tool}: {e}"))?;
    }
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

/// 按 file 后缀推断 LSP `textDocument/completion` 的 triggerCharacter。
/// 仅当 agent 显式不传 trigger 时启用（C++ / Rust / TS / JS / Py 共 5 系）。
/// ponytail: 这是文件后缀到 trigger 字符的固定映射表，新加 lang 时补一行即可，
///          不必上配置。
fn infer_trigger_char(file: &str) -> Option<String> {
    let ext = file.rsplit('.').next()?.to_ascii_lowercase();
    let ch: &'static str = match ext.as_str() {
        "cpp" | "c" | "cc" | "cxx" | "h" | "hpp" | "hxx" => ".",
        "rs" => "::",
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" => ".",
        _ => return None,
    };
    Some(ch.to_owned())
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

/// Phase 4 基建 Task 22b：把 CLI flag `--request-timeout` / `--index-timeout` 注入
/// `args._timeout_ms` / `args._index_timeout_ms` 私有字段（supervisor
/// `execute_tool` 入口 `sanitize_timeout_args` 会清掉）。`args` 必须是 object
/// 形态；其他形态（少见，子命令可能返 null/array）静默跳过。
fn inject_timeout_args(args: &mut serde_json::Value, req_ms: Option<u32>, idx_ms: Option<u32>) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    if let Some(ms) = req_ms {
        obj.insert("_timeout_ms".into(), serde_json::json!(ms));
    }
    if let Some(ms) = idx_ms {
        obj.insert("_index_timeout_ms".into(), serde_json::json!(ms));
    }
}
