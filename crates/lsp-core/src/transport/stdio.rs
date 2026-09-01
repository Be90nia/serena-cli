//! 子进程 stdio 传输：3 泵拓扑。
//!
//! ↖ mirror: ls_process.py@43ae021 `StdioLanguageServer` / `_read_ls_process_stdout` /
//! `_read_ls_process_stderr`（线程+队列 → tokio task 等价，ARCHITECTURE §3.2）：
//! - writer task 独占 `ChildStdin`——所有权即锁，从 outbound mpsc 收帧直写（写路径无锁）；
//! - stdout 泵跑同一 Content-Length 帧循环，**内联保序分发**（不转发无界 channel，
//!   防诊断代际倒序）；
//! - stderr 泵逐行 → tracing 分级（缺省 info；Task 8 接 logmap 表：clangd `I[..]/E[..]`）。

use bytes::BytesMut;
use std::sync::Arc;

use ls_runtime::process::ChildHandle;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::framing::{JsonRpc, decode, encode};

/// 内联分发回调（保序；Task 5 起响应帧改走 pending 表，本回调留予通知/服务器事件）。
pub type OnMsg = Arc<dyn Fn(JsonRpc) + Send + Sync>;

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

/// 拆解 `ChildHandle` 并起 3 个泵 task。`outbound_rx` 关闭后 writer 丢 stdin
/// （LS 看到 EOF，自行走退出流程；优雅 shutdown 序列属 Task 6）。
pub fn pump(child: ChildHandle, outbound_rx: mpsc::Receiver<JsonRpc>, on_msg: OnMsg) -> Pumps {
    let ChildHandle {
        stdin,
        stdout,
        stderr,
        job,
    } = child;

    // writer：独占 stdin，单一所有者天然串行（Δ 等价替换上游 `_stdin_lock`）。
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        let mut rx = outbound_rx;
        while let Some(msg) = rx.recv().await {
            let frame = encode(&msg);
            if let Err(e) = stdin.write_all(&frame).await {
                tracing::warn!(error = %e, "stdin write failed; LS process likely dead");
                break;
            }
        }
        // rx 关闭：stdin 在此 drop → LS 读到 EOF
    });

    // stdout 泵：帧解析 + 内联分发。
    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut buf = BytesMut::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) => break, // EOF；崩溃 drain pending 语义 = Task 5
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) => {
                    tracing::warn!(error = %e, "stdout read failed; aborting pump");
                    break;
                }
            }
            loop {
                match decode(&mut buf) {
                    Ok(Some(msg)) => dispatch(msg, &on_msg),
                    Ok(None) => break, // 半帧，继续读
                    Err(e) => {
                        // 帧损坏无法重同步，弃泵保命；Task 5 起此处升级为 Terminated 语义
                        tracing::error!(error = %e, "frame decode failed; aborting stdout pump");
                        return;
                    }
                }
            }
        }
    });

    // stderr 泵：逐行分级写日志。
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    // 缺省分级 info；Task 8 接 logmap（clangd `I[..]/E[..]` 前缀 → 级别）
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
fn dispatch(msg: JsonRpc, on_msg: &OnMsg) {
    if msg.method.is_none() {
        // 响应帧：Task 5 起 pending 表在此 pop 并完成对应 oneshot；
        // 本任务 pending 表未进场，响应帧直达回调（集成测试断言点）。
        on_msg(msg);
    } else if msg.id.is_some() {
        // 服务器→客户端请求：Task 5 挂点——pending 表进场后此处回默认 null 成功
        // 响应（vscode-languageserver-node 系把 registerCapability 错误当致命，
        // ARCHITECTURE §3.2）或交 adapter 注册的 handler。mock_ls 不触达此分支。
        todo!("Task 5: server→client request dispatch (null-success reply / handler)");
    } else {
        // 通知（publishDiagnostics 等）：内联按序分发。
        on_msg(msg);
    }
}
