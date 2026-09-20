//! 独立 cli smoke 测试 binary —— 绕过 cli main 的 #[tokio::main(multi_thread, 2)]
//! stack overflow 兼容问题（pre-existing，phase 5 待修；见 e2e_smoke9.sh L12-13）。
//!
//! 用 current_thread runtime + 手写子命令分发：
//!   `cli_run_bin doctor [--json] [--fix]`
//!   `cli_run_bin install-all`
//!   `cli_run_bin read-file <path>`      （--direct 路径，验证 auto-detect lang）
//!
//! 直接调用 crates/cli 的 cmd_doctor / cmd_install_all / Supervisor::direct()，
//! 不走 clap derive 解析 → 不触发主线程 stack overflow。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: cli_run_bin <doctor|install-all|read-file> ...");
        return ExitCode::from(2);
    }
    let lock_path = daemon::serve::default_lock_path();
    match args[1].as_str() {
        "doctor" => {
            let json = args.iter().any(|a| a == "--json");
            let fix = args.iter().any(|a| a == "--fix");
            run_doctor(json, fix, &lock_path).await
        }
        "install-all" => tokio::task::spawn_blocking(cmd_install_all)
            .await
            .unwrap_or(ExitCode::from(3)),
        "read-file" => {
            if args.len() < 3 {
                eprintln!("usage: cli_run_bin read-file <path>");
                return ExitCode::from(2);
            }
            run_read_file(Path::new(&args[2])).await
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            ExitCode::from(2)
        }
    }
}

async fn run_doctor(json: bool, fix: bool, lock_path: &Path) -> ExitCode {
    let report = supervisor::doctor::run_all(lock_path);
    if fix {
        for c in &report.checks {
            if c.status == supervisor::doctor::Status::Miss
                && c.category == "ls"
                && ls_registry::config::spec_for(c.id).is_some()
            {
                let _ = ls_registry::config::ensure_launch(c.id, None, true, false);
            }
        }
    }
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("json: {e}");
                return ExitCode::from(3);
            }
        }
    } else {
        print!("{}", supervisor::doctor::format_text(&report));
    }
    ExitCode::from(report.exit_code())
}

fn cmd_install_all() -> ExitCode {
    let ids: Vec<&str> = ls_registry::config::all_server_ids().collect();
    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for id in ids {
        match ls_registry::config::ensure_launch(id, None, true, false) {
            Ok((exe, _args)) => {
                ok += 1;
                eprintln!("[OK]    {id:<28} -> {}", exe.display());
            }
            Err(msg) => {
                failed.push((id.to_string(), msg.clone()));
                eprintln!("[FAIL]  {id:<28} {msg}");
            }
        }
    }
    let total = ok + failed.len();
    let payload = serde_json::json!({
        "ok": failed.is_empty(),
        "total": total,
        "installed": ok,
        "failed": failed.iter().map(|(id, m)| serde_json::json!({"id": id, "msg": m})).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

async fn run_read_file(file: &Path) -> ExitCode {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // 不传 --lang → 走 detect_language 推断（仅打印不传值给 fs_tools::read_file，
    // 它本身就是纯文件 I/O，不依赖 LanguageId；接 LS 才需要 lang）。
    let lang = ls_registry::file_detect::detect_language(file)
        .map(|l| l.as_str().to_string());
    eprintln!("[autodetect] {} -> {:?}", file.display(), lang);
    let rel = file.strip_prefix(&root).unwrap_or(file).to_string_lossy().to_string();
    match supervisor::fs_tools::read_file(&root, &rel, None, None).await {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("read-file: {e}");
            ExitCode::from(1)
        }
    }
}
