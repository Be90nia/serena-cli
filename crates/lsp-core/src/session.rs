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
        });

        // 握手：发 initialize → 等响应（最多 HANDSHAKE_TIMEOUT）→ 发 initialized 通知。
        match Self::handshake(&session, params).await {
            Ok(()) => {
                {
                    let mut state = session.state.lock().unwrap();
                    *state = SessionState::Ready;
                }
                // 就绪门放行：所有等门的 request() 唤醒。
                session.initialized_notify.notify_waiters();
                Ok(session)
            }
            Err(e) => {
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
    async fn handshake(session: &Arc<Self>, params: InitializeParams) -> Result<()> {
        let params_json = serde_json::to_value(params).map_err(|e| CoreError::Rpc {
            code: -1,
            message: format!("initialize params serialize: {e}"),
        })?;

        let _resp: Value = time::timeout(
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

        Ok(())
    }

    /// 当前状态快照。
    pub fn state(&self) -> SessionState {
        self.state.lock().unwrap().clone()
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

    #[test]
    fn session_state_clone_eq() {
        let s = SessionState::Ready;
        let s2 = s.clone();
        assert_eq!(format!("{s:?}"), format!("{s2:?}"));
        assert_eq!(s, SessionState::Ready);
    }
}
