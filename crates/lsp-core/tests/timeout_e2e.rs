//! Phase 4 Task 22b 端到端：Client.request timeout 真触发。
//!
//! 直接构造 `Session` + mock_ls（silent `documentSymbol`），验证 Client.request
//! 触发 `CoreError::Timeout`。该路径同时是 CLI → daemon → supervisor → Client.request
//! 的最终一段；CLI forwarding 已通过 `inject_timeout_args` + supervisor 单测覆盖。

use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_request_timeout_triggers_when_ls_silent() {
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_core::init_params::base_initialize_params;
    use lsp_core::session::Session;

    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![(
            "MOCK_LS_SILENT_METHODS".to_string(),
            "textDocument/documentSymbol".to_string(),
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
    let res: Result<serde_json::Value, _> = session
        .client()
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument": {"uri": "file:///x.cpp"}}),
            Duration::from_millis(500),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(res.is_err(), "silent LS 应触发超时错误");
    let err = res.unwrap_err();
    assert!(
        matches!(err, lsp_core::error::CoreError::Timeout { .. }),
        "应 CoreError::Timeout，实际 {err:?}",
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "超时应在 2s 内触发，实测 {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(400),
        "超时不应早于 timeout 设置的 80% ({elapsed:?})"
    );
    session.shutdown().await;
}
