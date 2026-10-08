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
            // bd j8b：idle 阈值环境变量可配（SERENA_IDLE_TIMEOUT_SECS /
            // SERENA_LS_IDLE_EVICTION_SECS），缺省与 ReaperIntervals::default 一致。
            intervals: crate::reaper::intervals_from_env(),
        }
    }
}

/// lock 路径：Windows `%LOCALAPPDATA%/serena/daemon.lock`；Unix 优先
/// `$XDG_RUNTIME_DIR/serena/daemon.lock`（runtime dir 是 tmpfs + 0700，lock 含
/// token 落这里比家目录更严，bd t5ji），未设 XDG_RUNTIME_DIR 回落
/// `~/.serena/daemon.lock`（既有布局，老升级路径不受影响）。
pub fn default_lock_path() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        PathBuf::from(base).join("serena").join("daemon.lock")
    }
    #[cfg(not(windows))]
    {
        unix_lock_path(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), std::env::var("HOME").ok().as_deref())
    }
}

/// `default_lock_path` 的 Unix 分支纯函数（参数注入供单测，不碰进程 env）。
#[cfg(not(windows))]
fn unix_lock_path(runtime_dir: Option<&std::ffi::OsStr>, home: Option<&str>) -> PathBuf {
    if let Some(runtime) = runtime_dir {
        return PathBuf::from(runtime).join("serena").join("daemon.lock");
    }
    PathBuf::from(home.unwrap_or(".")).join(".serena").join("daemon.lock")
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
    // bd xwi：启动接管超大旧日志（实证 164MB 场景）先轮转再续写，避免
    // 单代无上界。失败只影响本轮不轮转，不拦 daemon 启动。
    crate::http::rotate_invocation_log_at_startup(&default_invocation_log_path());
    // bind 先于 lock 仲裁：端口的 OS 排他性是第一道仲裁，bind 输家直接退出、
    // 不触碰 lock——lock 只由 bind 赢家创建/接管。否则"动过 lock 却起不来"
    // 的进程会删掉真主人的 lock，制造无 lock 孤儿 + 空 token 403（bd y2y）。
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr} failed: {e}; not starting daemon"))?;
    // lock 仲裁：败者直接退出（正常路径 CLI 已探活转发，不会走到这）。
    let outcome = lockfile::try_become_daemon(&cfg.lock_path, cfg.port)?;
    let (token, own_boot) = match outcome {
        Outcome::Won { token, boot_ms, .. } => (token, boot_ms),
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
        // 7rh：SERENA_NO_TOKEN_ESTIMATE=1 → 工具成功响应不附 ~tokens 估算。
        no_token_estimate: std::env::var("SERENA_NO_TOKEN_ESTIMATE").ok().as_deref() == Some("1"),
        // bd 7tk/e1p：观测面（/status 四字段 + invocation 日志增强）。
        obs: crate::http::ObsState::default(),
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
    // Ctrl-C/SIGINT（bd 1cv2）：tokio::signal::ctrl_c 三平台统一覆盖
    // （Unix SIGINT；Windows SetConsoleCtrlHandler），走与 /shutdown 同一套
    // signal_shutdown 收尾。
    let signal_state = state.clone();
    let shutdown_signal = state.shutdown_notify.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown_signal.notified() => {}
                _ = tokio::signal::ctrl_c() => signal_shutdown(&signal_state).await,
            }
        })
        .await?;
    // axum 退 → 等 reaper 完成 finish_shutdown（删 lock + 卸 LS）→ 返回。
    let _ = reaper.await;
    Ok(())
}

/// Ctrl-C / SIGINT 收尾（bd 1cv2）：与 /shutdown 同一套 draining 协议——置
/// draining（新请求拿 503 DAEMON_DRAINING）→ 排空窗口 → 广播停机通知；reaper
/// 被 notify 唤醒后走 finish_shutdown（删 lock + 卸 LS + exit 0）。
pub async fn signal_shutdown(state: &AppState) {
    if state
        .draining
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        // /shutdown 或 idle 自杀已在收尾：其自身路径会 notify，这里不重复排空。
        return;
    }
    tracing::info!("SIGINT/Ctrl-C received; entering ShutdownDraining");
    state.wait_drain().await;
    state.shutdown_notify.notify_waiters();
}

/// 把 lock 回填最终端口（bind 成功后调；M1 端口固定所以基本 no-op）。
#[allow(dead_code)]
pub fn backfill_lock(lock_path: &Path, entry: &LockEntry) -> anyhow::Result<()> {
    lockfile::write_final(lock_path, entry)?;
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    /// bd t5ji：XDG_RUNTIME_DIR 优先（tmpfs 0700，lock 含 token），缺省回
    /// ~/.serena（既有布局）。纯函数注入，不碰进程 env。
    #[test]
    fn unix_lock_path_prefers_xdg_runtime_dir() {
        use std::ffi::OsStr;
        let with_xdg = unix_lock_path(Some(OsStr::new("/run/user/1000")), Some("/home/u"));
        assert_eq!(
            with_xdg,
            PathBuf::from("/run/user/1000/serena/daemon.lock"),
            "XDG_RUNTIME_DIR 设置时必须优先"
        );
        let without_xdg = unix_lock_path(None, Some("/home/u"));
        assert_eq!(
            without_xdg,
            PathBuf::from("/home/u/.serena/daemon.lock"),
            "未设 XDG_RUNTIME_DIR 回落家目录"
        );
        let nothing = unix_lock_path(None, None);
        assert_eq!(
            nothing,
            PathBuf::from("./.serena/daemon.lock"),
            "全缺省落到 cwd（既有 unwrap_or 语义）"
        );
    }
}
