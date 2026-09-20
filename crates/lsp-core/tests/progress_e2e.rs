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
