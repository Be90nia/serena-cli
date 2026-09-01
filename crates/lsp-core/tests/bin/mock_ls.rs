//! 回放脚本式假语言服务器（Task 4+ 集成测试的 spawn 对象，勿手跑）。
//!
//! 读 stdin 的 Content-Length 帧：
//! - `initialize`                  → 回 capabilities JSON
//! - `textDocument/documentSymbol` → 回固定符号数组
//! - `shutdown`                    → 回 null 结果；`exit` 通知后退出
//!   其余带 id 的请求回 null 结果（防对端挂死），通知忽略。
//!
//! 测试经 `env!("CARGO_BIN_EXE_mock_ls")` 拉起；stdout 是协议通道，禁止额外打印。

use bytes::BytesMut;
use lsp_core::framing::{JsonRpc, decode, encode};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = BytesMut::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stdin.read(&mut chunk).await {
            Ok(0) | Err(_) => return, // EOF：父进程关 stdin，正常退出
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        while let Ok(Some(msg)) = decode(&mut buf) {
            if serve(&msg, &mut stdout).await {
                return; // 收到 exit 通知
            }
        }
    }
}

/// 处理一条入站消息；返回 true 表示应退出。
async fn serve(msg: &JsonRpc, out: &mut tokio::io::Stdout) -> bool {
    let reply = match (msg.method.as_deref(), &msg.id) {
        (Some("initialize"), Some(id)) => Some(JsonRpc::response_ok(id.clone(), capabilities())),
        (Some("textDocument/documentSymbol"), Some(id)) => {
            Some(JsonRpc::response_ok(id.clone(), document_symbols()))
        }
        (Some("shutdown"), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        (Some("exit"), _) => return true,
        (Some(_), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        _ => None, // 其余通知忽略
    };
    if let Some(reply) = reply {
        if out.write_all(&encode(&reply)).await.is_err() {
            return true; // 对端没了
        }
        let _ = out.flush().await;
    }
    false
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
