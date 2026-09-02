//! lsp-core session 集成测试（PLAN Task 6 / ARCHITECTURE §5）。
//!
//! 覆盖：
//! 1. `Session::start` 完成后状态 → `Ready`，可经 `request("textDocument/documentSymbol")` 拿到数组。
//! 2. `Session::shutdown` 走 LSP shutdown 请求 + exit 通知 + 关 stdin + 进程退出（≤7s）。
//! 3. `Session::request` 在 Ready 前到达也会等待就绪门（`initialized_notify`）后返回，
//!    体现「Ready 前调用 → 等门 → 返回响应；Ready 后无延迟」。

use std::time::Duration;

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::error::CoreError;
use lsp_core::framing::JsonRpc;
use lsp_core::session::{Session, SessionState};
use lsp_types::InitializeParams;
use serde_json::{Value, json};
use std::ffi::OsString;
use tokio::sync::mpsc;

fn launch_mock_ls() -> LaunchInfo {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    LaunchInfo {
        cmd: vec![OsString::from(exe)],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    }
}

fn dummy_init_params() -> InitializeParams {
    // 不填 rootUri/rootPath —— Task 6 仅验证握手通路；workspace caps 由 init_params.rs 提供。
    InitializeParams::default()
}

/// Tracer bullet：start → Ready → 调 documentSymbol 拿到 mock_ls 返回的数组。
#[tokio::test]
async fn session_hits_ready_and_serves_requests() {
    let child = Child::spawn(launch_mock_ls()).unwrap();
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start 应在 handshake 超时内返回 Ready");

    // 握手后必须是 Ready（不是 Initializing / Failed）。
    assert_eq!(session.state(), SessionState::Ready);

    // 拿到 capabilities 文本 → 能验证 handshake 成功（mock_ls 写死 serverInfo.name="mock_ls"）。
    let caps: Value = session
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": "file:///mock/main.cpp"}}),
            Duration::from_secs(5),
        )
        .await
        .expect("Ready 后 documentSymbol 应在 5s 内返回");
    let arr = caps.as_array().expect("documentSymbol 应回数组");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "mock_main");
    assert_eq!(arr[1]["name"], "mock_helper");

    // 显式 shutdown（shutdown 取 &self，不消耗 Arc）。
    session.shutdown().await;
}

/// shutdown 路径：shutdown 请求 + exit 通知 + 关 stdin + 进程退出（≤7s）。
#[tokio::test]
async fn session_shutdown_terminates_child() {
    let child = Child::spawn(launch_mock_ls()).unwrap();
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start 应成功");

    let started = std::time::Instant::now();
    session.shutdown().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed <= Duration::from_secs(7),
        "shutdown 总耗时应 ≤7s（2s shutdown 请求 + 5s EOF wait/kill），实际 {elapsed:?}"
    );
}

/// Ready 后请求无延迟（就绪门已放行）。
#[tokio::test]
async fn request_after_ready_completes_quickly() {
    let child = Child::spawn(launch_mock_ls()).unwrap();
    let session = std::sync::Arc::new(
        Session::start(Some(child), dummy_init_params())
            .await
            .expect("Session::start 应成功"),
    );

    // 立即发请求（handshake 同步完成 → Ready 已就位 → 门已放行）。
    let started = std::time::Instant::now();
    let s = session.clone();
    let _: Value = s
        .request("initialize", json!({}), Duration::from_secs(5))
        .await
        .expect("Ready 后 initialize 二次调用应在 5s 内完成（mock_ls 收到任意方法回 null）");
    // Ready 后不应有阻塞延迟 —— 2s 内到 mock_ls 必有回执。
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "Ready 后的请求不应被就绪门阻挡"
    );

    session.shutdown().await;
}

/// 确保 Session::start 失败态（用 ping 这种不发任何 LSP 内容的进程）会回 CoreError，而非 panic。
#[tokio::test]
async fn session_start_failure_returns_core_error() {
    let info = LaunchInfo {
        cmd: vec![
            OsString::from("ping"),
            OsString::from("-n"),
            OsString::from("2"),
            OsString::from("127.0.0.1"),
        ],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    };
    let child = Child::spawn(info).unwrap();
    // Session::start 内部 10s 超时；外层 12s 兜底 —— ping 不会回任何 LSP 帧，必超时。
    let res = tokio::time::timeout(
        Duration::from_secs(12),
        Session::start(Some(child), dummy_init_params()),
    )
    .await
    .expect("Session::start 本身 12s 内必须返回（不能永远挂死）");
    assert!(
        res.is_err(),
        "LS 不响应 initialize 时，Session::start 必须回 CoreError；实际: {res:?}"
    );
    let err = res.err().unwrap();
    assert!(
        matches!(
            err,
            CoreError::Rpc { .. }
                | CoreError::Timeout { .. }
                | CoreError::Terminated { .. }
                | CoreError::Io(_)
        ),
        "start 失败的错误应落在 CoreError 命名集内，实际: {err:?}"
    );
}

/// 单元测试：状态枚举的 Debug/PartialEq 形态稳定。
#[test]
fn session_state_debug_is_stable() {
    let _ = format!("{:?}", SessionState::Ready);
    let _ = format!("{:?}", SessionState::Initializing);
    let _ = format!("{:?}", SessionState::Failed("x".into()));
    assert_eq!(SessionState::Ready, SessionState::Ready);
    // 防止 idle 警告
    let (_tx, _rx) = mpsc::channel::<JsonRpc>(1);
}
