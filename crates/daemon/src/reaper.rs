//! IdleReaper + ShutdownDraining + LRU（PLAN Task 14 / DESIGN §3 I8 / ARCHITECTURE §3）。
//!
//! 常驻 tokio task，30s 巡检（测试可调到 100ms 级）：
//! - 单 LS 10min 未用 → 卸载
//! - 超 `max_loaded_ls=3` → LRU 驱逐（卸最久未用的）
//! - 全局 15min 空闲 → ShutdownDraining：503 + Retry-After → 等 in-flight ≤10s
//!   → 逐 LS shutdown（单 LS 5s 超时转 kill）→ 删 lock → exit 0
//!
//! ponytail: 全部状态复用 supervisor.last_used + daemon AppState.draining，
//! 不另建 reaper 私有状态表。

use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use supervisor::Supervisor;

use crate::http::AppState;
use crate::lockfile;

/// 巡检参数（生产值；测试调小到 100ms 级）。
#[derive(Debug, Clone, Copy)]
pub struct ReaperIntervals {
    /// 巡检 tick 周期。
    pub scan: Duration,
    /// 单 LS 最大空闲时长（超过即卸载）。
    pub ls_idle: Duration,
    /// 全局最大空闲时长（超过即 ShutdownDraining + exit）。
    pub global_idle: Duration,
    /// 同时加载的 LS 上限（LRU 驱逐线）。
    pub max_loaded_ls: usize,
}

impl Default for ReaperIntervals {
    fn default() -> Self {
        Self {
            scan: Duration::from_secs(30),
            ls_idle: Duration::from_secs(10 * 60),
            global_idle: Duration::from_secs(15 * 60),
            max_loaded_ls: 3,
        }
    }
}

/// 全局 last-activity 时间戳；daemon 启动时初始化，http 层工具调用后刷新。
static GLOBAL_LAST_ACTIVITY: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now()));

/// 工具调用成功后由 http 层调用：刷新全局活跃时钟。
pub fn note_activity() {
    *GLOBAL_LAST_ACTIVITY.lock().unwrap() = Instant::now();
}

/// 启动 reaper 常驻 task。返回 JoinHandle 供测试/停机取消。
///
/// `lock_path` 用于 shutdown 收尾删除 lock 文件；`None`（如测试）不删。
pub fn spawn_reaper(
    sup: Arc<Supervisor>,
    state: AppState,
    intervals: ReaperIntervals,
    lock_path: Option<std::path::PathBuf>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        reaper_loop(sup, state, intervals, lock_path).await;
    })
}

async fn reaper_loop(
    sup: Arc<Supervisor>,
    state: AppState,
    iv: ReaperIntervals,
    lock_path: Option<std::path::PathBuf>,
) {
    loop {
        tokio::time::sleep(iv.scan).await;

        // 已在 draining：走收尾并退出 task（进程随后自然退出）。
        if state.draining.load(Ordering::Acquire) {
            finish_shutdown(&sup, &state, &lock_path).await;
            return;
        }
        let now = Instant::now();

        // 0) Failed LS 驱逐：LS 死后 state 变 Failed, 下次工具调用才被动 evict.
        //    这里主动驱逐, 下次调用 session_for 慢路径自动 spawn 新实例。
        let evicted_failed = sup.evict_failed_instances().await;
        if evicted_failed > 0 {
            tracing::info!(count = evicted_failed, "evicted failed LS instances");
        }

        let entries = sup.loaded_entries();

        // 1) 全局空闲判定：最新活动（LS 或全局时钟）距今超阈值。
        let newest = entries
            .iter()
            .map(|(_, t)| *t)
            .max()
            .unwrap_or(*GLOBAL_LAST_ACTIVITY.lock().unwrap());
        if now.duration_since(newest) >= iv.global_idle {
            tracing::info!("global idle reached; entering ShutdownDraining");
            state.draining.store(true, Ordering::Release);
            finish_shutdown(&sup, &state, &lock_path).await;
            return;
        }

        // 2) 单 LS 空闲卸载。
        for (key, last) in &entries {
            if now.duration_since(*last) >= iv.ls_idle {
                tracing::info!(root = %key.root.display(), lang = %key.lang, "evict idle LS");
                let _ = sup.evict(key).await;
            }
        }

        // 3) LRU：超上限时按 last_used 升序（最久未用先卸）。
        let entries = sup.loaded_entries();
        if entries.len() > iv.max_loaded_ls {
            let mut by_age = entries;
            by_age.sort_by_key(|(_, t)| *t);
            let excess = by_age.len() - iv.max_loaded_ls;
            for (key, _) in by_age.into_iter().take(excess) {
                tracing::info!(root = %key.root.display(), lang = %key.lang, "LRU evict");
                let _ = sup.evict(&key).await;
            }
        }
    }
}

/// ShutdownDraining 收尾：等排空窗口 → 逐 LS shutdown → 删 lock。
async fn finish_shutdown(
    sup: &Arc<Supervisor>,
    state: &AppState,
    lock_path: &Option<std::path::PathBuf>,
) {
    // ponytail: in-flight 精确计数需要 http 层埋点；M1 用 draining 拒新 +
    // 固定 1s 排空窗口近似。正确性由 A6 双实例容忍兜底。
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 逐 LS shutdown（单 LS 5s 超时转 kill 在 Session::shutdown 内部实现）。
    for (key, _) in sup.loaded_entries() {
        let _ = tokio::time::timeout(Duration::from_secs(5), sup.evict(&key)).await;
    }

    // 删 lock。
    if let Some(p) = lock_path {
        let _ = lockfile::remove(p);
    }
    let _ = state;
    tracing::info!("daemon shutdown complete; lock removed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use supervisor::SupervisorTrait;

    /// 空 supervisor mock（不做真 spawn）。
    struct NoopSup;

    #[async_trait::async_trait]
    impl SupervisorTrait for NoopSup {
        async fn execute_tool(
            &self,
            _tool: &str,
            _root: &str,
            _args: serde_json::Value,
            _lang: Option<&str>,
        ) -> Result<serde_json::Value, supervisor::ToolError> {
            Ok(serde_json::json!(null))
        }
    }

    fn test_state() -> AppState {
        AppState {
            supervisor: Arc::new(NoopSup),
            token: Arc::new("t".into()),
            start_ts: Instant::now(),
            loaded_ls: Arc::new(Mutex::new(vec![])),
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fast_intervals() -> ReaperIntervals {
        ReaperIntervals {
            scan: Duration::from_millis(50),
            ls_idle: Duration::from_millis(200),
            global_idle: Duration::from_millis(400),
            max_loaded_ls: 3,
        }
    }

    #[tokio::test]
    async fn global_idle_triggers_draining() {
        let sup = Arc::new(Supervisor::direct().await.unwrap());
        let state = test_state();
        // 不 note_activity —— 全局时钟停留在进程启动时，400ms 后应 draining。
        // （注意：并行测试可能刷新全局时钟；本测试容忍最长 5s 等待窗口。）
        let handle = spawn_reaper(sup.clone(), state.clone(), fast_intervals(), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if state.draining.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            state.draining.load(Ordering::Acquire),
            "global idle 后应置 draining"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn default_intervals_match_plan() {
        let iv = ReaperIntervals::default();
        assert_eq!(iv.scan, Duration::from_secs(30));
        assert_eq!(iv.ls_idle, Duration::from_secs(600));
        assert_eq!(iv.global_idle, Duration::from_secs(900));
        assert_eq!(iv.max_loaded_ls, 3);
    }

    #[tokio::test]
    async fn draining_stops_reaper() {
        let sup = Arc::new(Supervisor::direct().await.unwrap());
        let state = test_state();
        state.draining.store(true, Ordering::Release);
        let handle = spawn_reaper(sup, state.clone(), fast_intervals(), None);
        // reaper 首个 tick 就应走 finish_shutdown 并 return —— handle 在 <1s 内结束。
        let started = Instant::now();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "draining 后 reaper 应尽快退出"
        );
    }
}
