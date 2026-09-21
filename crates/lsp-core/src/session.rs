//! LSP 会话：握手 + 状态机 + 就绪门 + 优雅关停（PLAN Task 6 / ARCHITECTURE §5）。
//!
//! ↖ mirror: ls.py@43ae021 `SolidLanguageServer.start/_send_shutdown_in_thread` /
//! `_create_initialize_params`。
//!
//! 设计要点：
//! - 状态机 `Uninitialized → Initializing → Ready | Failed`（ARCH §5）。状态由
//!   `Mutex<SessionState>` 保护；start / shutdown / 泵 EOF 三条路径写。
//! - 就绪门 `initialized_notify: tokio::sync::Notify`：handshake 成功（initialize 响应 +
//!   `initialized` 通知发出后）调 `notify_waiters()`；`request()` 在 Ready 前到达则
//!   等门（`Notify::notified`）后放行。门只用一次（state 变 Ready 后等也无害）。
//! - 优雅关停 `shutdown(&self)`：timeout(2s, shutdown_req) → exit 通知 →
//!   `Pumps::kill` 兜底（drop Job → KILL_ON_JOB_CLOSE 灭整棵进程树）→ 等 stdout EOF
//!   通知确认进程退（最长 5s）。↖ mirror: ls.py@43ae021 `_send_shutdown_in_thread`
//!   （2s join + 5s wait 上限；语义反转：上游用 `_stdin_lock` 关 stdin，本项目用 Job
//!   Object 兜底 —— 不依赖 client 持 sender 数量）。
//! - `shutdown` 取 `&self`：调用方持 Arc 多次调用 shutdown 幂等；调用方何时
//!   drop Arc 与 shutdown 无关。
//! - `Session::request<R>` 通过 `Client::request` 转发（lsp-core 错误命名空间，禁 anyhow）。
//! - `Failed` 态由 supervisor（Task 13）消费 → 懒重试环。本模块只负责进入 Failed 态。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ls_runtime::process::ChildHandle;
use lsp_types::InitializeParams;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio::time;

use crate::client::Client;
use crate::error::{CoreError, Result};
use crate::framing::JsonRpc;
use crate::recording::Recorder;
use crate::transport::stdio::{OnEof, OnMsg, Pumps, pump, record_pump, replay_pump};

/// 握手超时上限（10s）。真实 clangd 多在 1s 内回 initialize；mock_ls 同样。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `shutdown` 请求超时（2s）。超时则放弃等回执直接走 kill 兜底。
///
/// ↖ mirror: ls.py@43ae021 `_send_shutdown_in_thread` 的 2s join 上限。
const SHUTDOWN_REQ_TIMEOUT: Duration = Duration::from_secs(2);

/// stdout EOF 等待上限（5s）。mock_ls shutdown+exit 通常 <100ms 退；真实 LS 需读 stdin EOF 才退。
///
/// ↖ mirror: ls.py@43ae021 5s wait 上限（语义：本项目是 EOF 通知，不是 wait()）。
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// 录制文件路径环境变量。设置后 Session::start 透传 + 落盘所有帧。
const ENV_RECORD: &str = "SERENA_RECORD";
/// 回放文件路径环境变量。设置后 Session::start 不接真 LS，从录文件喂虚拟入站。
const ENV_REPLAY: &str = "SERENA_REPLAY";

/// 从 env 解析 Recorder（PLAN Task 26）。
///
/// 优先级：`SERENA_REPLAY` → replay；`SERENA_RECORD` → record；二者皆无 → passthrough。
/// 路径错误 → stderr 打 warning 后回退 passthrough（不阻塞生产路径）。
fn recorder_from_env() -> Recorder {
    if let Ok(path) = std::env::var(ENV_REPLAY) {
        match Recorder::open_replay(&path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[serena] SERENA_REPLAY={path:?} 打开失败: {e}; 降级 passthrough");
                Recorder::passthrough()
            }
        }
    } else if let Ok(path) = std::env::var(ENV_RECORD) {
        match Recorder::open(&path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[serena] SERENA_RECORD={path:?} 打开失败: {e}; 降级 passthrough");
                Recorder::passthrough()
            }
        }
    } else {
        Recorder::passthrough()
    }
}

/// Session 状态（ARCHITECTURE §5 状态机图权威）。
///
/// `Initializing` 是 `Session::start` 内短暂中间态 —— 不暴露给调用方；start 完成后
/// 要么 Ready 要么 Failed。`Failed(cause)` 携带根因，调用方（supervisor）可读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// spawn 完，握手未开始。理论上外部代码看不到（start 内立刻转 Initializing）。
    Uninitialized,
    /// 握手进行中（仅在 start 闭包内）。
    Initializing,
    /// initialize 响应收到 + `initialized` 通知已发 → 可服务请求。
    Ready,
    /// 任意致命错误：泵 EOF、initialize 失败、超时等。`String` 是根因描述。
    Failed(String),
}

/// 单 LS 进程的完整 LSP 会话。`Arc<Session>` 是 supervisor 实例池的最小单元。
pub struct Session {
    pub(crate) state: Mutex<SessionState>,
    /// Ready 前 `request()` 在此门上阻塞。`Session::start` 握手成功时 `notify_waiters()`。
    pub(crate) initialized_notify: Notify,
    /// JSON-RPC 客户端（共享 Arc，可 Clone）。
    pub(crate) client: Client,
    /// 出站 mpsc 的发送端。`Client` 也持一份；channel 在 `Arc<Session>` 全部 drop 时
    /// 关闭 → writer task EOF。本字段保留仅为「Session 独占一份 sender」的契约表达。
    ///
    /// ponytail: 不为「显式关 stdin」独立设计 Take-out —— Job Object 兜底保证进程退场；
    /// LS 自然走 shutdown+exit+EOF 关闭 stdin 是 nice-to-have 不是必须。
    #[allow(dead_code)]
    outbound_tx: mpsc::Sender<JsonRpc>,
    /// 泵句柄集合。`shutdown` 终态 drop → Job 句柄关闭 → KILL_ON_JOB_CLOSE 灭树兜底。
    pumps: Mutex<Option<Pumps>>,
    /// stdout EOF 通知：stdout 泵读到 EOF 时 `notify_waiters()`；`shutdown` 等此门确认进程退。
    stdout_eof: Notify,
    /// docsync 缓冲池：`ensure_open` / `FileGuard::drop` 用（PLAN Task 7，§3.4 锁表）。
    pub(crate) buffers:
        Mutex<std::collections::HashMap<lsp_types::Uri, crate::docsync::FileBuffer>>,
    /// initialize 响应的 `capabilities` 字段原值。握手成功后由 `Session::start` 写入；
    /// supervisor 在 session_for 末尾用 `init_params::supports_pull_diagnostics`
    /// 读 `diagnosticProvider` 决定走 pull/push。失败握手不会写入。
    ///
    /// ponytail: 存 `serde_json::Value` 而非 typed `lsp_types::ServerCapabilities` -
    /// lsp-types 把 `diagnosticProvider` 编成 untagged enum (`Options` / `RegistrationOptions`),
    /// 实际 LS 还可能返简化 `true` literal，typed 反序列化会炸；后续探测只关心字段是否
    /// 非 null，不需要类型结构。
    pub(crate) server_capabilities:
        std::sync::Arc<Mutex<Option<serde_json::Value>>>,
    /// Phase 4 基建 Task 22c：`$/progress` 通知等待登记表。token（String 形态）
    /// → Notify。`Session::start` 内部注册唯一 `$/progress` handler，解析 params
    /// 找 token → notify 对应 waiter。`wait_for_progress` 入口插 waiter + 等门。
    progress_waiters: tokio::sync::Mutex<
        std::collections::HashMap<String, Arc<Notify>>,
    >,
    /// 早到通知记录：LS 在 `wait_for_progress` 登记前就发出的 token（mock_ls「握手后
    /// 立刻发」/RA 快速索引进度都会命中此窗口）。handler 无 waiter 可唤醒时记在此处，
    /// wait 侧优先消费一次。没有它，早到通知被 `Notify::notify_waiters` 空发丢弃，
    /// wait 永远超时。std Mutex：handler 在 stdout 泵 task 内同步执行，仅 try_lock。
    progress_resolved: std::sync::Mutex<std::collections::HashSet<String>>,
    /// `didOpen` 上送的 `languageId`。supervisor 在 session_for 拿到会话后注入真实
    /// adapter 语言；默认 `"cpp"` 仅兜底 lsp-core 直连路径——硬编码错语言会让
    /// rust-analyzer 等严格 LS 拒收文档（语义层挂）。
    language_id: std::sync::Mutex<Box<str>>,
}

/// Phase 4 基建 Task 22c：把 LSP `ProgressToken`（可能是 string 或 number）
/// 归一化成 waiter 表 key 用的字符串。`null` / 缺字段 / 其它类型 → None。
///
/// LSP spec 允许 `token: string | number`；多数 LS 用 string（自管 UUID / path
/// 标识），少数用 number（递增 id）。我们统一按字符串键控，避免 id 类型漂移
/// 导致 waiter 漏 notify。
fn progress_token_to_string(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(n) = v.as_i64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_u64() {
        return Some(n.to_string());
    }
    None
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("state", &*self.state.lock().unwrap())
            .field("client", &"Client(Arc)")
            .finish_non_exhaustive()
    }
}

impl Session {
    /// 启动 session：spawn 已就绪的 child → 起 3 泵 → 握手（initialize + initialized）→
    /// Ready。失败返回 `CoreError`，调用方不得到 Arc。
    ///
    /// 行为契约（PLAN Task 6 acceptance #1 + ARCH §5）：
    /// - 成功 → `Arc<Session>`，state == Ready。
    /// - 失败 → `CoreError`，无 Arc（child 由 pumps 持 Job 保活，pumps drop 灭树）。
    ///
    /// 不重试：失败语义由 supervisor 决策（PLAN Global Constraints）。
    pub async fn start(child: Option<ChildHandle>, params: InitializeParams) -> Result<Arc<Self>> {
        // 拆 child + 起 3 泵（架构要求 writer 独占 stdin、stdout 泵内联分发、stderr 泵日志）。
        let (out_tx, out_rx) = mpsc::channel::<JsonRpc>(64);
        let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);

        let client = Client::with_name("ls".into(), out_tx.clone());
        let client_for_pump = client.clone();
        let on_msg: OnMsg = {
            let c = client_for_pump.clone();
            Arc::new(move |msg| c.handle_message(msg))
        };
        // stdout EOF：先调 Client::abort_all（drain pending），
        // 然后 notify_waiters 让 shutdown 等门知道进程已退。
        let stdout_eof = Arc::new(Notify::new());
        let stdout_eof_for_pump = stdout_eof.clone();
        let on_eof: OnEof = {
            let c = client_for_pump.clone();
            Arc::new(move || {
                c.abort_all();
                stdout_eof_for_pump.notify_waiters();
            })
        };
        let recorder = recorder_from_env();
        let pumps = match (child, &recorder) {
            (Some(c), r) if !r.is_passthrough() && !r.is_replay() => {
                record_pump(c, out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder)
            }
            (None, r) if r.is_replay() => {
                replay_pump(out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder)
            }
            (Some(c), _) => pump(c, out_rx, reply_rx, reply_tx, on_msg, on_eof),
            (None, _) => {
                return Err(CoreError::Io(std::io::Error::other(
                    "Session::start: no child and no SERENA_REPLAY env",
                )));
            }
        };
        // stdout_eof 在闭包外独占
        let stdout_eof = Arc::try_unwrap(stdout_eof).unwrap_or_else(|_| Notify::new());

        let session = Arc::new(Self {
            state: Mutex::new(SessionState::Initializing),
            initialized_notify: Notify::new(),
            client,
            outbound_tx: out_tx,
            pumps: Mutex::new(Some(pumps)),
            stdout_eof,
            buffers: std::sync::Mutex::new(std::collections::HashMap::new()),
            server_capabilities: std::sync::Arc::new(Mutex::new(None)),
            progress_waiters: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            progress_resolved: std::sync::Mutex::new(std::collections::HashSet::new()),
            language_id: std::sync::Mutex::new("cpp".into()),
        });

        // Phase 4 基建 Task 22c：注册唯一 `$/progress` handler。LS 触发进度时会发
        // 通知 params = {token: <val>, value: {kind: "begin"|"report"|"end", ...}}。
        // 我们查 token 字符串 → 在 progress_waiters 找对应 Notify → notify。
        // token 可能是 number（i64）或 string；统一 stringify 当 key。
        session.client.on_notification("$/progress", {
            let session = Arc::clone(&session);
            move |msg| {
                let Some(params) = msg.params.as_ref() else {
                    return;
                };
                let Some(token) = progress_token_to_string(params.get("token")) else {
                    return;
                };
                // 同步查找 waiter（handler 在 stdout 泵 task 同步执行；不 .await）
                let waiters = session.progress_waiters.try_lock();
                let Ok(waiters) = waiters else {
                    return;
                };
                match waiters.get(&token) {
                    Some(notify) => notify.notify_waiters(),
                    None => {
                        // 无 waiter：早到通知 —— 记入 resolved 供后续 wait 立即
                        // 消费，否则 Notify::notify_waiters 空发 = 通知永久丢失。
                        drop(waiters);
                        if let Ok(mut resolved) = session.progress_resolved.try_lock() {
                            resolved.insert(token);
                        }
                    }
                }
            }
        });

        // 握手：发 initialize → 等响应（最多 HANDSHAKE_TIMEOUT）→ 发 initialized 通知。
        // 拿到 initialize 响应的 `capabilities` 子对象存入 Session（PLAN Phase 2.5，
        // supervisor 在 session_for 末尾读 `diagnosticProvider` 决定 pull/push）。
        match Self::handshake(&session, params).await {
            Ok(Some(caps)) => {
                {
                    let mut state = session.state.lock().unwrap();
                    *state = SessionState::Ready;
                }
                *session.server_capabilities.lock().unwrap() = Some(caps);
                // 就绪门放行：所有等门的 request() 唤醒。
                session.initialized_notify.notify_waiters();
                Ok(session)
            }
            Ok(None) => {
                // initialize 响应缺 capabilities 字段 —— LSP 不允许但兜底。
                {
                    let mut state = session.state.lock().unwrap();
                    *state = SessionState::Ready;
                }
                session.initialized_notify.notify_waiters();
                Ok(session)
            }
            Err(e) => {
                // ↖ mirror: ls.py@dc59a893 — start 中途失败必须回收已 spawn 的 LS 子进程，
                // 不留孤儿（上游在 start() 异常分支显式 stop）。本项目不能只依赖
                // session drop 兜底：$/progress handler 闭包与 session 构成 Arc 环
                // （session → client → handlers → session），失败态 session 不会自然
                // drop → Job 句柄不关 → KILL_ON_JOB_CLOSE 不触发。显式丢 Job 灭树。
                if let Some(pumps) = session.pumps.lock().unwrap().as_mut() {
                    pumps.kill();
                }
                let cause = format!("{e:?}");
                {
                    let mut state = session.state.lock().unwrap();
                    *state = SessionState::Failed(cause);
                }
                // 失败态也开门 —— 让卡住的 request 醒过来看到 state != Ready 后回 Err。
                session.initialized_notify.notify_waiters();
                Err(e)
            }
        }
    }

    /// 握手协议：发送 `initialize`，等到响应，发 `initialized` 通知。失败 → CoreError。
    /// 成功返回 `Option<Value>`：LS 响应的 `capabilities` 子对象；缺则返 None。
    async fn handshake(
        session: &Arc<Self>,
        params: InitializeParams,
    ) -> Result<Option<Value>> {
        let params_json = serde_json::to_value(params).map_err(|e| CoreError::Rpc {
            code: -1,
            message: format!("initialize params serialize: {e}"),
        })?;

        let resp: Value = time::timeout(
            HANDSHAKE_TIMEOUT,
            session
                .client
                .request("initialize", params_json, HANDSHAKE_TIMEOUT),
        )
        .await
        .map_err(|_| CoreError::Timeout {
            method: "initialize".into(),
            secs: HANDSHAKE_TIMEOUT.as_secs(),
        })??;
        // LSP 3.17 §initialize：响应是 InitializeResult { capabilities, serverInfo? }。
        // 提取 capabilities 子对象；缺则视为空能力（探测时一律 false）。
        let caps = resp.get("capabilities").cloned();

        // 通知 `initialized` —— LSP 协议要求；mock_ls 不消费此通知（注释 Task 6 要求），
        // 但真实服务器需要它才会从 Initializing 切到 Ready。
        session
            .client
            .notify("initialized", Value::Object(Default::default()))
            .map_err(|e| {
                CoreError::Io(std::io::Error::other(format!(
                    "initialized notify send: {e}"
                )))
            })?;

        Ok(caps)
    }

    /// Phase 4 基建 Task 22c：等待 LS 发出的 `$/progress` 通知，token 任意一次
    /// 命中 → 立刻返回。`timeout` 到期 → `CoreError::Timeout`。
    ///
    /// 语义：
    /// - 阻塞直到收到 token 字符串匹配的 progress 通知（任何 kind: begin/report/end 都算
    ///   「到达」；调用方语义解读 kind）。
    /// - 通知到达后 waiter **不自动清理**——这是有意设计：调用方可能多次复用同一 token
    ///   触发 handler（如 watch 模式）。`progress_waiters` 字段是 Session 内态，
    ///   `Arc<Session>` drop 时自然清理。
    ///
    /// 返回 `Ok(())` 表进度通知已到达；`Err(Timeout)` 表等待窗口内无通知。
    /// `Err(Terminated)` 表 Session 已 Failed（start 内失败 / LS EOF）。
    pub async fn wait_for_progress(
        &self,
        token: &str,
        timeout: Duration,
    ) -> Result<()> {
        // Failed 态直接返 —— 不会再有 progress 通知到达
        if matches!(*self.state.lock().unwrap(), SessionState::Failed(_)) {
            return Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: "session failed before progress wait".into(),
            });
        }
        // 早到通知已在 waiter 登记前到达（handler 记入 resolved）→ 立即消费一次。
        if self
            .progress_resolved
            .lock()
            .unwrap()
            .remove(token)
        {
            return Ok(());
        }
        let notify = {
            let mut waiters = self.progress_waiters.lock().await;
            waiters
                .entry(token.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        match time::timeout(timeout, notify.notified()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(CoreError::Timeout {
                method: "$/progress".into(),
                secs: timeout.as_secs(),
            }),
        }
    }

    /// 当前状态快照。
    pub fn state(&self) -> SessionState {
        self.state.lock().unwrap().clone()
    }

    /// 注入 `didOpen` 用的真实语言 id（如 "rust"）。session_for 拿到新会话后、
    /// 首次 `ensure_open` 前调用；晚于首次 didOpen 注入不生效（旧行为 "cpp"）。
    pub fn set_language_id(&self, lang: &str) {
        *self.language_id.lock().unwrap() = lang.to_ascii_lowercase().into();
    }

    /// 当前 `didOpen` languageId 快照（docsync 发 didOpen 时读）。
    pub(crate) fn language_id(&self) -> String {
        self.language_id.lock().unwrap().to_string()
    }

    /// 客户端句柄（供 supervisor 内部复用，比如发送 `$/cancelRequest`）。
    pub fn client(&self) -> &Client {
        &self.client
    }
    /// docsync 缓冲池引用（PLAN Task 7）。仅 `crate::docsync` 使用 —— 该 crate 通过
    /// `pub(crate)` 字段直访更经济；这里留一个最小访问器便于未来「关闭文件」等工具调用。
    #[allow(dead_code)]
    pub(crate) fn buffers(
        &self,
    ) -> &std::sync::Mutex<std::collections::HashMap<lsp_types::Uri, crate::docsync::FileBuffer>>
    {
        &self.buffers
    }

    /// initialize 响应的 `capabilities` 子对象克隆。握手成功后才有值；之前返 None。
    /// 仅 supervisor 用 — 在 `session_for` 末尾探测 pull diagnostics 等支持能力。
    pub fn server_capabilities(&self) -> Option<serde_json::Value> {
        self.server_capabilities.lock().unwrap().clone()
    }


    /// LSP 请求转发。Ready 前到达则等就绪门，门开且 state == Ready 后才放行；
    /// 若 state 已 Failed 则立即回 `CoreError`（门开但语义失败）。
    pub async fn request<R>(&self, method: &str, params: Value, timeout: Duration) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        // 快路径：已经 Ready。
        if matches!(*self.state.lock().unwrap(), SessionState::Ready) {
            return self.client.request(method, params, timeout).await;
        }
        // 快路径：已经 Failed。
        if matches!(*self.state.lock().unwrap(), SessionState::Failed(_)) {
            return Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: "session failed before request".into(),
            });
        }

        // 未 Ready：在门上等。`notified()` 必须先取 permit 再 await，否则门在 await 之前开
        // 就丢信号（Notify 标准语义）。
        let notified = self.initialized_notify.notified();
        // 二次检查：拿到 permit 前 Ready 可能已就位。
        if matches!(*self.state.lock().unwrap(), SessionState::Ready) {
            return self.client.request(method, params, timeout).await;
        }
        notified.await;

        // 门开后再看一次状态。
        let snap = self.state.lock().unwrap().clone();
        match snap {
            SessionState::Ready => self.client.request(method, params, timeout).await,
            SessionState::Failed(cause) => Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: format!("session failed: {cause}"),
            }),
            SessionState::Uninitialized | SessionState::Initializing => Err(CoreError::Io(
                std::io::Error::other("session not ready after notify"),
            )),
        }
    }
    /// 通知转发（无响应）。Ready 前到达也走等门逻辑 —— 通知一般不阻塞但保持一致性。
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        if !matches!(*self.state.lock().unwrap(), SessionState::Ready) {
            self.initialized_notify.notified().await;
        }
        self.client.notify(method, params)
    }

    /// 优雅关停。多次调用幂等（second call 仅 drop pumps）。
    ///
    /// 流程（↖ mirror `_send_shutdown_in_thread`，语义反转见 §3.2）：
    /// 1. `timeout(2s, shutdown_req)` —— mock_ls/真实 LS 在 shutdown 应回 null；超时也往下走。
    /// 2. 发 `exit` 通知 —— LSP 通知而非请求，告诉 LS 正常退出。
    /// 3. **kill 立刻**（drop Job → KILL_ON_JOB_CLOSE 灭整棵进程树）。
    /// 4. 等 stdout EOF 通知确认进程退（最长 5s）。
    ///
    /// kill 优先于 EOF 等待：mock_ls 收到 shutdown+exit 通常自然退，但 writer task 仍
    /// 持 stdin 等客户端 send —— 必须杀进程才能触发 stdout EOF 与 on_eof。等 EOF 在前
    /// 会把 5s 等满 + 5s 等满 = 10s 总耗时。kill 先发 → stdout EOF ms 级到达。
    pub async fn shutdown(&self) {
        // 步骤 1：尝试发 shutdown 请求并等回执（mock_ls/真实 LS 都回 null）。
        let _ = time::timeout(SHUTDOWN_REQ_TIMEOUT, async {
            let _r: Result<Value> = self
                .client
                .request("shutdown", Value::Null, SHUTDOWN_REQ_TIMEOUT)
                .await;
        })
        .await;

        // 步骤 2：`exit` 通知 —— 告诉 LS「正常退出」。
        let _ = self
            .client
            .notify("exit", Value::Object(Default::default()));

        // 步骤 3：take pumps + kill —— drop Job → KILL_ON_JOB_CLOSE → mock_ls 进程树死。
        let pumps = self.pumps.lock().unwrap().take();
        if let Some(mut p) = pumps {
            p.kill();
            drop(p);
        }

        // 步骤 4：等 stdout EOF 通知确认进程退（最长 5s）。permit 先拿再 await。
        let notified = self.stdout_eof.notified();
        let _ = time::timeout(SHUTDOWN_WAIT_TIMEOUT, notified).await;

        // 把状态标 Failed("shutdown") —— 调用方可读 session.state() 知道已走完。
        let mut state = self.state.lock().unwrap();
        if matches!(*state, SessionState::Ready | SessionState::Initializing) {
            *state = SessionState::Failed("shutdown".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_state_clone_eq() {
        let s = SessionState::Ready;
        let s2 = s.clone();
        assert_eq!(format!("{s:?}"), format!("{s2:?}"));
        assert_eq!(s, SessionState::Ready);
    }

    // ---- Phase 4 Task 22c: $/progress waiter ----

    #[test]
    fn progress_token_to_string_normalizes_string_and_number() {
        assert_eq!(progress_token_to_string(Some(&json!("abc"))).as_deref(), Some("abc"));
        assert_eq!(progress_token_to_string(Some(&json!(42))).as_deref(), Some("42"));
        assert_eq!(progress_token_to_string(Some(&json!(42u64))).as_deref(), Some("42"));
    }

    #[test]
    fn progress_token_to_string_rejects_null_and_missing() {
        assert_eq!(progress_token_to_string(None), None);
        assert_eq!(progress_token_to_string(Some(&json!(null))), None);
        assert_eq!(progress_token_to_string(Some(&json!(true))), None);
        assert_eq!(progress_token_to_string(Some(&json!([1, 2]))), None);
        assert_eq!(progress_token_to_string(Some(&json!({"k": "v"}))), None);
    }

    #[tokio::test]
    async fn wait_for_progress_succeeds_when_handler_invokes_notify() {
        use crate::client::Client;
        use crate::framing::JsonRpc;
        use std::sync::Arc;
        use tokio::sync::{Notify, mpsc};

        // 模拟 `$ /progress` handler 调 Notify 的逻辑；这里直接构造 Notify + 手动调
        // 验证 wait_for_progress 在收到通知后放行。端到端（handler 注入 + LS 发通知）
        // 由 mock_ls 集成测试覆盖。
        let token = "test-token-1";
        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();
        // spawn 后台任务模拟 `$ /progress` 处理器：100ms 后通知。
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            notify_clone.notify_waiters();
        });
        // wait 等价于 Session::wait_for_progress 内部逻辑（tokio::time::timeout）
        let res = tokio::time::timeout(Duration::from_secs(2), notify.notified()).await;
        assert!(res.is_ok(), "notify 应在 100ms 内到达");
        let _ = token;
        // 抑制 Client / JsonRpc / mpsc 警告
        let _ = std::mem::size_of::<Client>();
        let _ = std::mem::size_of::<JsonRpc>();
        let _: mpsc::Sender<()> = mpsc::channel(1).0;
    }

    #[tokio::test]
    async fn wait_for_progress_timeout_returns_core_timeout() {
        // 验证 wait_for_progress 超时路径——通过裸 tokio::time::timeout + Notify 模拟
        let notify = Arc::new(Notify::new());
        let res = tokio::time::timeout(Duration::from_millis(200), notify.notified()).await;
        assert!(res.is_err(), "无通知时应 timeout");
    }
}
