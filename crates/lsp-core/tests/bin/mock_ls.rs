//! 回放脚本式假语言服务器（Task 4+ 集成测试的 spawn 对象，勿手跑）。
//!
//! 读 stdin 的 Content-Length 帧：
//! - `initialize`                  → 回 capabilities JSON
//! - `textDocument/documentSymbol` → 回固定符号数组
//! - `shutdown`                    → 回 null 结果；`exit` 通知后退出
//!
//! 其余带 id 的请求回 null 结果（防对端挂死），通知忽略。
//!
//! 环境变量驱动测试钩子（Task 5）：
//! - `MOCK_LS_STRING_ID_METHODS=initialize,foo` → 这些方法的回执 id 改为字符串。
//! - `MOCK_LS_SILENT_METHODS=nonexistent/method` → 这些方法不回执（用于超时用例）。
//! - `MOCK_LS_SEND_SERVER_REQUESTS=1` → 初始化后发 `client/registerCapability` 请求。
//! - `MOCK_LS_CONTENTMODIFIED_METHODS=documentSymbol` → 这些方法头 N 次回 -32801。
//! - `MOCK_LS_CONTENTMODIFIED_FAILS=2` → 配合上一项：前 N 次回 -32801，第 N+1 次成功。

use bytes::BytesMut;
use lsp_core::framing::{JsonRpc, RpcError, decode, encode};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default, Debug)]
struct Config {
    string_id_methods: HashSet<String>,
    silent_methods: HashSet<String>,
    contentmodified_methods: HashSet<String>,
    contentmodified_fails: u32,
    send_server_requests: bool,
}

/// 各方法已回 ContentModified 次数（仅 `contentmodified_methods` 内的方法计入）。
static CM_COUNT: LazyLock<std::sync::Mutex<HashMap<String, AtomicU32>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn load_config() -> Config {
    fn parse_set(v: Option<String>) -> HashSet<String> {
        v.map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
            .unwrap_or_default()
    }
    Config {
        string_id_methods: parse_set(std::env::var("MOCK_LS_STRING_ID_METHODS").ok()),
        silent_methods: parse_set(std::env::var("MOCK_LS_SILENT_METHODS").ok()),
        contentmodified_methods: parse_set(std::env::var("MOCK_LS_CONTENTMODIFIED_METHODS").ok()),
        contentmodified_fails: std::env::var("MOCK_LS_CONTENTMODIFIED_FAILS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        send_server_requests: std::env::var("MOCK_LS_SEND_SERVER_REQUESTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    }
}

#[tokio::main]
async fn main() {
    let config = load_config();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = BytesMut::new();
    let mut chunk = [0u8; 4096];
    let mut initialized = false;
    loop {
        match stdin.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        while let Ok(Some(msg)) = decode(&mut buf) {
            // silent 方法不回执
            let is_silent = msg
                .method
                .as_deref()
                .is_some_and(|m| config.silent_methods.contains(m));
            if is_silent {
                continue;
            }

            // ContentModified 计数
            let is_cm_method = msg
                .method
                .as_deref()
                .is_some_and(|m| config.contentmodified_methods.contains(m));
            let use_cm = if is_cm_method && msg.id.is_some() {
                let method = msg.method.clone().unwrap();
                let mut guard = CM_COUNT.lock().unwrap();
                let counter = guard.entry(method).or_insert(AtomicU32::new(0));
                let n = counter.fetch_add(1, Ordering::SeqCst);
                n < config.contentmodified_fails
            } else {
                false
            };

            let reply = if use_cm {
                let id = msg.id.clone().unwrap();
                Some(JsonRpc::response_err(
                    id,
                    RpcError {
                        code: -32801,
                        message: "content modified".into(),
                        data: None,
                    },
                ))
            } else {
                make_reply(&msg).map(|mut r| {
                    // string-id 模式
                    if let Some(method) = msg.method.as_deref()
                        && config.string_id_methods.contains(method)
                        && let Some(id) = r.id.take()
                    {
                        r.id = Some(Value::String(id.to_string()));
                    }
                    r
                })
            };

            if let Some(reply) = reply {
                if stdout.write_all(&encode(&reply)).await.is_err() {
                    return;
                }
                let _ = stdout.flush().await;
            }

            // server request 钩子
            if !initialized
                && config.send_server_requests
                && msg.method.as_deref() == Some("initialize")
            {
                initialized = true;
                let srv_req = JsonRpc::request(
                    9999_i64,
                    "client/registerCapability",
                    json!({"registrations": []}),
                );
                if stdout.write_all(&encode(&srv_req)).await.is_err() {
                    return;
                }
                let _ = stdout.flush().await;
            }

            if msg.method.as_deref() == Some("initialize") {
                initialized = true;
            }

            if msg.method.as_deref() == Some("exit") {
                return;
            }
        }
    }
}

fn make_reply(msg: &JsonRpc) -> Option<JsonRpc> {
    match (msg.method.as_deref(), &msg.id) {
        (Some("initialize"), Some(id)) => Some(JsonRpc::response_ok(id.clone(), capabilities())),
        (Some("textDocument/documentSymbol"), Some(id)) => {
            Some(JsonRpc::response_ok(id.clone(), document_symbols()))
        }
        (Some("shutdown"), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        (Some(_), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        _ => None,
    }
}

fn capabilities() -> Value {
    json!({
        "capabilities": {
            "positionEncoding": "utf-16",
            "textDocumentSync": 1,
            "documentSymbolProvider": true
        },
        "serverInfo": { "name": "mock_ls", "version": "0.1.0" }
    })
}

fn document_symbols() -> Value {
    let symbol = |name: &str, line: i64| {
        json!({
            "name": name,
            "kind": 12,
            "location": {
                "uri": "file:///mock/main.cpp",
                "range": {
                    "start": { "line": line, "character": 0 },
                    "end": { "line": line, "character": name.len() as i64 }
                }
            }
        })
    };
    json!([symbol("mock_main", 0), symbol("mock_helper", 5)])
}
