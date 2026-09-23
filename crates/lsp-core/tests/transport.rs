//! 泵拓扑集成测试（ARCHITECTURE §3.2）：spawn mock_ls → 3 泵 → 请求经 outbound mpsc
//! 写 stdin，响应帧到达 on_msg 回调。

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::client::{OutboundItem, Priority};
use lsp_core::framing::JsonRpc;
use lsp_core::transport::stdio::pump_with_priority;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;

/// tracer bullet：initialize 与 documentSymbol 两次请求往返，随后 kill 清场无残留。
#[tokio::test]
async fn pumps_roundtrip_requests() {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![exe.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    })
    .unwrap();

    let (tx, rx) = mpsc::channel::<OutboundItem>(64);
    let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);
    let (resp_tx, mut resp_rx) = mpsc::channel::<JsonRpc>(8);
    let on_msg: Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync> = Arc::new(move |msg| {
        if msg.id.is_some() && msg.method.is_none() {
            let _ = resp_tx.try_send(msg);
        }
        None
    });
    let on_eof: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

    let mut pumps = pump_with_priority(child, rx, reply_rx, reply_tx, on_msg, on_eof);

    // ① initialize 往返：mock 回 capabilities
    tx.send(OutboundItem {
        msg: JsonRpc::request(1, "initialize", json!({"capabilities": {}})),
        priority: Priority::Normal,
    })
    .await
    .unwrap();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), resp_rx.recv())
        .await
        .expect("initialize 响应 10s 内应到达 on_msg 回调")
        .unwrap();
    assert_eq!(resp.id, Some(json!(1)));
    assert_eq!(
        resp.result.unwrap()["capabilities"]["documentSymbolProvider"],
        json!(true)
    );

    // ② documentSymbol 往返：mock 回固定数组
    tx.send(OutboundItem {
        msg: JsonRpc::request(
            2,
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": "file:///mock/main.cpp"}}),
        ),
        priority: Priority::High,
    })
    .await
    .unwrap();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(10), resp_rx.recv())
        .await
        .expect("documentSymbol 响应 10s 内应到达 on_msg 回调")
        .unwrap();
    assert_eq!(resp.result.unwrap()[0]["name"], "mock_main");

    // ③ 显式 kill → Job 句柄关闭 → mock_ls 进程树无残留
    pumps.kill();
    drop(pumps);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while mock_ls_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "kill 后 mock_ls 进程仍残留：Job Object 未生效"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn mock_ls_running() -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq mock_ls.exe", "/NH"])
        .output()
        .expect("tasklist 可用（Windows 验收环境）");
    String::from_utf8_lossy(&out.stdout)
        .to_lowercase()
        .contains("mock_ls.exe")
}
