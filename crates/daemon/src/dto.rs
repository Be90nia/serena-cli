//! Wire DTO（ARCHITECTURE §6.3 / PLAN Task 12）。
//!
//! 跨 daemon 传输格式：CLI → daemon → supervisor 工具语义。
//!
//! - 工具级失败走 HTTP 200 + `{ok:false}`，传输层错误才用 4xx/5xx（A5）。
//! - 9 个错误码对应 ToolError 各变体（见 `wire_error_from_tool_error`）。

use serde::{Deserialize, Serialize};

/// `POST /tools/{name}` 请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub project_root: String,
    /// 工具参数（serde_json::Value 让各工具自行解析）。
    pub args: serde_json::Value,
    /// 多语言项目用：覆盖文件扩展名探测（如 `--lang typescript`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// 响应包装：`{ok, data|error}` 或 `data`+`format`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResponse {
    Ok {
        ok: bool, // true
        data: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    Err {
        ok: bool, // false
        error: WireError,
    },
}

/// 9 个错误码（ARCH §6.3 表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WireErrorCode {
    BadArgs,
    LsNotInstalled,
    LsSpawnFailed,
    LsNotReady,
    LsTerminated,
    LsTimeout,
    RpcError,
    WriteConflict,
    Internal,
}

impl WireErrorCode {
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::LsSpawnFailed | Self::LsNotReady | Self::LsTerminated | Self::LsTimeout
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: WireErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ls: Option<String>,
    pub retryable: bool,
}

/// `GET /status` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub uptime_secs: u64,
    pub pid: u32,
    pub loaded_ls: Vec<String>,
    pub draining: bool,
}

/// 把 supervisor 的 `ToolError` 翻译成 wire error。
///
/// ARCH §6.3 表的映射。CLI exit code 由 `wire_error_code_to_exit` 单独决定。
pub fn wire_error_from_tool_error(err: &supervisor::ToolError) -> WireError {
    use supervisor::ToolError;
    let (code, message) = match err {
        ToolError::BadArgs { detail } => (WireErrorCode::BadArgs, detail.clone()),
        ToolError::NotInstalled { language, hint } => {
            (WireErrorCode::LsNotInstalled, format!("{language}: {hint}"))
        }
        ToolError::Core(core) => match core {
            supervisor::CoreErrorWire::Rpc { code, message } => {
                (WireErrorCode::RpcError, format!("rpc {code}: {message}"))
            }
            supervisor::CoreErrorWire::Timeout { method, secs } => (
                WireErrorCode::LsTimeout,
                format!("{method} timed out after {secs}s"),
            ),
            supervisor::CoreErrorWire::Terminated { ls, cause } => {
                (WireErrorCode::LsTerminated, format!("{ls}: {cause}"))
            }
            supervisor::CoreErrorWire::ServerCancelled { method } => (
                WireErrorCode::RpcError,
                format!("server cancelled {method}"),
            ),
            // `Io` / `Framing` 走 INTERNAL 兜底（调用方无法按 IO/Framing 区分重试）。
            other => (WireErrorCode::Internal, format!("{}: {:?}", other, other)),
        },
        // Δ 43ae021：从 Launch 兜底拆出 Serialize/Protocol —— 确定性失败（daemon 序列化
        // bug、LS 违反协议语义）不再伪装成 retryable 的 LS_SPAWN_FAILED 诱发无意义重试。
        ToolError::Serialize(re) => (WireErrorCode::Internal, format!("serialize failed: {re}")),
        ToolError::Protocol { tool, reason } => {
            (WireErrorCode::RpcError, format!("{tool}: {reason}"))
        }
        ToolError::Launch(re) => (WireErrorCode::LsSpawnFailed, format!("{re}")),
        ToolError::WriteConflict { path, reason } => {
            (WireErrorCode::WriteConflict, format!("{path}: {reason}"))
        }
    };
    let ls = match err {
        ToolError::Core(supervisor::CoreErrorWire::Terminated { ls, .. }) => Some(ls.clone()),
        ToolError::NotInstalled { language, .. } => Some(language.clone()),
        _ => None,
    };
    WireError {
        code,
        message,
        ls,
        retryable: code.retryable(),
    }
}

/// ARCH §6.3 表的 CLI exit code 列。
pub fn wire_error_code_to_exit(code: WireErrorCode) -> u8 {
    match code {
        WireErrorCode::BadArgs => 2,
        WireErrorCode::WriteConflict => 1,
        WireErrorCode::Internal => 3,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_request_roundtrip() {
        let req = ToolRequest {
            project_root: "D:/proj".into(),
            args: serde_json::json!({"pattern": "Foo"}),
            lang: Some("rust".into()),
        };
        let j = serde_json::to_string(&req).unwrap();
        let back: ToolRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.project_root, "D:/proj");
    }

    #[test]
    fn wire_error_serializes_screaming() {
        let e = WireError {
            code: WireErrorCode::WriteConflict,
            message: "x".into(),
            ls: None,
            retryable: false,
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(j.contains("\"code\":\"WRITE_CONFLICT\""), "got: {j}");
    }

    #[test]
    fn response_ok_with_data() {
        let resp = ToolResponse::Ok {
            ok: true,
            data: serde_json::json!([1, 2, 3]),
            format: None,
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert!(j.contains("\"ok\":true"));
        assert!(j.contains("\"data\":[1,2,3]"));
    }

    #[test]
    fn response_err_with_error() {
        let resp = ToolResponse::Err {
            ok: false,
            error: WireError {
                code: WireErrorCode::BadArgs,
                message: "x".into(),
                ls: None,
                retryable: false,
            },
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert!(j.contains("\"ok\":false"));
        assert!(j.contains("\"code\":\"BAD_ARGS\""));
    }

    #[test]
    fn wire_error_from_bad_args() {
        let e = supervisor::ToolError::BadArgs {
            detail: "missing pattern".into(),
        };
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::BadArgs);
        assert!(!w.retryable);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::BadArgs), 2);
    }

    #[test]
    fn wire_error_from_not_installed() {
        let e = supervisor::ToolError::NotInstalled {
            language: "cpp".into(),
            hint: "install LLVM clangd".into(),
        };
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::LsNotInstalled);
        assert_eq!(w.ls.as_deref(), Some("cpp"));
        assert!(!w.retryable);
    }

    #[test]
    fn wire_error_from_write_conflict() {
        let e = supervisor::ToolError::WriteConflict {
            path: "src/x.rs".into(),
            reason: "hash mismatch".into(),
        };
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::WriteConflict);
        assert!(!w.retryable);
    }

    #[test]
    fn wire_error_from_core_timeout() {
        let e = supervisor::ToolError::Core(supervisor::CoreErrorWire::Timeout {
            method: "textDocument/definition".into(),
            secs: 30,
        });
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::LsTimeout);
        assert!(w.retryable);
    }

    #[test]
    fn wire_error_from_serialize_is_internal() {
        let e = supervisor::ToolError::Serialize(anyhow::anyhow!("map key is not a string"));
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::Internal);
        assert!(!w.retryable);
        assert_eq!(wire_error_code_to_exit(w.code), 3);
    }

    #[test]
    fn wire_error_from_protocol_is_rpc_error() {
        let e = supervisor::ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename returned null".into(),
        };
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::RpcError);
        assert!(!w.retryable);
        assert_eq!(wire_error_code_to_exit(w.code), 1);
        assert!(
            w.message.contains("rename returned null"),
            "got: {}",
            w.message
        );
    }

    #[test]
    fn wire_error_from_launch_is_retryable_spawn_failed() {
        let e = supervisor::ToolError::Launch(anyhow::anyhow!("runtime spawn error: boom"));
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::LsSpawnFailed);
        assert!(w.retryable);
    }
}
