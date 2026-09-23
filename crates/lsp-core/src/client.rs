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
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time;

use crate::error::{CoreError, Result};
use crate::framing::{JsonRpc, RpcError};

/// P0B：出站帧分类优先级。三级队列：High 永不排队等 Normal，Normal 累积 50ms
/// 降级 Background，Background 走 TokenBucket 限流（30/s、burst 2s）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// 用户面向 LSP 请求（textDocument/*、workspace/executeCommand 等）。直插队首，
    /// 不被任何 Normal/Background 帧阻塞。
    ///
    /// 审计 F6 更正（2026-09-23）：协议顺序敏感帧 `initialized` 与
    /// didOpen/didChange/didClose 也归 High（见 classify_method 文档）——
    /// 本 variant 的"中性通知"示例已过时，以 classify_method 实现为准。
    High,
    /// RA 自发通知中无顺序约束的（workspace/didChangeWatchedFiles、$/progress、
    /// exit 等）。默认 priority。
    Normal,
    /// 索引/监听/扫描类（$/workspace/_ping、workspace/symbol 走后台、crate 重解析）。
    Background,
}

/// 携带优先级标记的出站帧。`Client` 唯一发送形态，writer 三 channel 入口。
#[derive(Debug)]
pub struct OutboundItem {
    pub msg: JsonRpc,
    pub priority: Priority,
}

/// 按 method 归类出站优先级。命名规则：textDocument/* → High；workspace/... 但非
/// `_ping` → Normal；workspace/_ping、workspace/symbol（目录扫描）等后台 → Background；
/// $/progress、$/workspace/_ping 等 RA 私域 → Background。**规则保守优先：High 仅限
/// 用户面向，**索引/扫描类**一律 Background，绝不抢占用户请求**。
///
/// P0B race 修（2026-09-22）：`initialized` / didOpen / didChange / didClose 必须与
/// 其后续 textDocument 请求保持协议顺序 —— 它们若留在 Normal 会被 50ms demote 到
/// Background，让 High 请求插队到 `initialized` 之前（rust-analyzer 严格校验顺序，
/// 收到未握手请求直接退出 → 管道断 → channel closed），或插到 didChange 之前
/// （RA 读到旧内容 → 符号体错位 → 写坏代码）。升 High 后与请求同队列 FIFO，
/// 到达序即协议序，demote 永不触及。
pub fn classify_method(method: &str) -> Priority {
    // 协议顺序敏感：握手 + 文档同步必须先于依赖它们的请求写出。
    if method == "initialized"
        || method == "textDocument/didOpen"
        || method == "textDocument/didChange"
        || method == "textDocument/didClose"
    {
        return Priority::High;
    }
    // 后台探测/保活（rust-analyzer `$/workspace/_ping` 等）—— 优先 Background。
    if method.starts_with("$/workspace/_ping") || method == "workspace/_ping" {
        return Priority::Background;
    }
    // RA workspace/symbol 在大型 workspace 是重量级索引后端 → Background
    // （supervisor 端用户主动 search 用 High 重写方法名时再走 High）。
    if method == "workspace/symbol" {
        return Priority::Background;
    }
    // 用户面向的 LSP 文本请求：textDocument/*（documentSymbol/hover/references 等）。
    if method.starts_with("textDocument/")
        || method == "workspace/executeCommand"
        || method == "workspace/workspaceFolders"
        || method == "workspace/configuration"
        || method == "window/workDoneProgress/create"
    {
        return Priority::High;
    }
    // RA 自发 / 中性通知（didChangeWatchedFiles、$/progress、exit、shutdown 等）
    // 走 Normal。注意 initialized / didOpen / didChange / didClose 已在上面升 High
    // （协议顺序敏感，审计 F6：勿按旧行为"修正"回 Normal —— 会复现 RA 未握手退出）。
    Priority::Normal
}

/// 简易令牌桶：限 Background 帧速率。`per_sec` 长期速率上限；`burst` 允许瞬时积攒
/// 的令牌数上限（等价于 2s 抑制窗）。线程安全（内部 `Mutex<State>`，临界区仅几条
/// 整数 + Instant 比较，无 await）。
///
/// 用法：每次准备发 Background 帧前 `try_acquire(now)` → true 才放行，false 则 sleep。
pub struct TokenBucket {
    per_sec: u32,
    burst: u32,
    state: Mutex<TokenState>,
}

#[derive(Debug, Clone, Copy)]
struct TokenState {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// 构造。`per_sec` ≤ 0 等价 30/s；`burst` ≤ 0 等价 2s 抑制窗。
    pub fn new(per_sec: u32, burst: u32) -> Self {
        let per_sec = if per_sec == 0 { 30 } else { per_sec };
        let burst = if burst == 0 { per_sec.saturating_mul(2).max(1) } else { burst };
        Self {
            per_sec,
            burst,
            state: Mutex::new(TokenState {
                tokens: burst as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// 尝试取一个令牌。返回 true 表示放行（已扣 1）；false 表示节流（调用方应 sleep）。
    pub fn try_acquire(&self, now: Instant) -> bool {
        let mut s = self.state.lock().unwrap();
        // 按 elapsed 补满令牌（连续积攒上限 = burst）。
        let elapsed = now.saturating_duration_since(s.last_refill);
        let refill = (elapsed.as_secs_f64()) * (self.per_sec as f64);
        s.tokens = (s.tokens + refill).min(self.burst as f64);
        s.last_refill = now;
        if s.tokens >= 1.0 {
            s.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// 距下次放行的最短等待。测试断言常量。
    pub fn per_sec(&self) -> u32 {
        self.per_sec
    }
}

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
    /// 出站 channel：所有写帧（带 priority 标记）走它，由 transport writer task
    /// 通过 priority-aware router 独占消费。
    outbound: mpsc::Sender<OutboundItem>,
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
    /// 构造。`ls_name` 仅用于 Terminated 错误的展示字段；`outbound` 由 transport
    /// 提供。`Sender<OutboundItem>` 携带优先级，由 writer 端按 priority 路由。
    pub fn new(outbound: mpsc::Sender<OutboundItem>) -> Self {
        Self::with_name("ls".into(), outbound)
    }

    /// 显式指定服务器名（用于 Terminated 错误呈现）。见 [`Client::new`]。
    pub fn with_name(ls_name: String, outbound: mpsc::Sender<OutboundItem>) -> Self {
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

    /// 发送通知（无 id、不期待响应）。按 method 自动归类 priority（P0B）。
    /// textDocument/* 等用户面向请求由调用方走 [`Client::notify_at`] 显式 High；
    /// 本方法默认按 `classify_method` 推断（didOpen/didChange/didClose 等 Normal）。
    pub fn notify(&self, method: impl AsRef<str>, params: Value) -> Result<()> {
        let method_ref = method.as_ref();
        let msg = JsonRpc::notification(method_ref, params);
        let priority = classify_method(method_ref);
        self.inner
            .outbound
            .try_send(OutboundItem { msg, priority })
            .map_err(|e| match e {
                // writer 死亡 → channel 关闭：必须映射 Terminated，让 supervisor 的
                // with_session_retry 自愈门触发 evict + 换新 session（审计 F1——
                // 映射成 Io 会让死 session 以 Ready 缓存持续失败）。
                tokio::sync::mpsc::error::TrySendError::Closed(_) => CoreError::Terminated {
                    ls: self.inner.ls_name.clone(),
                    cause: "outbound channel closed (writer dead)".into(),
                },
                other => CoreError::Io(std::io::Error::other(format!("outbound: {other}"))),
            })
    }

    /// 按指定优先级发通知。`High` 用于 supervisor 显式标记的关键路径。
    pub fn notify_at(
        &self,
        method: impl AsRef<str>,
        params: Value,
        priority: Priority,
    ) -> Result<()> {
        let msg = JsonRpc::notification(method.as_ref(), params);
        self.inner
            .outbound
            .try_send(OutboundItem { msg, priority })
            .map_err(|e| match e {
                // 同 notify（审计 F1）：Closed 必须 → Terminated 才能触发自愈重试。
                tokio::sync::mpsc::error::TrySendError::Closed(_) => CoreError::Terminated {
                    ls: self.inner.ls_name.clone(),
                    cause: "outbound channel closed (writer dead)".into(),
                },
                other => CoreError::Io(std::io::Error::other(format!("outbound: {other}"))),
            })
    }

    /// 同步等待请求结果（按 method 自动分类 priority）。`timeout` 到期 →
    /// `CoreError::Timeout{method, secs}`。
    pub async fn request<R>(
        &self,
        method: impl AsRef<str>,
        params: Value,
        timeout: Duration,
    ) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        let priority = classify_method(method.as_ref());
        self.request_at(method, params, timeout, priority).await
    }

    /// 显式 priority 版的 [`Client::request`]。supervisor 调 `textDocument/*`
    /// 之类用户面向方法可走 High 抢占队列；走 Background 等后台探测可避免
    /// 抢占用户请求。
    pub async fn request_at<R>(
        &self,
        method: impl AsRef<str>,
        params: Value,
        timeout: Duration,
        priority: Priority,
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
            let reply = self
                .send_once_at(&method, params.clone(), timeout, priority)
                .await?;
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

    /// 显式 priority 版的 [`Client::send_once`]。
    async fn send_once_at(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        priority: Priority,
    ) -> Result<JsonRpc> {
        let id_num = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = Id::Num(id_num);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.insert(id.clone(), tx);
        }

        let msg = JsonRpc::request(id_num, method, params);
        if let Err(e) = self
            .inner
            .outbound
            .send(OutboundItem { msg, priority })
            .await
        {
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
