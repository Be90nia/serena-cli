//! 独立 daemon serve binary —— smoke 测试专用。
//!
//! 与 cli --daemon 等效，但用 current_thread runtime 避开 cli 的
//! multi_thread stack overflow 兼容问题（pre-existing，phase 5 待修）。
//!
//! 用法：daemon_serve_bin  → 起 daemon，监听 /shutdown，结束后 process::exit(0)。

use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let lock_path: PathBuf = std::env::var("SERENA_DAEMON_LOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            #[cfg(windows)]
            {
                let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
                PathBuf::from(base).join("serena").join("daemon.lock")
            }
            #[cfg(not(windows))]
            {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".serena").join("daemon.lock")
            }
        });

    let port: u16 = std::env::var("SERENA_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7860);

    let cfg = daemon::serve::ServeConfig {
        port,
        lock_path,
        intervals: daemon::reaper::ReaperIntervals {
            scan: std::time::Duration::from_secs(30),
            ls_idle: std::time::Duration::from_secs(600),
            global_idle: std::time::Duration::from_secs(900),
            max_loaded_ls: 3,
        },
    };
    daemon::serve::serve(cfg).await
}
