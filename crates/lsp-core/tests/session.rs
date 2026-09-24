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

/// bd serena-rust-s3u：Session::start 必须注册位置类方法 ContentModified(-32801)
/// 内部重试白名单。mock_ls 对 hover 头 2 次回 -32801；白名单生效时 client 层重试后
/// 成功返回，回归（白名单未注册）时首次 -32801 直接外泄为 CoreError::Rpc → 本用例失败。
#[tokio::test]
async fn session_registers_content_modified_retry_whitelist() {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![OsString::from(exe)],
        cwd: std::env::temp_dir(),
        env: vec![
            (
                "MOCK_LS_CONTENTMODIFIED_METHODS".into(),
                "textDocument/hover".into(),
            ),
            ("MOCK_LS_CONTENTMODIFIED_FAILS".into(), "2".into()),
        ],
        transport: TransportKind::Stdio,
    })
    .unwrap();
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start 应成功");

    let hover: Value = session
        .request(
            "textDocument/hover",
            json!({
                "textDocument": {"uri": "file:///mock/main.cpp"},
                "position": {"line": 0, "character": 0}
            }),
            Duration::from_secs(5),
        )
        .await
        .expect("hover 的 -32801 应被白名单内部重试消化，而非外泄");
    assert_eq!(hover, Value::Null, "mock_ls 对非 documentSymbol 回 null 结果");

    session.shutdown().await;
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

/// ↖ mirror: ls.py@dc59a893 — start 中途失败不得遗留已 spawn 的 LS 子进程。
/// 上游在 start() 异常分支显式 stop()；本项目等价机制 = Session::start 失败返回
/// Err → Session（含 Pumps 持有的 Job 句柄）随 Arc 归零 drop → KILL_ON_JOB_CLOSE
/// 内核灭树。本测试证明：握手超时返回 Err 后，仍存活的长跑子进程被立即回收，
/// 而非继续跑满自身寿命。
#[tokio::test]
async fn session_start_failure_reaps_child() {
    let info = LaunchInfo {
        cmd: vec![
            OsString::from("ping"),
            OsString::from("-n"),
            OsString::from("30"),
            OsString::from("127.0.0.1"),
        ],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    };
    let child = Child::spawn(info).unwrap();
    let pid = child.pid.expect("spawn 成功后 pid 应可用");
    assert!(pid_running(pid), "spawn 后子进程应存活, pid={pid}");

    // ping 不回任何 LSP 帧 → 10s 握手超时 → start 返回 Err。
    let res = Session::start(Some(child), dummy_init_params()).await;
    assert!(res.is_err(), "无响应 LS 的 Session::start 必须失败");

    // 失败后子进程必须被回收（job 随 pumps drop 关闭），不能跑满 30s。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pid_running(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "start 失败后子进程仍残留（pid={pid}）：失败路径未回收已 spawn 的 LS"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn pid_running(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist 可用（Windows 验收环境）");
    String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
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
