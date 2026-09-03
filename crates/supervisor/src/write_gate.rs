//! 全局写门（PLAN Task 15 / DESIGN §3.1 / ARCHITECTURE §3.4 A4）。
//!
//! `replace-body` 是进程内唯一的写路径工具；一把全局 `tokio::sync::Mutex<()>`
//! FIFO 等待即"互斥队列"。持锁范围覆盖：解析符号 range → 读盘对账 → 原子写 →
//! didChange 全量，保证"锁内看到的盘上内容 == LS 看到的内容"。
//!
//! ponytail: 全局单写门；若未来证明同文件高频并发写是热点，再按 project_root 分键。

use tokio::sync::Mutex;

/// 进程级唯一写门。FIFO 公平，等待者天然排队。
static WRITE_GATE: Mutex<()> = Mutex::const_new(());

/// 获取写门守卫。持有期间其它 replace-body 调用排队等待。
pub async fn acquire() -> tokio::sync::MutexGuard<'static, ()> {
    WRITE_GATE.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 并发两写串行化：两个 task 抢门，持有者顺序可预期（FIFO）。
    #[tokio::test]
    async fn concurrent_writes_serialize() {
        let gate_task1 = tokio::spawn(async {
            let _g = acquire().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        // 等 task1 真正拿门后再 spawn task2：测 task2 等待 ≥80ms。
        tokio::time::sleep(Duration::from_millis(10)).await;
        let start2 = Instant::now();
        let gate_task2 = tokio::spawn(async {
            let _g = acquire().await;
            Instant::now()
        });
        gate_task1.await.unwrap();
        let t2 = gate_task2.await.unwrap();
        let waited = t2.duration_since(start2);
        assert!(
            waited.as_millis() >= 80,
            "task2 应等 task1 释放后才拿到门，实际等了 {:?}",
            waited
        );
    }

    /// Arc 共享下也能串行（模拟 daemon 全局引用）。
    #[tokio::test]
    async fn gate_is_global_singleton() {
        // 直接拿两次守卫验证互斥：第一次拿住时第二次应等待。
        let g1 = acquire().await;
        let waiter = tokio::spawn(async {
            let _g2 = acquire().await;
            true
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "持门时 waiter 不应完成");
        drop(g1);
        assert!(waiter.await.unwrap());
    }
}
