//! serena-cli —— single-binary LSP CLI (PLAN Task 10 / ARCHITECTURE §2).
//!
//! M0 落地 `--direct --project <root> overview|def|refs <arg>`：
//! 单进程直连 clangd 跑只读三工具（PLAN Task 10 / ARCH §0）。
//! M1 加 `--daemon`（HTTP 转发 / lazy-spawn）。
//!
//! Windows 启动即 `SetConsoleOutputCP(65001)` —— 中文 Windows conhost / PowerShell / cmd
//! 默认 GBK 码页，不设则 UTF-8 输出乱码（ARCH §8 注）。
//!
//! Exit code 协议（ARCH §6.3）：
//! - 0  工具成功
//! - 1  LS 未安装 / 启动失败（ToolError::NotInstalled / Launch）
//! - 2  用户参数错（ToolError::BadArgs）
//! - 3  LSP 协议层错（ToolError::Core：RPC / 超时 / 进程崩溃）
//!
//! ponytail: 不为 M0 引入 stderr coloring / pretty-print —— JSON 一行，便于 agent 解析。
//! M1 加 --json 时已是结构化输出。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;
use supervisor::{Supervisor, ToolError};

/// M0 直接模式 CLI 入口。
#[derive(Parser, Debug)]
#[command(
    name = "serena-cli",
    version,
    about = "serena-rust LSP CLI (M0 --direct)"
)]
struct Cli {
    /// 直连模式：不走 daemon，单进程内拉 clangd 并直调（PLAN Task 10 / ARCH §2 A7）。
    #[arg(long)]
    direct: bool,

    /// 项目根（clangd 的 `rootUri` + ls-registry 实例键）。
    #[arg(long, value_name = "ROOT")]
    project: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出文件顶层符号（递归 children）。
    ///
    /// 用法：`--direct --project <root> overview <file>`
    Overview { file: String },
    /// 跳转到定义。
    ///
    /// 用法：`--direct --project <root> def <file> <line> <col>`（line/col 0-based）
    Def { file: String, line: u32, col: u32 },
    /// 列出引用。
    ///
    /// 用法：`--direct --project <root> refs <file> <line> <col>`（line/col 0-based）
    Refs { file: String, line: u32, col: u32 },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    // Windows 中文乱码 fix —— ARCH §8 注。
    #[cfg(windows)]
    unsafe {
        let r = windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
        if r == 0 {
            eprintln!("warning: SetConsoleOutputCP(65001) failed");
        }
    }

    let cli = Cli::parse();
    if !cli.direct {
        eprintln!("--direct is the only supported mode in M0 (daemon mode is M1)");
        return ExitCode::from(2);
    }

    let root = dunce::canonicalize(&cli.project).unwrap_or_else(|_| cli.project.clone());

    let sup = match Supervisor::direct().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor init failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    let res: Result<(), ToolError> = match cli.cmd {
        Cmd::Overview { file } => sup
            .tool_overview(&root, &file)
            .await
            .and_then(|hits| print_json(&json!(hits))),
        Cmd::Def { file, line, col } => sup
            .tool_def(&root, &file, line, col)
            .await
            .and_then(|opt| print_json(&json!(opt))),
        Cmd::Refs { file, line, col } => sup
            .tool_refs(&root, &file, line, col)
            .await
            .and_then(|vec| print_json(&json!(vec))),
    };

    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let exit = match &e {
                ToolError::NotInstalled { language, hint } => {
                    eprintln!(
                        "language server for `{language}` not installed; install_hint: {hint}"
                    );
                    1
                }
                ToolError::BadArgs { detail } => {
                    eprintln!("bad args: {detail}");
                    2
                }
                ToolError::Core(_) | ToolError::Launch(_) => {
                    eprintln!("LSP error: {e}");
                    3
                }
            };
            ExitCode::from(exit)
        }
    }
}

/// Print JSON pretty; map serde_json errors to ToolError::Launch (uniform exit 3 path).
fn print_json(v: &serde_json::Value) -> Result<(), ToolError> {
    let s = serde_json::to_string_pretty(v).map_err(|e| ToolError::Launch(anyhow::anyhow!(e)))?;
    println!("{s}");
    Ok(())
}
