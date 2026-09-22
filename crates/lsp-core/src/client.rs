//! LSP JSON-RPC 客户端：pending 表、id 归一化、超时、ContentModified 重试、
//! server→client 请求/通知分发。
//!
//! ↖ mirror: ls_process.py@43ae021 `LanguageServerInterface`（pending 表、handler 注册、
//!   `response_id.isdigit()` 字符串 id 回退、`set_content_modified_retry_methods` 内部重试）。
//!
//! 设计要点：
//! - 写路径无锁：`Client` 把 `JsonRpc` 帧扔进 outbound mpsc，由 transport 的 writer task
//!   独占 stdin 串行写（§3.2）；`Client` 内部只持数把 `Mutex`，临界区微秒、无 await（§3.4）。
//!   注：项目技术选型禁 `parking_lot`（ARCHITECTURE §8），此处用 std Mutex。
//! - Id 归一化：响应 id 既可能是数字也可能是字符串（quirk），pending 表查找时先按
//!   `Num(i64)` 后按 `Str(String)` 试 —— 两条路径任一命中即完成 oneshot。
//! - `ContentModified(-32801)` 不外泄：在 `request()` 内重试 3 次 + 200ms 间隔，
//!   仅当方法在 `content_modified_retry_methods` 白名单内才生效；非白名单直接
//!   返回 `CoreError::Rpc{code:-32801, ...}`（ARCHITECTURE §6.1）。
//! - 泵 EOF → `abort_all()` 把 pending 表 drain 干净，全体回 `CoreError::Terminated`。
//!   ↖ mirror: ls_process.py@43ae021 `_cancel_pending_requests`。
//! - server→client 请求默认回 `null` 成功响应（vscode-languageserver-node 系把
//!   registerCapability 错误响应当致命，ARCHITECTURE §3.2 ↖ 形态同 helix transport.rs）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time;

use crate::error::{CoreError, Result};
use crate::framing::{JsonRpc, RpcError};

/// 归一化 id：服务器既可能回 `id:1` 也可能回 `id:"1"`（quirk, `response_id.isdigit()`）。
/// pending 表的 key 类型，查找时先按 `Num` 后按 `Str` 试。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Num(i64),
    Str(String),
}

impl Id {
    /// 从 JSON-RPC id 反序列化得到的 `serde_json::Value` 归一化。`null`/无 → `None`。
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(Id::Num),
            Value::String(s) => {
                // 上游 quirk：纯数字字符串归一为 Num —— 这样 mock 回 `id:"1"` 也能命中
                // `request` 时分配的 Num(1)。非数字字符串按 Str 走。
                if let Ok(n) = s.parse::<i64>() {
                    Some(Id::Num(n))
                } else {
                    Some(Id::Str(s.clone()))
                }
            }
            _ => None,
        }
    }

    /// 还原成 `serde_json::Value`，回放进 JsonRpc 时保持形态。
    pub fn to_value(&self) -> Value {
        match self {
            Id::Num(n) => Value::Number((*n).into()),
            Id::Str(s) => Value::String(s.clone()),
        }
    }
}

/// LSP ErrorCodes.ContentModified —— 内部重试，不外泄为独立变体（ARCHITECTURE §6.1）。
const CONTENT_MODIFIED: i64 = -32801;
/// LSP ErrorCodes.ServerCancelled —— server 主动 cancel（未来 wiring 用）。
#[allow(dead_code)]
const SERVER_CANCELLED: i64 = -32802;

/// 通知 handler 闭包签名。参数为收到的通知，返回 `()`；handler 自行决定是否消费。
pub type NotificationHandler = Arc<dyn Fn(JsonRpc) + Send + Sync>;
/// server→client request handler 闭包签名。参数为收到的请求，返回
/// `Some(result)` 表示回执；返回 `None` 或未注册时默认回 null 成功。
pub type ServerRequestHandler = Arc<dyn Fn(JsonRpc) -> Option<Value> + Send + Sync>;

/// Client 内部共享状态。
struct ClientInner {
    /// 服务器标识，组装 Terminated 错误用。
    ls_name: String,
    /// 出站 channel：所有写帧走它，由 transport writer task 独占消费。
    outbound: mpsc::Sender<JsonRpc>,
    /// pending 表（id → oneshot 完结）。临界区微秒无 await（§3.4）。
    pending: Mutex<HashMap<Id, oneshot::Sender<Result<JsonRpc>>>>,
    /// id 分配计数器。无锁（§3.4）。
    next_id: AtomicI64,
    /// 通知 handler 表（method → handler）。未注册即丢弃。
    notification_handlers: Mutex<HashMap<String, NotificationHandler>>,
    /// server→client request handler 表。未注册 → 默认 null 成功。
    server_request_handlers: Mutex<HashMap<String, ServerRequestHandler>>,
    /// ContentModified 重试白名单。默认空；调用方按方法 opt-in。
    content_modified_retry_methods: Mutex<HashSet<String>>,
    /// ContentModified 重试参数：3 次 + 200ms 间隔（↖ mirror 上游默认值）。
    content_modified_max_retries: u32,
    content_modified_retry_delay: Duration,
}

/// JSON-RPC 客户端。`Clone` 廉价（Arc 内部数据）。
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

impl Client {
    /// 构造。`ls_name` 仅用于 Terminated 错误的展示字段；`outbound` 由 transport 提供。
    pub fn new(outbound: mpsc::Sender<JsonRpc>) -> Self {
        Self::with_name("ls".into(), outbound)
    }

    /// 显式指定服务器名（用于 Terminated 错误呈现）。
    pub fn with_name(ls_name: String, outbound: mpsc::Sender<JsonRpc>) -> Self {
        Self {
            inner: Arc::new(ClientInner {
                ls_name,
                outbound,
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicI64::new(1),
                notification_handlers: Mutex::new(HashMap::new()),
                server_request_handlers: Mutex::new(HashMap::new()),
                content_modified_retry_methods: Mutex::new(HashSet::new()),
                content_modified_max_retries: 3,
                content_modified_retry_delay: Duration::from_millis(200),
            }),
        }
    }

    /// 启用某方法的 ContentModified 重试（opt-in 白名单）。
    pub fn set_content_modified_retry<I, S>(&self, methods: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut guard = self.inner.content_modified_retry_methods.lock().unwrap();
        guard.clear();
        guard.extend(methods.into_iter().map(Into::into));
    }

    /// 注册通知 handler（method 精确匹配）。同 method 多次注册后者覆盖前者。
    pub fn on_notification<F>(&self, method: impl Into<String>, f: F)
    where
        F: Fn(JsonRpc) + Send + Sync + 'static,
    {
        let mut guard = self.inner.notification_handlers.lock().unwrap();
        guard.insert(method.into(), Arc::new(f));
    }

    /// 移除指定方法的通知 handler。返回是否曾注册。P2-y5u 配套：`Session::shutdown`
    /// 显式清 `$/progress` 闭包，主动断开 Session↔ClientInner 的 Arc 环路径。
    pub fn clear_notification(&self, method: &str) -> bool {
        let mut guard = self.inner.notification_handlers.lock().unwrap();
        guard.remove(method).is_some()
    }

    /// 注册 server→client request handler。返回 `None` 即默认 null 成功。
    pub fn on_server_request<F>(&self, method: impl Into<String>, f: F)
    where
        F: Fn(JsonRpc) -> Option<Value> + Send + Sync + 'static,
    {
        let mut guard = self.inner.server_request_handlers.lock().unwrap();
        guard.insert(method.into(), Arc::new(f));
    }

    /// 发送通知（无 id、不期待响应）。
    pub fn notify(&self, method: impl AsRef<str>, params: Value) -> Result<()> {
        let msg = JsonRpc::notification(method.as_ref(), params);
        self.inner
            .outbound
            .try_send(msg)
            .map_err(|e| CoreError::Io(std::io::Error::other(format!("outbound closed: {e}"))))
    }

    /// 同步等待请求结果。`timeout` 到期 → `CoreError::Timeout{method, secs}`。
    ///
    /// 若响应 `error.code == -32801` 且方法在重试白名单内：内部按 `3 × 200ms` 重试；
    /// 超限仍 -32801 或非白名单 → 返回 `CoreError::Rpc{code, message}`。
    pub async fn request<R>(
        &self,
        method: impl AsRef<str>,
        params: Value,
        timeout: Duration,
    ) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        let method = method.as_ref().to_string();
        let max_retries = self.inner.content_modified_max_retries;
        let retry_delay = self.inner.content_modified_retry_delay;
        let in_retry_list = {
            let g = self.inner.content_modified_retry_methods.lock().unwrap();
            g.contains(&method)
        };

        let mut attempt: u32 = 0;
        loop {
            let reply = self.send_once(&method, params.clone(), timeout).await?;
            match &reply.error {
                Some(RpcError { code, .. }) if *code == CONTENT_MODIFIED && in_retry_list => {
                    attempt += 1;
                    if attempt >= max_retries {
                        // 重试用尽 —— 按 Rpc 外抛，调用方可识别 -32801 自行决定。
                        return Err(rpc_err_from(reply.error.as_ref().unwrap()));
                    }
                    time::sleep(retry_delay).await;
                    continue;
                }
                Some(_) => return Err(rpc_err_from(reply.error.as_ref().unwrap())),
                None => {
                    let result = reply.result.unwrap_or(Value::Null);
                    return serde_json::from_value(result).map_err(|e| CoreError::Rpc {
                        code: -1,
                        message: format!("response decode failed: {e}"),
                    });
                }
            }
        }
    }

    /// 单次请求：分配 id、插 pending、发帧、等 oneshot。`Timeout` 在此产生。
    async fn send_once(&self, method: &str, params: Value, timeout: Duration) -> Result<JsonRpc> {
        let id_num = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = Id::Num(id_num);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.insert(id.clone(), tx);
        }

        let msg = JsonRpc::request(id_num, method, params);
        if let Err(e) = self.inner.outbound.send(msg).await {
            // outbound 关 → pending 也要清，否则 zombie oneshot
            let mut pending = self.inner.pending.lock().unwrap();
            pending.remove(&id);
            return Err(CoreError::Terminated {
                ls: self.inner.ls_name.clone(),
                cause: format!("outbound send failed: {e}"),
            });
        }

        let secs = timeout.as_secs();
        match time::timeout(timeout, rx).await {
            Ok(Ok(Ok(reply))) => Ok(reply),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_canceled)) => {
                // oneshot 被 drop —— 通常是被 abort_all drain 走了
                Err(CoreError::Terminated {
                    ls: self.inner.ls_name.clone(),
                    cause: "oneshot canceled".into(),
                })
            }
            Err(_elapsed) => {
                // 超时：从 pending 表移除自己，避免后续响应误完成一个已无人等的 channel。
                let mut pending = self.inner.pending.lock().unwrap();
                pending.remove(&id);
                Err(CoreError::Timeout {
                    method: method.to_string(),
                    secs,
                })
            }
        }
    }

    /// 处理一帧入站消息。返回 `Some(reply)` 仅当 server→client request 需回执。
    ///
    /// 调用方（transport 泵）拿到 `Some(reply)` 应经 outbound 写回 LS。
    pub fn handle_message(&self, msg: JsonRpc) -> Option<JsonRpc> {
        match (msg.id.as_ref(), msg.method.as_ref()) {
            // 响应帧（id 有，method 无）：pop pending，完结 oneshot。
            (Some(id_value), None) => {
                let Some(id) = Id::from_value(id_value) else {
                    tracing::warn!(?id_value, "response id 无法归一化，丢弃");
                    return None;
                };
                let sender = {
                    let mut pending = self.inner.pending.lock().unwrap();
                    // 数字与字符串双查：先按原 id 命中；miss 时按数字↔字符串镜像试一次。
                    pending.remove(&id).or_else(|| match &id {
                        Id::Num(n) => pending.remove(&Id::Str(n.to_string())),
                        Id::Str(s) => s
                            .parse::<i64>()
                            .ok()
                            .and_then(|n| pending.remove(&Id::Num(n))),
                    })
                };
                if let Some(sender) = sender {
                    let _ = sender.send(Ok(msg));
                } else {
                    tracing::debug!(?id, "收到未在 pending 表的响应（可能已超时）");
                }
                None
            }

            // 通知（id 无，method 有）：调 handler，未注册则丢弃。
            (None, Some(method)) => {
                let handlers = self.inner.notification_handlers.lock().unwrap();
                if let Some(handler) = handlers.get(method.as_str()) {
                    (handler)(msg);
                } else {
                    tracing::trace!(method = %method, "未注册的通知 handler，丢弃");
                }
                None
            }

            // server→client request（id 有，method 有）：调 handler，默认 null 成功。
            (Some(id_value), Some(method)) => {
                let Some(id) = Id::from_value(id_value) else {
                    tracing::warn!(?id_value, "server request id 无法归一化");
                    return None;
                };
                let result = {
                    let handlers = self.inner.server_request_handlers.lock().unwrap();
                    handlers
                        .get(method.as_str())
                        .and_then(|h| (h)(msg))
                        .unwrap_or(Value::Null)
                };
                Some(JsonRpc::response_ok(id.to_value(), result))
            }

            // 既无 id 又无 method —— 非法 JSON-RPC，丢弃。
            (None, None) => {
                tracing::warn!("收到既无 id 又无 method 的帧，丢弃");
                None
            }
        }
    }

    /// Drain pending 表：全体回 Terminated。泵 EOF / Session 关停时调用。
    pub fn abort_all(&self) {
        let drained: Vec<(Id, oneshot::Sender<Result<JsonRpc>>)> = {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.drain().collect()
        };
        let count = drained.len();
        for (_id, sender) in drained {
            let _ = sender.send(Err(CoreError::Terminated {
                ls: self.inner.ls_name.clone(),
                cause: "stdout pump EOF".into(),
            }));
        }
        if count > 0 {
            tracing::debug!(count, "drained pending requests on pump EOF");
        }
    }

    /// 当前 pending 表条目数（诊断用）。
    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.inner.pending.lock().unwrap().len()
    }
}

fn rpc_err_from(e: &RpcError) -> CoreError {
    if e.code == SERVER_CANCELLED {
        CoreError::ServerCancelled {
            method: "<response>".into(),
        }
    } else {
        CoreError::Rpc {
            code: e.code,
            message: e.message.clone(),
        }
    }
}
