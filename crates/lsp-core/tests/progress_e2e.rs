//! Phase 4 Task 22c 端到端：`Session::wait_for_progress` 等到 `$/progress` 通知。
//!
//! mock_ls 设 `MOCK_LS_PROGRESS_TOKEN=test-token-1` → 握手后立刻发 `$/progress`
//! 通知（kind="end"）。Session 注册的 `$/progress` handler 命中 token → 唤醒
//! wait_for_progress。

use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_progress_resolves_when_ls_sends_notification() {
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_core::init_params::base_initialize_params;
    use lsp_core::session::Session;

    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![(
            "MOCK_LS_PROGRESS_TOKEN".to_string(),
            "task-22c-token".to_string(),
        )],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");

    let params = base_initialize_params();
    let session = tokio::time::timeout(Duration::from_secs(10), Session::start(Some(child), params))
        .await
        .expect("session start within 10s")
        .expect("session start Ok");

    let started = Instant::now();
    let res = session
        .wait_for_progress("task-22c-token", Duration::from_secs(2))
        .await;
    let elapsed = started.elapsed();
    assert!(res.is_ok(), "mock_ls 应发 progress 通知：{res:?}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "progress 通知应在握手后立刻到达，实测 {elapsed:?}"
    );
    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_progress_returns_timeout_when_no_notification() {
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_core::init_params::base_initialize_params;
    use lsp_core::session::Session;

    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    // 不设 MOCK_LS_PROGRESS_TOKEN → mock_ls 不发通知
    let child = Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");

    let params = base_initialize_params();
    let session = tokio::time::timeout(Duration::from_secs(10), Session::start(Some(child), params))
        .await
        .expect("session start within 10s")
        .expect("session start Ok");

    let started = Instant::now();
    let res = session
        .wait_for_progress("never-arrives", Duration::from_millis(300))
        .await;
    let elapsed = started.elapsed();
    assert!(res.is_err(), "无通知时应 timeout");
    assert!(
        matches!(
            res.as_ref().err(),
            Some(lsp_core::error::CoreError::Timeout { .. })
        ),
        "应 CoreError::Timeout，实际 {res:?}",
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "不应早于 timeout 80%，实测 {elapsed:?}"
    );
    session.shutdown().await;
}

/// P2-y5u P0 判据 #1：`$/progress` handler 闭包捕获 `Arc::clone(&session)` 塞进
/// `ClientInner.notification_handlers` 表 → Session 与 ClientInner 通过 Arc 环互锁，
/// drop(session) 后 Arc 强计数不归零 → 进程长期持有已关停会话资源。
///
/// 修复路径：handler 改 `Weak<Session>`，shutdown 时显式清 handler 表。本测试
/// pre-fix 必 fail（weak.upgrade() Some → 环存在），post-fix 必 pass（None → 环断开）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_session_releases_arc_after_shutdown() {
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_core::init_params::base_initialize_params;
    use lsp_core::session::Session;

    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");

    let session = tokio::time::timeout(
        Duration::from_secs(10),
        Session::start(Some(child), base_initialize_params()),
    )
    .await
    .expect("session start within 10s")
    .expect("session start Ok");

    let weak = std::sync::Arc::downgrade(&session);
    session.shutdown().await;
    drop(session);
    // 给泵 task 一小段时间收尾（task 持有的 outbound receiver / ClientInner Arc drop）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        weak.upgrade().is_none(),
        "drop 后 Arc 环未断开：$/progress handler 闭包仍持 session Arc"
    );
}

/// P2-y5u P0 判据 #2：`ProgressRegistry.resolved` 表无界 —— LS 长会话触发大量 unique
/// progress token，handler 全部塞入 `HashSet<String>`，永不淘汰 → OOM 风险。
///
/// mock_ls 用 `MOCK_LS_PROGRESS_TOKENS_MULTI=20000` 在握手后立刻发 20000 个 unique
/// token 通知（无 waiter → 全落 resolved）。Pre-fix：resolved.len() == 20000；Post-fix：
/// 必须 ≤ 容量上限（实现用 LRU 容量上限，初始阈值 8192）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_resolved_table_is_bounded_under_burst() {
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_core::init_params::base_initialize_params;
    use lsp_core::session::Session;

    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![(
            "MOCK_LS_PROGRESS_TOKENS_MULTI".to_string(),
            "20000".to_string(),
        )],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");

    let session = tokio::time::timeout(
        Duration::from_secs(15),
        Session::start(Some(child), base_initialize_params()),
    )
    .await
    .expect("session start within 15s")
    .expect("session start Ok");

    // 等所有 burst 帧落进 handler（mock_ls 一次性写完后 stdout flush 一次）。
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let resolved_len = session.resolved_len();
    session.shutdown().await;
    // 上限阈值（实现细节可调，post-fix 必 ≤ 此值）。当前实现 LRU 容量 = 8192。
    assert!(
        resolved_len <= 8192,
        "progress.resolved 表无界（20000 个 burst 仍全保留），实测 {resolved_len}"
    );
}
