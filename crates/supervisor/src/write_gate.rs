//! 全局写门（PLAN Task 15 / DESIGN §3.1 / ARCHITECTURE §3.4 A4）。
//!
//! `replace-body` 是进程内唯一的写路径工具；一把全局 `tokio::sync::Mutex<()>`
//! FIFO 等待即"互斥队列"。持锁范围覆盖：解析符号 range → 读盘对账 → 原子写 →
//! didChange 全量，保证"锁内看到的盘上内容 == LS 看到的内容"。
//!
//! 超时（bd serena-rust-b3w）：acquire 不再永久等待——默认 60s（短于 CLI
//! FORWARD_TIMEOUT=300s，agent 能在转发硬顶内拿到结构化错误），env
//! `SERENA_WRITE_GATE_TIMEOUT_SECS` 可覆盖（非法值 warn + 用默认，对齐 j8b
//! parse_secs 惯例；0 视同非法——会退回永久等待）。超时映射 `CoreError::Timeout`
//! → wire LS_TIMEOUT（retryable）：门竞争是瞬态条件，"稍后重试"恰是正确动作。
//!
//! 排队回显（bd serena-rust-98l）：慢路径等待者向 tracing（daemon stderr，默认
//! info 级即可见 warn）打一行排队回显——第 N 位等待者、持门者工具标签、超时上限；
//! 拿到门 / 超时各补一行。持门者标签 = [`acquire`] 的 `who` 参数（守卫 drop 清除，
//! 不 stale）。2rj 的 per-root 分键升级路径不实现：超时 + 回显已消除链 B
//! 不可观测放大器。
//!
//! ponytail: 全局单写门；若未来证明同文件高频并发写是热点，再按 project_root 分键。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use lsp_core::error::CoreError;
use tokio::sync::Mutex;

/// 进程级唯一写门。FIFO 公平，等待者天然排队。
static WRITE_GATE: Mutex<()> = Mutex::const_new(());

/// 当前排队中的取门请求数（快路径不经此计数；拿到门/超时放弃各递减）。
static WAITERS: AtomicUsize = AtomicUsize::new(0);

/// 当前持门者的工具标签。同步 Mutex 只包一个 `Option<&str>`，临界区无 await。
static HOLDER: StdMutex<Option<&'static str>> = StdMutex::new(None);

const GATE_TIMEOUT_ENV: &str = "SERENA_WRITE_GATE_TIMEOUT_SECS";
/// 默认 60s = FORWARD_TIMEOUT（CLI 转发硬顶 300s）的 1/5：等门 60s 仍拿不到，
/// 多半是持门方挂死，早失败好过 300s 后才失败。
const DEFAULT_GATE_TIMEOUT_SECS: u64 = 60;

/// 解析超时秒数：env 未设 → 默认；非法/0 值 → warn + 默认。纯函数，单测直测。
fn gate_timeout_secs(env_raw: Option<&str>) -> u64 {
    match env_raw {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => secs,
            _ => {
                tracing::warn!(
                    "invalid {GATE_TIMEOUT_ENV}={raw:?}; using default {DEFAULT_GATE_TIMEOUT_SECS}s"
                );
                DEFAULT_GATE_TIMEOUT_SECS
            }
        },
        None => DEFAULT_GATE_TIMEOUT_SECS,
    }
}

/// 写门守卫。持有期间其它取门调用排队等待；drop 清除持门者标签。
#[derive(Debug)]
pub struct WriteGateGuard {
    _inner: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for WriteGateGuard {
    fn drop(&mut self) {
        *HOLDER.lock().unwrap() = None;
    }
}

/// 获取写门守卫（默认超时，env 可覆盖）。排队时打回显日志；超时返回
/// `CoreError::Timeout`（wire LS_TIMEOUT，retryable）而非永久阻塞。
pub async fn acquire(who: &'static str) -> Result<WriteGateGuard, CoreError> {
    let secs = gate_timeout_secs(std::env::var(GATE_TIMEOUT_ENV).ok().as_deref());
    acquire_with_timeout(who, Duration::from_secs(secs)).await
}

/// [`acquire`] 的显式超时版：单测注入短超时用（进程级 env 对并行测试是全局态，
/// 注入参数避免竞态）。
pub async fn acquire_with_timeout(
    who: &'static str,
    timeout: Duration,
) -> Result<WriteGateGuard, CoreError> {
    // 快路径：无竞争直接拿——零日志噪音、不进排队计数。
    if let Ok(guard) = WRITE_GATE.try_lock() {
        *HOLDER.lock().unwrap() = Some(who);
        return Ok(WriteGateGuard { _inner: guard });
    }
    // 慢路径：真排队。bd serena-rust-98l：第 N 位等待者 + 持门者概况回显，
    // 消除链 B 不可观测放大器。
    let position = WAITERS.fetch_add(1, Ordering::Relaxed) + 1;
    let holder = *HOLDER.lock().unwrap();
    tracing::warn!(
        "write gate contended: `{who}` queued as waiter #{position}; \
         holder: {}; will abort with timeout error after {}s",
        holder.unwrap_or("<unknown>"),
        timeout.as_secs()
    );
    let waited = tokio::time::timeout(timeout, WRITE_GATE.lock()).await;
    WAITERS.fetch_sub(1, Ordering::Relaxed);
    match waited {
        Ok(guard) => {
            *HOLDER.lock().unwrap() = Some(who);
            tracing::info!("write gate acquired by `{who}` (was waiter #{position})");
            Ok(WriteGateGuard { _inner: guard })
        }
        Err(_elapsed) => {
            let holder = *HOLDER.lock().unwrap();
            tracing::error!(
                "write gate TIMEOUT: `{who}` gave up after {}s; holder: {}; \
                 {} waiter(s) still queued",
                timeout.as_secs(),
                holder.unwrap_or("<unknown>"),
                WAITERS.load(Ordering::Relaxed)
            );
            // method 位带上持门者概况：wire 消息 "{method} timed out after {secs}s"
            // 直达 agent。LS_TIMEOUT/retryable——门竞争瞬态，稍后重试恰是正确动作。
            Err(CoreError::Timeout {
                method: format!("write-gate(holder={})", holder.unwrap_or("<unknown>")),
                secs: timeout.as_secs(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 并发两写串行化：两个 task 抢门，持有者顺序可预期（FIFO）。
    #[tokio::test]
    async fn concurrent_writes_serialize() {
        // task1 持门后经 channel 发信号，主测收到信号才开始计时。旧版固定
        // sleep(10ms) 猜"task1 已持门"：满载下 task1 尚未被 poll（task2 先抢到门，
        // waited≈0）或主测晚醒（task1 剩余持门 <80ms）都会误报——wall-clock sleep
        // race，supfix/h4i 两份报告在案。
        let (tx, rx) = tokio::sync::oneshot::channel();
        let gate_task1 = tokio::spawn(async move {
            let _g = acquire("test-task1").await.unwrap();
            let _ = tx.send(());
            // 持门 200ms，阈值取 100ms 留 50% 余量：信号后调度抖动只会推迟 task2
            // 的取样、不会提前（tokio 计时器不早触发，释放时机由 task1 独占），
            // waited 只会偏大；100ms 仍足以拦住"门失效、task2 立即拿到"的回归。
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        // 阻塞到 task1 真正持门（信号同步，非墙钟猜测；j8b deadline 轮询同思路）。
        rx.await.unwrap();
        let start2 = Instant::now();
        let gate_task2 = tokio::spawn(async {
            let _g = acquire("test-task2").await.unwrap();
            Instant::now()
        });
        gate_task1.await.unwrap();
        let t2 = gate_task2.await.unwrap();
        let waited = t2.duration_since(start2);
        assert!(
            waited.as_millis() >= 100,
            "task2 应等 task1 释放后才拿到门，实际等了 {:?}",
            waited
        );
    }

    /// Arc 共享下也能串行（模拟 daemon 全局引用）。
    #[tokio::test]
    async fn gate_is_global_singleton() {
        // 直接拿两次守卫验证互斥：第一次拿住时第二次应等待。
        let g1 = acquire("test-holder").await.unwrap();
        let waiter = tokio::spawn(async {
            let _g2 = acquire("test-waiter").await.unwrap();
            true
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "持门时 waiter 不应完成");
        drop(g1);
        assert!(waiter.await.unwrap());
    }

    /// bd serena-rust-b3w：短超时下被持门阻塞 → 结构化超时错误（非永久等待），
    /// 且门随后可正常获取（超时路径不泄漏/毒化门）。
    #[tokio::test]
    async fn acquire_timeout_returns_structured_error() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let holder = tokio::spawn(async move {
            let _g = acquire("test-timeout-holder").await.unwrap();
            let _ = tx.send(());
            // 持门 400ms；受害者 50ms 超时必先触发（tokio 计时器不早触发）。
            tokio::time::sleep(Duration::from_millis(400)).await;
        });
        rx.await.unwrap();
        let start = Instant::now();
        let err = acquire_with_timeout("test-timeout-victim", Duration::from_millis(50))
            .await
            .expect_err("持门 400ms 时 50ms 超时必须报错");
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "不得早于超时窗口返回"
        );
        match err {
            CoreError::Timeout { method, .. } => assert!(
                method.starts_with("write-gate(holder="),
                "method 应标识写门并带持门者概况: {method}"
            ),
            other => panic!("应映射 CoreError::Timeout，实际: {other:?}"),
        }
        holder.await.unwrap();
        let _g = acquire_with_timeout("test-after-timeout", Duration::from_secs(2))
            .await
            .unwrap();
    }

    /// bd serena-rust-98l 数据源：持门期间排队深度计数与持门者标签可读
    /// （守卫独占 ⇒ 我持门时 HOLDER 必为我）。回显日志行本身由 e2e 覆盖。
    #[tokio::test]
    async fn queue_depth_and_holder_visible_while_contended() {
        let g1 = acquire("test-depth-holder").await.unwrap();
        assert_eq!(*HOLDER.lock().unwrap(), Some("test-depth-holder"));
        let waiter = tokio::spawn(async {
            acquire_with_timeout("test-depth-waiter", Duration::from_secs(5)).await
        });
        // 轮询到有等待者入队（信号同步，非墙钟猜测——同 concurrent_writes_serialize；
        // 宽松取 >=1：并行跑的其它测试可能同刻排队）。
        tokio::time::timeout(Duration::from_secs(2), async {
            while WAITERS.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("waiter 应在超时前入队");
        assert!(!waiter.is_finished());
        drop(g1);
        assert!(waiter.await.unwrap().is_ok(), "释放后 waiter 应拿到门");
    }

    /// 超时解析：env 未设 → 默认；合法值生效；非法/0 值 → 默认
    /// （0 会退回 b3w 的永久等待，视同非法）。
    #[test]
    fn gate_timeout_env_parsing() {
        assert_eq!(gate_timeout_secs(None), 60);
        assert_eq!(gate_timeout_secs(Some("30")), 30);
        assert_eq!(gate_timeout_secs(Some(" 90 ")), 90);
        assert_eq!(gate_timeout_secs(Some("abc")), 60);
        assert_eq!(gate_timeout_secs(Some("0")), 60);
        assert_eq!(gate_timeout_secs(Some("")), 60);
    }
}
