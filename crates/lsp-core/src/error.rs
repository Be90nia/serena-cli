//! lsp-core 具名错误（ARCHITECTURE §6.1）。
//!
//! 全变体一次列全，本任务用到几个落几个；未在本任务接线的变体暂列占位（dead_code
//! 容忍，后续任务接线）。`anyhow` 不得越界进入 lsp-core —— 调用方需要按变体分类
//! 重试/上报。

use std::io;

/// lsp-core 错误。`ServerCancelled` 对应 LSP ErrorCodes.ServerCancelled(-32802)。
/// `ContentModified(-32801)` **不**设独立变体 —— 由 `client.rs` 在内部消化重试，
/// 不外泄（ARCHITECTURE §6.1）。
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Framing 变体占位到 M1 transport 把 `FrameError` 转 `CoreError::Framing`（stdio pump 解码失败路径）
pub enum CoreError {
    /// 帧解析失败（CONTENT_LENGTH 缺失/头非 UTF-8/JSON 不合法等）。
    #[error("framing error: {detail}")]
    Framing { detail: String },

    /// I/O 错误（pump 读写出错、子进程句柄异常）。
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// LS 返回 JSON-RPC error 且不在 client 内部消化的白名单内。
    /// `code` 字段对应 LSP ErrorCodes；调用方可按 code 判定语义。
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },

    /// 请求超时（client 在 `request()` 内对 oneshot 加 timeout）。
    #[error("request `{method}` timed out after {secs}s")]
    Timeout { method: String, secs: u64 },

    /// LS 进程崩溃 / EOF / 显式 kill —— 泵 task 退出前 drain pending 全体回此错。
    #[error("language server `{ls}` terminated: {cause}")]
    Terminated { ls: String, cause: String },

    /// 服务器主动 cancel 请求（LSP ErrorCodes.ServerCancelled=-32802）。
    /// ↖ mirror: ls_process.py@43ae021 `ServerCancelled`。
    #[error("server cancelled request `{method}`")]
    ServerCancelled { method: String },
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
