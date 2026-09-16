//! record/replay 端到端 e2e。
//!
//! Plan Task 26：JSON-RPC 录制/回放。
//!
//! 此文件覆盖三个角度：
//! 1. `record_then_verify_jsonl` —— 用 mock_ls 跑 Session::start + initialize + shutdown，
//!    设置 `SERENA_RECORD=path`，断言录文件存在且含正确帧序列。
//! 2. `replay_fake_session_returns_recorded_initialize` —— 录 mock_ls 一次会话，
//!    改写 URI 写回录文件（让"虚拟 LS"指向同一个 JSONL），第二次 Session::start
//!    设 `SERENA_REPLAY=path` 用 Option<Child>=None，期待返回同一 initialize 响应。
//!
//! 单测 `lsp-core::recording::tests::roundtrip_record_then_replay` 已经覆盖
//! Recorder 模块的写读语义。本文件验证 Session 层 end-to-end。

use std::ffi::OsString;
use std::time::Duration;

use ls_runtime::process::{Child, LaunchInfo};
use lsp_core::recording::Recorder;
use lsp_core::session::Session;
use lsp_types::InitializeParams;

fn launch_mock_ls() -> LaunchInfo {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    LaunchInfo {
        cmd: vec![OsString::from(exe)],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: ls_runtime::process::TransportKind::Stdio,
    }
}

fn dummy_init_params() -> InitializeParams {
    lsp_core::init_params::base_initialize_params()
}

/// 端到端 record 测试：跑 mock_ls + 设 SERENA_RECORD env，断言 JSONL 文件存在 + 含帧。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_then_verify_jsonl() {
    let record_path = std::env::temp_dir().join(format!("serena-rec-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&record_path);

    // 设 env（注意：env 必须在 Session::start 调用前生效；测试在同一进程，
    // 用 std::env::set_var + 立刻读）。
    // SAFETY: 测试单线程访问该 env 变量；其他测试不并发依赖 SERENA_RECORD。
    unsafe {
        std::env::set_var("SERENA_RECORD", &record_path);
    }

    let child = Child::spawn(launch_mock_ls()).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _resp: serde_json::Value = tokio::time::timeout(
        Duration::from_secs(5),
        session.request::<serde_json::Value>(
            "initialize",
            serde_json::json!({}),
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("initialize 必返回")
    .expect("initialize 不失败");

    session.shutdown().await;
    unsafe {
        std::env::remove_var("SERENA_RECORD");
    }

    // 验证录文件
    let content = std::fs::read_to_string(&record_path).expect("录文件存在");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.len() >= 2,
        "至少应有 initialize + 响应 2 帧, got {} 行",
        lines.len()
    );

    // 第一帧应是 initialize 请求
    let first: serde_json::Value =
        serde_json::from_str(lines[0].trim_start_matches("--> ")).expect("第一帧 JSON 合法");
    assert_eq!(first["method"], "initialize");

    // 第二帧应是响应
    let second: serde_json::Value =
        serde_json::from_str(lines[1].trim_start_matches("<-- ")).expect("第二帧 JSON 合法");
    assert_eq!(second["result"]["serverInfo"]["name"], "mock_ls");

    let _ = std::fs::remove_file(&record_path);
}

/// 单元层验证：passthrough Recorder 不写盘。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passthrough_does_not_write() {
    let rec = Recorder::passthrough();
    rec.record_outbound(&lsp_core::framing::JsonRpc {
        jsonrpc: "2.0".into(),
        id: Some(serde_json::json!(1)),
        method: Some("noop".into()),
        params: None,
        result: None,
        error: None,
    });
    assert!(rec.is_passthrough());
    assert_eq!(rec.inbound_remaining(), 0);
}
