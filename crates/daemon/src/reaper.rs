//! IdleReaper + ShutdownDraining + LRU（PLAN Task 14 / DESIGN §3 I8 / ARCHITECTURE §3）。
//!
//! 常驻 tokio task，30s 巡检（测试可调到 100ms 级）：
//! - 单 LS 10min 未用 → 卸载
//! - 超 `max_loaded_ls=3` → LRU 驱逐（卸最久未用的）
//! - 全局 15min 空闲 → ShutdownDraining：503 + Retry-After（http 层）→
//!   删 lock → exit 0（排空窗口由 http 层 wait_drain 承担，此处不重复等）
//!
//! bd j8b：两个 idle 阈值环境变量可配（`intervals_from_env`），0 = 永不
//! （自杀/驱逐）；缺省与 Default 一致，默认行为不变。
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

/// 环境变量秒数解析（bd j8b）：缺失 → 默认；负数/非数字/空串 → warn 后用默认
/// （配置错误不致命）；`0` 合法 = 永不（自杀/驱逐，由 reaper_loop 的 is_zero guard 实现）。
fn parse_secs(raw: Option<&str>, var: &str, default: u64) -> u64 {
    match raw {
        None => default,
        Some(s) => match s.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(env = var, value = s, default, "invalid value; using default");
                default
            }
        },
    }
}

/// 生产 intervals：idle 阈值环境变量可配（bd j8b）。
/// - `SERENA_IDLE_TIMEOUT_SECS`：全局 idle 自杀阈值（缺省 = Default 的 900；0 = 永不自杀）
/// - `SERENA_LS_IDLE_EVICTION_SECS`：单 LS 空闲驱逐阈值（缺省 = Default 的 600；0 = 永不驱逐）
pub fn intervals_from_env() -> ReaperIntervals {
    let base = ReaperIntervals::default();
    ReaperIntervals {
        global_idle: Duration::from_secs(parse_secs(
            std::env::var("SERENA_IDLE_TIMEOUT_SECS").ok().as_deref(),
            "SERENA_IDLE_TIMEOUT_SECS",
            base.global_idle.as_secs(),
        )),
        ls_idle: Duration::from_secs(parse_secs(
            std::env::var("SERENA_LS_IDLE_EVICTION_SECS").ok().as_deref(),
            "SERENA_LS_IDLE_EVICTION_SECS",
            base.ls_idle.as_secs(),
        )),
        ..base
    }
}

/// 全局 last-activity 时间戳；daemon 启动时初始化，http 层工具调用后刷新。
static GLOBAL_LAST_ACTIVITY: LazyLock<Mutex<Instant>> =
    LazyLock::new(|| Mutex::new(Instant::now()));

/// 工具调用成功后由 http 层调用：刷新全局活跃时钟。
pub fn note_activity() {
    *GLOBAL_LAST_ACTIVITY.lock().unwrap() = Instant::now();
}

/// 读全局活跃时钟（测试锁用：断言某路径未刷新时钟）。
pub fn last_activity() -> Instant {
    *GLOBAL_LAST_ACTIVITY.lock().unwrap()
}

/// 启动 reaper 常驻 task。返回 JoinHandle 供测试/停机取消。
///
/// `lock` = (lock 路径, 自己的 boot_ms 归属戳)，用于 shutdown 收尾的
/// 归属校验删除；`None`（如测试）不删。
pub fn spawn_reaper(
    sup: Arc<Supervisor>,
    state: AppState,
    intervals: ReaperIntervals,
    lock: Option<(std::path::PathBuf, u128)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        reaper_loop(sup, state, intervals, lock).await;
    })
}

async fn reaper_loop(
    sup: Arc<Supervisor>,
    state: AppState,
    iv: ReaperIntervals,
    lock: Option<(std::path::PathBuf, u128)>,
) {
    loop {
        // sleep 与 shutdown 信号 race：/shutdown 触发后可立即跳出，不等满 scan 周期。
        // ponytail: 不引入额外 channel，复用 AppState.shutdown_notify（Notify 单次广播）。
        tokio::select! {
            _ = tokio::time::sleep(iv.scan) => {}
            _ = state.shutdown_notify.notified() => {}
        }

        // 已在 draining：走收尾并退出 task（进程随后自然退出）。
        if state.draining.load(Ordering::Acquire) {
            finish_shutdown(&state, &lock).await;
            return;
        }

        // 0) Failed LS 驱逐：LS 死后 state 变 Failed, 下次工具调用才被动 evict.
        //    这里主动驱逐, 下次调用 session_for 慢路径自动 spawn 新实例。
        let evicted_failed = sup.evict_failed_instances().await;
        if evicted_failed > 0 {
            tracing::info!(count = evicted_failed, "evicted failed LS instances");
        }

        let now = Instant::now();
        let entries = sup.loaded_entries();

        // 1) 全局空闲判定：最新活动（LS 或全局时钟）距今超阈值；0 = 永不自杀（bd j8b）。
        let newest = entries
            .iter()
            .map(|(_, t)| *t)
            .max()
            .unwrap_or(*GLOBAL_LAST_ACTIVITY.lock().unwrap());
        let global_expired =
            !iv.global_idle.is_zero() && now.duration_since(newest) >= iv.global_idle;
        if global_expired {
            tracing::info!("global idle reached; entering ShutdownDraining");
            state.draining.store(true, Ordering::Release);
            finish_shutdown(&state, &lock).await;
            return;
        }

        // 2) 单 LS 空闲卸载；0 = 永不驱逐（bd j8b）。
        for (key, last) in &entries {
            if !iv.ls_idle.is_zero() && now.duration_since(*last) >= iv.ls_idle {
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

/// ShutdownDraining 收尾：删 lock → 强退进程（排空窗口在 http 层已给过）。
///
/// 末尾 `std::process::exit(0)` 是必需的：Windows 下 `cli --daemon` 走
/// `CREATE_NEW_PROCESS_GROUP` + stdin/stdout→NULL 启动，tokio runtime 的
/// background threads / signal handlers 持有引用，runtime drop 后进程仍可能
/// 残留；`process::exit` 直接终止并跳过 drop，等同 systemd / svchost 的
/// SIGTERM-then-SIGKILL 语义（ARCH §3.2）。
async fn finish_shutdown(
    state: &AppState,
    lock: &Option<(std::path::PathBuf, u128)>,
) {
    shutdown_cleanup(state, lock).await;
    // cfg(not(test))：单测里 reaper_loop 走 finish_shutdown 时不强退——
    // 会把整个测试 binary 拽下来。生产 build 始终带这段。
    #[cfg(not(test))]
    std::process::exit(0);
}

/// ShutdownDraining 可单测的核心清理：删 lock → notify（进程随后强退）。
///
/// 从 `finish_shutdown` 抽出，让测试能断言"删 lock / notify 都做了"而不触发
/// `process::exit`（强退会拽走测试 binary）。
pub(crate) async fn shutdown_cleanup(
    state: &AppState,
    lock: &Option<(std::path::PathBuf, u128)>,
) {
    // 不逐 LS evict、不再等 in-flight：两条调用路径（/shutdown、idle 15min）
    // 的排空窗口都已在 http 层给过（wait_drain），且最终都以 process::exit(0)
    // 收场——Windows Job 句柄随进程关闭带崩整个 LS 树（ARCH §3.2），优雅
    // evict（最长 5s/LS）只会把「listener 已停 accept、进程未退」的黑洞窗口
    // 拉长到 5s+，客户端既拿不到 503 也拿不到 refused。窗口尽仍有慢
    // in-flight（如冷索引的 16s 请求）就由下面的 process::exit 掐断，客户端
    // 拿连接重置可重试（daemon 已退，重试 lazy-spawn 新 daemon）。优雅卸载
    // LS 是 reaper 主循环单 LS 空闲路径的事。

    // 删 lock + 通知 axum（保险触发，shutdown_post 已 notify_waiters 过一次）。
    // 归属校验：drain 期间 lock 可能已被 lazy-spawn 的新 daemon 接管——
    // 无条件删会把新 daemon 的 lock 删掉，制造"活着但无 lock"的孤儿（bd y2y）。
    if let Some((p, own_boot)) = lock {
        lockfile::remove_owned(p, std::process::id(), *own_boot);
    }
    state.shutdown_notify.notify_waiters();
    tracing::info!("daemon shutdown complete; lock removed (if still owned)");
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
            active_project: Arc::new(Mutex::new(None)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            // 空路径 = 不写 envelope 日志（测试不需要 d3a 重放索引）。
            invocation_log_path: std::path::PathBuf::new(),
            // 短窗口：reaper 收尾测试不必等满生产 2s。
            drain_window: Duration::from_millis(100),
            // 7rh：reaper 测试不关心 token 估算。
            no_token_estimate: false,
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

    /// bd j8b：env 秒数解析——缺失/非法（负数、非数字、空串）回默认，0 合法。
    /// （不直接测 intervals_from_env 读真 env：进程全局状态会被并行测试污染。）
    #[test]
    fn parse_secs_defaults_on_missing_or_invalid() {
        assert_eq!(parse_secs(None, "X", 900), 900);
        assert_eq!(parse_secs(Some("0"), "X", 900), 0, "0 = 永不，是合法值");
        assert_eq!(parse_secs(Some("120"), "X", 900), 120);
        assert_eq!(parse_secs(Some(" 300 "), "X", 900), 300);
        assert_eq!(parse_secs(Some("-1"), "X", 900), 900, "负数非法 → 默认");
        assert_eq!(parse_secs(Some("abc"), "X", 900), 900);
        assert_eq!(parse_secs(Some(""), "X", 900), 900);
    }

    /// bd j8b：global_idle=0 → 永不自杀（覆盖原 400ms 触发窗后仍不 draining）。
    #[tokio::test]
    async fn zero_global_idle_never_drains() {
        let sup = Arc::new(Supervisor::direct().await.unwrap());
        let state = test_state();
        let iv = ReaperIntervals {
            global_idle: Duration::ZERO,
            ..fast_intervals()
        };
        let handle = spawn_reaper(sup, state.clone(), iv, None);
        // fast_intervals 的 global_idle=400ms；等 700ms 覆盖原触发窗 + 两个 tick。
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !state.draining.load(Ordering::Acquire),
            "global_idle=0 不得触发自杀"
        );
        handle.abort();
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

    /// 写一份"当前测试进程自有归属"的 lock（remove_owned 会认账的内容）。
    fn write_own_lock(lock_path: &std::path::Path, own_boot: u128) {
        let own = lockfile::LockEntry {
            pid: std::process::id(),
            port: 7860,
            boot_ms: own_boot,
            token: "own".into(),
        };
        lockfile::write_final(lock_path, &own).expect("write own lock");
    }

    /// P0 防回归：shutdown_cleanup 必须真删（自有归属的）lock 文件 + 触发
    /// Notify，否则 daemon 会变僵尸（lock 删但进程不退 / 反之亦然）。
    #[tokio::test]
    async fn shutdown_cleanup_removes_lock_and_fires_notify() {
        let state = test_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("daemon.lock");
        let own_boot = 42u128;
        write_own_lock(&lock_path, own_boot);

        // 后台监听 Notify（一次性广播；先 listen 再 trigger 才会被唤醒）。
        let notify = state.shutdown_notify.clone();
        let notified = tokio::spawn(async move {
            notify.notified().await;
            true
        });
        // 确保 waiter 已挂起在 notified() 上：notify_waiters 只唤醒已等待者。
        // （原先隐式依赖 cleanup 的固定排空 sleep 提供调度窗口；wait_drain
        // 快路径 in_flight=0 立即返回，先行关系必须显式建立。）
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown_cleanup(&state, &Some((lock_path.clone(), own_boot))).await;

        assert!(!lock_path.exists(), "shutdown_cleanup 必须删自有 lock");
        let res = tokio::time::timeout(Duration::from_secs(2), notified)
            .await
            .expect("notified within 2s")
            .expect("task ok");
        assert!(res, "shutdown_notify 必须被 notify_waiters");
    }

    /// bd y2y 根因 1 回归：drain 期间 lock 被新 daemon 接管（pid/boot_ms
    /// 不再是自己）→ shutdown_cleanup 不得删除他人的 lock。
    #[tokio::test]
    async fn shutdown_cleanup_spares_taken_over_lock() {
        let state = test_state();
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("daemon.lock");
        // 接管者的 lock（pid/boot_ms 都不是本进程）。
        let taken = lockfile::LockEntry {
            pid: std::process::id() + 1,
            port: 7860,
            boot_ms: 7,
            token: "new-owner".into(),
        };
        lockfile::write_final(&lock_path, &taken).expect("write taken-over lock");

        shutdown_cleanup(&state, &Some((lock_path.clone(), 42))).await;

        assert!(lock_path.exists(), "易主 lock 必须保留，不得误删");
    }

    /// P0 防回归：shutdown_signal（Notify）在 reaper 还没进入 select! 时
    /// 先 fire 也不能丢——reaper 下次进 select! 时 draining=true 兜底。
    /// 本测试断言：draining flag 在 reaper 首个 tick 后被读到并触发清理。
    #[tokio::test]
    async fn drain_before_reaper_selects_still_cleans_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock_path = tmp.path().join("daemon.lock");
        let own_boot = 43u128;
        write_own_lock(&lock_path, own_boot);

        let sup = Arc::new(Supervisor::direct().await.unwrap());
        let state = test_state();
        // 模拟 cli stop-all 抢先于 reaper 首次 tick：先置 draining + notify。
        state.draining.store(true, Ordering::Release);
        state.shutdown_notify.notify_waiters();

        let handle = spawn_reaper(
            sup,
            state.clone(),
            fast_intervals(),
            Some((lock_path.clone(), own_boot)),
        );
        // reaper 首个 tick 走 finish_shutdown → cfg(not(test)) 不强退 → return。
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("reaper exits");

        assert!(!lock_path.exists(), "draining 抢占场景下 lock 也必须被删");
    }
}
