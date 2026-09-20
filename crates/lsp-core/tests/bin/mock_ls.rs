//! 回放脚本式假语言服务器（Task 4+ 集成测试的 spawn 对象，勿手跑）。
//!
//! 读 stdin 的 Content-Length 帧：
//! - `initialize`                  → 回 capabilities JSON
//! - `textDocument/documentSymbol` → 回固定符号数组
//! - `shutdown`                    → 回 null 结果；`exit` 通知后退出
//!
//! 其余带 id 的请求回 null 结果（防对端挂死），通知忽略。
//!
//! 环境变量驱动测试钩子（Task 5 + 7）：
//! - `MOCK_LS_STRING_ID_METHODS=initialize,foo` → 这些方法的回执 id 改为字符串。
//! - `MOCK_LS_SILENT_METHODS=nonexistent/method` → 这些方法不回执（用于超时用例）。
//! - `MOCK_LS_SEND_SERVER_REQUESTS=1` → 初始化后发 `client/registerCapability` 请求。
//! - `MOCK_LS_CONTENTMODIFIED_METHODS=documentSymbol` → 这些方法头 N 次回 -32801。
//! - `MOCK_LS_CONTENTMODIFIED_FAILS=2` → 配合上一项：前 N 次回 -32801，第 N+1 次成功。
//! - `MOCK_LS_TRACK_FILE_EVENTS=/path/to/track.log` → 把收到的
//!   `textDocument/didOpen` / `didChange` / `didClose` 通知按收到顺序追加写一行
//!   JSON：`{"event":"didOpen|didChange|didClose","uri":"...","version":N}`（Task 7）。
//!
//! `didOpen`/`didChange`/`didClose` 通知：**不**是请求（无 id），所以默认「忽略」
//! 路径会丢。`track_file_events` 钩子负责把这些通知记下来给测试断言。

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
    /// Phase 4 Task 22c：初始化后立刻发 `$/progress` 通知（token, kind="end"）。
    /// e2e 测试用：让 client wait_for_progress 能等到真通知。
    progress_token: Option<String>,
    /// 文档事件跟踪日志路径（Task 7）。设了就把 didOpen/didChange/didClose 追加写入。
    track_file_events: Option<std::path::PathBuf>,
    /// 开启后 capabilities 加 `diagnosticProvider: { ... }`，
    /// `textDocument/diagnostic` 返 LSP 3.17 Full 报告（Phase 2.5 测试用）。
    diagnostic_provider: bool,
    /// 开启后 `textDocument/diagnostic` 返 -32601 MethodNotFound（fallback 路径测试用）。
    diagnostic_unsupported: bool,
    /// Phase 4 Task 22a：把收到的 `initialize` params 写到该路径（一行 JSON）。
    /// supervisor monorepo 接线测试断言 workspaceFolders 数组。
    log_initialize: Option<std::path::PathBuf>,
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
        progress_token: std::env::var("MOCK_LS_PROGRESS_TOKEN").ok(),
        track_file_events: std::env::var("MOCK_LS_TRACK_FILE_EVENTS")
            .ok()
            .map(std::path::PathBuf::from),
        diagnostic_provider: std::env::var("MOCK_LS_DIAGNOSTIC_PROVIDER")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        diagnostic_unsupported: std::env::var("MOCK_LS_DIAGNOSTIC_UNSUPPORTED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        log_initialize: std::env::var("MOCK_LS_LOG_INITIALIZE")
            .ok()
            .map(std::path::PathBuf::from),
    }
}

/// 把 initialize params 写到测试日志路径（Task 22a monorepo 接线断言用）。
/// 写失败 tracing::warn —— 不致命，测试失败可定位为「mock_ls 没收到 initialize」
/// vs 「supervisor 没构造 workspaceFolders」。
async fn log_initialize(log_path: &Option<std::path::PathBuf>, msg: &JsonRpc) {
    let Some(path) = log_path else {
        return;
    };
    let params = msg.params.clone().unwrap_or(Value::Null);
    let line = match serde_json::to_string(&params) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?e, "log_initialize serialize failed");
            return;
        }
    };
    match tokio::fs::write(path, line).await {
        Ok(()) => {}
        Err(e) => tracing::warn!(?e, path = ?path.display(), "log_initialize write failed"),
    }
}

/// 把文档事件追加写到跟踪日志（Task 7 docsync 测试断言用）。一行 JSON，
/// 测试线程读 tail 时一行一事件，断言事件序列。
///
/// 写入失败不致命（测试 flaky 时这里炸会掩盖真因）；只 tracing::warn。
async fn track_event(track: &Option<std::path::PathBuf>, msg: &JsonRpc) {
    let Some(path) = track else { return };
    let Some(method) = msg.method.as_deref() else {
        return;
    };
    let event = match method {
        "textDocument/didOpen" => "didOpen",
        "textDocument/didChange" => "didChange",
        "textDocument/didClose" => "didClose",
        _ => return,
    };
    let params = msg.params.clone().unwrap_or(Value::Null);
    let uri = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // version 留作 JSON 数字（i64），与 docsync 测试断言 `as_i64()` 兼容。
    let version = params
        .pointer("/textDocument/version")
        .cloned()
        .unwrap_or(Value::Null);
    // languageId 仅 didOpen 携带（didChange/didClose 无此字段 → null）。
    let language_id = params
        .pointer("/textDocument/languageId")
        .cloned()
        .unwrap_or(Value::Null);
    let line = json!({
        "event": event,
        "uri": uri,
        "version": version,
        "languageId": language_id,
    })
    .to_string();
    let mut f = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(?e, path = ?path.display(), "打开 file-event 跟踪日志失败");
            return;
        }
    };
    if let Err(e) = async {
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        f.flush().await
    }
    .await
    {
        tracing::warn!(?e, path = ?path.display(), "写入 file-event 跟踪日志失败");
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
            // 文件事件跟踪：didOpen/didChange/didClose 是通知（无 id），默认「忽略」
            // 路径会丢；这里按方法名前缀判断追加写日志。
            track_event(&config.track_file_events, &msg).await;

            // Phase 4 Task 22a：把 initialize params 写到测试日志路径，
            // 断言 supervisor 端构造的 workspaceFolders 数组形态。
            if !initialized && msg.method.as_deref() == Some("initialize") {
                log_initialize(&config.log_initialize, &msg).await;
            }

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
                make_reply(&msg, &config).map(|mut r| {
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

            // Phase 4 Task 22c：发 `$/progress` 通知（kind="end"，模拟 LS 完成）。
            if let Some(token) = &config.progress_token
                && msg.method.as_deref() == Some("initialize")
            {
                let progress = JsonRpc::notification(
                    "$/progress",
                    json!({
                        "token": token,
                        "value": { "kind": "end", "message": "mock_ls progress done" },
                    }),
                );
                if stdout.write_all(&encode(&progress)).await.is_err() {
                    return;
                }
                let _ = stdout.flush().await;
            }

            if msg.method.as_deref() == Some("exit") {
                return;
            }
        }
    }
}


fn capabilities(config: &Config) -> Value {
    let mut caps = json!({
        "positionEncoding": "utf-16",
        "textDocumentSync": 1,
        "documentSymbolProvider": true
    });
    if config.diagnostic_provider {
        // LSP 3.17 §DiagnosticOptions：interFileDependencies + workspaceDiagnostics 必填 bool。
        caps["diagnosticProvider"] = json!({
            "interFileDependencies": false,
            "workspaceDiagnostics": false
        });
    }
    json!({
        "capabilities": caps,
        "serverInfo": { "name": "mock_ls", "version": "0.1.0" }
    })
}

fn diagnostic_report() -> Value {
    json!({
        "kind": "full",
        "items": [{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "severity": 1,
            "code": "E0001",
            "source": "mock_ls",
            "message": "mock diagnostic"
        }]
    })
}

fn make_reply(msg: &JsonRpc, config: &Config) -> Option<JsonRpc> {
    match (msg.method.as_deref(), &msg.id) {
        (Some("initialize"), Some(id)) => {
            Some(JsonRpc::response_ok(id.clone(), capabilities(config)))
        }
        (Some("textDocument/documentSymbol"), Some(id)) => {
            Some(JsonRpc::response_ok(id.clone(), document_symbols()))
        }
        (Some("textDocument/diagnostic"), Some(id)) => {
            if config.diagnostic_unsupported {
                Some(JsonRpc::response_err(
                    id.clone(),
                    RpcError {
                        code: -32601,
                        message: "Method not found".into(),
                        data: None,
                    },
                ))
            } else {
                Some(JsonRpc::response_ok(id.clone(), diagnostic_report()))
            }
        }
        (Some("shutdown"), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        (Some(_), Some(id)) => Some(JsonRpc::response_ok(id.clone(), Value::Null)),
        _ => None,
    }
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
