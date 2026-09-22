//! daemon serve 入口（PLAN Task 16 的 daemon 侧；lock 仲裁 + axum + reaper 装配）。
//!
//! 流程：建 lock 父目录 → bind 端口（OS 排他仲裁）→ try_become_daemon → spawn
//! reaper → axum::serve（阻塞直至 shutdown）。败者不该走到这里（CLI 转发即可）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use supervisor::Supervisor;

use crate::http::{AppState, router};
use crate::lockfile::{self, LockEntry, Outcome};
use crate::reaper::{ReaperIntervals, spawn_reaper};

/// serve 配置。
pub struct ServeConfig {
    /// 监听端口（lock 中回填）。
    pub port: u16,
    /// lock 文件路径。
    pub lock_path: PathBuf,
    /// reaper 参数。
    pub intervals: ReaperIntervals,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            port: 7860,
            lock_path: default_lock_path(),
            intervals: ReaperIntervals::default(),
        }
    }
}

/// `%LOCALAPPDATA%/serena/daemon.lock`（非 Windows 落 `~/.serena/daemon.lock`）。
pub fn default_lock_path() -> PathBuf {
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
}

/// 工具调用重放日志（d3a）：daemon.lock 同目录 `invocations.jsonl`。
/// 行首键即 invocation_id，`grep <id> invocations.jsonl` 即索引。
pub fn default_invocation_log_path() -> PathBuf {
    default_lock_path()
        .parent()
        .map(|p| p.join("invocations.jsonl"))
        .unwrap_or_else(|| PathBuf::from("invocations.jsonl"))
}

/// daemon serve 主入口。胜者才走到 axum::serve（阻塞直至 shutdown）。
pub async fn serve(cfg: ServeConfig) -> anyhow::Result<()> {
    // lock 父目录可能不存在（首次运行）。
    if let Some(parent) = cfg.lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // bind 先于 lock 仲裁：端口的 OS 排他性是第一道仲裁，bind 输家直接退出、
    // 不触碰 lock——lock 只由 bind 赢家创建/接管。否则"动过 lock 却起不来"
    // 的进程会删掉真主人的 lock，制造无 lock 孤儿 + 空 token 403（bd y2y）。
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr).await
        .map_err(|e| anyhow::anyhow!("bind {addr} failed: {e}; not starting daemon"))?;
    // lock 仲裁：败者直接退出（正常路径 CLI 已探活转发，不会走到这）。
    let outcome = lockfile::try_become_daemon(&cfg.lock_path, cfg.port)?;
    let (token, own_boot) = match outcome {
        Outcome::Won {
            token,
            boot_ms,
            ..
        } => (token, boot_ms),
        Outcome::Lost { addr } => {
            anyhow::bail!("another daemon already at {addr}; not starting a second one")
        }
    };

let sup = Arc::new(Supervisor::direct().await?);
    let state = AppState {
        supervisor: sup.clone(),
        token: Arc::new(token),
        start_ts: Instant::now(),
        loaded_ls: Arc::new(Mutex::new(vec![])),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_project: Arc::new(std::sync::Mutex::new(None)),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        // envelope 日志（d3a）：生产路径写 daemon.lock 同目录 invocations.jsonl。
        invocation_log_path: default_invocation_log_path(),
        // 15s：上限而非固定窗——in-flight 归零即退（空载秒退）。压测 20
        // 并发的残余请求流 ~10-12s，10s 上限时窗口外仍有 ~17% refused；
        // 15s 把 stop-all 触发后仍在途的请求基本都覆盖成 503 DAEMON_DRAINING。
        drain_window: std::time::Duration::from_secs(15),
    };

    // reaper 常驻：draining → 删 lock → 卸 LS。lock 归属戳 (path, boot_ms)
    // 供收尾 remove_owned 校验——防止删掉接管者的 lock。
    // 末尾 await reaper 让 main 自然返回：graceful shutdown 让 axum::serve 退出，
    // reaper 跑完 finish_shutdown 后再返。Windows Job 句柄随进程关闭，
    // LS 进程树陪葬（ARCH §3.2）。
    let reaper = spawn_reaper(
        sup,
        state.clone(),
        cfg.intervals,
        Some((cfg.lock_path.clone(), own_boot)),
    );

    let app = router(state.clone());
    tracing::info!("daemon listening on {addr}");
    // graceful shutdown 桥接：/shutdown POST 在 http::shutdown_post 中调
    // state.shutdown_notify.notify_waiters()，此处 await notified 触发退出。
    let shutdown_signal = state.shutdown_notify.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_signal.notified().await })
        .await?;
    // axum 退 → 等 reaper 完成 finish_shutdown（删 lock + 卸 LS）→ 返回。
    let _ = reaper.await;
    Ok(())
}

/// 把 lock 回填最终端口（bind 成功后调；M1 端口固定所以基本 no-op）。
#[allow(dead_code)]
pub fn backfill_lock(lock_path: &Path, entry: &LockEntry) -> anyhow::Result<()> {
    lockfile::write_final(lock_path, entry)?;
    Ok(())
}
