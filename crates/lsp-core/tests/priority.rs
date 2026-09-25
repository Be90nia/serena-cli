//! P0B：priority 分类 + TokenBucket 限流单元测试。
//!
//! 覆盖：
//! 1. `classify_method` 按方法名归类：textDocument/* → High；workspace/_ping → Background；
//!    didOpen/didChange/didClose/initialized → Normal。
//! 2. `TokenBucket::try_acquire` 限流：burst 后立即扣令牌、扣完返回 false、空闲后
//!    refill 恢复。验证长期速率 = per_sec。
//!
//! `pump_with_priority` 的端到端 priority 排序（High 抢占 Background）由 mock_ls
//! 集成测试在 `tests/transport.rs::priority_*` 覆盖（见后续添加）。

use lsp_core::client::{Priority, TokenBucket, classify_method};
use std::time::{Duration, Instant};

#[test]
fn classify_text_document_is_high() {
    assert_eq!(
        classify_method("textDocument/documentSymbol"),
        Priority::High
    );
    assert_eq!(classify_method("textDocument/definition"), Priority::High);
    assert_eq!(classify_method("textDocument/hover"), Priority::High);
    assert_eq!(classify_method("textDocument/references"), Priority::High);
}

#[test]
fn classify_execute_command_and_work_progress_are_high() {
    assert_eq!(classify_method("workspace/executeCommand"), Priority::High);
    assert_eq!(
        classify_method("workspace/workspaceFolders"),
        Priority::High
    );
    assert_eq!(classify_method("workspace/configuration"), Priority::High);
    assert_eq!(
        classify_method("window/workDoneProgress/create"),
        Priority::High
    );
}

#[test]
fn classify_workspace_ping_is_background() {
    assert_eq!(classify_method("$/workspace/_ping"), Priority::Background);
    assert_eq!(classify_method("workspace/_ping"), Priority::Background);
}

#[test]
fn classify_workspace_symbol_is_background() {
    // 大型 workspace 下重量级索引后端 → Background。
    assert_eq!(classify_method("workspace/symbol"), Priority::Background);
}

#[test]
fn classify_normal_methods_are_normal() {
    // 契约（P0B + race 修）：`initialized` 是协议握手帧，必须先于一切后续请求
    // 写出 —— 升 High 与请求同 FIFO 队列保序（Normal 的 50ms demote 会让 High
    // 请求插队到它之前，rust-analyzer 严格校验顺序直接退出）。
    // RA 自身中立通知走 Normal。
    assert_eq!(classify_method("initialized"), Priority::High);
    assert_eq!(
        classify_method("workspace/didChangeWatchedFiles"),
        Priority::Normal
    );
    assert_eq!(classify_method("exit"), Priority::Normal);
    assert_eq!(classify_method("shutdown"), Priority::Normal);
    assert_eq!(classify_method("$/progress"), Priority::Normal);
}

#[test]
fn classify_text_document_did_events_are_high_per_prefix_rule() {
    // 契约明确：textDocument/* 一律 High（含 didOpen/didChange/didClose）。
    // docsync 路径要 user-facing priority 直通 LS。
    assert_eq!(classify_method("textDocument/didOpen"), Priority::High);
    assert_eq!(classify_method("textDocument/didChange"), Priority::High);
    assert_eq!(classify_method("textDocument/didClose"), Priority::High);
}

#[test]
fn token_bucket_initial_burst_available() {
    // 默认 per_sec=30、burst=60（前 2s 抑制窗）。构造即满 burst → 前 burst 次都该成功。
    let bucket = TokenBucket::new(30, 0);
    let now = Instant::now();
    for i in 0..60 {
        assert!(
            bucket.try_acquire(now),
            "burst 内前 60 次应都成功，第 {i} 次失败"
        );
    }
}

#[test]
fn token_bucket_refuses_after_burst() {
    let bucket = TokenBucket::new(30, 0);
    let now = Instant::now();
    // 扣完 burst。
    for _ in 0..60 {
        assert!(bucket.try_acquire(now));
    }
    // 第 61 次应被拒（无时间流逝 → 无 refill）。
    assert!(
        !bucket.try_acquire(now),
        "扣完 burst 后无 refill 应返回 false"
    );
}

#[test]
fn token_bucket_refills_over_time() {
    let bucket = TokenBucket::new(100, 0); // 100/s → 10ms / token
    let now = Instant::now();
    // 扣空 burst（默认 2s = 200 token）。
    for _ in 0..200 {
        assert!(bucket.try_acquire(now));
    }
    assert!(!bucket.try_acquire(now), "burst 扣完应拒");

    // 等 50ms → 应 refill 5 个（100/s × 0.05s = 5）。
    let later = now + Duration::from_millis(50);
    for i in 0..5 {
        assert!(bucket.try_acquire(later), "refill 后第 {i} 次应成功");
    }
    assert!(!bucket.try_acquire(later), "refill 用完应再次拒绝");
}

#[test]
fn token_bucket_default_rates_match_spec() {
    // 契约：默认 per_sec=30、burst=60（2s 抑制窗）。
    let bucket = TokenBucket::new(0, 0); // 0 走默认
    assert_eq!(bucket.per_sec(), 30);
    let now = Instant::now();
    // 60 次 burst 都能成功。
    for _ in 0..60 {
        assert!(bucket.try_acquire(now));
    }
    // 第 61 次失败。
    assert!(!bucket.try_acquire(now));
}
