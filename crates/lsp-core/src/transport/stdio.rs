//! 子进程 stdio 传输：3 泵拓扑。
//!
//! ↖ mirror: ls_process.py@43ae021 `StdioLanguageServer` / `_read_ls_process_stdout` /
//! `_read_ls_process_stderr`（线程+队列 → tokio task 等价，ARCHITECTURE §3.2）：
//! - writer task 独占 `ChildStdin`——所有权即锁，从 outbound mpsc 收帧直写（写路径无锁）；
//! - stdout 泵跑同一 Content-Length 帧循环，**内联保序分发**（不转发无界 channel，
//!   防诊断代际倒序）；
//! - stderr 泵逐行 → tracing 分级（缺省 info；Task 8 接 logmap 表：clangd `I[..]/E[..]`）。
//!
//! 响应帧走 Client `pending` 表完成 oneshot；

use bytes::BytesMut;
use std::sync::Arc;

use ls_runtime::process::ChildHandle;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client::{OutboundItem, Priority};
use crate::framing::{Decoder, JsonRpc, encode};
use crate::recording::Recorder;

/// 内联分发回调（保序；Task 5：返回 `Some(reply)` 表示 server→client request 需回执）。
///
/// 收到响应帧/通知时返回 `None`（不需回写 LS）；收到 server→client 请求时返回
/// 默认/handler 计算的响应帧，由 pump 经 `reply_tx` → writer 写回 LS。
pub type OnMsg = Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync>;

/// 泵 stdout EOF 时的回调。让上层（Client）drain pending 并对所有等待者回 Terminated。
pub type OnEof = Arc<dyn Fn() + Send + Sync>;

/// 泵集合：三个常驻 task 的 JoinHandle + Job 保活句柄。
///
/// drop `Pumps` → Job 句柄关闭 → KILL_ON_JOB_CLOSE 清掉整棵 LS 进程树。
pub struct Pumps {
    pub writer: JoinHandle<()>,
    pub stdout: JoinHandle<()>,
    pub stderr: JoinHandle<()>,
    /// 保活：drop 即灭树（Unix 为 None，PDEATHSIG 等价路径后续标注）。
    pub job: Option<win32job::Job>,
}

impl Pumps {
    /// 显式终止进程树：丢 Job → 句柄关闭 → 内核清场。
    pub fn kill(&mut self) {
        self.job.take();
    }
}

/// 拆解 `ChildHandle` 并起 3 个泵 task。
///
/// - `outbound_rx`：调用方写入的请求/通知帧。drop → writer 丢 stdin（LS 走 EOF 退出）。
/// - `reply_rx` + `reply_tx`：服务器→客户端请求的回执通道；writer `select!` 此与 outbound_rx。
/// - `on_msg`：每帧到达时调用；返回 `Some(reply)` 即经 `reply_tx` 写回 LS。
/// - `on_eof`：stdout 读到 EOF 时调用；典型实现 = `Client::abort_all`。
pub fn pump(
    child: ChildHandle,
    outbound_rx: mpsc::Receiver<JsonRpc>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
) -> Pumps {
    let ChildHandle {
        stdin,
        stdout,
        stderr,
        job,
        ..
    } = child;

    let reply_tx_for_dispatch = reply_tx.clone();

    // writer：独占 stdin，单一所有者天然串行（Δ 等价替换上游 `_stdin_lock`）。
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        let mut out_rx = outbound_rx;
        let mut rep_rx = reply_rx;
        let mut buf_out = Vec::with_capacity(4096);
        let mut out_closed = false;
        let mut rep_closed = false;
        while !(out_closed && rep_closed) {
            tokio::select! {
                biased;
                msg = out_rx.recv(), if !out_closed => {
                    match msg {
                        Some(msg) => {
                            let frame = encode(&msg);
                            buf_out.clear();
                            buf_out.extend_from_slice(&frame);
                            if let Err(e) = stdin.write_all(&buf_out).await {
                                tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                                break;
                            }
                        }
                        None => out_closed = true,
                    }
                }
                msg = rep_rx.recv(), if !rep_closed => {
                    match msg {
                        Some(msg) => {
                            let frame = encode(&msg);
                            buf_out.clear();
                            buf_out.extend_from_slice(&frame);
                            if let Err(e) = stdin.write_all(&buf_out).await {
                                tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                                break;
                            }
                        }
                        None => rep_closed = true,
                    }
                }
            }
        }
    });

    // stdout 泵：帧解析 + 内联分发。EOF → on_eof（drain pending）。
    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = BytesMut::new();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 8192];
        let mut saw_eof = false;
        while !saw_eof {
            match stdout.read(&mut chunk).await {
                Ok(0) => {
                    saw_eof = true;
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "stdout read failed; aborting pump");
                    saw_eof = true;
                }
            }
            loop {
                match decoder.decode(&mut buf) {
                    Ok(Some(msg)) => dispatch(msg, &on_msg, &reply_tx_for_dispatch),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "frame decode failed; aborting stdout pump");
                        saw_eof = true;
                        break;
                    }
                }
            }
        }
        (on_eof)();
    });

    // stderr 泵：逐行分级写日志。
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    tracing::info!(target: "lsp_stderr", "{line}");
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "stderr pump read error");
                    break;
                }
            }
        }
    });

    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job,
    }
}

/// stdout 泵的内联分发（按到达顺序，保序）。
///
/// 响应帧 / 通知 / server→client request 三种入站形态由 `OnMsg`（= Client）自行处理；
/// server→client request 产生的回执由 pump 经 `reply_tx` → writer 写回 LS。
fn dispatch(msg: JsonRpc, on_msg: &OnMsg, reply_tx: &mpsc::Sender<JsonRpc>) {
    if let Some(reply) = (on_msg)(msg)
        && let Err(e) = reply_tx.try_send(reply)
    {
        tracing::warn!(error = %e, "reply_tx 满/关，丢弃 server→client request 回执");
    }
}

/// record 模式 pump（PLAN Task 26）：透传 + 写盘所有出/入帧。
pub fn record_pump(
    child: ChildHandle,
    outbound_rx: mpsc::Receiver<JsonRpc>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
    recorder: Recorder,
) -> Pumps {
    let ChildHandle {
        stdin,
        stdout,
        stderr,
        job,
        ..
    } = child;
    let reply_tx_for_dispatch = reply_tx.clone();
    let rec_w = recorder.clone();
    let rec_r = recorder;
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        let mut out_rx = outbound_rx;
        let mut rep_rx = reply_rx;
        let mut buf_out = Vec::with_capacity(4096);
        let mut out_closed = false;
        let mut rep_closed = false;
        while !(out_closed && rep_closed) {
            tokio::select! {
                biased;
                msg = out_rx.recv(), if !out_closed => match msg {
                    Some(m) => {
                        rec_w.record_outbound(&m);
                        let frame = encode(&m);
                        buf_out.clear();
                        buf_out.extend_from_slice(&frame);
                        if let Err(e) = stdin.write_all(&buf_out).await {
                            tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                            break;
                        }
                    }
                    None => out_closed = true,
                },
                msg = rep_rx.recv(), if !rep_closed => match msg {
                    Some(m) => {
                        rec_w.record_outbound(&m);
                        let frame = encode(&m);
                        buf_out.clear();
                        buf_out.extend_from_slice(&frame);
                        if let Err(e) = stdin.write_all(&buf_out).await {
                            tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                            break;
                        }
                    }
                    None => rep_closed = true,
                },
            }
        }
    });
    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = BytesMut::new();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 8192];
        let mut saw_eof = false;
        while !saw_eof {
            match stdout.read(&mut chunk).await {
                Ok(0) => saw_eof = true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "stdout read failed; aborting pump");
                    saw_eof = true;
                }
            }
            loop {
                match decoder.decode(&mut buf) {
                    Ok(Some(m)) => {
                        rec_r.record_inbound(&m);
                        dispatch(m, &on_msg, &reply_tx_for_dispatch);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "frame decode failed; aborting stdout pump");
                        saw_eof = true;
                        break;
                    }
                }
            }
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::info!(target: "lsp_stderr", "{line}"),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "stderr pump read error");
                    break;
                }
            }
        }
    });
    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job,
    }
}

/// replay 模式 pump（PLAN Task 26）：不接真 LS。
pub fn replay_pump(
    outbound_rx: mpsc::Receiver<JsonRpc>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
    recorder: Recorder,
) -> Pumps {
    let mut out_rx = outbound_rx;
    let mut rep_rx = reply_rx;
    let reply_tx_for_dispatch = reply_tx;
    let rec_w = recorder.clone();
    let rec_r = recorder;
    let on_eof = on_eof.clone();
    let writer = tokio::spawn(async move {
        let mut out_closed = false;
        let mut rep_closed = false;
        while !(out_closed && rep_closed) {
            tokio::select! {
                biased;
                msg = out_rx.recv(), if !out_closed => match msg {
                    Some(m) => { rec_w.record_outbound(&m); }
                    None => out_closed = true,
                },
                msg = rep_rx.recv(), if !rep_closed => match msg {
                    Some(m) => { rec_w.record_outbound(&m); }
                    None => rep_closed = true,
                },
            }
        }
    });
    let stdout_task = tokio::spawn(async move {
        while let Some(m) = rec_r.next_inbound() {
            dispatch(m, &on_msg, &reply_tx_for_dispatch);
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {});
    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job: None,
    }
}

// ────────────────────────── P0B：priority-aware writer ──────────────────────────
//
// 三路 outbound（High/Normal/Background）+ TokenBucket 限流。Background 累积超出
// burst 后 sleep 等令牌，**不**抢占用户面向（High/Normal）流量。reply_rx 仍与
// outbound 并行 select（server→client request 回执直写，零延迟）。
//
// 单调用方写路径延迟：High 直送（≤一次 mpsc recv + 编码 + write）；Normal 直送（同
// 上）；Background 受 burst 限（稳态 30/s）。`select!` 的 `biased` 保证三路 High
// 永不排队等 Normal —— 与无 priority 版本差异：同一时刻多个 channel 都有帧时，
// 旧版本走 FIFO，新版本按 priority 排序。

/// P0B：构造生产 writer。channel 64 缓冲；Normal 累积 >50ms 自动降级 Background。
pub fn pump_with_priority(
    child: ChildHandle,
    outbound_rx: mpsc::Receiver<OutboundItem>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
) -> Pumps {
    let ChildHandle {
        stdin,
        stdout,
        stderr,
        job,
        ..
    } = child;
    let reply_tx_for_dispatch = reply_tx.clone();
    let rec_w = Recorder::passthrough();
    let rec_r = rec_w.clone();
    let writer = tokio::spawn(priority_writer_loop(
        Some(stdin),
        outbound_rx,
        reply_rx,
        reply_tx,
        move |msg| {
            rec_w.record_outbound(&msg);
            Some(msg)
        },
    ));
    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = BytesMut::new();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 8192];
        let mut saw_eof = false;
        while !saw_eof {
            match stdout.read(&mut chunk).await {
                Ok(0) => saw_eof = true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "stdout read failed; aborting pump");
                    saw_eof = true;
                }
            }
            loop {
                match decoder.decode(&mut buf) {
                    Ok(Some(msg)) => dispatch(msg, &on_msg, &reply_tx_for_dispatch),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "frame decode failed; aborting stdout pump");
                        saw_eof = true;
                        break;
                    }
                }
            }
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::info!(target: "lsp_stderr", "{line}"),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "stderr pump read error");
                    break;
                }
            }
        }
    });
    let _ = rec_r; // suppress unused for non-record branch
    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job,
    }
}

/// P0B：record 模式（同 record_pump 行为 + priority 调度）。
pub fn record_pump_with_priority(
    child: ChildHandle,
    outbound_rx: mpsc::Receiver<OutboundItem>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
    recorder: Recorder,
) -> Pumps {
    let ChildHandle {
        stdin,
        stdout,
        stderr,
        job,
        ..
    } = child;
    let reply_tx_for_dispatch = reply_tx.clone();
    let rec_w = recorder.clone();
    let rec_r = recorder;
    let writer = tokio::spawn(priority_writer_loop(
        Some(stdin),
        outbound_rx,
        reply_rx,
        reply_tx,
        move |msg| {
            rec_w.record_outbound(&msg);
            Some(msg)
        },
    ));
    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = BytesMut::new();
        let mut decoder = Decoder::default();
        let mut chunk = [0u8; 8192];
        let mut saw_eof = false;
        while !saw_eof {
            match stdout.read(&mut chunk).await {
                Ok(0) => saw_eof = true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "stdout read failed; aborting pump");
                    saw_eof = true;
                }
            }
            loop {
                match decoder.decode(&mut buf) {
                    Ok(Some(msg)) => {
                        rec_r.record_inbound(&msg);
                        dispatch(msg, &on_msg, &reply_tx_for_dispatch);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::error!(error = %e, "frame decode failed; aborting stdout pump");
                        saw_eof = true;
                        break;
                    }
                }
            }
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::info!(target: "lsp_stderr", "{line}"),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "stderr pump read error");
                    break;
                }
            }
        }
    });
    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job,
    }
}

/// P0B：replay 模式（同 replay_pump 行为 + priority 调度）。不接真 LS，因此
/// writer 仅做 record_outbound 后丢帧（不写 stdin）。
pub fn replay_pump_with_priority(
    outbound_rx: mpsc::Receiver<OutboundItem>,
    reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    on_msg: OnMsg,
    on_eof: OnEof,
    recorder: Recorder,
) -> Pumps {
    let rec_w = recorder.clone();
    let rec_r = recorder;
    let reply_tx_for_dispatch = reply_tx;
    let writer = tokio::spawn(priority_writer_loop(
        None::<tokio::process::ChildStdin>,
        outbound_rx,
        reply_rx,
        reply_tx_for_dispatch.clone(),
        move |msg| {
            rec_w.record_outbound(&msg);
            Some(msg)
        },
    ));
    let stdout_task = tokio::spawn(async move {
        while let Some(m) = rec_r.next_inbound() {
            dispatch(m, &on_msg, &reply_tx_for_dispatch);
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {});
    Pumps {
        writer,
        stdout: stdout_task,
        stderr: stderr_task,
        job: None,
    }
}

/// P0B writer 主循环（替换原单 outbound_rx → 写 stdin）：
/// - select! `biased; High → Normal → Background → reply`；reply 直送（无 priority）。
/// - Normal 累积 >50ms → 把队首 Normal 帧（如果有）转入 Background 通道以释放
///   出队带宽给 Background 单帧发送。
/// - Background 帧在发送前必须经 `TokenBucket::try_acquire`；false 时 sleep
///   1/per_sec 等令牌（保持速率稳定 30/s、burst 2s）。
/// - `wrap_for_write` 在 record 模式记录、passthrough 模式直接透传；返回 `None`
///   表示外部已处理（replay 模式）。
async fn priority_writer_loop<W>(
    stdin: Option<W>,
    mut outbound_rx: mpsc::Receiver<OutboundItem>,
    mut reply_rx: mpsc::Receiver<JsonRpc>,
    reply_tx: mpsc::Sender<JsonRpc>,
    wrap_for_write: impl Fn(JsonRpc) -> Option<JsonRpc> + Send + 'static,
) where
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    use tokio::io::AsyncWriteExt;
    // `reply_tx` 仅作 sender 端生命周期控制 —— writer 不回写 reply（server→client
    // request 回执由 stdout 泵经 `dispatch` → reply_tx → writer 的 reply_rx 分支
    // 消费；reply_tx_for_dispatch 在 stdout task 持有，writer 收到的 reply_rx 是
    // 同一 channel 的对端）。本任务保留 `reply_tx` 占位，避免「全部 sender drop」
    // 提前关闭 reply_rx；writer 退出靠 outbound_rx 真收到 None（on_session_drop）。
    let _ = reply_tx;

    // 共享队列状态：本 task 内 select 路由 + 写帧交替执行；用 tokio::sync::Mutex
    // 保证跨 await 安全。`std::sync::Mutex` 不能跨 .await 持锁，会死锁 runtime。
    let queues = std::sync::Arc::new(tokio::sync::Mutex::new(Queues {
        high: std::collections::VecDeque::new(),
        normal: std::collections::VecDeque::new(),
        bg: std::collections::VecDeque::new(),
        first_normal_at: None,
    }));
    let bucket = crate::client::TokenBucket::new(30, 0);
    let mut buf_out: Vec<u8> = Vec::with_capacity(4096);
    let mut stdin = stdin;
    let mut out_closed = false;
    let mut rep_closed = false;

    while !out_closed {
        // 嵌套两个简单 select：外层在 reply / inbound 两者间选；选出后内层做"写一帧
        // + 收尾 demote check"。sleep 不再常驻 select 分支 —— needs_demote 的检查
        // 移入 inner loop 写完一帧后单次判断；消除「50ms sleep 永远 ready 抢带宽」
        // 假象（实测：biased 仍优先 outbound，但带 50ms timer 等同隐性 delay）
        let next_action: NextAction = tokio::select! {
            biased;
            msg = reply_rx.recv(), if !rep_closed => match msg {
                Some(m) => NextAction::WriteReply(Box::new(m)),
                None => { rep_closed = true; NextAction::None }
            },
            it = outbound_rx.recv(), if !out_closed => match it {
                Some(it) => { route_into(&queues, it).await; NextAction::None }
                None => { out_closed = true; NextAction::None }
            },
        };

        // P0B 修：每次 select 后先 try_recv drain 排队的 inbound —— 否则单条 inbound
        // 触发单条 write（按出队序）会让"先 BG×5 后 High"被写成 BG×5 再 High 而非
        // High 抢先。drain 后 pick_next 看到全部排队帧，按优先级 High > Normal > BG
        // 取最高。
        if !out_closed {
            while let Ok(it) = outbound_rx.try_recv() {
                route_into(&queues, it).await;
            }
        }

        // 写一帧：优先 High > Normal > Background；若 Background 无令牌则 sleep 等。
        match next_action {
            NextAction::WriteReply(m) => {
                if let Some(stdin) = stdin.as_mut() {
                    let frame = encode(&m);
                    buf_out.clear();
                    buf_out.extend_from_slice(&frame);
                    if let Err(e) = stdin.write_all(&buf_out).await {
                        tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                        return;
                    }
                }
                // 写完 reply 后检查 demote：保持 Normal 累积 50ms → Background 的节流
                if needs_demote(&queues).await {
                    demote_one_normal(&queues).await;
                }
            }
            NextAction::None => {
                // P0B 修：drain 全部 High + Normal（直送无令牌）；Background 走
                // TokenBucket，每帧前 try_acquire → false 时 sleep 等。单次循环
                // 内 high/normal 全部弹出；bg 写到令牌空即停，下次 select 再续。
                loop {
                    // 审计 F4：出站热流（尤其 BG 持续写）期间 reply 通道也要消费，
                    // 否则 server→client 回执在 mpsc(8) 积压后被 dispatch try_send 丢弃。
                    while let Ok(m) = reply_rx.try_recv() {
                        if let Some(stdin) = stdin.as_mut() {
                            let frame = encode(&m);
                            buf_out.clear();
                            buf_out.extend_from_slice(&frame);
                            if let Err(e) = stdin.write_all(&buf_out).await {
                                tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                                return;
                            }
                        }
                    }
                    // 是否还有 High 或 Normal 待写？
                    let has_priority = {
                        let q = queues.lock().await;
                        !q.high.is_empty() || !q.normal.is_empty()
                    };
                    if has_priority {
                        let m = pick_next(&queues).await.unwrap();
                        if let Some(m) = wrap_for_write(m)
                            && let Some(stdin) = stdin.as_mut()
                        {
                            let frame = encode(&m);
                            buf_out.clear();
                            buf_out.extend_from_slice(&frame);
                            if let Err(e) = stdin.write_all(&buf_out).await {
                                tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                                return;
                            }
                        }
                        // 写完一帧后检查 demote：移出 select 的常驻 sleep 分支后，
                        // demote 改在写路径每帧尾部触发（仍守 needs_demote 真值）。
                        if needs_demote(&queues).await {
                            demote_one_normal(&queues).await;
                        }
                        continue;
                    }
                    // Background 一帧（受 TokenBucket 限流）
                    let has_bg = { !queues.lock().await.bg.is_empty() };
                    if !has_bg {
                        break;
                    }
                    let now = std::time::Instant::now();
                    if !bucket.try_acquire(now) {
                        let d = bg_wait_duration(&bucket);
                        tokio::time::sleep(d).await;
                        // 令牌按时间持续恢复：睡完直接重试 try_acquire，而非 break 回
                        // select —— select 无 timer 分支，安静期（无入站帧/回执）会让
                        // BG 积压帧无限期停摆（审计 P1-1）。
                        continue;
                    }
                    let m = { queues.lock().await.bg.pop_front().unwrap() };
                    if let Some(m) = wrap_for_write(m)
                        && let Some(stdin) = stdin.as_mut()
                    {
                        let frame = encode(&m);
                        buf_out.clear();
                        buf_out.extend_from_slice(&frame);
                        if let Err(e) = stdin.write_all(&buf_out).await {
                            tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                            return;
                        }
                    }
                    // 写完 BG 后同样检查 demote
                    if needs_demote(&queues).await {
                        demote_one_normal(&queues).await;
                    }
                }
            }
        }
    }
}

enum NextAction {
    WriteReply(Box<JsonRpc>),
    None,
}

async fn route_into(queues: &std::sync::Arc<tokio::sync::Mutex<Queues>>, it: OutboundItem) {
    let mut q = queues.lock().await;
    match it.priority {
        Priority::High => {
            q.high.push_back(it.msg);
            q.first_normal_at = None;
        }
        Priority::Normal => {
            if q.first_normal_at.is_none() {
                q.first_normal_at = Some(std::time::Instant::now());
            }
            q.normal.push_back(it.msg);
        }
        Priority::Background => {
            q.bg.push_back(it.msg);
            q.first_normal_at = None;
        }
    }
}

async fn needs_demote(queues: &std::sync::Arc<tokio::sync::Mutex<Queues>>) -> bool {
    let q = queues.lock().await;
    matches!(q.first_normal_at, Some(t) if t.elapsed() >= std::time::Duration::from_millis(50))
}

async fn demote_one_normal(queues: &std::sync::Arc<tokio::sync::Mutex<Queues>>) {
    let mut q = queues.lock().await;
    if let Some(front) = q.normal.pop_front() {
        q.bg.push_back(front);
    }
    q.first_normal_at = None;
}

fn bg_wait_duration(bucket: &crate::client::TokenBucket) -> std::time::Duration {
    let secs = 1.0 / (bucket.per_sec() as f64);
    std::time::Duration::from_secs_f64(secs)
}

async fn pick_next(queues: &std::sync::Arc<tokio::sync::Mutex<Queues>>) -> Option<JsonRpc> {
    let mut q = queues.lock().await;
    if let Some(m) = q.high.pop_front() {
        Some(m)
    } else if let Some(m) = q.normal.pop_front() {
        if q.normal.is_empty() {
            q.first_normal_at = None;
        }
        Some(m)
    } else {
        q.bg.pop_front()
    }
}

/// 共享队列状态：路由与 write 路径在同一 task 内交替使用。
struct Queues {
    high: std::collections::VecDeque<JsonRpc>,
    normal: std::collections::VecDeque<JsonRpc>,
    bg: std::collections::VecDeque<JsonRpc>,
    first_normal_at: Option<std::time::Instant>,
}
