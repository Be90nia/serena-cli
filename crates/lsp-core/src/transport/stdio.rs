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

use crate::framing::{JsonRpc, decode, encode};
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
                match decode(&mut buf) {
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
    let ChildHandle { stdin, stdout, stderr, job } = child;
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
        let mut chunk = [0u8; 8192];
        let mut saw_eof = false;
        while !saw_eof {
            match stdout.read(&mut chunk).await {
                Ok(0) => saw_eof = true,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => { tracing::warn!(error = %e, "stdout read failed; aborting pump"); saw_eof = true; }
            }
            loop {
                match decode(&mut buf) {
                    Ok(Some(m)) => {
                        rec_r.record_inbound(&m);
                        dispatch(m, &on_msg, &reply_tx_for_dispatch);
                    }
                    Ok(None) => break,
                    Err(e) => { tracing::error!(error = %e, "frame decode failed; aborting stdout pump"); saw_eof = true; break; }
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
                Err(e) => { tracing::debug!(error = %e, "stderr pump read error"); break; }
            }
        }
    });
    Pumps { writer, stdout: stdout_task, stderr: stderr_task, job }
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
    let reply_tx_for_dispatch = reply_tx.clone();
    let rec_w = recorder.clone();
    let rec_r = recorder;
    let writer = tokio::spawn(async move {
        let mut out_rx = outbound_rx;
        let mut rep_rx = reply_rx;
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
        loop {
            match rec_r.next_inbound() {
                Some(m) => dispatch(m, &on_msg, &reply_tx_for_dispatch),
                None => break,
            }
        }
        (on_eof)();
    });
    let stderr_task = tokio::spawn(async move {});
    Pumps { writer, stdout: stdout_task, stderr: stderr_task, job: None }
}
