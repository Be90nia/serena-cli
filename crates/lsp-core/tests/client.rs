//! lsp-core client 集成测试（ARCHITECTURE §3.2/§3.4/§6.1）。
//!
//! 覆盖 PLAN Task 5 的 6 条用例：
//! 1. 正常 id 关联（mock 回 `{"id":1,...}`）。
//! 2. 字符串 id 回退（mock 回 `"id":"1"` 仍完成请求 —— `response_id.isdigit()` quirk）。
//! 3. 超时 → `CoreError::Timeout{method, secs}`。
//! 4. 泵 EOF → pending 全体收到 `CoreError::Terminated`。
//! 5. server→client 请求默认 null 响应（ARCHITECTURE §3.2 vscode-languageserver-node
//!    系 registerCapability 错误当致命 —— 我们默认回 null 成功）。
//! 6. ContentModified 重试：白名单 + 3 次 + 200ms 间隔。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::client::{Client, Id};
use lsp_core::error::CoreError;
use lsp_core::framing::JsonRpc;
use lsp_core::transport::stdio::{Pumps, pump};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct Rig {
    client: Client,
    pumps: Option<Pumps>,
}

impl Rig {
    fn kill(mut self) {
        if let Some(mut p) = self.pumps.take() {
            p.kill();
        }
    }
}

async fn boot(envs: &[(&str, &str)]) -> Rig {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![exe.into_os_string()],
        cwd: std::env::temp_dir(),
        env: envs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        transport: TransportKind::Stdio,
    })
    .unwrap();

    let (tx, rx) = mpsc::channel::<JsonRpc>(64);
    let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);

    let client = Client::with_name("mock_ls".into(), tx);
    let client_for_pump = client.clone();
    let on_msg: Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync> = {
        let c = client_for_pump.clone();
        Arc::new(move |msg| c.handle_message(msg))
    };
    let on_eof: Arc<dyn Fn() + Send + Sync> = {
        let c = client_for_pump.clone();
        Arc::new(move || c.abort_all())
    };
    let pumps = pump(child, rx, reply_rx, reply_tx, on_msg, on_eof);

    Rig {
        client,
        pumps: Some(pumps),
    }
}

#[tokio::test]
async fn normal_id_roundtrip() {
    let rig = boot(&[]).await;
    let result: Value = rig
        .client
        .request(
            "initialize",
            json!({"capabilities": {}}),
            Duration::from_secs(5),
        )
        .await
        .expect("initialize 应在 5s 内返回");
    assert_eq!(result["serverInfo"]["name"], "mock_ls");
    rig.kill();
}

#[tokio::test]
async fn string_id_fallback_completes_request() {
    let rig = boot(&[("MOCK_LS_STRING_ID_METHODS", "initialize")]).await;
    let result: Value = rig
        .client
        .request(
            "initialize",
            json!({"capabilities": {}}),
            Duration::from_secs(5),
        )
        .await
        .expect("字符串 id 也应走 i64 → 字符串归一化回退完成请求");
    assert_eq!(result["serverInfo"]["name"], "mock_ls");
    rig.kill();
}

#[tokio::test]
async fn timeout_returns_core_timeout() {
    let rig = boot(&[("MOCK_LS_SILENT_METHODS", "nonexistent/method")]).await;
    let err = rig
        .client
        .request::<Value>("nonexistent/method", json!({}), Duration::from_millis(300))
        .await
        .expect_err("无响应方法应在超时后返回 Err");
    assert!(
        matches!(&err, CoreError::Timeout { method, secs: 0 } if method == "nonexistent/method"),
        "应为 CoreError::Timeout，实际: {err:?}"
    );
    rig.kill();
}

#[tokio::test]
async fn eof_drains_pending_with_terminated() {
    let mut rig = boot(&[("MOCK_LS_SILENT_METHODS", "nonexistent/method")]).await;
    let _: Value = rig
        .client
        .request("initialize", json!({}), Duration::from_secs(5))
        .await
        .expect("initialize 必成功");

    let client2 = rig.client.clone();
    let pending_fut = tokio::spawn(async move {
        client2
            .request::<Value>("nonexistent/method", json!({}), Duration::from_secs(5))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    if let Some(mut p) = rig.pumps.take() {
        p.kill();
    }

    let res = pending_fut.await.unwrap();
    assert!(
        matches!(&res, Err(CoreError::Terminated { ls, .. }) if ls == "mock_ls"),
        "EOF drain 应让 pending 收到 CoreError::Terminated，实际: {res:?}"
    );
}

#[tokio::test]
async fn server_to_client_request_gets_default_null_reply() {
    let (out_tx, _out_rx) = mpsc::channel::<JsonRpc>(8);
    let client = Client::with_name("mock_ls".into(), out_tx);
    let srv_req = JsonRpc {
        jsonrpc: "2.0".into(),
        id: Some(Value::Number(42.into())),
        method: Some("client/registerCapability".into()),
        params: Some(json!({})),
        result: None,
        error: None,
    };
    let reply = client
        .handle_message(srv_req)
        .expect("server→client request 未注册 handler 时必须返回默认 null 回执");
    assert_eq!(reply.method, None, "回执应是 response 帧（method=None）");
    assert_eq!(
        reply.id,
        Some(Value::Number(42.into())),
        "回执 id 必须镜像原请求"
    );
    assert_eq!(
        reply.result,
        Some(Value::Null),
        "未注册 handler 默认回 null"
    );
}

#[tokio::test]
async fn server_to_client_request_e2e_with_mock_ls() {
    let (capture_tx, mut capture_rx) = mpsc::channel::<JsonRpc>(4);
    let count = Arc::new(AtomicUsize::new(0));

    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![exe.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![("MOCK_LS_SEND_SERVER_REQUESTS".into(), "1".into())],
        transport: TransportKind::Stdio,
    })
    .unwrap();

    let (out_tx, out_rx) = mpsc::channel::<JsonRpc>(64);
    let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);
    let client = Client::with_name("mock_ls".into(), out_tx.clone());
    let capture_tx_clone = capture_tx.clone();
    let count_clone = count.clone();
    let on_msg: Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync> = {
        let c = client.clone();
        Arc::new(move |msg| {
            let r = c.handle_message(msg);
            if let Some(reply) = &r {
                count_clone.fetch_add(1, Ordering::SeqCst);
                let _ = capture_tx_clone.try_send(reply.clone());
            }
            r
        })
    };
    let on_eof: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
    let mut pumps = pump(child, out_rx, reply_rx, reply_tx, on_msg, on_eof);

    // 先发 initialize 触发 mock_ls 完成握手 + 触发它发 server→client request。
    let init_resp: Value = client
        .request("initialize", json!({}), Duration::from_secs(5))
        .await
        .expect("initialize 必成功");
    assert_eq!(init_resp["serverInfo"]["name"], "mock_ls");

    let reply = tokio::time::timeout(Duration::from_secs(5), capture_rx.recv())
        .await
        .expect("server→client 请求 5s 内应得到默认 null 回执")
        .unwrap();
    assert_eq!(reply.method, None);
    assert!(reply.id.is_some());
    assert_eq!(reply.result, Some(Value::Null));
    assert!(
        count.load(Ordering::SeqCst) >= 1,
        "handle_message 至少产生 1 个回执"
    );

    drop(capture_tx);
    pumps.kill();
    drop(pumps);
    let _ = out_tx;
}

#[tokio::test]
async fn content_modified_retries_then_succeeds() {
    let rig = boot(&[
        (
            "MOCK_LS_CONTENTMODIFIED_METHODS",
            "textDocument/documentSymbol",
        ),
        ("MOCK_LS_CONTENTMODIFIED_FAILS", "2"),
    ])
    .await;
    rig.client
        .set_content_modified_retry(["textDocument/documentSymbol"]);

    let symbols: Value = rig
        .client
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": "file:///x"}}),
            Duration::from_secs(5),
        )
        .await
        .expect("白名单内 ContentModified 应被内部重试直到成功");
    assert_eq!(symbols[0]["name"], "mock_main");
    rig.kill();
}

/// Id 归一化枚举的单元测试：数字与字符串按规范归到 Num/Str。
#[test]
fn id_normalizes_from_json_value() {
    let n = Id::from_value(&json!(42)).expect("整数应归一化");
    assert_eq!(n, Id::Num(42));
    let s = Id::from_value(&json!("abc")).expect("字符串应归一化");
    assert_eq!(s, Id::Str("abc".into()));
    let digit_str = Id::from_value(&json!("1")).expect("数字字符串应归一化到 Num");
    assert_eq!(digit_str, Id::Num(1));
    assert!(Id::from_value(&json!(null)).is_none());
}
