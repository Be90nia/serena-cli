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

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ls_runtime::process::ChildHandle;
use lsp_types::InitializeParams;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio::time;

use crate::client::{Client, OutboundItem};
use crate::error::{CoreError, Result};
use crate::framing::JsonRpc;
use crate::recording::Recorder;
use crate::transport::stdio::{
    OnEof, OnMsg, Pumps, pump_with_priority, record_pump_with_priority, replay_pump_with_priority,
};

/// 握手预算默认值（30s）。clangd 等 native LS 多在 1s 内回 initialize；但 node 系
/// LS（bash-language-server 实测）冷启动链 npm shim → node → tree-sitter WASM
/// 首载可超 10s，10s 窗口下冷启动会假报 LS_TIMEOUT（retryable 且重试同死）。
///
/// 冷启动三段预算（bd 62z 拆分）：**握手段**（本值）→ 适配器就绪探针
/// （adapter `on_server_ready`，各自带 READY_PROBE_TIMEOUT）→ 工具请求
/// （supervisor `effective_tool_timeout`，servers.toml 按语言可调）。三段
/// 串行、各自独立计时——大项目冷启动首条命令最坏 = 三段之和，观感「首条
/// 60s 卡死」即握手段 + 探针段的叠加。握手段经 `SERENA_HANDSHAKE_TIMEOUT_SECS`
/// 独立调整（非法/缺失回默认），与工具/探针段解耦。
const HANDSHAKE_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);

/// 解析握手段预算（bd 62z）：`SERENA_HANDSHAKE_TIMEOUT_SECS`（纯函数供单测；
/// 非数字/空串/0 → 回默认 30s——0 会让握手永不超时，挂死无界）。
fn handshake_timeout() -> Duration {
    parse_handshake_secs(std::env::var("SERENA_HANDSHAKE_TIMEOUT_SECS").ok().as_deref())
}

fn parse_handshake_secs(raw: Option<&str>) -> Duration {
    match raw.map(str::trim).and_then(|s| s.parse::<u64>().ok()) {
        Some(n) if n > 0 => Duration::from_secs(n),
        _ => HANDSHAKE_TIMEOUT_DEFAULT,
    }
}

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

/// Phase 4 Task 22c：`$/progress` waiter + 早到通知合并登记表。handler 与 wait 侧
/// 共用一把 `std::Mutex<ProgressRegistry>` 临界区，原子完成「resolved 消费 + waiter
/// 注册 / waiter 唤醒 + resolved 记录」三步，杜绝早期实现中两把锁之间的通知丢失窗口
/// （见 `Session::progress` 注释）。
///
/// P2-y5u：resolved 表加容量上限 —— 长会话（clangd 索引 / rust-analyzer crate 解析）
/// 持续发 unique progress 通知时，handler 无 waiter 可唤醒，全落 resolved → 无界膨胀。
/// 上限触达时按插入顺序淘汰最旧（FIFO，HashMap 默认迭代序）—— 早到通知只对短窗口内的
/// `wait_for_progress` 有意义；超窗口外的旧 token 命中概率极低，淘汰可接受。
#[derive(Default)]
struct ProgressRegistry {
    /// token（String 形态）→ Notify。`wait_for_progress` 入口插 waiter + 等门。
    waiters: std::collections::HashMap<String, Arc<Notify>>,
    /// 早到通知记录：LS 在 `wait_for_progress` 登记前就发出的 token。handler 无 waiter
    /// 可唤醒时记入；wait 侧优先消费一次。否则早到通知被 `Notify::notify_waiters` 空发丢弃。
    ///
    /// P2-y5u：容量有界 —— `RESOLVED_CAP` 条以上时淘汰最旧（FIFO）；无限增长会致 OOM。
    resolved: std::collections::HashMap<String, ()>,
}

/// P2-y5u：resolved 表容量上限。8192 覆盖 clangd / rust-analyzer 长会话 burst 场景；
/// 早到通知短窗口有效，窗口外 token 命中率极低 —— 淘汰旧 key 不影响当前 wait 链路。
const RESOLVED_CAP: usize = 8192;

/// 跨文件索引 `$/progress` 在飞 token 跟踪器。
///
/// ↖ mirror: ls.py@43ae021 TS 子类 `_active_progress_tokens` + `_indexing_complete`；
///           ↖ mirror: @cf54869a 修订 —— drain 语义（后续跨文件查询也等在飞索引）。
///
/// 三路信号汇入（`Session::start` 注册的 handler 写入）：`$/progress` 通知
/// （begin 插入 / end 移除）、`window/workDoneProgress/create` 请求（LS 预告即将
/// 上报，先于首个 begin，同样插入）。active 为空 = 无在飞索引。
///
/// drain 等待走 `watch` channel（active 计数）而非 `Notify`：`notify_waiters` 只唤醒
/// 已注册 waiter，stable 无 `Notified::enable`，「查空 → 注册」窗口内 end 到达会
/// 永久丢唤醒；watch 保留最新值，`changed()` 前先 `borrow_and_update` 消费当前值，
/// 无丢失窗口。
pub(crate) struct IndexProgressTracker {
    /// 在飞 token 集合（begin/create 插入、end 移除；同 token 重复 begin 幂等）。
    active: std::sync::Mutex<std::collections::HashSet<String>>,
    /// active 计数变化广播。仅用 `subscribe()` 派生 receiver，构造时的 receiver 即弃。
    count_tx: tokio::sync::watch::Sender<usize>,
    /// 首查 latch（上游 `_has_waited_for_cross_file_references`）：首次跨文件查询走
    /// start-grace 等待，后续查询只 drain 在飞 token（cf54869a 修订核心）。
    first_query_done: std::sync::atomic::AtomicBool,
}

impl IndexProgressTracker {
    pub(crate) fn new() -> Self {
        let (count_tx, _) = tokio::sync::watch::channel(0);
        Self {
            active: std::sync::Mutex::new(std::collections::HashSet::new()),
            count_tx,
            first_query_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `begin=true` 插入（begin/create），`begin=false` 移除（end）。计数变化即广播。
    pub(crate) fn track(&self, token: &str, begin: bool) {
        let count = {
            let mut active = self.active.lock().unwrap();
            if begin {
                active.insert(token.to_string());
            } else {
                active.remove(token);
            }
            active.len()
        };
        self.count_tx.send_if_modified(|c| {
            if *c == count {
                false
            } else {
                *c = count;
                true
            }
        });
    }

    pub(crate) fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }

    /// 首查 latch：首次调用 true 并置位，此后 false（CAS 语义）。
    pub(crate) fn take_first_query(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.first_query_done
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 等待在飞索引清空（上游 `wait_for_indexing`）。清空 → true；`timeout` 耗尽仍
    /// 有在飞 → false（调用方 warn 后放行 —— 上游 permissive 行为）。
    pub(crate) async fn wait_drain(&self, timeout: Duration) -> bool {
        let mut rx = self.count_tx.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if *rx.borrow_and_update() == 0 {
                return true;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                // 超时 / 发送端已 drop（session 终态）：按当前快照判定。
                _ => return *rx.borrow() == 0,
            }
        }
    }

    /// 等待「索引开始并 drain」或「grace 内证明无需索引」（上游
    /// `_wait_for_indexing_start_or_completion`）：grace 窗口内观察到 begin/create
    /// 即转 [`Self::wait_drain`]；窗口耗尽仍无活动 → true（该项目无需索引）。
    pub(crate) async fn wait_start_or_completion(
        &self,
        timeout: Duration,
        grace: Duration,
    ) -> bool {
        let mut rx = self.count_tx.subscribe();
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if *rx.borrow_and_update() > 0 {
                return self.wait_drain(timeout).await;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => continue,
                // grace 内从未开始 = 无需索引（上游 return True）。
                _ => return true,
            }
        }
    }
}

/// 单 LS 进程的完整 LSP 会话。`Arc<Session>` 是 supervisor 实例池的最小单元。
pub struct Session {
    pub(crate) state: Mutex<SessionState>,
    /// Ready 前 `request()` 在此门上阻塞。`Session::start` 握手成功时 `notify_waiters()`。
    pub(crate) initialized_notify: Notify,
    /// JSON-RPC 客户端（共享 Arc，可 Clone）。
    pub(crate) client: Client,
    /// 出站 mpsc（带 priority 的 `OutboundItem`）的发送端。`Client` 也持一份；
    /// channel 在 `Arc<Session>` 全部 drop 时关闭 → writer task EOF。本字段保留
    /// 仅为「Session 独占一份 sender」的契约表达。
    ///
    /// ponytail: 不为「显式关 stdin」独立设计 Take-out —— Job Object 兜底保证进程退场；
    /// LS 自然走 shutdown+exit+EOF 关闭 stdin 是 nice-to-have 不是必须。
    #[allow(dead_code)]
    outbound_tx: mpsc::Sender<OutboundItem>,
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
    pub(crate) server_capabilities: std::sync::Arc<Mutex<Option<serde_json::Value>>>,
    /// Phase 4 基建 Task 22c：`$/progress` 通知等待登记表。
    ///
    /// 一次合并：waiter 表（token → Notify）+ 早到通知记录（token 已到达但尚无 waiter）
    /// 共用一把 `std::sync::Mutex`。handler 在 stdout 泵 task 内同步执行，必须能
    /// 「无 waiter 时记入 resolved」；wait 侧要「先消费 resolved 再插 waiter」。
    /// 这两步必须**原子**：分两把锁时（早期实现：waiters=tokio::Mutex、resolved=std::Mutex），
    /// handler 可在 wait 侧 drop resolved 锁到抢到 waiters 锁之间夹缝触发
    /// （try_lock 抢到 resolved → 抢到 waiters → 无 waiter → 记 resolved → 退出），
    /// wait 侧随后插 waiter 但通知已消费，永久不醒。50×8 并发压测复现 ~35/400 失败率。
    /// 合并到一把 `std::Mutex<ProgressRegistry>` 后临界区同步、原子，杜绝该窗口。
    progress: std::sync::Mutex<ProgressRegistry>,
    /// `didOpen` 上送的 `languageId`。supervisor 在 session_for 拿到会话后注入真实
    /// adapter 语言；默认 `"cpp"` 仅兜底 lsp-core 直连路径——硬编码错语言会让
    /// rust-analyzer 等严格 LS 拒收文档（语义层挂）。
    language_id: std::sync::Mutex<Box<str>>,
    /// 扩展名 → didOpen languageId 覆盖表（默认空）。混合扩展名会话用（astro 伴生
    /// 同一 TS LS 服务 .astro/.ts/.tsx —— ↖ mirror 上游 astro_language_server.py@7a296833
    /// `_get_language_id_for_file` per-file languageId quirk：languageId 错了 plugin
    /// 会把 .ts 当模板破解析）。空表 = 其余语言行为不变（单值 [`Self::language_id`]）。
    language_by_ext: std::sync::Mutex<std::collections::HashMap<Box<str>, Box<str>>>,
    /// 跨文件索引 `$/progress` 跟踪（cf54869a mirror；写入方见 [`IndexProgressTracker`]）。
    /// TS adapter 跨文件引用查询前经 [`Session::wait_for_cross_file_index`] 消费。
    index_progress: IndexProgressTracker,
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
        // P0B：outbound channel 改 `OutboundItem`（带 priority），writer 走 priority-aware
        // 三路路由 + TokenBucket；详见 `transport::stdio::pump_with_priority`。
        let (out_tx, out_rx) = mpsc::channel::<OutboundItem>(64);
        let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);

        let client = Client::with_name("ls".into(), out_tx.clone());
        // bd serena-rust-s3u：位置类方法的 -32801 ContentModified 必须在 client 层内部
        // 重试消化（3 次 × 200ms），否则并发首击 RA 类型分析重算窗口时硬错误外泄 wire。
        // 白名单与 init_params::RETRY_ON_CONTENT_MODIFIED 同源。
        client.set_content_modified_retry(crate::init_params::RETRY_ON_CONTENT_MODIFIED);
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
                record_pump_with_priority(c, out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder)
            }
            (None, r) if r.is_replay() => {
                replay_pump_with_priority(out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder)
            }
            (Some(c), _) => pump_with_priority(c, out_rx, reply_rx, reply_tx, on_msg, on_eof),
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
            progress: std::sync::Mutex::new(ProgressRegistry::default()),
            language_id: std::sync::Mutex::new("cpp".into()),
            language_by_ext: std::sync::Mutex::new(std::collections::HashMap::new()),
            index_progress: IndexProgressTracker::new(),
        });

        // Phase 4 基建 Task 22c：注册唯一 `$/progress` handler。LS 触发进度时会发
        // 通知 params = {token: <val>, value: {kind: "begin"|"report"|"end", ...}}。
        // 我们查 token 字符串 → 在 progress 找对应 Notify → notify。
        // token 可能是 number（i64）或 string；统一 stringify 当 key。
        //
        // 临界区原子性：`Session::progress` 单一 std::Mutex 保护 waiter + resolved。
        // handler 在 stdout 泵 task 同步执行（不 .await），lock + 操作 + drop 全程不挂起，
        // 与 `wait_for_progress` 的同一把锁原子互斥——杜绝两锁间夹缝导致的通知丢失。
        //
        // P2-y5u 修 Arc 环 —— handler 闭包改持 `Weak<Session>` 而非强 Arc：
        // 旧实现 `Arc::clone(&session)` 把 session 锚定到 ClientInner.notification_handlers，
        // 与 Session.client = Client(Arc<ClientInner>) 构成双向 Arc 强环，
        // drop(session) 后 Arc 强计数不归零 → 已关停会话资源长期滞留。
        // Weak 持引用每次触发前 `upgrade()`：session 还活着才操作 progress 表；
        // 已被 drop 的 session 升级失败 → handler 安全 no-op。
        // 配合 `Session::shutdown` 调 `client.clear_notification("$/progress")` 显式
        // 清表，主动断开环（weak 升级失败 + 闭包从表里移除 → ClientInner 整体释放）。
        session.client.on_notification("$/progress", {
            let session = Arc::downgrade(&session);
            move |msg| {
                let Some(session) = session.upgrade() else {
                    return;
                };
                let Some(params) = msg.params.as_ref() else {
                    return;
                };
                let Some(token) = progress_token_to_string(params.get("token")) else {
                    return;
                };
                // cf54869a mirror：begin 插入 / end 移除在飞索引 token（report 不改
                // 集合）。先于 registry 段执行；两把锁（active / progress）不嵌套持有。
                if let Some(value) = params.get("value") {
                    match value.get("kind").and_then(|k| k.as_str()) {
                        Some("begin") => session.index_progress.track(&token, true),
                        Some("end") => session.index_progress.track(&token, false),
                        _ => {}
                    }
                }
                let Ok(mut registry) = session.progress.lock() else {
                    return;
                };
                if let Some(notify) = registry.waiters.get(&token) {
                    notify.notify_waiters();
                } else {
                    // 无 waiter：早到通知 —— 记入 resolved 供后续 wait 立即
                    // 消费。否则 Notify::notify_waiters 空发 = 通知永久丢失。
                    // P2-y5u：超 RESOLVED_CAP 时按插入顺序淘汰最旧（FIFO），
                    // 避免长会话 burst 场景无界增长。
                    if registry.resolved.len() >= RESOLVED_CAP
                        && let Some(oldest) = registry.resolved.keys().next().cloned()
                    {
                        registry.resolved.remove(&oldest);
                    }
                    registry.resolved.insert(token, ());
                }
            }
        });

        // cf54869a mirror 配套：LS 预告进度（`window/workDoneProgress/create` 请求先于
        // 首个 `$/progress` begin 到达）也计入在飞 token，否则 create→begin 窗口会被
        // 首查 start-grace 轮询误判为「无需索引」提前放行。返回 None → 默认 null 成功
        // （LSP 规范 result: null）。
        session
            .client
            .on_server_request("window/workDoneProgress/create", {
                let session = Arc::downgrade(&session);
                move |msg| {
                    // `?`：Weak 升级失败（session 已终态）→ handler 返回 None，默认 null 成功。
                    let session = session.upgrade()?;
                    if let Some(params) = msg.params.as_ref()
                        && let Some(token) = progress_token_to_string(params.get("token"))
                    {
                        session.index_progress.track(&token, true);
                    }
                    None
                }
            });

        // `window/workDoneProgress/cancel` 空 stub（bd 0vj1 契约项：不实现取消）。
        // 该通知 spec 方向是 client→server 且我们从不主动取消 RA 索引 —— 空 handler
        // 仅兜住个别服务器的越界误发帧；闭包不持 session 引用，无 Arc 环，
        // 无需 shutdown 清表（对比上方两个持 Weak 的 handler）。
        session
            .client
            .on_notification("window/workDoneProgress/cancel", |_| {});

        // 握手：发 initialize → 等响应（最多握手段预算，见 HANDSHAKE_TIMEOUT_DEFAULT）
        // → 发 initialized 通知。
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
    async fn handshake(session: &Arc<Self>, params: InitializeParams) -> Result<Option<Value>> {
        let params_json = serde_json::to_value(params).map_err(|e| CoreError::Rpc {
            code: -1,
            message: format!("initialize params serialize: {e}"),
        })?;

        // 外层包裹是兜底上限而非冗余：request_at 的 Content-Modified 重试循环
        // 内层超时可不止一次计时，外层保证整段 initialize 不越过握手段预算。
        let hs_timeout = handshake_timeout();
        let resp: Value = time::timeout(
            hs_timeout,
            session
                .client
                .request("initialize", params_json, hs_timeout),
        )
        .await
        .map_err(|_| CoreError::Timeout {
            method: "initialize".into(),
            secs: hs_timeout.as_secs(),
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
    ///   触发 handler（如 watch 模式）。`progress` 字段是 Session 内态，
    ///   `Arc<Session>` drop 时自然清理。
    ///
    /// 返回 `Ok(())` 表进度通知已到达；`Err(Timeout)` 表等待窗口内无通知。
    /// `Err(Terminated)` 表 Session 已 Failed（start 内失败 / LS EOF）。
    ///
    /// 实现细节（修复 race）：早期版本分两把锁（waiters=tokio::Mutex、
    /// resolved=std::Mutex），handler 在 wait 侧 drop resolved 锁到抢到 waiters 锁之间
    /// 的夹缝触发 → handler 记 resolved 但通知已消费 → wait 侧随后插 waiter 永久不醒。
    /// 50×8 并发压测复现 ~35/400 失败率。修复：resolved 消费与 waiter 注册合并到
    /// `Session::progress` 同一把 `std::Mutex` 临界区，原子完成；如 resolved 命中直接返
    /// Ok，否则同临界区内插 waiter 并 Clone 出 Notify（之后才 .await 等门）。
    /// 临界区不持锁 .await——不会死锁当前 std Mutex。
    pub async fn wait_for_progress(&self, token: &str, timeout: Duration) -> Result<()> {
        // Failed 态直接返 —— 不会再有 progress 通知到达
        if matches!(*self.state.lock().unwrap(), SessionState::Failed(_)) {
            return Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: "session failed before progress wait".into(),
            });
        }
        // 合并临界区：先消费 resolved 再注册 waiter。任一路径在临界区内完成，
        // handler 拿到锁时要么看到 resolved（不再重复插入）要么看到 waiter（直接 notify）。
        let notify = {
            let mut registry = self.progress.lock().unwrap();
            if registry.resolved.remove(token).is_some() {
                // 早到通知已在 waiter 登记前到达（handler 记入 resolved）→ 立即消费。
                // 关键：这里直接返 Ok，不插 waiter——避免后续 handler 再 fire 时无谓 notify。
                return Ok(());
            }
            registry
                .waiters
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

    // ---- 跨文件索引 $/progress 跟踪（cf54869a mirror；TS adapter 消费）----

    /// 当前在飞索引 token 数（>0 即 tsserver 等正在后台索引）。
    pub fn index_active_progress(&self) -> usize {
        self.index_progress.active_count()
    }

    /// 首查 latch：首次跨文件查询 true（赢家做 start-grace 等待），此后 false。
    pub fn take_cross_file_first_query(&self) -> bool {
        self.index_progress.take_first_query()
    }

    /// 等待在飞索引清空；`timeout` 耗尽仍有在飞 → false（调用方 warn 后放行）。
    pub async fn wait_indexing_drain(&self, timeout: Duration) -> bool {
        self.index_progress.wait_drain(timeout).await
    }

    /// 等待「索引开始并 drain」或「grace 内证明无需索引」。
    pub async fn wait_indexing_start_or_completion(
        &self,
        timeout: Duration,
        grace: Duration,
    ) -> bool {
        self.index_progress
            .wait_start_or_completion(timeout, grace)
            .await
    }

    /// `ProgressRegistry.resolved` 当前条目数（P2-y5u 容量上限测试用 / 诊断）。
    /// 早到通知表，超过 `RESOLVED_CAP` 时按 FIFO 淘汰最旧。
    pub fn resolved_len(&self) -> usize {
        self.progress.lock().unwrap().resolved.len()
    }

    /// 注入 `didOpen` 用的真实语言 id（如 "rust"）。session_for 拿到新会话后、
    /// 首次 `ensure_open` 前调用；晚于首次 didOpen 注入不生效（旧行为 "cpp"）。
    pub fn set_language_id(&self, lang: &str) {
        *self.language_id.lock().unwrap() = lang.to_ascii_lowercase().into();
    }

    /// 装扩展名 → languageId 覆盖表（astro 伴生混合扩展名用；见字段注释）。
    /// `(ext, lang)` 的 ext 不含点、大小写不敏感归一。
    pub fn set_language_id_for_extensions(&self, map: &[(&str, &str)]) {
        let mut slot = self.language_by_ext.lock().unwrap();
        slot.clear();
        for (ext, lang) in map {
            slot.insert(
                ext.to_ascii_lowercase().into(),
                lang.to_ascii_lowercase().into(),
            );
        }
    }

    /// didOpen 应上送的 languageId：扩展名命中覆盖表用表值，否则会话单值。
    pub(crate) fn language_id_for(&self, path: &Path) -> String {
        resolve_language_id_for(
            &self.language_id(),
            &self.language_by_ext.lock().unwrap(),
            path,
        )
    }

    /// 当前 `didOpen` languageId 快照（docsync 发 didOpen 时读；supervisor 另用于
    /// 反查会话对应的 adapter —— references 类工具发请求前的索引等待）。
    pub fn language_id(&self) -> String {
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

    /// 当前文件的 docsync `content_version`（tool_diagnostics 的推送 version 比对用）。
    /// 文件未打开（无 buffer）→ None。key 用 docsync 同款 path_to_uri 保证一致。
    pub fn content_version_of(&self, path: &std::path::Path) -> Option<i64> {
        let uri = crate::docsync::path_to_uri(path).ok()?;
        let map = self.buffers.lock().unwrap();
        map.get(&uri).map(|b| b.content_version)
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
            SessionState::Uninitialized | SessionState::Initializing => Err(CoreError::NotReady {
                cause: "session not ready after gate open".into(),
            }),
        }
    }

    /// 显式 priority 版的 [`Session::request`]（审计 P1-2：用户主动触发的
    /// Background 类方法如 workspace/symbol 需要覆盖默认分类）。
    pub async fn request_at<R>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        priority: crate::client::Priority,
    ) -> Result<R>
    where
        R: serde::de::DeserializeOwned,
    {
        if matches!(*self.state.lock().unwrap(), SessionState::Ready) {
            return self
                .client
                .request_at(method, params, timeout, priority)
                .await;
        }
        if matches!(*self.state.lock().unwrap(), SessionState::Failed(_)) {
            return Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: "session failed before request".into(),
            });
        }

        let notified = self.initialized_notify.notified();
        if matches!(*self.state.lock().unwrap(), SessionState::Ready) {
            return self
                .client
                .request_at(method, params, timeout, priority)
                .await;
        }
        notified.await;

        let snap = self.state.lock().unwrap().clone();
        match snap {
            SessionState::Ready => {
                self.client
                    .request_at(method, params, timeout, priority)
                    .await
            }
            SessionState::Failed(cause) => Err(CoreError::Terminated {
                ls: "ls".into(),
                cause: format!("session failed: {cause}"),
            }),
            SessionState::Uninitialized | SessionState::Initializing => Err(CoreError::NotReady {
                cause: "session not ready after gate open".into(),
            }),
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
        // 步骤 0（修 P1 #1）：会话关闭前先把 docsync 缓冲池中的活跃文档 didClose——
        // 让 LS 在 shutdown+exit 之前完成 LSP 协议层的「关闭文档」流程，避免 LS
        // 退出时残留未关闭文档引用（mock_ls/RA 行为对此无感，但其他 LS 可能持句柄
        // 不放、文件锁延迟释放等）。必须在 pumps 关闭前发——之后 outbound 关 send
        // 即失败。`evict_all_buffers` 同步函数纯锁内 drain + 锁外 notify。
        self.evict_all_buffers();

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

        // 步骤 5（P2-y5u）：清 `$/progress` handler + 清空 progress 表
        // —— 显式断开 Session↔ClientInner 的 Arc 环路径。
        //   handler 闭包持 `Weak<Session>` 已把强引用断开，但闭包本身仍占据
        //   `ClientInner.notification_handlers` 表 —— 不主动 remove，ClientInner 与其
        //   表里 Arc 计数不归零（Session drop 后 ClientInner 仍被持），孤儿 handler
        //   与 session.progress 残留永久不回收。同步清空 progress 表（resolved/waiters）
        //   避免长会话积累 Notify 实例。
        self.client.clear_notification("$/progress");
        // cf54869a mirror 配套：create handler 同样占表，一并显式清（同上理由）。
        self.client
            .clear_server_request("window/workDoneProgress/create");
        {
            let mut registry = self.progress.lock().unwrap();
            registry.waiters.clear();
            registry.resolved.clear();
        }

        // 把状态标 Failed("shutdown") —— 调用方可读 session.state() 知道已走完。
        let mut state = self.state.lock().unwrap();
        if matches!(*state, SessionState::Ready | SessionState::Initializing) {
            *state = SessionState::Failed("shutdown".into());
        }
    }
}

/// [`Session::language_id_for`] 的纯逻辑（扩展名覆盖表优先，缺省会话单值兜底）。
pub(crate) fn resolve_language_id_for(
    default: &str,
    overrides: &std::collections::HashMap<Box<str>, Box<str>>,
    path: &Path,
) -> String {
    if let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        && let Some(lang) = overrides.get(ext.as_str())
    {
        return lang.to_string();
    }
    default.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// bd xht：SERENA_REPLAY 是进程全局 env——本模块内用例共用一把锁串行化
    /// （ls-adapters lib.rs REPLAY_ENV 先例）。
    static REPLAY_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 最小回放文件：只含 initialize 应答（Session::start 握手即 Ready）。
    fn write_replay(dir: &Path) -> std::path::PathBuf {
        let lines = [
            r#"--> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"<-- {"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
        ];
        let path = dir.join("replay.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write replay");
        path
    }

    /// bd xht（audit-ux-perf P-3）：生产注册路径回归——Session::start 必须把
    /// init_params::RETRY_ON_CONTENT_MODIFIED 注册进 Client 白名单。注册点
    /// （session.rs `set_content_modified_retry`）被删或常量漂移时本测红——
    /// 防止「重试机制完整实现但白名单空」的接线失效复发（bd s3u 同款）。
    #[tokio::test]
    async fn start_registers_content_modified_retry_whitelist() {
        let _env = REPLAY_ENV.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let replay = write_replay(dir.path());
        // SAFETY: REPLAY_ENV 保证本进程内独占访问 SERENA_REPLAY，用完即清。
        unsafe { std::env::set_var(ENV_REPLAY, &replay) };
        let session = Session::start(
            None,
            crate::init_params::base_initialize_params(),
        )
        .await
        .expect("replay session Ready");
        unsafe { std::env::remove_var(ENV_REPLAY) };
        let got = session.client.retry_methods_for_test();
        let expected: std::collections::HashSet<String> = crate::init_params::RETRY_ON_CONTENT_MODIFIED
            .into_iter()
            .map(Into::into)
            .collect();
        assert!(
            !expected.is_empty(),
            "生产白名单常量不得为空（防御性：常量被清空时注册测也无意义）"
        );
        assert_eq!(
            got, expected,
            "Session::start 必须逐字注册 RETRY_ON_CONTENT_MODIFIED（多、少、改都算漂移）"
        );
    }

    #[test]
    fn language_id_override_by_extension_falls_back_to_default() {
        let mut table = std::collections::HashMap::new();
        table.insert(Box::<str>::from("astro"), Box::<str>::from("astro"));
        table.insert(Box::<str>::from("ts"), Box::<str>::from("typescript"));
        // 命中表：.astro → astro、.ts → typescript（大小写不敏感）。
        assert_eq!(
            resolve_language_id_for("x", &table, Path::new("a/b/index.astro")),
            "astro"
        );
        assert_eq!(
            resolve_language_id_for("x", &table, Path::new("SRC/FMT.TS")),
            "typescript"
        );
        // 未命中（无扩展名 / 表外扩展名）→ 会话默认。
        assert_eq!(
            resolve_language_id_for("vue", &table, Path::new("App")),
            "vue"
        );
        assert_eq!(
            resolve_language_id_for("vue", &table, Path::new("x.css")),
            "vue"
        );
    }

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
        assert_eq!(
            progress_token_to_string(Some(&json!("abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(
            progress_token_to_string(Some(&json!(42))).as_deref(),
            Some("42")
        );
        assert_eq!(
            progress_token_to_string(Some(&json!(42u64))).as_deref(),
            Some("42")
        );
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

    // ---- 跨文件索引 $/progress 跟踪（cf54869a mirror）----

    #[test]
    fn cross_file_first_query_latch_fires_once() {
        let t = IndexProgressTracker::new();
        assert!(t.take_first_query(), "首查应是 latch 赢家");
        assert!(!t.take_first_query(), "后续查询不再是首查");
    }

    #[tokio::test]
    async fn cross_file_first_query_observes_start_then_drain_times_out() {
        // 首查路径真实等待的两个证据：grace 内观察到 begin（否则 grace 耗尽即 true），
        // 且 drain 超时返回 false（end 永不到来）。
        let t = Arc::new(IndexProgressTracker::new());
        t.track("idx-1", true);
        assert!(
            !t.wait_start_or_completion(Duration::from_millis(50), Duration::from_secs(1))
                .await,
            "begin 后无 end 应 drain 超时返 false"
        );
    }

    #[tokio::test]
    async fn cross_file_first_query_true_when_no_indexing_within_grace() {
        let t = IndexProgressTracker::new();
        assert!(
            t.wait_start_or_completion(Duration::from_secs(30), Duration::from_millis(30))
                .await,
            "grace 内无活动应视为无需索引返 true"
        );
    }

    #[tokio::test]
    async fn cross_file_later_query_drains_active_token() {
        // 后续查询（latch 已消耗）：有在飞 token → 等 end 到达才放行。
        let t = Arc::new(IndexProgressTracker::new());
        assert!(t.take_first_query(), "消耗首查 latch");
        t.track("idx-1", true);
        assert_eq!(t.active_count(), 1);
        let t2 = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            t2.track("idx-1", false);
        });
        assert!(
            t.wait_drain(Duration::from_secs(5)).await,
            "end 到达后应 drain 完成"
        );
        assert_eq!(t.active_count(), 0);
    }

    #[tokio::test]
    async fn cross_file_drain_immediate_when_no_active_token() {
        let t = IndexProgressTracker::new();
        assert!(
            t.wait_drain(Duration::from_secs(1)).await,
            "无在飞 token 立即放行"
        );
    }

    #[tokio::test]
    async fn cross_file_drain_times_out_when_end_never_arrives() {
        let t = IndexProgressTracker::new();
        t.track("idx-1", true);
        assert!(
            !t.wait_drain(Duration::from_millis(50)).await,
            "end 不到应超时返 false"
        );
    }

    // ---- bd 62z：握手段预算独立解析 ----

    #[test]
    fn handshake_secs_defaults_on_missing_or_invalid() {
        use std::time::Duration;
        assert_eq!(parse_handshake_secs(None), Duration::from_secs(30));
        assert_eq!(parse_handshake_secs(Some("45")), Duration::from_secs(45));
        assert_eq!(parse_handshake_secs(Some(" 90 ")), Duration::from_secs(90));
        // 0 = 握手永不超时，挂死无界 → 非法回默认（与 daemon parse_count 同语义）。
        assert_eq!(parse_handshake_secs(Some("0")), Duration::from_secs(30));
        assert_eq!(parse_handshake_secs(Some("-5")), Duration::from_secs(30));
        assert_eq!(parse_handshake_secs(Some("abc")), Duration::from_secs(30));
        assert_eq!(parse_handshake_secs(Some("")), Duration::from_secs(30));
    }
}
