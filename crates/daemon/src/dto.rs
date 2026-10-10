//! Wire DTO（ARCHITECTURE §6.3 / PLAN Task 12）。
//!
//! 跨 daemon 传输格式：CLI → daemon → supervisor 工具语义。
//!
//! - 工具级失败走 HTTP 200 + `{ok:false}`，传输层错误才用 4xx/5xx（A5）。
//! - 9 个错误码对应 ToolError 各变体（见 `wire_error_from_tool_error`）。

use serde::{Deserialize, Serialize};

/// wire 协议版本（d3a）。9 错误码 + `{ok,data|error}` 契约 = v1。
/// bump 判据与字段级变更点见 ARCHITECTURE §6.3（wire v1 错误码表）与 docs/rpc-catalog.json
/// ——改字段/错误码前先读那两处，bump 后客户端按此字符串协商。
pub const WIRE_PROTOCOL_VERSION: &str = "1";

/// 编排兼容证据（d3a / orca review §2.1）：调用方环境自述，仅入重放日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatEvidence {
    /// 调用方运行环境（如 `windows-x86_64`）。
    pub env: String,
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// 编排 envelope（d3a）：请求侧可选附加，响应结构不动。
/// 优先级：body envelope > `X-Invocation-Id` header > daemon 自动生成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationEnvelope {
    pub invocation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat_evidence: Option<CompatEvidence>,
}

/// `POST /tools/{name}` 请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    pub project_root: String,
    /// 工具参数（serde_json::Value 让各工具自行解析）。
    pub args: serde_json::Value,
    /// 多语言项目用：覆盖文件扩展名探测（如 `--lang typescript`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// 编排 envelope（d3a）：老客户端不发 → None（向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<InvocationEnvelope>,
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
        /// 7rh：响应体 token 估算（序列化字节/4，无 tokenizer 依赖）。None =
        /// 不附字段（错误响应、/batch、SERENA_NO_TOKEN_ESTIMATE=1 时）。
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "~tokens")]
        approx_tokens: Option<u64>,
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
    /// 行动指引（bd serena-rust-i4j）：WRITE_CONFLICT 携带，告诉 agent 冲突不是
    /// 参数错、重读后重试即可。其余错误码 None（序列化时省略，向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// bd 7tk：`recent_errors` 环条目（daemon 侧最近 N 条工具级失败）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRecentError {
    pub ts_ms: u64,
    pub tool: String,
    pub code: WireErrorCode,
}

/// bd b09i：加载中的 LS 结构化条目。sessions = 该 lang 的实例池键数
/// （(project_root, lang) 各占一键 = 一个 LS 进程；daemon 数据模型里 worker 与
/// session 同物，不再拆语义不明的第二个数字）。替代旧 `["rust x3"]` 字符串形态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedLs {
    pub lang: String,
    pub sessions: u32,
}

/// `GET /status` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub uptime_secs: u64,
    pub pid: u32,
    /// bd b09i：结构化形态（wire 替换评估结论：CLI/daemon 同仓同发——升级硬步骤
    /// 已含 stop-all——且 status 是观测面；旧字符串形态不兼容保留）。
    #[serde(default)]
    pub loaded_ls: Vec<LoadedLs>,
    pub draining: bool,
    /// 最近一次工具请求的 project_root（daemon 启动时不带 project，为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_project: Option<String>,
    /// bd 7tk 观测四字段（wire v1 追加式扩展）。旧 daemon 响应缺字段 → 客户端
    /// `#[serde(default)]` 兜底；新 daemon 多出的键对旧客户端无害（serde 忽略
    /// 未知字段）。空环省略键 = B0 hint 字段同款 skip 语义。
    /// 正在执行的工具请求数（drain 排空判据，负载信号）。
    #[serde(default)]
    pub in_flight: u64,
    /// daemon 生命周期内累计工具调用数（= invocations.jsonl 本代行数）。
    #[serde(default)]
    pub invocation_count: u64,
    /// 最近 N 条工具级失败（旧者先出；无失败时省略）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_errors: Vec<StatusRecentError>,
    /// 最近 N 个 invocation_id 前缀（多 agent 场景：不同编排方各有 id 命名空间；
    /// 无调用时省略）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_agents: Vec<String>,
    /// bd ulq：daemon 编译期版本（CARGO_PKG_VERSION）。客户端比对磁盘上新装
    /// CLI 版本即知 daemon 是否还在跑旧 binary（升级后 lazy-spawn 前提失效类
    /// 问题的自检锚）。旧 daemon 响应缺字段 → `#[serde(default)]` 兜底。
    #[serde(default)]
    pub daemon_version: String,
    /// bd ulq：daemon 进程自身 binary 路径（current_exe 解析失败省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
}

/// 把 supervisor 的 `ToolError` 翻译成 wire error。
///
/// ARCH §6.3 表的映射。CLI exit code 由 `wire_error_code_to_exit` 单独决定。
pub fn wire_error_from_tool_error(err: &supervisor::ToolError) -> WireError {
    use supervisor::ToolError;
    let (code, message) = match err {
        ToolError::BadArgs { detail } => (WireErrorCode::BadArgs, detail.clone()),
        // NotInstalled 走 Display 全文（含 hint + ls-use 注册指引，bd serena-rust-4ux）
        // ——手拼 `{language}: {hint}` 会绕过中央注入点，agent 消费的 wire 消息拿不到指引。
        ToolError::NotInstalled { .. } => (WireErrorCode::LsNotInstalled, err.to_string()),
        ToolError::Core(core) => match core {
            supervisor::CoreErrorWire::Rpc { code, message } => {
                (WireErrorCode::RpcError, format!("rpc {code}: {message}"))
            }
            // bd serena-rust-iyz：未就绪竞态（gate 开但握手未 settle）曾误走 Io
            // 兜底冒充 INTERNAL，LsNotReady 沦为死码。ARCH §5：Ready 是唯一
            // 服务态，其余状态按 §6.3 返回 LS_NOT_READY（retryable）。
            supervisor::CoreErrorWire::NotReady { cause } => {
                (WireErrorCode::LsNotReady, cause.clone())
            }
            supervisor::CoreErrorWire::Timeout { method, secs } => (
                WireErrorCode::LsTimeout,
                format!("{method} timed out after {secs}s"),
            ),
            supervisor::CoreErrorWire::Terminated { ls, cause } => {
                (
                    WireErrorCode::LsTerminated,
                    // blindtest v5 P2-D：零 hint → 加恢复指引（stop-all + retry；
                    // 懒 spawn 语义下次调用自动重拉）。
                    format!(
                        "{ls}: {cause}; the language server process exited — run \
                         `serena-cli stop-all` and retry (a fresh LS spawns lazily on the next call)"
                    ),
                )
            }
            supervisor::CoreErrorWire::ServerCancelled { method } => (
                WireErrorCode::RpcError,
                format!("server cancelled {method}"),
            ),
            // bd serena-rust-4kh：Io / Framing 曾与未知变体共用 `{:?}` debug 兜底，
            // 同码 INTERNAL 下「重读文件可解」(Io) 与「LS 流损坏须重启」(Framing)
            // 不可分辨（审计 F8 / sec-S5 共治）。wire v1 码集不变，只拆 message 臂。
            supervisor::CoreErrorWire::Io(e) => (WireErrorCode::Internal, format!("io error: {e}")),
            supervisor::CoreErrorWire::Framing { detail } => (
                WireErrorCode::Internal,
                format!("framing error: {detail}; the LS stream is corrupt, restart the daemon"),
            ),
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
    // bd serena-rust-i4j：写门/内容冲突统一带自检 hint —— agent 拿到的是
    // 「盘上内容与预期不符」而非参数错，指引重读重试，避免误诊为 BAD_ARGS。
    let hint = match err {
        ToolError::WriteConflict { .. } => Some(
            "another write may hold the gate or content has changed; re-read the file and retry"
                .to_string(),
        ),
        _ => None,
    };
    WireError {
        code,
        message,
        ls,
        retryable: code.retryable(),
        hint,
    }
}

/// ARCH §6.3 表的 CLI exit code 列。
///
/// bd 719：retryable 工具错（LS_TIMEOUT/LS_TERMINATED/LS_SPAWN_FAILED/LS_NOT_READY
/// 瞬态）单独成桶 exit 5 —— 全折叠为 1 时 agent 必须读 body 才知道可重试；
/// 非 retryable 维持既有契约（BadArgs=2 / WriteConflict=1 / Internal=3）不变。
pub fn wire_error_code_to_exit(code: WireErrorCode) -> u8 {
    match code {
        WireErrorCode::BadArgs => 2,
        WireErrorCode::WriteConflict => 1,
        WireErrorCode::Internal => 3,
        other if other.retryable() => 5,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd 719：retryable → 5（瞬态可重试桶）；非 retryable 既有契约不变。
    #[test]
    fn wire_error_code_to_exit_buckets_retryable() {
        assert_eq!(wire_error_code_to_exit(WireErrorCode::BadArgs), 2);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::WriteConflict), 1);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::Internal), 3);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::LsNotInstalled), 1);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::RpcError), 1);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::LsTimeout), 5);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::LsTerminated), 5);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::LsSpawnFailed), 5);
        assert_eq!(wire_error_code_to_exit(WireErrorCode::LsNotReady), 5);
    }

    #[test]
    fn tool_request_roundtrip() {
        let req = ToolRequest {
            project_root: "D:/proj".into(),
            args: serde_json::json!({"pattern": "Foo"}),
            lang: Some("rust".into()),
            envelope: None,
        };
        let j = serde_json::to_string(&req).unwrap();
        let back: ToolRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.project_root, "D:/proj");
    }

    /// d3a：老客户端 body 无 envelope 字段 → 反序列化为 None（wire 向后兼容）。
    #[test]
    fn legacy_request_without_envelope_deserializes() {
        let req: ToolRequest =
            serde_json::from_str(r#"{"project_root":"D:/proj","args":{}}"#).unwrap();
        assert!(req.envelope.is_none());
    }

    /// d3a：envelope 三键 + 可选 compat_evidence roundtrip。
    #[test]
    fn envelope_roundtrip_with_compat_evidence() {
        let j = serde_json::json!({
            "project_root": "D:/proj",
            "args": {},
            "envelope": {
                "invocation_id": "0f0e1d2c-3b4a-5968-7788-99aabbccddee",
                "server_version": "0.1.0",
                "protocol_version": WIRE_PROTOCOL_VERSION,
                "compat_evidence": {
                    "env": "windows-x86_64",
                    "protocol_version": WIRE_PROTOCOL_VERSION,
                    "capabilities": []
                }
            }
        });
        let req: ToolRequest = serde_json::from_value(j).unwrap();
        let env = req.envelope.expect("envelope present");
        assert_eq!(env.invocation_id, "0f0e1d2c-3b4a-5968-7788-99aabbccddee");
        assert_eq!(env.server_version.as_deref(), Some("0.1.0"));
        let ev = env.compat_evidence.expect("evidence present");
        assert_eq!(ev.env, "windows-x86_64");
        assert!(ev.capabilities.is_empty());
    }

    #[test]
    fn wire_error_serializes_screaming() {
        let e = WireError {
            code: WireErrorCode::WriteConflict,
            message: "x".into(),
            ls: None,
            retryable: false,
            hint: None,
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
            approx_tokens: None,
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert!(j.contains("\"ok\":true"));
        assert!(j.contains("\"data\":[1,2,3]"));
    }

    /// 7rh：approx_tokens wire 键名 `~tokens`；None 时字段缺席（向后兼容）；
    /// 旧 daemon 响应（无该字段）反序列化 → None。
    #[test]
    fn approx_tokens_wire_shape() {
        let with = ToolResponse::Ok {
            ok: true,
            data: serde_json::json!({"n": 1}),
            format: None,
            approx_tokens: Some(7),
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["~tokens"], 7);

        let without = ToolResponse::Ok {
            ok: true,
            data: serde_json::json!({"n": 1}),
            format: None,
            approx_tokens: None,
        };
        let v = serde_json::to_value(&without).unwrap();
        assert!(v.get("~tokens").is_none(), "None 时字段不得上 wire");

        let legacy: ToolResponse =
            serde_json::from_value(serde_json::json!({"ok": true, "data": {}})).unwrap();
        match legacy {
            ToolResponse::Ok { approx_tokens, .. } => assert_eq!(approx_tokens, None),
            ToolResponse::Err { .. } => panic!("应匹配 Ok 变体"),
        }
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
                hint: None,
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
        // wire message 必须带 ls-use 中央指引（bd serena-rust-4ux）——agent 消费面。
        assert!(
            w.message.contains("serena-cli ls-use"),
            "wire message 缺 ls-use 指引: {}",
            w.message
        );
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

    /// bd serena-rust-i4j：WRITE_CONFLICT 必须携带自检 hint（重读重试指引），
    /// 其余错误码 hint 为 None（序列化省略）—— agent 靠它区分「并发冲突」
    /// 与「参数错」，不再把 needle 失配误诊为 BAD_ARGS。
    #[test]
    fn wire_error_from_write_conflict_carries_reread_hint() {
        let e = supervisor::ToolError::WriteConflict {
            path: "src/x.rs".into(),
            reason: "needle not found in symbol body".into(),
        };
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::WriteConflict);
        let hint = w.hint.expect("WRITE_CONFLICT must carry hint");
        assert!(hint.contains("re-read"), "got: {hint}");
        assert!(hint.contains("retry"), "got: {hint}");

        let bad = wire_error_from_tool_error(&supervisor::ToolError::BadArgs {
            detail: "missing pattern".into(),
        });
        assert!(bad.hint.is_none(), "BAD_ARGS must not carry hint");
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

    /// bd serena-rust-iyz：未就绪竞态（Initializing 窗口）→ LS_NOT_READY +
    /// retryable，不再错层 INTERNAL（LsNotReady 死码回归）。
    #[test]
    fn wire_error_from_core_not_ready_maps_ls_not_ready() {
        let e = supervisor::ToolError::Core(supervisor::CoreErrorWire::NotReady {
            cause: "session not ready after gate open".into(),
        });
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::LsNotReady);
        assert!(w.retryable);
        assert!(
            w.message.contains("not ready"),
            "AI 判读关键词: {}",
            w.message
        );
    }

    /// bd serena-rust-iyz：Failed 态到来的请求走 LS_TERMINATED（现有码表内
    /// 合理映射：supervisor 懒重启，重试即触发）——锁语义防回归。
    #[test]
    fn wire_error_from_failed_session_maps_ls_terminated() {
        let e = supervisor::ToolError::Core(supervisor::CoreErrorWire::Terminated {
            ls: "ls".into(),
            cause: "session failed before request".into(),
        });
        let w = wire_error_from_tool_error(&e);
        assert_eq!(w.code, WireErrorCode::LsTerminated);
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

    /// bd serena-rust-4kh：Io / Framing 不再走 `{:?}` debug 兜底——码集不变
    /// （INTERNAL），但 message 人类可读且两条处置路径（重读 vs 重启）可分辨。
    #[test]
    fn io_and_framing_core_errors_get_distinct_readable_messages() {
        let io = supervisor::ToolError::Core(supervisor::CoreErrorWire::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        ));
        let w = wire_error_from_tool_error(&io);
        assert_eq!(w.code, WireErrorCode::Internal);
        assert!(!w.retryable);
        assert!(w.message.starts_with("io error: "), "got: {}", w.message);
        assert!(w.message.contains("gone"), "cause must survive: {}", w.message);

        let fr = supervisor::ToolError::Core(supervisor::CoreErrorWire::Framing {
            detail: "missing CONTENT_LENGTH".into(),
        });
        let w = wire_error_from_tool_error(&fr);
        assert_eq!(w.code, WireErrorCode::Internal);
        assert!(
            w.message.starts_with("framing error: ") && w.message.contains("restart"),
            "got: {}",
            w.message
        );
    }

    /// bd 7tk：空环省略键（hint 字段同款 skip 语义）；计数键恒在。
    #[test]
    fn status_obs_fields_skip_when_rings_empty() {
        let resp = StatusResponse {
            uptime_secs: 1,
            pid: 2,
            loaded_ls: vec![],
            draining: false,
            active_project: None,
            in_flight: 0,
            invocation_count: 0,
            recent_errors: vec![],
            recent_agents: vec![],
            daemon_version: "0.2.0".into(),
            binary_path: None,
        };
        let j = serde_json::to_value(&resp).unwrap();
        assert!(j.get("recent_errors").is_none(), "empty ring must omit key");
        assert!(j.get("recent_agents").is_none(), "empty ring must omit key");
        assert_eq!(j["in_flight"], 0);
        assert_eq!(j["invocation_count"], 0);
        assert!(j.get("binary_path").is_none(), "None 路径省略键");
        assert_eq!(j["daemon_version"], "0.2.0");
    }
    /// bd 7tk：旧 daemon 响应（只有 5 个旧字段）→ 新客户端 default 兜底反序列化。
    #[test]
    fn status_legacy_response_without_obs_fields_deserializes() {
        let resp: StatusResponse = serde_json::from_str(
            r#"{"uptime_secs":9,"pid":42,"loaded_ls":[],"draining":false}"#,
        )
        .unwrap();
        assert_eq!(resp.in_flight, 0);
        assert_eq!(resp.invocation_count, 0);
        assert!(resp.recent_errors.is_empty());
        assert!(resp.recent_agents.is_empty());
    }

    /// bd 7tk：带值时四字段全量序列化 + roundtrip（错误码走 SCREAMING 形态）。
    #[test]
    fn status_obs_fields_roundtrip() {
        let resp = StatusResponse {
            uptime_secs: 3,
            pid: 4,
            loaded_ls: vec![crate::dto::LoadedLs { lang: "rust".into(), sessions: 3 }],
            draining: false,
            active_project: Some("D:/proj".into()),
            in_flight: 2,
            invocation_count: 7,
            recent_errors: vec![StatusRecentError {
                ts_ms: 123,
                tool: "hover".into(),
                code: WireErrorCode::LsTimeout,
            }],
            recent_agents: vec!["orca-ab12".into()],
            daemon_version: "0.2.0".into(),
            binary_path: Some("D:/bin/serena-cli.exe".into()),
        };
        let j = serde_json::to_string(&resp).unwrap();
        assert!(j.contains("\"code\":\"LS_TIMEOUT\""), "got: {j}");
        let back: StatusResponse = serde_json::from_str(&j).unwrap();
        assert_eq!(back.in_flight, 2);
        assert_eq!(back.invocation_count, 7);
        assert_eq!(back.recent_errors.len(), 1);
        assert_eq!(back.recent_errors[0].code, WireErrorCode::LsTimeout);
        assert_eq!(back.recent_agents, vec!["orca-ab12".to_string()]);
    }
}
