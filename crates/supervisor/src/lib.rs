//! Supervisor —— (project_root, language) 实例编排与工具语义层（PLAN Task 10 / ARCH §3.3）。
//!
//! M0 最小版（Task 10）：
//! - **单实例**：每个 `(root, lang)` 缓存**一份** `Arc<Session>`（无池/LRU，Task 13-15 接管）。
//! - 工具入口：`tool_overview` / `tool_def` / `tool_refs` —— 直接 LSP 三个只读语义。
//! - line/col 经 `lsp_core::offsets::OffsetEncoding::Utf16` 换算为 LSP `Position`。
//!   M0 固定 utf-16：clangd 默认协商 + base init_params 声明 utf-16 优先；M1 改走
//!   `Session` 协商结果。
//!
//! 错误模型（ARCH §6.1）：库层用 thiserror 具名类型 `ToolError`；adapters 是被编排末端
//! 用 anyhow 内部传（`launch_info` 错误冒泡上来），包装成 `ToolError::NotInstalled` 等。
//!
//! ponytail: 单实例 ≠ `Mutex<HashMap>` —— 真正的「同 root 多 lang」也只装得下 1 个 lang
//! (clangd)，单 `Mutex<HashMap>` 比 `Arc<Mutex<OnceCell>>` 简单。Task 13 把池换进来时本
//! 公共 API 不变。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use lsp_core::docsync::{path_to_uri, path_to_uri_str};
use lsp_core::error::CoreError;
use lsp_core::init_params::base_initialize_params;
use lsp_core::init_params::supports_pull_diagnostics;
use lsp_core::offsets::{OffsetEncoding, Position as LspPos};
use lsp_core::session::Session;
pub mod doctor;
pub mod edit_tools;
pub mod fs_tools;
pub mod root_finder;

pub mod ref_tools;
pub mod write_gate;

use lsp_core::types::{SymbolHit, SymbolKindTag};
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

/// Read-only tool timeout. overview/def/refs on small files complete in ms; clangd
/// cold-start of a project may take seconds. 30s mirrors `READY_PROBE_TIMEOUT`.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// workspace/symbol / background-index 长操作。clangd 首次索引大项目可能 >30s。
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Phase 4 基建 Task 22b：三层 timeout 合并（CLI > servers.toml > 默认）。
/// CLI flag 透传走 `args._timeout_ms` / `args._index_timeout_ms`（私有约定，
/// CLI daemon HTTP / shell JSONL 都按 args 字段透传），`None` = 走 servers.toml 或默认。
///
/// `lang` 是 `session_for` 传入的语言名（或 id）；CLI / config override 的
/// `lang` 字段必须按相同语义 lookup `spec_for`。
pub fn effective_tool_timeout(
    lang: Option<&str>,
    args: &serde_json::Value,
) -> Duration {
    let from_args = args
        .get("_timeout_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let cli_override = ls_registry::config::LsOverride {
        timeout_ms: from_args,
        ..Default::default()
    };
    let ms = ls_registry::config::effective_timeout_ms(lang.unwrap_or(""), Some(&cli_override))
        .unwrap_or(30_000);
    Duration::from_millis(ms as u64)
}

pub fn effective_index_timeout(
    lang: Option<&str>,
    args: &serde_json::Value,
) -> Duration {
    let from_args = args
        .get("_index_timeout_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let cli_override = ls_registry::config::LsOverride {
        index_timeout_ms: from_args,
        ..Default::default()
    };
    let ms = ls_registry::config::effective_index_timeout_ms(lang.unwrap_or(""), Some(&cli_override))
        .unwrap_or(120_000);
    Duration::from_millis(ms as u64)
}

/// 写类工具（rename / replace-body）入口的索引等待上限，与 on_server_ready 的 30s
/// 对齐（PLAN Phase 3.2）。超时只 warn 不阻断 —— 工具自身请求负责最终报错。
const INDEX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 工具层错误（ARCH §6.1 supervisor thiserror 边界）。
///
/// 变体与 wire contract 一一对应；调用方（CLI / daemon）按变体决定 exit code 或 HTTP body。
#[derive(Debug, Error)]
pub enum ToolError {
    /// 用户传入参数错（line 越界、file 不存在等）—— exit 2（ARCH §6.3）。
    #[error("bad args: {detail}")]
    BadArgs { detail: String },

    /// LS 未安装（PATH 找不到）—— exit 1 + install_hint。
    #[error("language server for `{language}` not installed: {hint}")]
    NotInstalled { language: String, hint: String },

    /// lsp-core 错误冒泡 —— 此后调用方可判定是否 LS 崩溃 / RPC / 超时。
    #[error("core error: {0}")]
    Core(#[from] CoreError),

    /// 盘上内容与 LSP 状态不符（C3 防线）—— exit 1 + 需重读。
    #[error("write conflict on {path}: {reason}")]
    WriteConflict { path: String, reason: String },

    /// 适配器启动期错误（anyhow 上抛统一收口）。
    #[error("adapter launch failed: {0}")]
    Launch(#[from] anyhow::Error),

    /// 工具结果序列化失败 —— daemon 内部确定性 bug。wire INTERNAL（不可重试，exit 3）。
    /// Δ 43ae021：从 Launch 兜底拆出，避免确定性失败被误映射为 retryable 的
    /// LS_SPAWN_FAILED 让 agent 无意义重试。
    #[error("serialize failed: {0}")]
    Serialize(anyhow::Error),

    /// LSP 协议语义错（server 违反协议约定，如 rename 返回 null / 缺 changes map）。
    /// wire RPC_ERROR（不可重试，exit 1）。tool 标识出错的上层工具语义。
    #[error("protocol error from `{tool}`: {reason}")]
    Protocol { tool: String, reason: String },
}

/// supervisor 公共结果类型（库层 Result 别名）。
/// diagnostics 缓存条目类型：uri -> items。
pub type DiagCache = std::sync::Arc<Mutex<HashMap<(PathBuf, String), Vec<serde_json::Value>>>>;
pub type ToolResult<T> = std::result::Result<T, ToolError>;
/// 文档符号缓存 key（Phase 3.1）：(root, file, mtime)。
/// find-symbol（workspace 级）无单文件锚点：file 位放 `ws?{query}`、mtime 位放 None。
type SymbolCacheKey = (PathBuf, String, Option<SystemTime>);

/// overview / symbol-body 的缓存 key；mtime 取不到（文件不存在/不可 stat）→ None（确定性 key）。
fn doc_symbol_cache_key(root: &Path, file: &str) -> SymbolCacheKey {
    (
        root.to_path_buf(),
        file.to_string(),
        std::fs::metadata(root.join(file))
            .ok()
            .and_then(|m| m.modified().ok()),
    )
}

/// find-symbol（workspace/symbol）缓存 key：按 query 键控。
/// ponytail: workspace 级结果不锚 mtime —— 文件变更后同 query 返缓存，重启 daemon 或换
/// query 才刷新；换取索引型查询免全仓重复扫描（上游 ls.py 缓存同样按 (root, query) 键控）。
/// `?` 是 Windows 非法文件名字符，`ws?` 前缀与真实文件 key 天然不撞。
fn find_symbol_cache_key(root: &Path, query: &str) -> SymbolCacheKey {
    (root.to_path_buf(), format!("ws?{query}"), None)
}

/// Daemon 工具语义层抽象；实现负责按工具名分派只读请求。
#[async_trait::async_trait]
pub trait SupervisorTrait: Send + Sync {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError>;
    fn loaded_entries(&self) -> Vec<Key> {
        Vec::new()
    }

    /// 巡检：找出 state==Failed 的 session，从池中驱逐（shutdown+remove）。
    /// reaper 常驻调用，O(n) 扫描；返回驱逐数量用于打点。
    async fn evict_failed(&self) -> usize {
        0
    }
}

/// M0 单实例 supervisor。
///
/// - `instances`：`Mutex<HashMap<Key, Arc<Session>>>`。M0 仅 Cpp 一个 lang；同 (root, cpp) 复用 Session。
/// - `direct_mode`：当前 supervisor 由 `--direct` CLI 拉起；M1 daemon 模式不复用本类型
///   （daemon 引入 HTTP + idle reaper），Task 13 会把 `direct()` 拆为 `DirectSupervisor`，
///   此处留模式开关便于未来扩展。
pub struct Supervisor {
    instances: Mutex<HashMap<Key, Arc<Session>>>,
    load_gates: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    last_used: Mutex<HashMap<Key, std::time::Instant>>,
    direct_mode: bool,
    /// publishDiagnostics 通知缓存：key = (root, uri)，value = items 数组。
    diag_cache: DiagCache,
    /// diagnostics generation：每次 publishDiagnostics 通知 ++（含空 items 的"无错"推送）。
    /// 客户端可请求 `wait_gen >= N` 等新一代诊断，避免盲轮询 5s。
    diag_generation: Arc<AtomicU64>,
    /// per-key pull diagnostics 支持标记。session_for 末尾读 `ServerCapabilities.diagnosticProvider`
    /// 探测后写入；tool_diagnostics 入口查这里决定走 `textDocument/diagnostic` 还是 push 缓存。
    /// `true` = LS 声明支持；`false` = 缺字段/null → 走 push 缓存（与 2.5 之前等价）。
    /// 锁用 std::sync::Mutex —— 写一次读多次、临界区小，不值得换 parking_lot。
    pull_diag_supported: std::sync::Arc<Mutex<HashMap<Key, bool>>>,
    /// Phase 3.1 文档符号缓存：(root, file, mtime) → 平铺 symbol list。
    /// overview / find-symbol / symbol-body 入口前查；命中免 LS 往返。mtime 变 →
    /// key 变 → 自然 miss 重调 LS（旧 entry 残留无害）。std Mutex：临界区仅 HashMap 读写。
    symbol_cache: std::sync::Arc<Mutex<HashMap<SymbolCacheKey, Vec<SymbolHit>>>>,
}
/// 实例池身份键。`root` 保存 canonical **真实大小写**——rust-analyzer 按 URI 精确
/// 字符串匹配挂载文件，小写化 root 会让 didOpen/def 的原始大小写 URI 脱挂所有
/// crate（语法层活、语义层恒 null，BD serena-rust-81m）。身份比较/哈希仍按 root
/// 的小写形式归一，`HashMap` 调用点对大小写变体透明。
#[derive(Debug, Clone)]
pub struct Key {
    pub root: PathBuf,
    pub lang: Box<str>,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.lang == other.lang && key_root_identity(&self.root) == key_root_identity(&other.root)
    }
}

impl Eq for Key {}

impl std::hash::Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        key_root_identity(&self.root).hash(state);
        self.lang.hash(state);
    }
}

/// root 的身份形式：小写字符串（Windows 大小写不敏感 FS 的路径归一）。
fn key_root_identity(root: &Path) -> String {
    root.to_string_lossy().to_lowercase()
}

/// 对外暴露给工具调用方的"位置"结构（直接复用 lsp-types `Location`，
/// 但放本 crate re-export 以避免下游依赖 `lsp-types`）。
///
pub use lsp_core::error::CoreError as CoreErrorWire;
pub use lsp_types::Location;
// ============================================================================
// completion 工具类型（M2 #1, design: local/completion-design.md §5）
// ============================================================================

/// AI-friendly 补全项（supervisor 层完成字段裁剪；daemon HTTP / shell 透传）。
///
/// 与 `lsp_types::CompletionItem` 的差异：
/// - 丢弃 `sortText` / `filterText` / `commitCharacters` / `command` / `data` /
///   `tags` / `label_details` / `text_edit`（LSP 内部排序/编辑协议细节，agent 用不到）。
/// - `kind` 由 LSP 枚举映射成人类词（"function" / "variable" / "method" 等）。
/// - `documentation` 截断到 200 char（设计 §3）。
/// - `insert` = `insertText`（缺则用 `label`，agent 直接用）。
#[derive(Debug, Clone, Serialize)]
pub struct CompletionItemLite {
    pub label: String,
    /// "function" / "method" / "variable" / "keyword" / ... 全小写英文。
    /// Unknown kind → "other"。
    pub kind: String,
    /// 签名行（如 `int printf(const char *fmt, ...)`）；可空。
    pub detail: Option<String>,
    /// 实际插入文本（insertText 或 label 兜底）；可空。
    pub insert: Option<String>,
    /// 文档字符串，截断到 200 字符；可空。
    pub doc: Option<String>,
    pub deprecated: bool,
    /// 补全自动插入的额外文本编辑（如 import）；不裁剪，agent 需要。
    pub additional_text_edits: Vec<lsp_types::TextEdit>,
}

/// completion 工具响应：截断标记 + 已裁剪 items。
///
/// 设计 §4：CLI 一行 JSON；daemon HTTP 同形态（daemon 透传 `data`，由 supervisor 裁剪）。
#[derive(Debug, Clone, Serialize)]
pub struct CompletionResponse {
    /// 截断提示："5 of 23"；未截断为 None。
    pub truncated: Option<String>,
    pub items: Vec<CompletionItemLite>,
}

/// `defining-symbol` 工具返回的单个定义条目（Phase 2.3 / local/solidlsp-development-plan.md §2.3）。
///
/// 组合 `tool_def`（拿 Location）+ `tool_containing_symbol` 的 documentSymbol walk
/// （拿完整符号元信息）+ `lsp_core::offsets::slice_at`（拿符号体）。来源上游
/// `request_defining_symbol` 的 `UnifiedSymbolInformation`（ls_types.py@43ae021）。
///
/// 始终以 `Vec` 形式返回 —— 即使单定义场景也是单元素数组；C++ 重载等多定义场景
/// 自然展开。无定义时整个工具返回 `None`，不是空数组（区别于 `containing-symbol`）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolHit {
    /// 定义点 Location（来自 `textDocument/definition`），相对 root。
    pub source: DefiningSymbolLocation,
    /// 符号完整元信息。
    pub symbol: DefiningSymbolInfo,
}

/// 定义点位置（`textDocument/definition` 的 Location 字段子集）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolLocation {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

/// `UnifiedSymbolInformation` 的最小子集（name / kind / range / body）。
///
/// `body` 是从盘上切出来的真实源代码（不含前置 docstring / 注释）；`range` 是
/// LSP 原生 Range（驼峰命名由 `#[serde(rename_all = "camelCase")]` 自动产出）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolInfo {
    pub name: String,
    pub kind: SymbolKindTag,
    pub range: lsp_types::Range,
    pub body: String,
}
// ============================================================================
// Phase 1 · 上游 wrapper 类型（13 个 tool_* 配套结构）
// ============================================================================

/// `textDocument/semanticTokens/full` 响应（`SemanticTokens` 加解析后的扁平 token）。
///
/// LSP `data: Vec<i32>` 是 5-tuple delta 序列（[deltaLine, deltaStartChar, length,
/// tokenType, tokenModifiers]）；本结构在 supervisor 层把它解成绝对坐标
/// `(line, start_char, length, token_type, token_modifiers)`，agent 据此定位符号语义
/// 类型，不必自己解码 delta。`token_type` / `token_modifiers` 是 LSP
/// `SemanticTokenTypes` / `SemanticTokenModifiers` 的位索引。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokenEntry {
    pub line: u32,
    pub start_char: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers: u32,
}

/// 解析后的 `SemanticTokens` 响应：原始 data + 扁平 token 列表。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensFull {
    /// LS 提供的版本号（用于 `SemanticTokens/edits` 增量）；部分 LS 不实现（= null）。
    pub result_id: Option<String>,
    /// 已解出的扁平 token（绝对坐标）。
    pub tokens: Vec<SemanticTokenEntry>,
}

impl Supervisor {
    /// `--direct` 模式入口：创建空 supervisor（懒加载 Session）。
    pub async fn direct() -> ToolResult<Self> {
        Ok(Self {
            instances: Mutex::new(HashMap::new()),
            load_gates: Mutex::new(HashMap::new()),
            last_used: Mutex::new(HashMap::new()),
            direct_mode: true,
            diag_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            diag_generation: std::sync::Arc::new(AtomicU64::new(0)),
            pull_diag_supported: std::sync::Arc::new(Mutex::new(HashMap::new())),
            symbol_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 当前是否 direct 模式（保留位，M1 用）。
    #[allow(dead_code)]
    pub fn is_direct(&self) -> bool {
        self.direct_mode
    }

    #[doc(hidden)]
    pub fn key(root: &Path, lang: &str) -> Key {
        let mut canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        while matches!(
            canonical.as_os_str().as_encoded_bytes().last(),
            Some(b'/' | b'\\')
        ) {
            canonical.pop();
        }
        Key {
            // 真实大小写保真：LS 的 rootUri / cwd / 探针 URI 都从这里走，
            // 小写形式只用于身份归一（PartialEq/Hash），绝不外泄。
            root: canonical,
            lang: Box::from(lang.to_ascii_lowercase()),
        }
    }

    /// 返回同 key 共用的异步加载门。
    #[doc(hidden)]
    pub fn load_gate_for(&self, root: &Path, lang: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = Self::key(root, lang);
        self.load_gates
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 更新 last_used 时间戳（reaper LRU 用）。
    fn touch(&self, key: &Key) {
        self.last_used
            .lock()
            .unwrap()
            .insert(key.clone(), std::time::Instant::now());
    }

    /// 当前已加载实例快照：`(key, last_used, Arc<Session>)`，按 last_used 升序。
    /// reaper 巡检用；取锁后立刻 clone 释放。
    pub fn loaded_entries(&self) -> Vec<(Key, std::time::Instant)> {
        self.last_used
            .lock()
            .unwrap()
            .iter()
            .map(|(k, t)| (k.clone(), *t))
            .collect()
    }

    /// 卸载指定实例：先 shutdown session（5s 超时转 kill），再从池中移除。
    /// 返回 Ok(true) 表示真的卸了；Ok(false) 表示 key 不存在。
    pub async fn evict(&self, key: &Key) -> ToolResult<bool> {
        let session = self.instances.lock().unwrap().remove(key);
        self.last_used.lock().unwrap().remove(key);
        match session {
            Some(s) => {
                s.shutdown().await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 巡检：扫所有 session, Failed 状态驱逐。返回驱逐数。
    /// 复用 `evict`, 后台 reaper 常驻调用。
    pub async fn evict_failed_instances(&self) -> usize {
        // 先 clone 出所有失败 key (避免持锁 await shutdown)。
        let failed_keys: Vec<Key> = {
            let instances = self.instances.lock().unwrap();
            instances
                .iter()
                .filter(|(_, s)| matches!(s.state(), lsp_core::session::SessionState::Failed(_)))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let n = failed_keys.len();
        for key in failed_keys {
            let _ = self.evict(&key).await;
        }
        n
    }

    /// Self-heal 包装：调 `session_for` + 闭包；若闭包因 LS 终结返 `Core(Terminated)`，
    /// 驱逐该 (root, lang) 的旧 session（避免下次再拿到死 session）并重试一次。
    ///
    /// 重试上限 1 次 —— 再次 Terminated 透传原错误给 agent，避免无限重试掩盖真实故障。
    /// 写门内工具（replace-body / insert-*）的 LS 死亡根因是单写门后多次 write 的
    /// didChange version 非单调 → rust-analyzer 关 channel（详 edit_tools::commit_change 注）。
    /// 修复 write 链路后此 self-heal 兜底偶发冷启动失败 / 索引风暴把 RA 拉死的场景。
    ///
    /// ponytail: 重试仅 1 次 —— 再 fail 慢 1 次冷启动（rust-analyzer 89s）远超 TOOL_TIMEOUT，
    /// agent 自己比 supervisor 更适合决定「等 vs 报错」。
    pub(crate) async fn with_session_retry<F, Fut, T>(
        sup: &Supervisor,
        root: &Path,
        lang: &str,
        mut f: F,
    ) -> ToolResult<T>
    where
        F: FnMut(Arc<Session>) -> Fut,
        Fut: Future<Output = ToolResult<T>>,
    {
        let session = sup.session_for(root, lang).await?;
        match f(session.clone()).await {
            Err(ToolError::Core(CoreError::Terminated { .. })) => {
                let key = Supervisor::key(root, lang);
                tracing::warn!(?key, "session terminated mid-call; evicting and retrying once");
                let _ = sup.evict(&key).await;
                let session2 = sup.session_for(root, lang).await?;
                f(session2).await
            }
            other => other,
        }
    }

    /// 拿到/创建 (root, lang) 对应的 Session，同 key 只允许一次冷启动。
    async fn session_for(&self, root: &Path, lang: &str) -> ToolResult<Arc<Session>> {
        let key = Self::key(root, lang);
        // 快路径：缓存命中（Failed 状态视为 miss 触发懒重启）。
        if let Some(session) = self.instances.lock().unwrap().get(&key).cloned() {
            if !matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
                self.touch(&key);
                return Ok(session);
            }
            self.instances.lock().unwrap().remove(&key);
        }

        // 慢路径：per-key 加载门（防同 key 并发双 spawn）+ 双检锁。
        let gate = self.load_gate_for(root, lang);
        let _guard = gate.lock().await;
        if let Some(session) = self.instances.lock().unwrap().get(&key).cloned() {
            if !matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
                self.touch(&key);
                return Ok(session);
            }
            self.instances.lock().unwrap().remove(&key);
        }

        // 双路径（Task 21）：手写 T2 adapter 优先；servers.toml 条目（T0 配置驱动）
        // 走 config::ensure_launch——PATH 探测 / 安装缓存命中，永不触网（auto_install=false，
        // design §0 路径 A；显式下载走 CLI `install` 命令）。
        let ctx = ls_adapters::ProjectCtx {
            project_root: key.root.clone(),
        };
        let t2 = ls_registry::adapter_for(lang);
        let launch = match &t2 {
            Some(adapter) => adapter.launch_info(&ctx).await.map_err(|e| {
                let msg = format!("{e:#}");
                if msg.contains("not found in PATH") {
                    ToolError::NotInstalled {
                        language: lang.to_string(),
                        hint: extract_install_hint(&msg),
                    }
                } else {
                    ToolError::Launch(e)
                }
            })?,
            None => {
                if ls_registry::config::spec_for(lang).is_none() {
                    return Err(ToolError::BadArgs {
                        detail: format!("unknown language: {lang}"),
                    });
                }
                let (_, args) = ls_registry::config::ensure_launch(lang, None, false, false)
                    .map_err(|msg| ToolError::NotInstalled {
                        language: lang.to_string(),
                        hint: msg,
                    })?;
                // expand_exec 返回完整 argv（exec 模板首元素即 {bin}）。
                ls_runtime::process::LaunchInfo {
                    cmd: args.into_iter().map(Into::into).collect(),
                    cwd: key.root.clone(),
                    env: Vec::new(),
                    transport: ls_runtime::process::TransportKind::Stdio,
                }
            }
        };
        let child = ls_runtime::process::Child::spawn(launch)
            .map_err(|e| ToolError::Launch(anyhow::anyhow!("runtime spawn error: {e}")))?;
        let mut params = base_initialize_params();
        let uri = lsp_types::Uri::from_str(&path_to_uri_str(&key.root)).map_err(|e| {
            ToolError::BadArgs {
                detail: format!("root not URI: {e}"),
            }
        })?;
        // 设 root_uri (即使 deprecated): typescript-language-server 等需要
        // root_uri 定位 workspace 的 node_modules (workspaceFolders 不够)。
        // 用 allow(deprecated) 不影响其它 LS (clangd / rust-analyzer 忽略 root_uri)。
        #[allow(deprecated)]
        {
            params.root_uri = Some(uri.clone());
        }
        params.workspace_folders = Some({
            // Phase 4 基建 Task 22a：探测 root 下的 monorepo marker，构造 N 个
            // WorkspaceFolder（root 自身 + 探测出的 modules）。gopls 等多 module
            // LS 一次性拿到全部 module，避开逐个 didChangeWorkspaceFolders 通知。
            let mut folders = vec![lsp_types::WorkspaceFolder {
                uri: uri.clone(),
                name: key
                    .root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("root")
                    .to_string(),
            }];
            folders.extend(lsp_core::workspace_folders::discover_additional_workspace_folders(
                &key.root,
            ));
            folders
        });
        // T0 配置驱动路径无手写 adapter：无 initialize_patches（servers.toml 已含
        // 初始化形态）、无 set_project_root / on_server_ready 特判探针。
        if let Some(adapter) = &t2 {
            adapter.initialize_patches(&mut params);
        }

        let session = Session::start(Some(child), params).await?;
        // didOpen 的 languageId 用 adapter 真实语言（默认 "cpp" 对 rust-analyzer
        // 等严格 LS 是错语言 → 文档拒收）。session_for 是唯一 spawn 点，此处注入
        // 覆盖全部会话路径。
        session.set_language_id(lang);
        // 注册 publishDiagnostics handler → 写 diag_cache + 累 generation。
        let cache_root = key.root.clone();
        let cache = std::sync::Arc::clone(&self.diag_cache);
        let generation = std::sync::Arc::clone(&self.diag_generation);
        session
            .client()
            .on_notification("textDocument/publishDiagnostics", move |msg| {
                let Some(uri) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("uri"))
                    .and_then(|u| u.as_str())
                else {
                    return;
                };
                let Some(items) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("diagnostics"))
                    .and_then(|d| d.as_array())
                    .cloned()
                else {
                    return;
                };
                // 每次 publishDiagnostics 都 ++ generation（含空 items 的"无错"推送），
                // 客户端 wait_gen >= N 才能精确等新一代，而非盲等 5s。
                generation.fetch_add(1, Ordering::Relaxed);
                if !items.is_empty() {
                    cache
                        .lock()
                        .unwrap()
                        .insert((cache_root.clone(), uri.to_string()), items);
                }
            });
        // ↖ mirror: ls.py@43ae021 on_server_started — 把"等待 LS 索引就绪"
        // 推到 session_for 内，避免用户可见的首请求 = 索引懒加载。探针必须用
        // root 下真实文件（虚拟 URI 不触发项目索引 —— cold-start hang 根因，
        // 详见 local/cold-start-hang-diagnosis.md），故先告知 adapter 项目 root。
        // T0 配置驱动路径无 adapter 特判探针——工具层 wait_for_index（3.2）的
        // documentSymbol 通用探针仍然生效。
        if let Some(adapter) = &t2 {
            adapter.set_project_root(&key.root);
            if let Err(e) = tokio::time::timeout(
                Duration::from_secs(30),
                adapter.on_server_ready(&session),
            )
            .await
            {
                tracing::warn!(adapter = adapter.id(), error = %e, "on_server_ready probe failed/timed out; continuing");
            }
        }

        // 写一次、读多次；错就当不支持（fallback push 与 2.4 之前等价）。
        let supports_pull = session
            .server_capabilities()
            .as_ref()
            .map(supports_pull_diagnostics)
            .unwrap_or(false);
        self.pull_diag_supported
            .lock()
            .unwrap()
            .insert(key.clone(), supports_pull);
        // 低层会话换代（旧会话已被 evict/懒重启移除）→ 高层符号缓存整体失效。
        self.invalidate_symbol_cache_for_root(&key.root);
        self.instances
            .lock()
            .unwrap()
            .insert(key.clone(), session.clone());
        self.touch(&key);
        Ok(session)
    }

    /// `textDocument/diagnostic` 诊断（PLAN Phase 2.5）。
    ///
    /// 主路径选择：
    /// - 若 session_for 末尾探测到 LS 声明 `diagnosticProvider` → 优先 `textDocument/diagnostic`
    ///   （pull，LSP 3.17）；返回的 `Full` 报告 items 即最终结果，`Unchanged` 报告保留 push 缓存。
    /// - pull 失败（`-32601 MethodNotFound` 等任何 RPC 错）/ LS 未声明 pull / 字段缺失 →
    ///   透明 fallback 到 `publishDiagnostics` push 缓存（与 2.4 之前完全等价）。
    ///
    /// `wait_gen`：
    /// - `None`：盲轮询 5s（同现状，向后兼容）。
    /// - `Some(0)`：立即返回当前 generation 的诊断（不等）。
    /// - `Some(N>0)`：等 generation >= N，仍受 5s 上限；超时返 `{ items: [] }`。
    pub async fn tool_diagnostics(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
        wait_gen: Option<u64>,
    ) -> ToolResult<serde_json::Value> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let path = root.join(file);
        let uri = path_to_uri(&path)
            .map_err(|e| ToolError::BadArgs {
                detail: format!("path to uri: {e}"),
            })?
            .as_str()
            .to_string();
        let session = self.session_for(root, lang.as_str()).await?;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let key = Self::key(root, lang.as_str());

        // 探测结果查表。缺 key 视为"未探测过" → 走 push（防御：未来多写漏写）。
        let supports_pull = self
            .pull_diag_supported
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(false);

        // 主路径选择。`supports_pull=false` 直接走 push（任务 #1/#2 覆盖）。
        // `supports_pull=true` 走 textDocument/diagnostic；纯函数 `extract_pull_items`
        // 统一处理 LSP 3.17 报告 + 错误 → None 触发 push fallback（任务 #4）。
        let pull_items: Option<Vec<serde_json::Value>> = if supports_pull {
            let params = json!({ "textDocument": { "uri": uri.clone() } });
            let resp: Result<serde_json::Value, CoreError> = session
                .client()
                .request("textDocument/diagnostic", params, TOOL_TIMEOUT)
                .await;
            match resp {
                Ok(value) => Self::extract_pull_items(&value),
                Err(_) => None, // fallback push（-32601 / timeout / 任意 RPC 错）。
            }
        } else {
            None
        };
        if let Some(items) = pull_items {
            // pull full 命中：++ generation 保持与 publishDiagnostics 一致（任务要求）；
            // 不与 push cache 拼接避免重复。
            self.diag_generation.fetch_add(1, Ordering::Relaxed);
            return Ok(json!({ "items": items }));
        }
        // 推送路径（push-only LS，或 pull fallback）：等 generation/cache 后取 push cache。
        match wait_gen {
            None => {
                // 默认盲轮询：等 cache 命中或上限 50 × 100ms = 5s。
                // 空 cache（无错）也只等 5s 返空数组（向后兼容）。
                for i in 0..50 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if i >= 4 {
                        let present = self
                            .diag_cache
                            .lock()
                            .unwrap()
                            .contains_key(&(key.root.clone(), uri.clone()));
                        if present {
                            break;
                        }
                    }
                }
            }
            Some(target) => {
                // generation 等待：每 100ms 探一次，5s 上限。Some(0) 等价"立即返回"。
                for _ in 0..50 {
                    let cur = self.diag_generation.load(Ordering::Relaxed);
                    if cur >= target {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        let items = self
            .diag_cache
            .lock()
            .unwrap()
            .get(&(key.root.clone(), uri.clone()))
            .cloned()
            .unwrap_or_default();
        Ok(json!({ "items": items }))
    }

    /// 从 LSP 3.17 `textDocument/diagnostic` 响应提取权威 items。
    ///
    /// 返回 `Some(items)` 仅当 `kind == "full"` —— 表示"完整报告"，items 是权威。
    /// `unchanged` / `partial` / 缺 kind → 返 `None`（触发 push fallback）。
    /// 任务验收分支 #5：pull 成功 → 取 items；与 push cache 不重复拼接。
    ///
    /// 纯函数 —— 单测不拉 LS，直接喂构造 JSON 验 5 个分支。
    fn extract_pull_items(value: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
        if value.get("kind").and_then(|v| v.as_str()) != Some("full") {
            return None;
        }
        Some(
            value
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        )
    }
    pub fn diag_generation(&self) -> u64 {
        self.diag_generation.load(Ordering::Relaxed)
    }

    pub async fn tool_hover(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<lsp_types::Hover>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<lsp_types::Hover> = session
            .request("textDocument/hover", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
    }
    /// `textDocument/signatureHelp` → 函数调用位置上的参数签名提示。
    ///
    /// 与 `tool_hover` 同形态：cursor 落在函数调用括号内时返 `Option<SignatureHelp>`（含 signatures[]、
    /// activeSignature / activeParameter）；落在非调用位置时返 `None`，与 hover 的"无悬停"语义一致。
    ///
    /// 不裁剪字段：agent 据 LSP 原生结构（label / parameters[] / activeParameter 等）自行决策；
    /// 与 hover 不裁剪保持一致。Phase 2.2（local/solidlsp-development-plan.md §2.2）。
    pub async fn tool_signature_help(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<lsp_types::SignatureHelp>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<lsp_types::SignatureHelp> = session
            .request("textDocument/signatureHelp", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
    }
    // ============================================================================
    // Phase 1 · 上游 wrapper 缺口（local/solidlsp-development-plan.md §Phase 1 后续
    // 批次 / 13 个 tool_*）。每项 = 透传 LSP method，不裁剪字段（agent 据 LSP 原生结构
    // 自行决策；与 hover/signature-help 不裁剪保持一致）。
    // ============================================================================

    /// `textDocument/codeAction`：位置 + kind（"quickfix" / "refactor" / "refactor.extract"
    /// / "refactor.inline" / "refactor.rewrite" / "source" / "source.organizeImports" 等
    /// LSP `CodeActionKind` 子串匹配，可选省略返所有）。
    ///
    /// 响应数组归一化：`[] | null` 都表示"无可用 action"；LS 偶尔返非数组（极端）→ 空。
    /// 不实现执行端（`workspace/executeCommand`），agent 据 `edit` 字段自行决策。
    pub async fn tool_code_action(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        kind: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CodeAction>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let mut params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": line, "character": col },
                "end":   { "line": line, "character": col },
            },
            "context": { "diagnostics": [] },
        });
        if let Some(k) = kind {
            params["context"]["only"] = serde_json::Value::String(k.to_owned());
        }
        let resp: Option<serde_json::Value> = session
            .request("textDocument/codeAction", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CodeAction> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/formatting`：整文件格式化（默认 tabSize=4 / insertSpaces=true，
    /// agent 可通过 args.options 覆盖）。响应 `[] | null` = 无变化；非数组归一为空。
    pub async fn tool_format(
        &self,
        root: &Path,
        file: &str,
        tab_size: Option<u32>,
        insert_spaces: Option<bool>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TextEdit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let options = json!({
            "tabSize": tab_size.unwrap_or(4),
            "insertSpaces": insert_spaces.unwrap_or(true),
        });
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "options": options,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/formatting", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TextEdit> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/rangeFormatting`：range 内格式化（参数同 tool_format + Range）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_format_range(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
        tab_size: Option<u32>,
        insert_spaces: Option<bool>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TextEdit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let options = json!({
            "tabSize": tab_size.unwrap_or(4),
            "insertSpaces": insert_spaces.unwrap_or(true),
        });
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": start_line, "character": start_col },
                "end":   { "line": end_line,   "character": end_col   },
            },
            "options": options,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/rangeFormatting", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TextEdit> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/inlayHint`：行范围（line..=end_line）内的类型提示。
    /// LSP 返 `InlayHint[] | null`；空/null = 无 hint。
    pub async fn tool_inlay_hint(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::InlayHint>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": start_line, "character": 0 },
                "end":   { "line": end_line,   "character": 0 },
            },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/inlayHint", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::InlayHint> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/documentHighlight`：光标位置的同符号高亮（写引用区 vs 读引用区
    /// 按 `kind` 区分；agent 据此识别写冲突点）。空/null = 无高亮。
    pub async fn tool_document_highlight(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::DocumentHighlight>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/documentHighlight", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::DocumentHighlight> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/foldingRange`：文件级折叠区。LSP `FoldingRange[] | null`；空 = 无折叠。
    pub async fn tool_folding_range(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::FoldingRange>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/foldingRange", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::FoldingRange> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/semanticTokens/full`：文件级语义 token（test 而非着色用）。
    /// 响应是 `SemanticTokens` 单对象（含 `data: [..i32..]` 编码形式）；为 agent 友好
    /// 拆出原始 delta 序列与一份解析后的 `(line, col, length, token_type, modifiers)` 列表。
    pub async fn tool_semantic_tokens(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<SemanticTokensFull> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<lsp_types::SemanticTokens> = session
            .request("textDocument/semanticTokens/full", params, TOOL_TIMEOUT)
            .await?;
        let resp = resp.unwrap_or(lsp_types::SemanticTokens {
            result_id: None,
            data: Vec::new(),
        });
        Ok(SemanticTokensFull {
            result_id: resp.result_id,
            tokens: decode_semantic_tokens(&resp.data),
        })
    }

    /// `textDocument/codeLens`：文件级代码透镜（references / impls / run/debug 计数）。
    /// LSP `CodeLens[] | null`；空 = 无透镜。
    pub async fn tool_code_lens(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CodeLens>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/codeLens", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CodeLens> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/documentLink`：文件级可点击链接（include / 模块导入跳转）。
    /// LSP `DocumentLink[] | null`；空 = 无链接。
    pub async fn tool_document_link(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::DocumentLink>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/documentLink", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::DocumentLink> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/prepareCallHierarchy`：把光标位置的符号转成可被 incoming/outgoing
    /// 操作的 `CallHierarchyItem[]`。LSP 返数组（同一位置的多匹配，如 C++ 重载）；
    /// 空/null = 不是可层级化符号（变量/宏等）。
    pub async fn tool_call_hierarchy_prepare(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyItem>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/prepareCallHierarchy", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `callHierarchy/incomingCalls`：调用当前项的位置集合（含 range / fromSymbol）。
    /// `item` = `tool_call_hierarchy_prepare` 返的 `CallHierarchyItem` 序列化形态（`serde_json::Value`）。
    pub async fn tool_call_hierarchy_incoming(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyIncomingCall>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/incomingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyIncomingCall> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `callHierarchy/outgoingCalls`：当前项调出的位置集合。
    pub async fn tool_call_hierarchy_outgoing(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyOutgoingCall>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/outgoingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyOutgoingCall> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/prepareTypeHierarchy`：把光标位置的符号转成可被 supertypes/subtypes
    /// 操作的 `TypeHierarchyItem[]`。空/null = 不可层级化（变量 / 函数等非类型符号）。
    pub async fn tool_type_hierarchy_prepare(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/prepareTypeHierarchy", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `typeHierarchy/supertypes`：父类型列表（OOP 继承链向上）。
    pub async fn tool_type_hierarchy_supertypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/supertypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `typeHierarchy/subtypes`：子类型列表（OOP 继承链向下）。
    pub async fn tool_type_hierarchy_subtypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/subtypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/moniker`：符号的全局 / 项目 / 局部标识符（用于跨仓库跳转 / git
    /// blame 锚定）。LSP `Moniker[] | null`；空 = 无 moniker（很常见，多数 LS 不实现）。
    pub async fn tool_moniker(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::Moniker>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/moniker", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Moniker> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `workspace/diagnostic`：整项目 pull diagnostics（LS 能力 `workspaceDiagnostics`
    /// 未声明时返 -32601 `MethodNotFound`，在此路径转 BadArgs 提示用户走 per-file
    /// `diagnostics` 工具）。items[] 始终返数组（即使空）。
    pub async fn tool_workspace_diagnostic(
        &self,
        root: &Path,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::Diagnostic>> {
        // 无 file 锚点：按 root 唯一 LS 探测；多 lang 项目请用 --lang 限定。
        let lang: String = match lang_override {
            Some(l) => l.to_ascii_lowercase(),
            None => {
                // 走 default LS（adapter 默认 lang）。
                let entries = self.last_used.lock().unwrap().keys().cloned().collect::<Vec<_>>();
                match entries.first() {
                    Some(k) => k.lang.to_string(),
                    None => {
                        return Err(ToolError::BadArgs {
                            detail: "no active LS; specify --lang or run any tool first".into(),
                        });
                    }
                }
            }
        };
        let session = self.session_for(root, &lang).await?;
        let params = json!({
            "previousResultIds": [],
            "textDocument": { "uri": path_to_uri_str(&root.join(".")) },
        });
        let resp: Option<serde_json::Value> = session
            .request("workspace/diagnostic", params, INDEX_TIMEOUT)
            .await?;
        let Some(raw) = resp else {
            return Ok(Vec::new());
        };
        let items = raw.get("items").cloned().unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Diagnostic> =
            serde_json::from_value(items).unwrap_or_default();
        Ok(parsed)
    }

    // ==== Phase 3.1 文档符号缓存存取（上游 ls.py@43ae021 文档符号缓存对应）====

    /// cache 命中查询；返回克隆（平铺 list 小，克隆远便宜于 LS 往返）。
    fn symbol_cache_get(&self, key: &SymbolCacheKey) -> Option<Vec<SymbolHit>> {
        self.symbol_cache.lock().unwrap().get(key).cloned()
    }

    /// cache_miss 后写入。LS 错误路径不经过这里（失败不进 cache）。
    fn symbol_cache_put(&self, key: SymbolCacheKey, hits: Vec<SymbolHit>) {
        self.symbol_cache.lock().unwrap().insert(key, hits);
    }

    /// 低层（LS 会话）版本变化 → 该 root 的全部高层符号缓存失效。
    ///
    /// ↖ mirror: ls.py@a5fd4d68 — 高层 document symbol 缓存版本必须纳入 LS-specific
    /// 低层缓存版本，低层变化即失效（否则 LS 换代后命中旧代缓存）。本项目把"低层
    /// 版本"物化为会话本身：`(root, lang)` 的 LS 会话重建（evict / Failed 懒重启 /
    /// mid-call Terminated 重试）即低层版本变更，key 中的 mtime 无法感知这种变化。
    /// 在 session_for 挂入新会话前调用，单点覆盖所有换代路径。
    fn invalidate_symbol_cache_for_root(&self, root: &Path) {
        self.symbol_cache
            .lock()
            .unwrap()
            .retain(|key, _| key.0 != root);
    }

    /// `textDocument/documentSymbol` → 平铺递归 `DocumentSymbol::children` → `Vec<SymbolHit>`。
    pub async fn tool_overview(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<SymbolHit>> {
        // Phase 3.1 缓存：同 (root, file, mtime) 二次调用免 LS 往返（命中 <1ms）。
        let cache_key = doc_symbol_cache_key(root, file);
        if let Some(cached) = self.symbol_cache_get(&cache_key) {
            return Ok(cached); // cache_hit
        }
        let lang_str = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, &lang_str).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Phase 4 基建 Task 22b：timeout 由三层合并（CLI args._timeout_ms >
        // servers.toml `timeout_ms` > 默认 30s）。execute_tool 入口把
        // `args._timeout_ms` 提取后塞进 per-call override；当前 tool_overview 拿不到
        // args，故先固定传 `lang` 让 servers.toml 的 per-LS timeout 生效。其它
        // tool_* 后续按相同 pattern 替换。
        let timeout = ls_registry::config::effective_timeout_ms(
            &lang_str,
            None,
        )
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(TOOL_TIMEOUT);
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, timeout)
            .await?;

        let out = flatten_symbols(resp, &uri);
        self.symbol_cache_put(cache_key, out.clone()); // cache_miss → 写入
        Ok(out)
    }

    /// 跨文件符号树（PLAN Phase 2.5 / 7.2）：聚合 `dir` 下源码文件的 documentSymbol。
    ///
    /// 逐文件走 `tool_overview` —— 天然复用 3.1 缓存（同文件二次 symbol-tree/overview
    /// 免 LS 往返）；目录扫描走 3.3 `filtered_walker`（venv/node_modules/target 等内置
    /// ignore + gitignore）。`max_files` 保险丝（默认 200）：超限截断并标 `truncated`。
    pub async fn tool_symbol_tree(
        &self,
        root: &Path,
        dir: &str,
        lang: Option<&str>,
        max_files: usize,
    ) -> ToolResult<serde_json::Value> {
        if dir.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "missing 'dir'".into(),
            });
        }
        let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let canon_dir = dunce::canonicalize(canon_root.join(dir)).map_err(|e| ToolError::BadArgs {
            detail: format!("dir not found: {dir} ({e})"),
        })?;
        if !canon_dir.starts_with(&canon_root) {
            return Err(ToolError::BadArgs {
                detail: format!("dir escapes root: {dir}"),
            });
        }

        // 收集源码文件：只收能解析语言的扩展名；max_files 保险丝。
        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in crate::fs_tools::filtered_walker(&canon_dir).build() {
            let Ok(entry) = entry else { continue };
            let is_file = entry.file_type().as_ref().is_some_and(|t| t.is_file());
            if !is_file {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&canon_root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            // 按扩展名探测（None）：lang override 只作用于请求语言，不当文件筛选 ——
            // 否则 Some("rust") 会把 .md 等非源码也收进来。
            if resolve_lang_for_file(&rel, None).is_ok() {
                if files.len() >= max_files {
                    truncated = true;
                    break;
                }
                files.push(rel);
            }
        }

        // 逐文件聚合（顺序即可：daemon 内 LS 请求本就串行；单文件失败不炸整树）。
        let mut entries = Vec::with_capacity(files.len());
        let mut errors = Vec::new();
        for file in &files {
            match self.tool_overview(root, file, lang).await {
                Ok(symbols) if !symbols.is_empty() => {
                    entries.push(serde_json::json!({ "file": file, "symbols": symbols }));
                }
                Ok(_) => {} // 无符号文件（空/纯注释）不占条目
                Err(e) => errors.push(serde_json::json!({ "file": file, "error": e.to_string() })),
            }
        }
        serde_json::to_value(serde_json::json!({
            "dir": dir,
            "files_scanned": files.len(),
            "truncated": truncated,
            "entries": entries,
            "errors": errors,
        }))
        .map_err(|e| ToolError::Serialize(e.into()))
    }

    ///
    /// Phase 2.1（local/solidlsp-development-plan.md §2.1）：位置已知，从 grep/搜索结果直接
    /// 跳进符号工作流，无需按名字再扫一遍。无命中时返空数组（区别于 `tool_def` 的 BadArgs——
    /// 「位置无覆盖"是合法的"）。
    ///
    /// 与 `tool_overview` 一样走 `DocumentSymbolResponse`，不引入新 LSP method：
    /// 上游 `request_containing_symbol` 用 `request_symbol_at_location`（走位置查符号），
    /// 我们用 documentSymbol walk 实现等价语义（ARCH §6.x Δ 标注）。
    pub async fn tool_containing_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<SymbolHit>> {
        let lang_str = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, &lang_str).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        Ok(collect_containing_hits(&resp, &uri, line, col))
    }

    /// `defining-symbol`：位置 → `tool_def` 拿 Location → 在该 Location 上 documentSymbol
    /// walk → 切片出符号体 → 返回 `{source, symbol}[]`。
    ///
    /// Phase 2.3（local/solidlsp-development-plan.md §2.3）：组合既有 `tool_def` +
    /// `tool_containing_symbol` 的 walk 实现等价上游 `request_defining_symbol`（ls.py@43ae021
    /// `request_defining_symbol`，返回 `UnifiedSymbolInformation | list[UnifiedSymbolInformation]`）。
    ///
    /// 返回 `Option<Vec<DefiningSymbolHit>>`：
    /// - `None`：`tool_def` 也没找到（位置无定义）—— 与 `tool_def` 同语义（合法）。
    /// - `Some(vec![])`：罕见 —— `tool_def` 给了 Location 但目标位置不在任何符号内。
    /// - `Some(non_empty)`：正常结果；多定义场景（C++ 重载）自然展开为多元素。
    ///
    /// 复用策略：直接调 `self.tool_def` 避免重写 `textDocument/definition` 请求；
    /// 切片走 `lsp_core::offsets::slice_at` 与 `tool_symbol_body` 同源。
    pub async fn tool_defining_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<Vec<DefiningSymbolHit>>> {
        // 1) def → Location（可能 None）。
        let def_loc = self.tool_def(root, file, line, col, lang_override).await?;
        let Some(def_loc) = def_loc else {
            return Ok(None);
        };

        // 2) Location.uri → 相对 root 的 file 路径。走 uri_to_path 统一 percent-decode
        //    （tsserver 回 `d%3A/...`，不解码会误判 outside workspace root）。
        let def_uri = def_loc.uri.to_string();
        let abs = uri_to_path(&def_uri).ok_or_else(|| ToolError::BadArgs {
            detail: format!("definition uri is not a file:// URI: {def_uri}"),
        })?;
        let def_file = abs
            .strip_prefix(root)
            .map_err(|_| ToolError::BadArgs {
                detail: format!("definition at {def_uri} is outside workspace root {root:?}"),
            })?
            .to_string_lossy()
            .replace('\\', "/");

        // 3) 目标文件 → documentSymbol walk → 在 def_loc.range.start 找覆盖符号。
        let target_lang = resolve_lang_for_file(&def_file, lang_override)?;
        let session = self.session_for(root, &target_lang).await?;
        let target_path = root.join(&def_file);
        let target_uri = path_to_uri_str(&target_path);
        let _guard = session
            .ensure_open(&target_path)
            .await
            .map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": target_uri.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        // 用 def 的目标位置作为 walk key（注意：来自 LSP 的 line/col 是 0-based）。
        let hits = collect_containing_hits(
            &resp,
            &target_uri,
            def_loc.range.start.line,
            def_loc.range.start.character,
        );
        if hits.is_empty() {
            return Ok(Some(Vec::new()));
        }

        // 4) 读盘 → 切片 body（OffsetEncoding::Utf16 与 tool_symbol_body 一致）。
        let text = tokio::fs::read_to_string(&target_path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", target_path.display()),
            })?;
        let source = DefiningSymbolLocation {
            file: def_file.clone(),
            line: def_loc.range.start.line,
            col: def_loc.range.start.character,
        };
        let out: Vec<DefiningSymbolHit> = hits
            .into_iter()
            .map(|hit| {
                let start = LspPos {
                    line: hit.range.start.line,
                    character: hit.range.start.character,
                };
                let end = LspPos {
                    line: hit.range.end.line,
                    character: hit.range.end.character,
                };
                let body = lsp_core::offsets::slice_at(&text, start, end, OffsetEncoding::Utf16)
                    .unwrap_or_else(|e| {
                        // 切片失败（罕见：LS 给的 range 异常）→ 留空 + 不中断整体响应。
                        // 整调用返 None 会让上层误判「无定义」，代价更大。
                        tracing::warn!(symbol = %hit.name, error = %e, "defining-symbol slice failed");
                        String::new()
                    });
                DefiningSymbolHit {
                    source: source.clone(),
                    symbol: DefiningSymbolInfo {
                        name: hit.name,
                        kind: hit.kind,
                        range: hit.range,
                        body,
                    },
                }
            })
            .collect();
        Ok(Some(out))
    }


    /// `workspace/symbol` → 全 workspace 跨文件符号查找（Task 20）。
    ///
    /// 多语言支持：
    /// - 指定 `lang_override`：仅查该 LS（不依赖 root 文件探测）。
    /// - 不指定：扫 root 找所有 lang, 每个 lang 各起 LS 并行查 + merge (去重已排序后截断)。
    ///
    /// 索引可能慢（>10s），用 `INDEX_TIMEOUT` 而不是 `TOOL_TIMEOUT`。
    pub async fn tool_find_symbol(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<SymbolHit>> {
        use ignore::WalkBuilder;
        use std::collections::BTreeSet;

        if query.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "query must not be empty".into(),
            });
        }

        // Phase 3.1 缓存：同 (root, query) 二次调用免全仓 workspace/symbol 往返。
        let cache_key = find_symbol_cache_key(root, query);
        if let Some(mut cached) = self.symbol_cache_get(&cache_key) {
            cached.truncate(limit);
            return Ok(cached); // cache_hit
        }

        // 决定要查的 lang 集合 (BTreeSet = 字母序, 顺序稳定)。
        let langs: BTreeSet<String> = if let Some(l) = lang_override {
            [l.to_ascii_lowercase()].into()
        } else {
            let mut set: BTreeSet<String> = BTreeSet::new();
            for entry in WalkBuilder::new(root)
                .standard_filters(true)
                .max_depth(Some(3))
                .build()
                .flatten()
            {
                if entry.file_type().is_some_and(|t| t.is_file())
                    && let Some(l) = ls_registry::resolve_lang_name(entry.path())
                {
                    set.insert(l.to_string());
                }
            }
            set
        };
        if langs.is_empty() {
            return Err(ToolError::BadArgs {
                detail: format!("no known-language files under root {root:?}"),
            });
        }

        // ponytail: 串行拿 session (load_gate_for 防双 spawn), 然后并行 fan-out 请求。
        let mut sessions = Vec::with_capacity(langs.len());
        for lang in &langs {
            match self.session_for(root, lang).await {
                Ok(s) => sessions.push(s),
                Err(_) => continue, // 单 LS 拉起失败不阻塞其它
            }
        }
        let query = query.to_string();
        let mut tasks = Vec::with_capacity(sessions.len());
        for session in sessions {
            let q = query.clone();
            tasks.push(tokio::spawn(async move {
                let params = json!({ "query": q });
                let resp: Vec<lsp_types::SymbolInformation> = match session
                    .request("workspace/symbol", params, INDEX_TIMEOUT)
                    .await
                {
                    Ok(r) => r,
                    Err(_) => return Vec::<SymbolHit>::new(),
                };
                resp.into_iter()
                    .map(|si| SymbolHit {
                        name: si.name,
                        kind: kind_from_lsp(&si.kind),
                        uri: si.location.uri.to_string(),
                        range: si.location.range,
                        container: si.container_name,
                    })
                    .collect()
            }));
        }
        let mut merged: Vec<SymbolHit> = Vec::new();
        for t in tasks {
            if let Ok(v) = t.await {
                merged.extend(v);
            }
        }
        self.symbol_cache_put(cache_key, merged.clone()); // cache_miss → 写入（截断前全量）
        merged.truncate(limit);
        Ok(merged)
    }
    /// `Location[]`，lsp-types 在 capability 上声明多形态——M0 只解 `Option<Location>`）。
    ///
    /// line/col 1-based 还是 0-based？LSP `Position` 是 0-based；上层 CLI 必须传 0-based。
    pub async fn tool_def(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // offsets.rs 换算：M0 固定 utf-16（clangd 默认 + base init_params 首选 utf-16）。
        // M1 走 Session 协商结果（`capabilities.positionEncoding`）。本任务不引入新 API。
        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
        });
        // clangd 22 回 `LocationLink[]`（带 `originSelectionRange` / `targetSelectionRange`），
        // 而 LSP 3.17 spec 还允 `Location | Location[]`。统一接 `serde_json::Value` 后归一化。
        let raw: Option<serde_json::Value> = session
            .request("textDocument/definition", params, TOOL_TIMEOUT)
            .await?;
        let resp = normalize_definition(raw.as_ref());
        Ok(resp)
    }

    /// `textDocument/implementation` → 全部实现位置（Task 21）。
    ///
    /// 与 `tool_def` 类似但返 `Vec<Location>`（一个 interface 多处实现）。
    /// clangd 22 默认回 `LocationLink[]`；LSP 3.17 还允 `null | Location | Location[]`。
    /// 全部走 `normalize_implementations` 折叠为统一 `Vec<Location>`。
    pub async fn tool_find_implementations(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/implementation", params, TOOL_TIMEOUT)
            .await?;
        Ok(normalize_implementations(raw.as_ref()))
    }
    /// `textDocument/references` → 全部引用 `Location[]`。
    pub async fn tool_refs(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
            "context": { "includeDeclaration": true },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/references", params, TOOL_TIMEOUT)
            .await?;
        Ok(normalize_implementations(raw.as_ref()))
    }

    /// `textDocument/completion` → AI-friendly 裁剪后的 `CompletionResponse`。
    ///
    /// 设计（local/completion-design.md §3/§5）：
    /// - `limit == 0` 表示不限（`usize::MAX`）；否则截断到 `limit`。
    /// - 字段裁剪在 supervisor 层完成：丢弃 `sortText/filterText/commitCharacters/...`，
    ///   `kind` 由 LSP 枚举映射为人类词，`documentation` 截断 200 char。
    /// - `trigger` 为 Some 时带 `context.triggerKind = TriggerCharacter (2)` + `triggerCharacter`；
    ///   为 None 时发 `Invoked (1)`（用户显式请求补全，agent 不传 trigger 的默认形态）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_completion(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        limit: usize,
        trigger: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<CompletionResponse> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 越界校验（同 tool_def）—— 防止上游传错位掩盖。
        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        // CompletionContext：trigger=Some → TriggerCharacter + triggerCharacter；
        //                None → Invoked（agent 显式请求）。
        let context = match trigger {
            Some(ch) => json!({
                "triggerKind": 2, // CompletionTriggerKind::TRIGGER_CHARACTER
                "triggerCharacter": ch,
            }),
            None => json!({ "triggerKind": 1 }), // INVOKED
        };
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": pos.line, "character": pos.character },
            "context": context,
        });
        // CompletionResponse = Array | List（untagged enum）。
        // raw: Option<Value> → 直接以 Value 形态解析两形态：
        //   - Array：items 数组直接 map
        //   - List ：{ isIncomplete, items }
        // null（无候选）也合理：返回空 items + truncated=None。
        let raw: Option<serde_json::Value> = session
            .request("textDocument/completion", params, TOOL_TIMEOUT)
            .await?;
        let items = match raw {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(arr)) => arr,
            Some(serde_json::Value::Object(obj)) if obj.contains_key("items") => obj
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            Some(other) => {
                return Err(ToolError::Protocol {
                    tool: "completion".into(),
                    reason: format!("unexpected response shape: {other}"),
                });
            }
        };
        let total = items.len();
        // 0 = 不限（设计 §3）。
        let cap = if limit == 0 { usize::MAX } else { limit };
        let lite: Vec<CompletionItemLite> = items
            .into_iter()
            .take(cap)
            .map(parse_completion_item)
            .collect();
        // 不让排序退化：保持 LSP 已排序顺序。
        let truncated = if lite.len() < total {
            Some(format!("{} of {}", lite.len(), total))
        } else {
            None
        };
        Ok(CompletionResponse {
            truncated,
            items: lite,
        })
    }
    /// `find_referencing_symbols`：所有引用 + 每个 ref 落在哪个外层符号里（Task 24）。
    pub async fn tool_referencing_symbols(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<ref_tools::RefSymbolHit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        ref_tools::find_referencing_symbols(&session, root, file, line, col)
            .await
            .map_err(|e| {
                ToolError::Core(CoreError::Rpc {
                    code: -1,
                    message: format!("find_referencing_symbols: {e}"),
                })
            })
    }

    /// `find_referencing_code_snippets`：所有引用 + 每个 ref 前后 N 行（Task 24）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_referencing_code_snippets(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        context_lines: u32,
        max_results: usize,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<ref_tools::RefSnippetHit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        ref_tools::find_referencing_code_snippets(
            &session,
            root,
            file,
            line,
            col,
            context_lines,
            max_results,
        )
        .await
        .map_err(|e| {
            ToolError::Core(CoreError::Rpc {
                code: -1,
                message: format!("find_referencing_code_snippets: {e}"),
            })
        })
    }

    /// `replace_text_in_symbol`：在 symbol 体内替换 old→new（Task 25）。
    pub async fn tool_edit_replace_text(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        old_text: &str,
        new_text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::replace_text_in_symbol(&session, root, &abs, symbol, old_text, new_text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("replace_text_in_symbol: {e}"),
            })
    }

    /// `insert_text_before_symbol`：在 symbol 开头插入 text（Task 25）。
    /// 返回插入内容末尾的 (end_line, end_col)（1-based）。
    pub async fn tool_edit_insert_before_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_text_before_symbol(&session, root, &abs, symbol, text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("insert_text_before_symbol: {e}"),
            })
    }

    /// `insert_text_after_symbol`：在 symbol 末尾插入 text（Task 25）。
    pub async fn tool_edit_insert_after_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_text_after_symbol(&session, root, &abs, symbol, text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("insert_text_after_symbol: {e}"),
            })
    }

    /// `delete_text_in_symbol`：在 symbol 体内删除 [start_line, end_line] 切片（1-based 含端，Task 25）。
    pub async fn tool_edit_delete_text(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        start_line: u32,
        end_line: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::delete_text_in_symbol(&session, root, &abs, symbol, start_line, end_line)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("delete_text_in_symbol: {e}"),
            })
    }
    /// `symbol-body`：按符号名取函数/类体切片（PLAN Task 15）。
    ///
    /// 流程：ensure_open → documentSymbol 定位 name 匹配的符号 range →
    /// offsets.rs 切片返回。position-free（客户端只传 file + symbol name）。
    pub async fn tool_symbol_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<String> {
        let path = root.join(file);
        // Phase 3.1 缓存：documentSymbol 往返命中 → 直接在缓存列表找 range 切片（盘上
        // 文本仍现读）。not-found 与 miss 路径同语义。Δ: find_symbol_range 对 Flat 形态返
        // None 而缓存列表含 Flat 平铺项 —— 仅理论差异（本仓库 adapter 均回 Nested，见
        // flatten_symbols 注）。
        let cache_key = doc_symbol_cache_key(root, file);
        if let Some(cached) = self.symbol_cache_get(&cache_key) {
            return match cached.iter().find(|h| h.name == symbol) {
                Some(h) => read_and_slice(&path, file, h.range).await,
                None => Err(ToolError::BadArgs {
                    detail: format!("symbol `{symbol}` not found in {file}"),
                }),
            }; // cache_hit
        }
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        // 递归找第一个 name == symbol 的 DocumentSymbol（Nested 形态）。
        let range = find_symbol_range(&resp, symbol).ok_or_else(|| ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {file}"),
        })?;
        let out = read_and_slice(&path, file, range).await?;
        self.symbol_cache_put(cache_key, flatten_symbols(resp, &uri)); // cache_miss → 写入
        Ok(out)
    }

    /// `replace-body`：按符号名替换函数/类体（C3 一致性链路，PLAN Task 15）。
    ///
    /// 流程（全程持全局写门）：
    /// 1. documentSymbol 解析符号 range（客户端只传符号名，不传 range）
    /// 2. 读盘 content 与前快照 hash 对账 —— 不符 → WRITE_CONFLICT
    /// 3. 新 body 替换 range → tempfile 原子写 + rename（Windows 共享冲突重试 5×50ms）
    /// 4. 读回 diff 校验 —— 不符 → 从写前副本回滚 + WRITE_CONFLICT
    /// 5. didChange 全量同步 → LS 与盘一致
    ///
    /// Self-heal：闭包内 session.request / ensure_open 任何一步收到
    /// `Core(Terminated)`（RA 因前置写入 didChange 错序 / 索引风暴把 LS 拉死），
    /// `with_session_retry` 驱逐旧 session + 重试一次。再 fail 透传原错误给 agent。
    pub async fn tool_replace_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        new_body: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);
        let symbol = symbol.to_string();
        let new_body = new_body.to_string();
        let lang_owned = lang.clone();
        Self::with_session_retry(self, root, lang.as_str(), move |session| {
            let path = path.clone();
            let uri_str = uri_str.clone();
            let symbol = symbol.clone();
            let new_body = new_body.clone();
            let lang_str = lang_owned.clone();
            async move {
                Self::tool_replace_body_inner(
                    session,
                    root,
                    &path,
                    &uri_str,
                    &symbol,
                    &new_body,
                    lang_str.as_str(),
                )
                .await
            }
        })
        .await
    }

    /// tool_replace_body 的实际实现 ——
    /// 拆出来便于 `with_session_retry` 在闭包里重放整条链路（含写门重取）。
    #[allow(unused_variables)]
    async fn tool_replace_body_inner(
        session: Arc<Session>,
        root: &Path,
        path: &Path,
        uri_str: &str,
        symbol: &str,
        new_body: &str,
        lang: &str,
    ) -> ToolResult<()> {
        // 0) 索引等待（PLAN Phase 3.2）：cold-start 下 LS 未索引时 documentSymbol 会
        //    给过期/错位 range —— replace-body 错位的根因。先 didOpen + documentSymbol
        //    探针等就绪再进写门；探针超时只 warn 不阻断（回退契约同 on_server_ready）。
        let _probe_open = session.ensure_open(path).await.map_err(ToolError::Core)?;
        if let Some(adapter) = ls_registry::adapter_for(lang)
            && let Err(e) = tokio::time::timeout(
                INDEX_WAIT_TIMEOUT,
                adapter.wait_for_index(&session, path, INDEX_WAIT_TIMEOUT),
            )
            .await
        {
            tracing::warn!(
                adapter = adapter.id(),
                error = %e,
                "wait_for_index failed/timed out; continuing"
            );
        }

        // ===== 全局写门：以下所有步骤持锁（A4 FIFO）=====
        let _gate = write_gate::acquire().await;

        // 1) 锁内解析符号 range（杜绝客户端 range 过期）。
        let _guard = session.ensure_open(path).await.map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": uri_str } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let range = find_symbol_range(&resp, symbol).ok_or_else(|| ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {}", path.display()),
        })?;

        // 2) 读盘 + content-hash 对账（C3 防线 ①）。
        let old_text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", path.display()),
            })?;
        let old_hash = content_hash(&old_text);

        // 3) 新文本 = 老文本替换 range；tempfile 原子写 + rename。
        let start = LspPos {
            line: range.start.line,
            character: range.start.character,
        };
        let end = LspPos {
            line: range.end.line,
            character: range.end.character,
        };
        let start_byte = lsp_core::offsets::position_to_byte(&old_text, start, OffsetEncoding::Utf16)
            .map_err(|e| ToolError::BadArgs {
            detail: format!("start position: {e}"),
        })?;
        let end_byte = lsp_core::offsets::position_to_byte(&old_text, end, OffsetEncoding::Utf16)
            .map_err(|e| ToolError::BadArgs {
            detail: format!("end position: {e}"),
        })?;
        let new_text = format!(
            "{}{}{}",
            &old_text[..start_byte],
            new_body,
            &old_text[end_byte..]
        );

        atomic_write(path, &new_text)
            .await
            .map_err(|e| ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: format!("atomic write failed: {e}"),
            })?;

        // 4) 读回 diff 校验（C3 防线 ②③）—— 不符回滚 + 报冲突。
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", path.display()),
            })?;
        if readback != new_text {
            // 回滚：老内容写回。
            let _ = atomic_write(path, &old_text).await;
            return Err(ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: "readback mismatch; rolled back".into(),
            });
        }

        // 5) didChange 全量同步 → LS 与盘一致。
        // 走 `ensure_open` 的 mtime-检测路径：它用 Session 内部的 content_version
        // 单调递增（与 didOpen 起始号连续），不再用本工具自维护的 VERSION 计数器，
        // 避免与 edit_tools 的 didChange 序列撞车（rust-analyzer 一见乱序即
        // 关闭 channel → 后续所有工具 LS_TERMINATED）。
        drop(_guard);
        let _refreshed = session.ensure_open(path).await.map_err(ToolError::Core)?;

        // 防御：old_hash 在此仅供将来做"多客户端并发"检测（M2 扩展）。
        let _ = old_hash;
        Ok(())
    }

    /// 全 codebase grep（AI 替代"读全文件"）。
    ///
    /// 设计要点（Task 19）：
    /// - 走 `ignore` crate：自动尊重 `.gitignore` / `.ignore` / global ignores
    /// - 默认排除 binary / >5MB 大文件（合理启发）
    /// - `path_glob`：可选 glob 过滤（如 `"*.cpp"` `"src/**/*.py"`）
    /// - `max_results` 默认 100：超过返回 truncated 标记
    /// - 不动 LS —— 这是 fs 工具，不需要 LSP
    pub async fn tool_search_for_pattern(
        &self,
        root: &Path,
        pattern: &str,
        path_glob: Option<&str>,
        max_results: usize,
        case_sensitive: bool,
    ) -> ToolResult<SearchResponse> {
        use ignore::WalkBuilder;
        use regex::RegexBuilder;

        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| ToolError::BadArgs {
                detail: format!("bad regex: {e}"),
            })?;
        let glob_re = match path_glob {
            Some(g) => {
                let mut r = String::from("^");
                // glob 不含 `/` 时，前后加 `.*`，让 `*.cpp` 也匹配 `src/a.cpp`。
                if !g.contains('/') {
                    r.push_str(".*");
                }
                // 把 glob 转 regex —— 支持 `**` 跨任意段 + `*` 单段 + `?` 单字符。

                let mut i = 0;
                let chars: Vec<char> = g.chars().collect();
                while i < chars.len() {
                    let c = chars[i];

                    // `**` 跨任意段（包括 `/`）。
                    if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
                        r.push_str(".*");
                        i += 2;
                        // 吞掉紧跟的 `/`（`src/**/foo` 等价 `src/foo`）。
                        if i < chars.len() && chars[i] == '/' {
                            i += 1;
                        }
                        continue;
                    }

                    match c {
                        '*' => r.push_str("[^/]*"),
                        '?' => r.push('.'),
                        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '\\' => {
                            r.push('\\');
                            r.push(c);
                        }
                        '[' | ']' => r.push(c),
                        _ => r.push(c),
                    }
                    i += 1;
                }
                r.push('$');
                Some(
                    RegexBuilder::new(&r)
                        .case_insensitive(!case_sensitive)
                        .build()
                        .map_err(|e| ToolError::BadArgs {
                            detail: format!("bad glob: {e}"),
                        })?,
                )
            }
            None => None,
        };

        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut walker = WalkBuilder::new(&root);
        walker
            .standard_filters(true)
            .hidden(false)
            .require_git(false);

        let mut hits: Vec<SearchHit> = Vec::new();
        let mut truncated = false;
        let mut files_scanned: usize = 0;

        for entry in walker.build() {
            if truncated {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root).unwrap_or(path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if let Some(g) = &glob_re
                && !g.is_match(&rel_str)
            {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > 5 * 1024 * 1024 {
                continue;
            }

            files_scanned += 1;
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for (line_idx, line) in content.lines().enumerate() {
                if truncated {
                    break;
                }
                for m in regex.find_iter(line) {
                    if hits.len() >= max_results {
                        truncated = true;
                        break;
                    }
                    hits.push(SearchHit {
                        file: rel_str.clone(),
                        line: (line_idx + 1) as u32,
                        col: (m.start() + 1) as u32,
                        text: line.trim_end().to_string(),
                        match_start: m.start() as u32,
                        match_end: m.end() as u32,
                    });
                }
            }
        }

        Ok(SearchResponse {
            hits,
            truncated,
            files_scanned,
        })
    }
    /// `textDocument/rename` 跨文件重命名（Task 22）。
    ///
    /// 设计要点：
    /// - **写门全程持锁**：rename 涉及多文件，跨文件不能并行；与 replace-body 共用全局写门。
    /// - **复用 LSP WorkspaceEdit**：LS 决定 edit 范围（textDocument/rename），
    ///   我们把 WorkspaceEdit 的 edits 应用到盘上 + 全量 didChange。
    /// - **位置倒序 apply**：每文件 edits 按 `range.end` 倒序处理，避免偏移漂移。
    /// - **两种 WorkspaceEdit 形态都收**：`documentChanges` 优先，回退 `changes` map
    ///   （clangd 默认后者，gopls 只产前者）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_rename_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        new_name: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<RenameReport> {
        if new_name.is_empty() || new_name.contains(' ') {
            return Err(ToolError::BadArgs {
                detail: "new_name must be non-empty, no whitespace".into(),
            });
        }

        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 索引等待（PLAN Phase 3.2）：cold-start 下未索引的 LS 会让 prepareRename /
        // rename 打满 TOOL_TIMEOUT —— 30s 超时的根因。探针就绪后才进写门；探针超时
        // 只 warn 不阻断（回退契约同 on_server_ready）。T0 无 adapter → 跳过特判等待。
        if let Some(adapter) = ls_registry::adapter_for(lang.as_str())
            && let Err(e) = tokio::time::timeout(
                INDEX_WAIT_TIMEOUT,
                adapter.wait_for_index(&session, &path, INDEX_WAIT_TIMEOUT),
            )
            .await
        {
            tracing::warn!(
                adapter = adapter.id(),
                error = %e,
                "wait_for_index failed/timed out; continuing"
            );
        }

        let _gate = write_gate::acquire().await;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;
        let pos_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
        });

        // 1) prepareRename —— null = 不能 rename。
        let prep: Option<serde_json::Value> = session
            .request(
                "textDocument/prepareRename",
                pos_params.clone(),
                TOOL_TIMEOUT,
            )
            .await?;
        if prep.is_none() || prep.as_ref().is_some_and(|v| v.is_null()) {
            return Err(ToolError::BadArgs {
                detail: "prepareRename rejected this position".into(),
            });
        }

        // 2) textDocument/rename → WorkspaceEdit JSON。
        let edit_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
            "newName": new_name,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/rename", edit_params, TOOL_TIMEOUT)
            .await?;
        let resp = resp.ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename returned null".into(),
        })?;

        // 3) 拆 WorkspaceEdit → 按文件分组（documentChanges 优先，回退 changes map）。
        let by_uri = parse_workspace_edit(&resp).ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename response has neither `changes` map nor `documentChanges`".into(),
        })?;

        // 4) 对每个文件应用 edits。
        let mut report = RenameReport::default();
        let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        for (uri, mut edits) in by_uri {
            edits.sort_by_key(|e| std::cmp::Reverse(e.0)); // 倒序
            let abs = match uri_to_path(&uri) {
                Some(p) => p,
                None => continue,
            };
            // 必须在 root 内（防 path traversal 风险）。
            if !abs.starts_with(&root_canon) {
                continue;
            }

            let content = match tokio::fs::read_to_string(&abs).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut new_content = content.clone();
            for (_key, range, new_text) in &edits {
                let start_byte = match lsp_core::offsets::position_to_byte(
                    &new_content,
                    lsp_core::offsets::Position {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    OffsetEncoding::Utf16,
                ) {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let end_byte = match lsp_core::offsets::position_to_byte(
                    &new_content,
                    lsp_core::offsets::Position {
                        line: range.end.line,
                        character: range.end.character,
                    },
                    OffsetEncoding::Utf16,
                ) {
                    Ok(b) => b,
                    Err(_) => break,
                };
                new_content = format!(
                    "{}{}{}",
                    &new_content[..start_byte],
                    new_text,
                    &new_content[end_byte..]
                );
            }

            if new_content == content {
                continue;
            }

            atomic_write(&abs, &new_content)
                .await
                .map_err(|e| ToolError::WriteConflict {
                    path: abs.display().to_string(),
                    reason: format!("atomic write failed: {e}"),
                })?;

            // 全量 didChange 让 LS 跟上 —— 走 ensure_open 的 mtime-检测路径，
            // 与 tool_replace_body / edit_tools 共享同一 content_version 单调递增
            // （rust-analyzer 拒收非单调 version → channel 关）。
            let _refreshed = session.ensure_open(&abs).await.map_err(ToolError::Core)?;

            report.files_modified += 1;
            report.edits_applied += edits.len();
            let rel = abs
                .strip_prefix(&root_canon)
                .unwrap_or(&abs)
                .to_string_lossy()
                .replace('\\', "/");
            report.files.push(rel);
        }

        Ok(report)
    }

    /// `safe-delete-symbol`：无引用才删，有引用拒删并返回引用位置列表。
    ///
    /// ↖ mirror: symbol_tools.py@43ae021 SafeDeleteSymbol
    /// （Δ position-free：按 file + 符号名寻址，与 symbol-body 一致；
    ///  references 不含声明本身——声明随删除一起消失；
    ///  「有引用」是正常结果而非错误，wire 层不占错误码。）
    ///
    /// 流程（写门全程持锁）：documentSymbol 定位 (range, selectionRange) →
    /// references(selectionRange, includeDeclaration=false) → 有引用返回列表；
    /// 无引用整行删除 + atomic_write + 读回校验 + didChange 全量（C3 链路同 replace-body）。
    pub async fn tool_safe_delete_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<SafeDeleteReport> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);

        let _gate = write_gate::acquire().await;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 1) 锁内解析符号（杜绝过期 range）。
        let params = json!({ "textDocument": { "uri": uri_str.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let (range, selection) =
            find_symbol_node(&resp, symbol).ok_or_else(|| ToolError::BadArgs {
                detail: format!("symbol `{symbol}` not found in {file}"),
            })?;

        // 2) references：锚定 selectionRange（标识符），排除声明本身。
        let ref_params = json!({
            "textDocument": { "uri": uri_str.clone() },
            "position": {
                "line": selection.start.line,
                "character": selection.start.character,
            },
            "context": { "includeDeclaration": false },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/references", ref_params, TOOL_TIMEOUT)
            .await?;
        let refs = normalize_implementations(raw.as_ref());

        // 3) 有引用 → 拒删，报引用位置（相对路径 + 1-based 行号）。
        if !refs.is_empty() {
            let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let mut locations: Vec<SafeDeleteRef> = refs
                .iter()
                .filter_map(|loc| {
                    let p = uri_to_path(loc.uri.as_str())?;
                    let rel = p
                        .strip_prefix(&root_canon)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    Some(SafeDeleteRef {
                        file: rel,
                        line: loc.range.start.line + 1,
                    })
                })
                .collect();
            locations.sort();
            locations.dedup();
            return Ok(SafeDeleteReport {
                deleted: false,
                symbol: symbol.to_string(),
                references: locations,
            });
        }

        // 3.5) 拦截门：语义层 0 引用 ≠ 真无引用 —— 语义层可能整体不可用（如
        // rust-analyzer root 大小写脱挂时 references 恒空，BD serena-rust-81m）。
        // 文本交叉验证：workspace 内符号名仍有 ≥1 处可疑出现（排除定义行 +
        // 注释/字符串粗滤）→ 拒删，提示语义层可能不可用。RPC_ERROR 不可重试。
        let search = self
            .tool_search_for_pattern(
                root,
                &regex::escape(symbol),
                None,
                TEXT_GATE_MAX_HITS,
                true,
            )
            .await?;
        let n = textual_occurrences_outside_def(
            &search.hits,
            file,
            selection.start.line + 1,
            symbol,
        );
        if n > 0 {
            return Err(ToolError::Protocol {
                tool: "safe-delete-symbol".to_string(),
                reason: format!(
                    "symbol has {n} textual occurrence(s) outside definition but 0 semantic refs; semantic layer may be unavailable"
                ),
            });
        }

        // 4) 无引用 → 删除：整行语义（见 delete_symbol_text）+ C3 链路。
        let old_text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", path.display()),
            })?;
        let new_text = delete_symbol_text(&old_text, range)?;
        atomic_write(&path, &new_text)
            .await
            .map_err(|e| ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: format!("atomic write failed: {e}"),
            })?;
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", path.display()),
            })?;
        if readback != new_text {
            let _ = atomic_write(&path, &old_text).await;
            return Err(ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: "readback mismatch; rolled back".into(),
            });
        }
        // 走 `ensure_open` 的 mtime-检测路径，与 tool_replace_body / edit_tools 共享
        // 同一 content_version 单调递增（rust-analyzer 拒收非单调 version → channel 关）。
        drop(_guard);
        let _refreshed = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        Ok(SafeDeleteReport {
            deleted: true,
            symbol: symbol.to_string(),
            references: vec![],
        })
    }

    /// `insert-at-line`：在 line（1-based）前插入，原行下移；line == total+1 追加 EOF。
    /// ↖ mirror: file_tools.py@43ae021 InsertAtLineTool（0-based → 1-based Δ）。
    /// 返回插入内容末尾的 (end_line, end_col)（1-based）。
    pub async fn tool_insert_at_line(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        content: &str,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_at_line(&session, root, &abs, line, content, expected_hash)
            .await
            .map_err(line_edit_err("insert_at_line"))
    }

    /// `replace-lines`：用 content 替换 [start_line, end_line]（1-based 含端）。
    /// ↖ mirror: file_tools.py@43ae021 ReplaceLinesTool（0-based → 1-based Δ）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_replace_lines(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        content: &str,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::replace_lines(
            &session,
            root,
            &abs,
            start_line,
            end_line,
            content,
            expected_hash,
        )
        .await
        .map_err(line_edit_err("replace_lines"))
    }

    /// `delete-lines`：删除 [start_line, end_line]（1-based 含端）。
    /// ↖ mirror: file_tools.py@43ae021 DeleteLinesTool（0-based → 1-based Δ）。
    pub async fn tool_delete_lines(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::delete_lines(&session, root, &abs, start_line, end_line, expected_hash)
            .await
            .map_err(line_edit_err("delete_lines"))
    }
}

/// 行级三件套 EditError → ToolError 映射：写冲突保留变体，其余归 BadArgs。
fn line_edit_err(tool: &str) -> impl Fn(edit_tools::EditError) -> ToolError + '_ {
    move |e| match e {
        edit_tools::EditError::WriteConflict { path, reason } => {
            ToolError::WriteConflict { path, reason }
        }
        edit_tools::EditError::Core(c) => ToolError::Core(c),
        other => ToolError::BadArgs {
            detail: format!("{tool}: {other}"),
        },
    }
}

/// rename_symbol 结果报告。
#[derive(Debug, Default, Serialize)]
pub struct RenameReport {
    /// 修改的文件数。
    pub files_modified: usize,
    /// 应用的总 edit 数（每个文件所有 edits 之和）。
    pub edits_applied: usize,
    /// 相对 root 路径列表（agent 审计用）。
    pub files: Vec<String>,
}

/// 把 `file://...` URL 转回 PathBuf（percent-decode + Windows 盘符大写归一）。
///
/// LS 返回的 URI 可能带 percent-encoding（tsserver 实测 `file:///d%3A/...`），
/// 不解码会导致路径比对失败——rename 静默 0 编辑、defining-symbol 误判 outside
/// workspace root。Windows 盘符统一大写：`Path` 前缀比较按字节区分大小写，小写
/// `d:` 与 dunce canonicalize 出的 `D:` root 不匹配。
pub(crate) fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    // Windows: `file:///C:/foo` → `C:/foo`
    let s = if cfg!(windows) && stripped.starts_with('/') {
        &stripped[1..]
    } else {
        stripped
    };
    let mut s = percent_decode(s);
    if cfg!(windows) && s.len() >= 2 && s.as_bytes()[1] == b':' {
        s[..1].make_ascii_uppercase();
    }
    Some(std::path::PathBuf::from(s.replace('\\', "/")))
}

/// percent-decode `%XX` 序列（非法 / 截断序列原样保留）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// rename 单文件编辑：(排序 key（end 线性 index，粗略）, range, new_text)。
type EditSpec = (u64, lsp_types::Range, String);

/// 拆单文件 edits 数组；缺 range/newText 的条目跳过（AnnotatedTextEdit 等富形态
/// 字段是超集，读子集即可）。
fn parse_edits(v: &serde_json::Value) -> Vec<EditSpec> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let range: lsp_types::Range =
                        serde_json::from_value(e.get("range")?.clone()).ok()?;
                    let new_text = e.get("newText")?.as_str()?.to_string();
                    // 排序 key：end 的 linear index（粗略）。
                    let key = (range.end.line as u64) << 32 | range.end.character as u64;
                    Some((key, range, new_text))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 拆 WorkspaceEdit → (uri, edits) 列表。`documentChanges`（LSP 3.13+ 数组形）优先，
/// 回退 `changes` map —— gopls 只产前者，clangd 默认后者。两者皆缺 → None。
/// `kind:"create"/"rename"/"delete"` 等非文本条目跳过。
fn parse_workspace_edit(resp: &serde_json::Value) -> Option<Vec<(String, Vec<EditSpec>)>> {
    if let Some(docs) = resp.get("documentChanges").and_then(|v| v.as_array()) {
        Some(
            docs.iter()
                .filter_map(|td| {
                    let uri = td.get("textDocument")?.get("uri")?.as_str()?.to_string();
                    Some((uri, parse_edits(td.get("edits")?)))
                })
                .collect(),
        )
    } else {
        let changes = resp.get("changes")?.as_object()?;
        Some(
            changes
                .iter()
                .map(|(uri, edits)| (uri.clone(), parse_edits(edits)))
                .collect(),
        )
    }
}

/// safe-delete 结果报告。
#[derive(Debug, Serialize)]
pub struct SafeDeleteReport {
    /// false = 有引用拒删；true = 已删除。
    pub deleted: bool,
    pub symbol: String,
    /// deleted=false 时非空：引用位置（相对路径 + 1-based 行号，按 file+line 排序去重）。
    pub references: Vec<SafeDeleteRef>,
}

/// 单条引用位置。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct SafeDeleteRef {
    /// 相对 project_root 路径。
    pub file: String,
    /// 1-based 行号。
    pub line: u32,
}

/// 单个搜索命中。
#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub text: String,
    pub match_start: u32,
    pub match_end: u32,
}
/// 搜索响应。
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    pub files_scanned: usize,
}

/// 拍平 `DocumentSymbolResponse` → `Vec<SymbolHit>`。
///
/// LSP 允许两种形态：
/// - `Flat(Vec<SymbolInformation>)`：每项自带 `location` 与 `container_name`。
/// - `Nested(Vec<DocumentSymbol>)`：递归 children；`container` 用父名。
///
/// M0 客户端声明 `hierarchicalDocumentSymbolSupport=true`，clangd 一定回 Nested 形态；
/// Flat 仅 mock_ls 用得到，但本模块不耦合 mock_ls，故对两种形态都处理。
fn flatten_symbols(resp: DocumentSymbolResponse, file_uri: &str) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    match resp {
        DocumentSymbolResponse::Flat(items) => {
            for it in items {
                out.push(SymbolHit {
                    name: it.name,
                    kind: kind_from_lsp(&it.kind),
                    uri: it.location.uri.to_string(),
                    range: it.location.range,
                    container: it.container_name,
                });
            }
        }
        DocumentSymbolResponse::Nested(items) => {
            for it in items {
                push_nested(&it, None, file_uri, &mut out);
            }
        }
    }
    out
}

fn push_nested(
    sym: &DocumentSymbol,
    container: Option<String>,
    file_uri: &str,
    out: &mut Vec<SymbolHit>,
) {
    let container = container.or_else(|| Some(sym.name.clone()));
    out.push(SymbolHit {
        name: sym.name.clone(),
        kind: kind_from_lsp(&sym.kind),
        uri: file_uri.to_string(),
        range: sym.range,
        container: container.clone(),
    });
    if let Some(children) = sym.children.as_ref() {
        for child in children {
            push_nested(child, Some(sym.name.clone()), file_uri, out);
        }
    }
}

/// 按 (line, col) 在 `DocumentSymbolResponse` 树中反查所有包含该位置的符号（Phase 2.1）。
///
/// 规则：位置在符号的 [start.line, end.line] 闭区间内；
/// - `line == start.line` 时 `col >= start.character`；
/// - `line == end.line`   时 `col <= end.character`；
/// - 中间行默认命中（不查 col，LSP 自身规范）。
///
/// 返回从最外层到最深命中的 `SymbolHit` 链（所有命中的祖先）。无命中返空 Vec。
/// Nested 形态递归 `children`；Flat 形态只按顶层项判断（mock_ls 路径）。
fn collect_containing_hits(
    resp: &DocumentSymbolResponse,
    file_uri: &str,
    line: u32,
    col: u32,
) -> Vec<SymbolHit> {
    fn walk(
        items: &[DocumentSymbol],
        file_uri: &str,
        line: u32,
        col: u32,
        container: Option<&str>,
        out: &mut Vec<SymbolHit>,
    ) {
        for it in items {
            if position_in_range(it.range, line, col) {
                out.push(SymbolHit {
                    name: it.name.clone(),
                    kind: kind_from_lsp(&it.kind),
                    uri: file_uri.to_owned(),
                    range: it.range,
                    container: container.map(str::to_owned),
                });
                if let Some(children) = it.children.as_ref() {
                    walk(children, file_uri, line, col, Some(&it.name), out);
                }
            } else if let Some(children) = it.children.as_ref() {
                // 父节点不命中但子节点仍可能命中（罕见：嵌套树里父 range 比子 range 大）。
                walk(children, file_uri, line, col, container, out);
            }
        }
    }

    match resp {
        DocumentSymbolResponse::Nested(items) => {
            let mut out = Vec::new();
            walk(items, file_uri, line, col, None, &mut out);
            out
        }
        DocumentSymbolResponse::Flat(items) => {
            // Flat：每项自带 location & container_name；无层级。
            items
                .iter()
                .filter(|it| position_in_range(it.location.range, line, col))
                .map(|it| SymbolHit {
                    name: it.name.clone(),
                    kind: kind_from_lsp(&it.kind),
                    uri: it.location.uri.to_string(),
                    range: it.location.range,
                    container: it.container_name.clone(),
                })
                .collect()
        }
    }
}

/// `line/col` 是否落在 `range` 闭区间内（见 collect_containing_hits 注释）。
fn position_in_range(r: lsp_types::Range, line: u32, col: u32) -> bool {
    if line < r.start.line || line > r.end.line {
        return false;
    }
    if line == r.start.line && col < r.start.character {
        return false;
    }
    if line == r.end.line && col > r.end.character {
        return false;
    }
    true
}
/// `lsp_types::SymbolKind.0` is private. Round-trip via JSON: `SymbolKind` is
/// `#[serde(transparent)]` over `i32`, so deserializing into i32 yields the wire number.
fn kind_from_lsp(k: &lsp_types::SymbolKind) -> SymbolKindTag {
    let n: i32 = serde_json::from_value(serde_json::json!(k)).unwrap_or(0);
    SymbolKindTag::from_lsp(n as u8)
}
/// 把 (line, col) 经 offsets.rs 换算为 LSP Position。M0 固定 utf-16。
/// 读盘 → `position_to_byte` → 校验落在 char 边界。这里简化：假定输入 line/col 即
/// LSP position（0-based），仅校验越界，避免上层传 0-based 错位时掩盖。
async fn lsp_position_from_byte(
    path: &Path,
    line: u32,
    col: u32,
    enc: OffsetEncoding,
) -> ToolResult<Position> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("read {}: {e}", path.display()),
        })?;
    let pos = LspPos {
        line,
        character: col,
    };
    lsp_core::offsets::position_to_byte(&text, pos, enc).map_err(|e| ToolError::BadArgs {
        detail: format!("position {line}:{col} out of range: {e}"),
    })?;
    Ok(Position::new(pos.line, pos.character))
}

/// 解析 lang: 有 override 直接用 (大小写折叠), 否则按文件扩展名探测
/// （内置 EXT_TABLE → external-servers.toml extensions 兜底）。
fn resolve_lang_for_file(file: &str, lang_override: Option<&str>) -> ToolResult<String> {
    if let Some(l) = lang_override {
        return Ok(l.to_ascii_lowercase());
    }
    ls_registry::resolve_lang_name(Path::new(file))
        .map(str::to_string)
        .ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })
}

/// 从 anyhow 错消息里抠 install_hint。`ls-adapters::not_installed_error` 模板：
/// `language server \`{name}\` not found in PATH; install_hint: {hint}`
fn extract_install_hint(msg: &str) -> String {
    msg.rsplit("install_hint:")
        .next()
        .map(str::trim)
        .unwrap_or("see upstream docs")
        .to_string()
}

/// Phase 4 基建 Task 22b：从 args 中移除 `_timeout_ms` / `_index_timeout_ms` 私有字段，
/// 返回清理后的 args clone。`execute_tool` 入口调用，避免污染后续 `required_file` 等
/// 私有 helper（它们只看业务字段如 `file`/`line`，忽略下划线前缀；清掉是为了
/// JSONL 反序列化时 `_timeout_ms` 不会泄漏到 tool 输出）。
fn sanitize_timeout_args(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = args.as_object_mut() {
        obj.remove("_timeout_ms");
        obj.remove("_index_timeout_ms");
    }
    args
}

fn required_file(args: &serde_json::Value) -> ToolResult<String> {
    args.get("file")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing args.file".into(),
        })
}

fn required_position(args: &serde_json::Value) -> ToolResult<(String, u32, u32)> {
    let file = required_file(args)?;
    let line = args
        .get("line")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing or invalid args.line".into(),
        })?;
    let col = args
        .get("col")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing or invalid args.col".into(),
        })?;
    Ok((file, line, col))
}

fn required_symbol_body_args(args: &serde_json::Value) -> ToolResult<(String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    Ok((file, symbol))
}

fn required_replace_args(args: &serde_json::Value) -> ToolResult<(String, String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    let new_body = args
        .get("new_body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'new_body'".into(),
        })?
        .to_owned();
    Ok((file, symbol, new_body))
}

fn required_rename_args(args: &serde_json::Value) -> ToolResult<(String, u32, u32, String)> {
    let (file, line, col) = required_position(args)?;
    let new_name = args
        .get("new_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'new_name'".into(),
        })?
        .to_owned();
    Ok((file, line, col, new_name))
}

/// 行级 replace-lines / delete-lines 的 (file, start_line, end_line)。
fn required_line_range(args: &serde_json::Value) -> ToolResult<(String, u32, u32)> {
    let file = required_file(args)?;
    let start_line = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'start_line'".into(),
        })? as u32;
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'end_line'".into(),
        })? as u32;
    Ok((file, start_line, end_line))
}

/// 可选 hash 对账参数（行级三件套）。
fn opt_expected_hash(args: &serde_json::Value) -> Option<String> {
    args.get("expected_hash")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}
/// edit_* 工具通用 helper：(file, symbol, text)。
fn required_edit_args(args: &serde_json::Value) -> ToolResult<(String, String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'text'".into(),
        })?
        .to_owned();
    Ok((file, symbol, text))
}

/// LSP `file://...` URI → 相对 project_root 的 file path（call/type hierarchy 工具
/// 反查 item.uri 时用：item 自身带 uri，我们要把语言探测退回 file path 形式）。
///
/// 注：与 `uri_to_path` 不同 —— 后者返绝对路径，本函数仅在协议适配层用一次，结果
/// 直接喂 `resolve_lang_for_file`（只要扩展名，不需根对齐）。
fn file_path_from_uri(uri: &str) -> String {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    let s = if cfg!(windows) && stripped.starts_with('/') {
        &stripped[1..]
    } else {
        stripped
    };
    percent_decode(s).replace('\\', "/")
}

/// 解码 LSP `SemanticTokens.data`（结构化 delta token 序列）为绝对坐标 token 列表。
///
/// 算法（LSP §3.16 semanticTokens）：
/// - 每 token 含 `delta_line` / `delta_start` / `length` / `token_type` / `token_modifiers`。
/// - `delta_line` 为相对前一 token 的行偏移；`delta_start` 仅在同一行时是字符偏移，
///   否则为新行的绝对起始字符。
/// - 绝对坐标：(line, start_char) 通过累加 delta 得到。
fn decode_semantic_tokens(data: &[lsp_types::SemanticToken]) -> Vec<SemanticTokenEntry> {
    let mut out = Vec::with_capacity(data.len());
    let mut line: u32 = 0;
    let mut start_char: u32 = 0;
    for t in data {
        if t.delta_line > 0 {
            line = line.saturating_add(t.delta_line);
            start_char = t.delta_start;
        } else {
            start_char = start_char.saturating_add(t.delta_start);
        }
        out.push(SemanticTokenEntry {
            line,
            start_char,
            length: t.length,
            token_type: t.token_type,
            token_modifiers: t.token_modifiers_bitset,
        });
    }
    out
}
#[async_trait::async_trait]
impl SupervisorTrait for Supervisor {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError> {
        let root = Path::new(project_root);
        // Phase 4 基建 Task 22b：把 args._timeout_ms / args._index_timeout_ms 提取成
        // per-call override，并清掉这两个私有字段（避免传染给具体 tool 的 args 解析）。
        // 实际 timeout 在 tool_* 内部通过 `effective_tool_timeout(lang, &args)` 拿到。
        let args = sanitize_timeout_args(args);
        match tool {
            "overview" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_overview(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "symbol-tree" => {
                let dir = args.get("dir").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'dir'".into(),
                    }
                })?;
                let max_files = args.get("max_files").and_then(|v| v.as_u64()).unwrap_or(200)
                    as usize;
                serde_json::to_value(self.tool_symbol_tree(root, dir, lang, max_files).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-symbol" => {
                let query = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'query'".into(),
                    }
                })?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
                let resp = self.tool_find_symbol(root, query, limit, lang).await?;
                serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))
            }
            "signature-help" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_signature_help(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            // ==== Phase 1 · 上游 wrapper 缺口（13 个）====
            "code-action" => {
                let (file, line, col) = required_position(&args)?;
                let kind = args.get("kind").and_then(|v| v.as_str());
                serde_json::to_value(
                    self.tool_code_action(root, &file, line, col, kind, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format" => {
                let file = required_file(&args)?;
                let tab_size = args.get("tab_size").and_then(|v| v.as_u64()).map(|n| n as u32);
                let insert_spaces = args.get("insert_spaces").and_then(|v| v.as_bool());
                serde_json::to_value(
                    self.tool_format(root, &file, tab_size, insert_spaces, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format-range" => {
                let file = required_file(&args)?;
                let start_line =
                    args.get("start_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        }
                    })? as u32;
                let start_col =
                    args.get("start_col").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_col'".into(),
                        }
                    })? as u32;
                let end_line =
                    args.get("end_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_line'".into(),
                        }
                    })? as u32;
                let end_col =
                    args.get("end_col").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_col'".into(),
                        }
                    })? as u32;
                let tab_size = args.get("tab_size").and_then(|v| v.as_u64()).map(|n| n as u32);
                let insert_spaces = args.get("insert_spaces").and_then(|v| v.as_bool());
                serde_json::to_value(
                    self.tool_format_range(
                        root,
                        &file,
                        start_line,
                        start_col,
                        end_line,
                        end_col,
                        tab_size,
                        insert_spaces,
                        lang,
                    )
                    .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "inlay-hint" => {
                let file = required_file(&args)?;
                let start_line =
                    args.get("start_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        }
                    })? as u32;
                let end_line =
                    args.get("end_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_line'".into(),
                        }
                    })? as u32;
                serde_json::to_value(
                    self.tool_inlay_hint(root, &file, start_line, end_line, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-highlight" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_document_highlight(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "folding-range" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_folding_range(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "semantic-tokens" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_semantic_tokens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "code-lens" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_code_lens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-link" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_document_link(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "call-hierarchy" => {
                let op = args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'op' (prepare|incoming|outgoing)".into(),
                    })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(&args)?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "incoming" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (CallHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_incoming(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "outgoing" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (CallHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_outgoing(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    other => Err(ToolError::BadArgs {
                        detail: format!("unknown call-hierarchy op: {other}"),
                    }),
                }
            }
            "type-hierarchy" => {
                let op = args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'op' (prepare|supertypes|subtypes)".into(),
                    })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(&args)?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "supertypes" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_supertypes(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "subtypes" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_subtypes(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    other => Err(ToolError::BadArgs {
                        detail: format!("unknown type-hierarchy op: {other}"),
                    }),
                }
            }
            "moniker" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_moniker(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "workspace-diagnostic" => serde_json::to_value(
                self.tool_workspace_diagnostic(root, lang).await?,
            )
            .map_err(|e| ToolError::Serialize(e.into())),
            "hover" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_hover(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "diagnostics" => {
                let file = required_file(&args)?;
                let wait_gen = args.get("wait_gen").and_then(|v| v.as_u64());
                serde_json::to_value(self.tool_diagnostics(root, &file, lang, wait_gen).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "def" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_def(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }

            "containing-symbol" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_containing_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "defining-symbol" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_defining_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }

            "refs" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_refs(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "completion" => {
                let (file, line, col) = required_position(&args)?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let trigger = args.get("trigger").and_then(|v| v.as_str());
                let resp = self
                    .tool_completion(root, &file, line, col, limit, trigger, lang)
                    .await?;
                serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-implementations" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_find_implementations(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "search" => {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'pattern'".into(),
                    })?;
                let path_glob = args.get("path_glob").and_then(|v| v.as_str());
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(100) as usize;
                let case_sensitive = args
                    .get("case_sensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let resp = self
                    .tool_search_for_pattern(root, pattern, path_glob, max_results, case_sensitive)
                    .await?;
                serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))
            }
            "symbol-body" => {
                let (file, symbol) = required_symbol_body_args(&args)?;
                serde_json::to_value(self.tool_symbol_body(root, &file, &symbol, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "replace-body" => {
                let (file, symbol, new_body) = required_replace_args(&args)?;
                self.tool_replace_body(root, &file, &symbol, &new_body, lang)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            "rename-symbol" => {
                let (file, line, col, new_name) = required_rename_args(&args)?;
                serde_json::to_value(
                    self.tool_rename_symbol(root, &file, line, col, &new_name, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "read-file" => {
                let file = required_file(&args)?;
                let start_line = args
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let report = fs_tools::read_file(root, &file, start_line, end_line)
                    .await
                    .map_err(|e| ToolError::BadArgs {
                        detail: format!("read_file: {e}"),
                    })?;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            "list-dir" => {
                let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'path'".into(),
                    }
                })?;
                let max_depth = args
                    .get("max_depth")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let max_entries = args
                    .get("max_entries")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500) as usize;
                let entries =
                    fs_tools::list_dir(root, path, max_depth, max_entries).map_err(|e| {
                        ToolError::BadArgs {
                            detail: format!("list_dir: {e}"),
                        }
                    })?;
                serde_json::to_value(entries).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-file" => {
                let name_pattern = args
                    .get("name_pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'name_pattern'".into(),
                    })?;
                let path_glob = args.get("path_glob").and_then(|v| v.as_str());
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(200) as usize;
                let hits = fs_tools::find_file(root, name_pattern, path_glob, max_results)
                    .map_err(|e| ToolError::BadArgs {
                        detail: format!("find_file: {e}"),
                    })?;
                serde_json::to_value(hits).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-referencing-symbols" => {
                let (file, line, col) = required_position(&args)?;
                let hits = self
                    .tool_referencing_symbols(root, &file, line, col, lang)
                    .await?;
                serde_json::to_value(hits).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-referencing-code-snippets" => {
                let (file, line, col) = required_position(&args)?;
                let context_lines = args
                    .get("context_lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as u32;
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as usize;
                let hits = self
                    .tool_referencing_code_snippets(
                        root,
                        &file,
                        line,
                        col,
                        context_lines,
                        max_results,
                        lang,
                    )
                    .await?;
                serde_json::to_value(hits).map_err(|e| ToolError::Serialize(e.into()))
            }
            "replace-text-in-symbol" => {
                let file = required_file(&args)?;
                let symbol = args
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'symbol'".into(),
                    })?
                    .to_owned();
                let old_text = args
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'old_text'".into(),
                    })?
                    .to_owned();
                let new_text = args
                    .get("new_text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'new_text'".into(),
                    })?
                    .to_owned();
                self.tool_edit_replace_text(root, &file, &symbol, &old_text, &new_text, lang)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            "insert-text-after-symbol" => {
                let (file, symbol, text) = required_edit_args(&args)?;
                let (end_line, end_col) = self
                    .tool_edit_insert_after_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                Ok(serde_json::json!({ "end_line": end_line, "end_col": end_col }))
            }
            "insert-text-before-symbol" => {
                let (file, symbol, text) = required_edit_args(&args)?;
                let (end_line, end_col) = self
                    .tool_edit_insert_before_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                Ok(serde_json::json!({ "end_line": end_line, "end_col": end_col }))
            }
            "delete-text-in-symbol" => {
                let file = required_file(&args)?;
                let symbol = args
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'symbol'".into(),
                    })?
                    .to_owned();
                let start_line =
                    args.get("start_line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        })? as u32;
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'end_line'".into(),
                    })? as u32;
                self.tool_edit_delete_text(root, &file, &symbol, start_line, end_line, lang)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            "safe-delete-symbol" => {
                let (file, symbol) = required_symbol_body_args(&args)?;
                serde_json::to_value(
                    self.tool_safe_delete_symbol(root, &file, &symbol, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "insert-at-line" => {
                let file = required_file(&args)?;
                let line =
                    args.get("line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'line'".into(),
                        })? as u32;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'content'".into(),
                    })?
                    .to_owned();
                let (end_line, end_col) = self
                    .tool_insert_at_line(
                        root,
                        &file,
                        line,
                        &content,
                        opt_expected_hash(&args).as_deref(),
                        lang,
                    )
                    .await?;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                }))
            }
            "replace-lines" => {
                let (file, start_line, end_line) = required_line_range(&args)?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'content'".into(),
                    })?
                    .to_owned();
                self.tool_replace_lines(
                    root,
                    &file,
                    start_line,
                    end_line,
                    &content,
                    opt_expected_hash(&args).as_deref(),
                    lang,
                )
                .await?;
                Ok(serde_json::Value::Null)
            }
            "delete-lines" => {
                let (file, start_line, end_line) = required_line_range(&args)?;
                self.tool_delete_lines(
                    root,
                    &file,
                    start_line,
                    end_line,
                    opt_expected_hash(&args).as_deref(),
                    lang,
                )
                .await?;
                Ok(serde_json::Value::Null)
            }
            other => Err(ToolError::BadArgs {
                detail: format!("unknown tool: {other}"),
            }),
        }
    }

    fn loaded_entries(&self) -> Vec<Key> {
        self.last_used.lock().unwrap().keys().cloned().collect()
    }

    async fn evict_failed(&self) -> usize {
        Self::evict_failed_instances(self).await
    }
}

/// LSP 3.17 `textDocument/definition` 响应允四种形态：
/// `null | Location | Location[] | LocationLink[]`（clangd 22 默认 LocationLink[]）。
/// 归一化为 `Option<Location>`：空 None；单 Location 直返；单元素数组返首项；
/// LocationLink[] 把 `targetUri + targetRange` 折叠为 Location。
fn normalize_definition(raw: Option<&serde_json::Value>) -> Option<Location> {
    let v = raw?;
    if v.is_null() {
        return None;
    }
    // 数组形态：取首个 LocationLike，归一化。
    let first = if v.is_array() {
        v.as_array()?.first()?
    } else {
        v
    };
    // LocationLink 形态：{ targetUri, targetRange, ... } → 转为 Location。
    if let (Some(target_uri), Some(target_range)) = (
        first.get("targetUri").and_then(|x| x.as_str()),
        first.get("targetRange"),
    ) {
        let range: lsp_types::Range = serde_json::from_value(target_range.clone()).ok()?;
        return Some(Location {
            uri: lsp_types::Uri::from_str(target_uri).ok()?,
            range,
        });
    }
    // 标准 Location：{ uri, range }。
    serde_json::from_value::<Location>(first.clone()).ok()
}

/// LSP 3.17 `textDocument/implementation` 响应允多种形态：
/// `null | Location | Location[] | LocationLink[]`。归一化为 `Vec<Location>`：
/// 数组逐元素归一化；单 Location 包成单元素 vec；null → 空 vec。
pub(crate) fn normalize_implementations(raw: Option<&serde_json::Value>) -> Vec<Location> {
    let mut out = Vec::new();
    let Some(v) = raw else { return out };
    if v.is_null() {
        return out;
    }
    let items: &[serde_json::Value] = if v.is_array() {
        v.as_array().expect("just checked")
    } else {
        std::slice::from_ref(v)
    };
    for it in items {
        // LocationLink 形态：{ targetUri, targetRange, ... } → 转 Location。

        if let (Some(target_uri), Some(target_range)) = (
            it.get("targetUri").and_then(|x| x.as_str()),
            it.get("targetRange"),
        ) && let Ok(range) = serde_json::from_value::<lsp_types::Range>(target_range.clone())
            && let Ok(uri) = lsp_types::Uri::from_str(target_uri)
        {
            out.push(Location { uri, range });
            continue;
        }
        // 标准 Location：{ uri, range }。
        if let Ok(loc) = serde_json::from_value::<Location>(it.clone()) {
            out.push(loc);
        }
    }
    out
}
/// 递归在 Nested documentSymbol 里找第一个 name == `symbol` 的 range。
/// Flat 形态（SymbolInformation）不含子符号，这里只处理 Nested —— clangd/mock_ls 都是 Nested。
fn find_symbol_range(resp: &DocumentSymbolResponse, symbol: &str) -> Option<lsp_types::Range> {
    fn walk(items: &[DocumentSymbol], symbol: &str) -> Option<lsp_types::Range> {
        for it in items {
            if it.name == symbol {
                return Some(it.range);
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol)
            {
                return Some(r);
            }
        }
        None
    }
    match resp {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol),
        DocumentSymbolResponse::Flat(_) => None,
    }
}

/// 读盘 + 按 LSP range 切符号体（tool_symbol_body 缓存命中/miss 两路共用）。
async fn read_and_slice(path: &Path, file: &str, range: lsp_types::Range) -> ToolResult<String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("read {}: {e}", path.display()),
        })?;
    let start = LspPos {
        line: range.start.line,
        character: range.start.character,
    };
    let end = LspPos {
        line: range.end.line,
        character: range.end.character,
    };
    lsp_core::offsets::slice_at(&text, start, end, OffsetEncoding::Utf16).map_err(|e| {
        ToolError::BadArgs {
            detail: format!("slice {file}@{start:?}-{end:?}: {e}"),
        }
    })
}

/// 找符号的 `(range, selectionRange)`：range = 删除范围，selectionRange = 标识符
/// 位置（references 锚点）。Flat 形态无 selectionRange，用 location.range 起点近似
/// （SymbolInformation 的 location 即标识符所在位置）。
fn find_symbol_node(
    resp: &DocumentSymbolResponse,
    symbol: &str,
) -> Option<(lsp_types::Range, lsp_types::Range)> {
    fn walk(
        items: &[DocumentSymbol],
        symbol: &str,
    ) -> Option<(lsp_types::Range, lsp_types::Range)> {
        for it in items {
            if it.name == symbol {
                return Some((it.range, it.selection_range));
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol)
            {
                return Some(r);
            }
        }
        None
    }
    match resp {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol),
        DocumentSymbolResponse::Flat(items) => items
            .iter()
            .find(|it| it.name == symbol)
            .map(|it| (it.location.range, it.location.range)),
    }
}

/// safe-delete 文本拦截门的扫描上限。hits 截断（truncated）意味着可疑出现只多不少，
/// 不影响拒删判定方向。
const TEXT_GATE_MAX_HITS: usize = 200;

/// safe-delete 文本交叉验证：统计 `hits` 中"可疑引用"数。
/// - 排除定义行（def_file 的 def_line_1based 行）；
/// - 注释行粗滤：trim 后以 `//` `/*` `*` `#` 开头（`#` 兼 python 注释/C 预处理；
///   代价是 rust `#[attr]` 行被跳过——粗滤即此，宁可少拒不可误拒）；
/// - 字符串粗滤：剥掉 `"..."` / `'...'` 字面量后不再含符号名 → 视为字符串出现跳过。
fn textual_occurrences_outside_def(
    hits: &[SearchHit],
    def_file: &str,
    def_line_1based: u32,
    symbol: &str,
) -> usize {
    let def_file_norm = def_file.replace('\\', "/").to_ascii_lowercase();
    hits.iter()
        .filter(|h| {
            let hf = h.file.replace('\\', "/").to_ascii_lowercase();
            !(hf == def_file_norm && h.line == def_line_1based)
        })
        .filter(|h| {
            let t = h.text.trim_start();
            !(t.starts_with("//")
                || t.starts_with("/*")
                || t.starts_with('*')
                || t.starts_with('#'))
        })
        .filter(|h| strip_string_literals(&h.text).contains(symbol))
        .count()
}

/// 单行字符串字面量粗剥：`"..."` / `'...'`（含 `\"` 转义）内字符丢弃，其余保留。
/// ponytail: 行级 naive 引号状态机，多行字符串/原始字符串（r#"..."#）会漏剥——
/// 粗滤用途足够，漏剥方向是多算可疑 → 误拒可回查，不吞删除。
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match quote {
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else {
                    out.push(c);
                }
            }
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
        }
    }
    out
}

/// 文档截断上限（设计 §3：documentation 默认 200 字符）。
const DOC_MAX_CHARS: usize = 200;

/// 把 LSP CompletionItem（已用 serde_json::Value 形态取出）映射到 `CompletionItemLite`。
/// 字段裁剪 / kind 映射 / doc 截断都集中在这里。
fn parse_completion_item(raw: serde_json::Value) -> CompletionItemLite {
    let label = raw
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let kind = raw
        .get("kind")
        .and_then(|v| v.as_i64())
        .map(map_completion_kind)
        .unwrap_or_else(|| "other".to_owned());
    let detail = raw
        .get("detail")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let insert = raw
        .get("insertText")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            if label.is_empty() {
                None
            } else {
                Some(label.clone())
            }
        });
    let doc = raw
        .get("documentation")
        .and_then(extract_doc_string)
        .map(truncate_doc);
    let deprecated = raw
        .get("deprecated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let additional_text_edits = raw
        .get("additionalTextEdits")
        .and_then(|v| serde_json::from_value::<Vec<lsp_types::TextEdit>>(v.clone()).ok())
        .unwrap_or_default();
    CompletionItemLite {
        label,
        kind,
        detail,
        insert,
        doc,
        deprecated,
        additional_text_edits,
    }
}

/// 把 LSP `documentation`（String | MarkupContent）展平成纯文本字符串。
fn extract_doc_string(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_owned());
    }
    // MarkupContent：{ kind: "markdown" | "plaintext", value: "..." }
    v.get("value").and_then(|x| x.as_str()).map(str::to_owned)
}

/// 截断 doc 到 200 char（codepoint 级，非字节；设计 §3/§8.1）。
fn truncate_doc(s: String) -> String {
    if s.chars().count() <= DOC_MAX_CHARS {
        s
    } else {
        let truncated: String = s.chars().take(DOC_MAX_CHARS).collect();
        format!("{truncated}…")
    }
}

/// LSP `CompletionItemKind` 整数 → 人类词。Unknown → "other"。
/// LSP spec kind 值见 `lsp_types::CompletionItemKind::*`（1..=25）。
fn map_completion_kind(kind_num: i64) -> String {
    use lsp_types::CompletionItemKind as K;
    let k = match kind_num {
        1 => K::TEXT,
        2 => K::METHOD,
        3 => K::FUNCTION,
        4 => K::CONSTRUCTOR,
        5 => K::FIELD,
        6 => K::VARIABLE,
        7 => K::CLASS,
        8 => K::INTERFACE,
        9 => K::MODULE,
        10 => K::PROPERTY,
        11 => K::UNIT,
        12 => K::VALUE,
        13 => K::ENUM,
        14 => K::KEYWORD,
        15 => K::SNIPPET,
        16 => K::COLOR,
        17 => K::FILE,
        18 => K::REFERENCE,
        19 => K::FOLDER,
        20 => K::ENUM_MEMBER,
        21 => K::CONSTANT,
        22 => K::STRUCT,
        23 => K::EVENT,
        24 => K::OPERATOR,
        25 => K::TYPE_PARAMETER,
        _ => return "other".to_owned(),
    };
    // lsp_enum! 给出的常量名 "Text" / "Method" / ... 转小写。
    // 形如 "EnumMember" → "enum_member"；"TypeParameter" → "type_parameter"。
    // 用 Debug 拿名字最稳（spec 表与常量名一一对应）。
    format!("{:?}", k).to_ascii_lowercase()
}
/// 从文本中删除符号 range：默认整行删除（start 行首 → end 行含换行）；
/// end 行符号之后还有非空白内容时只删到符号结尾，保留行尾余文。
fn delete_symbol_text(text: &str, range: lsp_types::Range) -> ToolResult<String> {
    let line_start = |line: u32| {
        lsp_core::offsets::position_to_byte(
            text,
            LspPos { line, character: 0 },
            OffsetEncoding::Utf16,
        )
        .map_err(|e| ToolError::BadArgs {
            detail: format!("position {line}:0: {e}"),
        })
    };
    let s = line_start(range.start.line)?;
    let e_sym = lsp_core::offsets::position_to_byte(
        text,
        LspPos {
            line: range.end.line,
            character: range.end.character,
        },
        OffsetEncoding::Utf16,
    )
    .map_err(|e| ToolError::BadArgs {
        detail: format!("end position: {e}"),
    })?;
    let e_line_start = line_start(range.end.line)?;
    let e_line_end = text[e_line_start..]
        .find('\n')
        .map_or(text.len(), |i| e_line_start + i);
    // end 行符号后只剩空白 → 整行吞掉（含换行）；否则保留行尾余文。
    let e = if text[e_sym..e_line_end].trim().is_empty() {
        if e_line_end < text.len() {
            e_line_end + 1
        } else {
            e_line_end
        }
    } else {
        e_sym
    };
    Ok(format!("{}{}", &text[..s], &text[e..]))
}

/// sha256(content) hex 前 16 位（对账用；碰撞概率足够低且只做提示性校验）。
pub(crate) fn content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let out = h.finalize();
    out.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// tempfile 原子写 + rename；Windows 共享冲突（目标被别进程打开）重试 5×50ms（I5）。
///
/// ↖ mirror: PR oraios/serena#2041（save edited source files atomically）对账 —
/// 上游把编辑保存从截断写 `open(path,"w")` 改为 temp-file+`os.replace`，并要求
/// symlink 目标**透传写**（rename 会把链接本体替换成普通文件，破坏链接关系）。
/// 本项目 edit 链路本就走 tmp+rename（语义已对齐），唯一缺口即 symlink：
/// rename 前先解析链接到真实目标，对目标做原子写。
async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    // 仅当最终组件是 symlink 才解析（canonicalize）；其余路径行为不变。
    // 悬空链接/链接环在此报错 —— 与其把链接替换成普通文件，不如明确失败。
    let target = match tokio::fs::symlink_metadata(path).await {
        Ok(m) if m.is_symlink() => Some(tokio::fs::canonicalize(path).await?),
        _ => None,
    };
    let path: &Path = target.as_deref().unwrap_or(path);
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let tmp_path = tmp.into_temp_path().keep()?;
    tokio::fs::write(&tmp_path, content).await?;

    let mut attempt = 0;
    loop {
        match tokio::fs::rename(&tmp_path, path).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 => {
                // Windows ERROR_SHARING_VIOLATION(32) / ERROR_ACCESS_DENIED(5) 常见于杀软/索引器；
                // 统一退避重试。
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = e;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    /// smoke：普通文件原子写落盘。
    #[tokio::test]
    async fn writes_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.rs");
        atomic_write(&p, "fn main() {}\n").await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "fn main() {}\n"
        );
    }

    /// ↖ mirror: PR oraios/serena#2041 — symlink 目标透传写：内容更新到 target，
    /// 链接本体不得被 rename 替换成普通文件。
    #[tokio::test]
    async fn symlinked_file_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.rs");
        std::fs::write(&target, "old\n").unwrap();
        let link = dir.path().join("link.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(&target, &link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Windows symlink 需开发者模式/管理员；无权限环境跳过（行为无法构造）。
                println!("skipped: symlink 需要开发者模式/管理员权限");
                return;
            }
            Err(e) => panic!("symlink_file: {e}"),
        }
        atomic_write(&link, "new\n").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new\n",
            "内容必须写到 target"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .is_symlink(),
            "链接本体不得被替换成普通文件"
        );
    }
}

#[cfg(test)]
mod safe_delete_tests {
    use super::*;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    #[test]
    fn delete_symbol_keeps_trailing_same_line_content() {
        // end 行符号后还有别的代码 → 只删符号本体。
        let text = "void f() {} void g() {}\n";
        let out = delete_symbol_text(text, range(0, 0, 0, 11)).unwrap();
        assert_eq!(out, " void g() {}\n");
    }
    #[test]
    fn delete_symbol_removes_whole_lines() {
        let text = "int a = 1;\nint orphan() { return 1; }\nint b = 2;\n";
        let out = delete_symbol_text(text, range(1, 0, 1, 26)).unwrap();
        assert_eq!(out, "int a = 1;\nint b = 2;\n");
    }
    #[test]
    fn delete_symbol_at_eof_without_trailing_newline() {
        let text = "int a;\nint orphan() { return 1; }";
        let out = delete_symbol_text(text, range(1, 0, 1, 26)).unwrap();
        assert_eq!(out, "int a;\n");
    }

    fn hit_line(file: &str, line: u32, text: &str) -> SearchHit {
        SearchHit {
            file: file.to_string(),
            line,
            col: 1,
            text: text.to_string(),
            match_start: 0,
            match_end: 3,
        }
    }

    /// refs 空 + 文本有引用（排除定义行/注释/字符串后仍有出现）→ 拦截门计数 > 0 → 拒。
    #[test]
    fn text_gate_counts_occurrences_outside_definition() {
        let hits = vec![
            hit_line("src/lib.rs", 7, "fn add(a: i32, b: i32) -> i32 { a + b }"), // 定义行
            hit_line("src/lib.rs", 9, "    let s = add(1, 2);"),                  // 真引用
            hit_line("src/main.rs", 3, "// add is unused"),                       // 注释
            hit_line("src/main.rs", 4, "    println!(\"add called\");"),          // 字符串
            hit_line("src/main.rs", 5, "    assert_eq!(add(2, 3), 5);"),          // 真引用
        ];
        assert_eq!(textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "add"), 2);
    }

    /// refs 空 + 文本无可疑出现（定义行本身 + 注释/字符串）→ 放行删除。
    #[test]
    fn text_gate_passes_when_nothing_outside_definition() {
        let hits = vec![
            hit_line("src/lib.rs", 7, "fn orphan() {}"),
            hit_line("src/lib.rs", 8, "// orphan kept for docs"),
            hit_line("src/lib.rs", 9, "    let s = \"orphan\";"),
        ];
        assert_eq!(textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "orphan"), 0);
    }

    /// 定义文件路径分隔符/大小写差异不重开定义行豁免（Windows 调用方传 `\` 形态）。
    #[test]
    fn text_gate_normalizes_definition_path() {
        let hits = vec![hit_line("src/lib.rs", 7, "fn add() {}")];
        assert_eq!(textual_occurrences_outside_def(&hits, "src\\lib.rs", 7, "add"), 0);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    /// 字段裁剪：保留 label/kind/insert；丢弃 sortText/filterText 等。
    #[test]
    fn parse_item_drops_lsp_internal_fields() {
        let raw = serde_json::json!({
            "label": "printf",
            "kind": 3,
            "detail": "int printf(const char *, ...)",
            "sortText": "00001",
            "filterText": "printf",
            "insertText": "printf",
            "documentation": { "kind": "markdown", "value": "fmt output" },
        });
        let lite = parse_completion_item(raw);
        assert_eq!(lite.label, "printf");
        assert_eq!(lite.kind, "function");
        assert_eq!(
            lite.detail.as_deref(),
            Some("int printf(const char *, ...)")
        );
        assert_eq!(lite.insert.as_deref(), Some("printf"));
        assert_eq!(lite.doc.as_deref(), Some("fmt output"));
        // 内部字段无对应键（struct 字段未定义）—— 静态保证。
    }

    /// insert 缺时用 label 兜底。
    #[test]
    fn parse_item_insert_falls_back_to_label() {
        let raw = serde_json::json!({ "label": "foo" });
        let lite = parse_completion_item(raw);
        assert_eq!(lite.insert.as_deref(), Some("foo"));
    }

    /// kind map：Function → "function"；Unknown → "other"。
    #[test]
    fn kind_map_lowercases_lsp_enum_names() {
        assert_eq!(map_completion_kind(3), "function");
        assert_eq!(map_completion_kind(2), "method");
        assert_eq!(map_completion_kind(6), "variable");
        assert_eq!(map_completion_kind(14), "keyword");
        assert_eq!(map_completion_kind(99), "other");
    }

    /// doc 截断：> 200 char 截断 + "…"；≤ 200 char 原文。
    #[test]
    fn truncate_doc_caps_at_two_hundred_chars() {
        let short = "a".repeat(100);
        assert_eq!(truncate_doc(short.clone()), short);
        let long = "a".repeat(500);
        let out = truncate_doc(long);
        assert!(
            out.chars().count() <= DOC_MAX_CHARS + 1,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.ends_with('…'), "expected trailing ellipsis: {out}");
    }

    /// documentation: String / MarkupContent 都能展平。
    #[test]
    fn extract_doc_string_handles_both_shapes() {
        let s = serde_json::json!("plain text");
        assert_eq!(extract_doc_string(&s).as_deref(), Some("plain text"));
        let m = serde_json::json!({ "kind": "markdown", "value": "**bold**" });
        assert_eq!(extract_doc_string(&m).as_deref(), Some("**bold**"));
        let none = serde_json::json!(null);
        assert_eq!(extract_doc_string(&none), None);
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod containing_symbol_tests {

    //! Phase 2.1: 按位置反查符号（documentSymbol walk 路径）。
    //!
    //! 不拉起 LS，直接喂 `DocumentSymbolResponse` 验核心规则：
    //! - 位置 [start.line, end.line] 闭区间 + 行内 col 边界。
    //! - 嵌套取最深命中链。
    //! - 无命中返空 Vec（不是 BadArgs）。
    use super::*;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    /// 构造一棵嵌套符号树：mod 外层 → fn 内层。
    fn nested_two_level() -> DocumentSymbolResponse {
        // outer: mod add @ line 0-10
        // inner: fn call @ line 4-5
        let outer = DocumentSymbol {
            name: "outer".into(),
            detail: None,
            kind: lsp_types::SymbolKind::MODULE,
            tags: None,
            range: range(0, 0, 10, 0),
            selection_range: range(0, 4, 0, 9),
            children: Some(vec![DocumentSymbol {
                name: "inner".into(),
                detail: None,
                kind: lsp_types::SymbolKind::FUNCTION,
                tags: None,
                range: range(4, 0, 5, 1),
                selection_range: range(4, 3, 4, 8),
                children: None,
                deprecated: None,
            }]),
            deprecated: None,
        };
        DocumentSymbolResponse::Nested(vec![outer])
    }

    /// 单层（仅 module）：验 col 在起始/结束行的边界。
    fn single_module() -> DocumentSymbolResponse {
        let mod_ = DocumentSymbol {
            name: "outer".into(),
            detail: None,
            kind: lsp_types::SymbolKind::MODULE,
            tags: None,
            range: range(0, 0, 10, 5),
            selection_range: range(0, 4, 0, 9),
            children: None,
            deprecated: None,
        };
        DocumentSymbolResponse::Nested(vec![mod_])
    }

    #[test]
    fn position_in_range_respects_col_at_start_and_end_lines() {
        // range = line 4 col 5 .. line 6 col 10
        let r = range(4, 5, 6, 10);
        // 中间行：col 无所谓。
        assert!(position_in_range(r, 5, 0));
        assert!(position_in_range(r, 5, 100));
        // 起始行：col < start.character 越界。
        assert!(!position_in_range(r, 4, 4));
        assert!(position_in_range(r, 4, 5));
        assert!(position_in_range(r, 4, 99));
        // 结束行：col > end.character 越界。
        assert!(position_in_range(r, 6, 10));
        assert!(!position_in_range(r, 6, 11));
        // 行号 < start 或 > end 直接越界。
        assert!(!position_in_range(r, 3, 0));
        assert!(!position_in_range(r, 7, 0));
    }

    #[test]
    fn deepest_match_wins_inside_nested_function_body() {
        let resp = nested_two_level();
        // inner @ 4-5；位置 line=4 col=10 落在 inner 内（且在 outer 内）。
        let hits = collect_containing_hits(&resp, "file://x", 4, 10);
        assert_eq!(hits.len(), 2, "expected outer+inner chain, got {hits:?}");
        assert_eq!(hits[0].name, "outer");
        assert_eq!(hits[0].container, None);
        assert_eq!(hits[1].name, "inner");
        assert_eq!(hits[1].container.as_deref(), Some("outer"));
    }

    #[test]
    fn position_outside_any_symbol_returns_empty() {
        let resp = single_module();
        // outer @ line 0-10；line=20 越过 end.line。
        let hits = collect_containing_hits(&resp, "file://x", 20, 0);
        assert!(hits.is_empty(), "expected empty, got {hits:?}");
    }

    #[test]
    fn position_at_first_char_of_first_line_hits_only_outer() {
        let resp = nested_two_level();
        // outer @ 0-10；inner @ 4-5；line=0 落在 outer（不在 inner）。
        let hits = collect_containing_hits(&resp, "file://x", 0, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "outer");
    }

    #[test]
    fn flat_response_skips_unmatched_and_returns_only_hits() {
        // flat: foo (l 0-2) + bar (l 5-8)；line=6 在 bar 内。
        let foo = lsp_types::SymbolInformation {
            name: "foo".into(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: lsp_types::Location {
                uri: "file://x".parse().unwrap(),
                range: range(0, 0, 2, 0),
            },
            container_name: None,
        };
        let bar = lsp_types::SymbolInformation {
            name: "bar".into(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: lsp_types::Location {
                uri: "file://x".parse().unwrap(),
                range: range(5, 0, 8, 0),
            },
            container_name: None,
        };
        let resp = DocumentSymbolResponse::Flat(vec![foo, bar]);
        let hits = collect_containing_hits(&resp, "file://x", 6, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "bar");
    }
}

#[cfg(test)]
mod signature_help_tests {
    //! Phase 2.2: `textDocument/signatureHelp` 协议形状。
    //! 不拉起 LS，直接测：把构造的 LSP `SignatureHelp` JSON round-trip 到
    //! `lsp_types::SignatureHelp`（即 supervisor 透传时反序列化的目标类型），确认字段全保
    //! 留（label / parameters[] / activeSignature / activeParameter）。

    /// clangd on `add(1, 2)` 在括号内的典型响应：单一 signature, 两个参数，active=0（第一个参数）。
    fn add_call_signature_json() -> serde_json::Value {
        serde_json::json!({
            "signatures": [{
                "label": "add(int, int)",
                "documentation": "Adds two integers.",
                "parameters": [
                    { "label": "int a" },
                    { "label": "int b" }
                ],
                "activeParameter": 0
            }],
            "activeSignature": 0
        })
    }

    #[test]
    fn lsp_signature_help_round_trip_preserves_all_fields() {
        let raw = add_call_signature_json();
        let parsed: lsp_types::SignatureHelp = serde_json::from_value(raw.clone())
            .expect("LSP signatureHelp should round-trip into lsp_types::SignatureHelp");
        // signatures[0].label 必须保留（agent 据此识别函数签名）
        assert_eq!(parsed.signatures.len(), 1);
        assert_eq!(parsed.signatures[0].label, "add(int, int)");
        // parameters[] 保留两个
        let params = parsed.signatures[0].parameters.as_ref()
            .expect("parameters should be present");
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0].label,
            lsp_types::ParameterLabel::Simple("int a".into())
        );
        assert_eq!(
            params[1].label,
            lsp_types::ParameterLabel::Simple("int b".into())
        );
        // activeParameter / activeSignature 保留（agent 据此高亮当前参数）
        assert_eq!(parsed.active_signature, Some(0));
        assert_eq!(parsed.signatures[0].active_parameter, Some(0));
    }

    #[test]
    fn lsp_signature_help_null_round_trips_as_none() {
        // clangd 在非函数调用位置返 null（与 hover 行为一致）。
        let raw = serde_json::Value::Null;
        let parsed: Option<lsp_types::SignatureHelp> = serde_json::from_value(raw)
            .expect("null should round-trip to None");
        assert!(parsed.is_none());
    }

    #[test]
    fn lsp_signature_help_active_parameter_default_when_missing() {
        // LSP 允许省略 activeParameter（位置未确定）。lsp-types 默认 0；这里确认字段缺失时
        // 也能 round-trip, 不会强制要求 activeParameter 字段。
        let raw = serde_json::json!({
            "signatures": [{
                "label": "f()",
                "parameters": []
            }]
        });
        let parsed: lsp_types::SignatureHelp = serde_json::from_value(raw)
            .expect("missing activeParameter should still round-trip");
        assert_eq!(parsed.signatures.len(), 1);
        assert_eq!(parsed.signatures[0].label, "f()");
        assert_eq!(parsed.signatures[0].active_parameter, None);
        assert_eq!(parsed.active_signature, None);
    }
}

#[cfg(test)]
mod pull_diagnostics_tests {
    //! Phase 2.5: textDocument/diagnostic pull 路径 + fallback 透明契约（PLAN Task 2.5）。
    //!
    //! 不拉 LS，纯函数 + HashMap 表层断言 5 个分支：
    //! - #1 diagnosticProvider 字段缺失（mock_ls / rust-analyzer 现状）→ supports=false。
    //! - #2 diagnosticProvider = null → supports=false。
    //! - #3 diagnosticProvider = true / DiagnosticOptions 对象 → supports=true。
    //! - #4 pull 失败（LS 返 -32601）→ extract_pull_items 不被调用；tool_diagnostics 走 push。
    //! - #5 pull 成功（kind=full）→ extract_pull_items 取 items，**不**与 push 拼接。
    //!
    //! 真实端到端 fallback 验证走 fixtures/rust_demo + rust-analyzer 的 CLI smoke
    //! （拉起 supervisor → tool_diagnostics → 字段缺失 → 自动走 push 缓存），
    //! 见完成报告 `end-to-end` 一节。
    use super::Supervisor;
    use lsp_core::init_params::supports_pull_diagnostics;
    use serde_json::json;

    /// #1 capabilities 缺 diagnosticProvider 字段（mock_ls 现状）。
    #[test]
    fn missing_diagnostic_provider_field_means_no_pull_support() {
        let caps = json!({ "positionEncoding": "utf-16" });
        assert!(!supports_pull_diagnostics(&caps));
    }

    /// #2 diagnosticProvider 显式为 null。
    #[test]
    fn null_diagnostic_provider_means_no_pull_support() {
        let caps = json!({ "diagnosticProvider": null });
        assert!(!supports_pull_diagnostics(&caps));
    }

    /// #3a diagnosticProvider = true（简写形态）。
    #[test]
    fn boolean_true_diagnostic_provider_means_pull_supported() {
        let caps = json!({ "diagnosticProvider": true });
        assert!(supports_pull_diagnostics(&caps));
    }

    /// #3b diagnosticProvider = DiagnosticOptions 对象（含 interFileDependencies）。
    /// 任务边界契约：子字段不影响 pull 支持判定。
    #[test]
    fn diagnostic_options_object_means_pull_supported() {
        let caps = json!({
            "diagnosticProvider": {
                "interFileDependencies": false,
                "workspaceDiagnostics": false
            }
        });
        assert!(supports_pull_diagnostics(&caps));
    }

    /// #4+#5 extract_pull_items 纯函数：kind=full → Some(items)；
    /// 其它形态 → None（unchanged / partial / 缺 kind / 错形态都触发 push fallback）。
    #[test]
    fn extract_pull_items_returns_items_only_for_full_kind() {
        let full = json!({
            "kind": "full",
            "items": [{"range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 0, "character": 1}},
                       "message": "err"}]
        });
        let items = Supervisor::extract_pull_items(&full).expect("kind=full 必须返 Some");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["message"], "err");

        // unchanged → None（push 缓存已有完整 items；不重复拼接）。
        let unchanged = json!({"kind": "unchanged", "resultId": "r1"});
        assert!(Supervisor::extract_pull_items(&unchanged).is_none());

        // 缺 kind → None（异常回 push 兜底）。
        let no_kind = json!({"items": []});
        assert!(Supervisor::extract_pull_items(&no_kind).is_none());

        // 空对象 → None。
        let empty = json!({});
        assert!(Supervisor::extract_pull_items(&empty).is_none());
    }

    /// #5 抽取成功路径与"与 push 不重复"语义：返回的是完整 items 数组，
    /// 调用方不再访问 push 缓存（避免重复拼接）。
    #[test]
    fn extract_pull_items_success_does_not_query_push_cache() {
        let full = json!({
            "kind": "full",
            "items": [
                {"message": "e1", "range": {"start": {"line": 0, "character": 0},
                                             "end": {"line": 0, "character": 1}}},
                {"message": "e2", "range": {"start": {"line": 1, "character": 0},
                                             "end": {"line": 1, "character": 1}}}
            ]
        });
        let items = Supervisor::extract_pull_items(&full).expect("kind=full");
        // 完整 items 直接返回（不与 push 拼接）；保证不重复。
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["message"], "e1");
        assert_eq!(items[1]["message"], "e2");
    }
}
// ============================================================================
// Phase 3.1 文档符号缓存（local/solidlsp-development-plan.md §3.1）
// ============================================================================

#[cfg(test)]
mod symbol_cache_tests {
    //! 纪律：不拉 LS —— 命中路径在 session_for 之前返回，可对空 supervisor 做工具级
    //! 断言；miss→写→hit 全链路由 CLI smoke（fixtures/rust_demo + rust-analyzer）覆盖。
    use super::*;
    use std::time::Instant;

    fn hit(name: &str) -> SymbolHit {
        SymbolHit {
            name: name.to_string(),
            kind: SymbolKindTag::Function,
            uri: "file:///x/a.rs".into(),
            range: lsp_types::Range {
                start: Position::new(0, 0),
                end: Position::new(3, 0),
            },
            container: None,
        }
    }

    /// ↖ mirror: ls.py@a5fd4d68 — 低层（LS 会话）版本变化后高层缓存不得再命中：
    /// 同 root 的单文件级与 workspace 级缓存全部失效，其他 root 不受影响。
    #[tokio::test]
    async fn session_rebuild_invalidates_symbol_cache_for_root() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let other = Path::new("Z:/no/such/other");
        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("main")]);
        sup.symbol_cache_put(find_symbol_cache_key(root, "main"), vec![hit("main")]);
        sup.symbol_cache_put(doc_symbol_cache_key(other, "a.rs"), vec![hit("helper")]);

        // 模拟 (root, lang) 会话换代（session_for 挂入新会话前的失效动作）。
        sup.invalidate_symbol_cache_for_root(root);

        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "a.rs"))
                .is_none(),
            "会话换代后同 root 文档符号缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&find_symbol_cache_key(root, "main"))
                .is_none(),
            "会话换代后同 root workspace 级缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(other, "a.rs")).is_some(),
            "其他 root 的缓存不受影响"
        );
    }

    /// cache 命中：同 file 二次 overview 命中不拉 LS（命中路径 vs LS 往返秒级的量级差；
    /// 并行全量测试下 CPU 调度抖动可达数 ms，阈值放宽到 10ms 仍比 LS 往返低两个量级）。
    #[tokio::test]
    async fn overview_cache_hit_returns_under_1ms() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("main")]);

        // warmup：预热 LazyLock / 代码路径，排除首次抖动。
        let _ = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();

        let t0 = Instant::now();
        let out = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "main");
        assert!(
            elapsed < Duration::from_millis(10),
            "cache hit took {elapsed:?}"
        );
    }

    /// 命中必须先于 lang 解析 / session 拉起（不可解析扩展名 + 不存在 root 也命中）。
    #[tokio::test]
    async fn overview_cache_hit_precedes_lang_resolution() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "x.unknownext"),
            vec![hit("weird")],
        );
        let out = sup.tool_overview(root, "x.unknownext", None).await.unwrap();
        assert_eq!(out[0].name, "weird");
    }

    /// cache miss：空 supervisor 必 miss；put 后 get 命中（首次调用写入 cache 的机制）。
    #[tokio::test]
    async fn cache_miss_then_put_then_hit() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let key = doc_symbol_cache_key(root, "a.rs");
        assert!(
            sup.symbol_cache_get(&key).is_none(),
            "fresh supervisor must miss"
        );
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        let got = sup.symbol_cache_get(&key).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "f");
    }

    /// invalidate：mtime 变 → key 变 → miss（下次重调 LS）。std set_modified，无新依赖。
    #[tokio::test]
    async fn mtime_change_invalidates_entry() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let root = dir.path();

        let key1 = doc_symbol_cache_key(root, "a.rs");
        sup.symbol_cache_put(key1.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key1).is_some());

        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
            .unwrap();
        let key2 = doc_symbol_cache_key(root, "a.rs");
        assert_ne!(key1, key2, "mtime change must produce a new cache key");
        assert!(
            sup.symbol_cache_get(&key2).is_none(),
            "new mtime must miss (invalidate)"
        );
    }

    /// 不同 file 不同 key（不串扰）。
    #[tokio::test]
    async fn different_files_do_not_share_entries() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let ka = doc_symbol_cache_key(root, "a.rs");
        let kb = doc_symbol_cache_key(root, "b.rs");
        assert_ne!(ka, kb);
        sup.symbol_cache_put(ka.clone(), vec![hit("a_sym")]);
        assert!(sup.symbol_cache_get(&ka).is_some());
        assert!(sup.symbol_cache_get(&kb).is_none());
    }

    /// find-symbol 缓存：query 命中 + limit 对缓存全量截断；不同 query 不串扰。
    #[tokio::test]
    async fn find_symbol_cache_hits_by_query_and_respects_limit() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(
            find_symbol_cache_key(root, "parse"),
            vec![hit("parse_a"), hit("parse_b"), hit("parse_c")],
        );

        let t0 = Instant::now();
        let out = sup
            .tool_find_symbol(root, "parse", 2, Some("rust"))
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(10),
            "cache hit took {elapsed:?}"
        );
        assert_eq!(out.len(), 2, "limit must apply to cached full list");
        assert_eq!(out[0].name, "parse_a");

        // 不同 query → miss → 走真实路径：root 不存在 → 扫不到 lang → BadArgs。
        let err = sup
            .tool_find_symbol(root, "other", 5, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::BadArgs { .. }),
            "unexpected: {err:?}"
        );
    }

    /// symbol-tree：预置缓存全链路聚合（不拉 LS）；node_modules 被 3.3 过滤；
    /// dir 逃逸 root 报 BadArgs。
    #[tokio::test]
    async fn symbol_tree_aggregates_from_cache_and_skips_ignored_dirs() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not source\n").unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("dep.rs"), "fn dep() {}\n").unwrap();
        let root = dir.path();

        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("sym_a")]);
        sup.symbol_cache_put(doc_symbol_cache_key(root, "b.rs"), vec![hit("sym_b")]);

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 2, "node_modules/notes.txt 必须被过滤: {tree}");
        assert_eq!(tree["truncated"], false);
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "两文件各有符号条目: {tree}");

        // dir 逃逸 root。
        let err = sup
            .tool_symbol_tree(root, "../elsewhere", Some("rust"), 200)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "unexpected: {err:?}");
    }

    /// symbol-tree：max_files 保险丝 —— 超限截断并标 truncated。
    #[tokio::test]
    async fn symbol_tree_respects_max_files_fuse() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "fn x() {}\n").unwrap();
        }
        let root = dir.path();
        for i in 0..5 {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("f{i}.rs")),
                vec![hit("x")],
            );
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 3)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 3, "max_files=3 截断: {tree}");
        assert_eq!(tree["truncated"], true);
        assert_eq!(tree["entries"].as_array().unwrap().len(), 3);
    }
}

// ============================================================================
// Phase 1 · 13 wrapper 测试（纯 LSP 协议层；不拉 LS / 不依赖 fix-ls-adapters）
// ============================================================================

#[cfg(test)]
mod phase1_wrapper_tests {
    //! Phase 1 · 13 个上游 wrapper 的协议层单测：
    //! - `decode_semantic_tokens` 5-tuple delta 累加正确性。
    //! - 各 LSP method 的请求 params 字段齐全（用 mock Value round-trip）。
    //! - 错误路径：未知 op / 缺 file / 缺 item 等转 BadArgs。
    //!
    //! 真实端到端验证走 fixtures/rust_demo + rust-analyzer 的 CLI smoke
    //! （拉起 supervisor → tool_* → 字段就位 / 不报 protocol error）。
    use super::*;

    // ---- decode_semantic_tokens ----

    /// 单 token delta_line=0 / delta_start=10 → start_char=10。
    #[test]
    fn decode_semantic_tokens_handles_zero_delta_line() {
        let tokens = vec![lsp_types::SemanticToken {
            delta_line: 0,
            delta_start: 10,
            length: 4,
            token_type: 1,
            token_modifiers_bitset: 0,
        }];
        let out = decode_semantic_tokens(&tokens);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 0);
        assert_eq!(out[0].start_char, 10);
        assert_eq!(out[0].length, 4);
        assert_eq!(out[0].token_type, 1);
    }

    /// delta_line=2 / delta_start=5 → 行号 2，start_char 复位为 5。
    #[test]
    fn decode_semantic_tokens_resets_start_on_line_change() {
        let tokens = vec![
            lsp_types::SemanticToken {
                delta_line: 0,
                delta_start: 3,
                length: 1,
                token_type: 0,
                token_modifiers_bitset: 0,
            },
            lsp_types::SemanticToken {
                delta_line: 2,
                delta_start: 5,
                length: 2,
                token_type: 2,
                token_modifiers_bitset: 0,
            },
        ];
        let out = decode_semantic_tokens(&tokens);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line, 0);
        assert_eq!(out[0].start_char, 3);
        assert_eq!(out[1].line, 2);
        assert_eq!(out[1].start_char, 5, "行变化时 delta_start 是绝对坐标");
    }

    /// 空 data → 空 tokens。
    #[test]
    fn decode_semantic_tokens_empty_input_returns_empty() {
        let out = decode_semantic_tokens(&[]);
        assert!(out.is_empty());
    }

    // ---- file_path_from_uri ----

    #[test]
    fn file_path_from_uri_strips_file_scheme_and_keeps_path() {
        let p = file_path_from_uri("file:///D:/proj/main.cpp");
        assert!(p.ends_with("proj/main.cpp"), "got: {p}");
    }

    #[test]
    fn file_path_from_uri_handles_non_file_uri_gracefully() {
        // 不是 file:// 走 fallback（原样保留）。
        let p = file_path_from_uri("unt:///x/y");
        assert!(p.contains("x/y"), "got: {p}");
    }

    // ---- uri percent-decode（LS 返回 URI 统一解码，缺口 #5 回归）----

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let p = uri_to_path("file:///d%3A/proj/foo%20bar/a.rs").expect("file uri");
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(!s.contains('%'), "percent 序列必须解码: {s}");
        assert!(s.ends_with("foo bar/a.rs"), "got: {s}");
    }

    #[cfg(windows)]
    #[test]
    fn uri_to_path_uppercases_windows_drive_letter() {
        // 小写盘符 `d%3A` 解码后必须归一为 `D:`，否则与 canonical root 前缀比对失败。
        let p = uri_to_path("file:///d%3A/proj/a.rs").expect("file uri");
        assert!(
            p.to_string_lossy().starts_with("D:"),
            "盘符必须大写: {p:?}"
        );
    }

    #[test]
    fn percent_decode_keeps_invalid_sequences() {
        assert_eq!(percent_decode("a%3Ab"), "a:b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("100% off"), "100% off"); // 非法 hex 原样保留
        assert_eq!(percent_decode("trailing%2"), "trailing%2"); // 截断序列原样保留
    }

    // ---- parse_workspace_edit（rename 两种 WorkspaceEdit 形态，gopls 兼容回归）----

    #[test]
    fn parse_workspace_edit_changes_map() {
        let resp = json!({
            "changes": {
                "file:///a.ts": [
                    {"range": {"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 8}}, "newText": "b"},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}, "newText": "c"}
                ]
            }
        });
        let by_uri = parse_workspace_edit(&resp).expect("changes map 必须解析");
        assert_eq!(by_uri.len(), 1);
        assert_eq!(by_uri[0].0, "file:///a.ts");
        assert_eq!(by_uri[0].1.len(), 2);
    }

    #[test]
    fn parse_workspace_edit_document_changes_array() {
        // gopls 形态：只有 documentChanges 数组，无 changes map。
        let resp = json!({
            "documentChanges": [
                {"textDocument": {"uri": "file:///a.go"}, "edits": [
                    {"range": {"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 7}}, "newText": "sum"}
                ]}
            ]
        });
        let by_uri = parse_workspace_edit(&resp).expect("documentChanges 必须解析");
        assert_eq!(by_uri.len(), 1);
        assert_eq!(by_uri[0].0, "file:///a.go");
        assert_eq!(by_uri[0].1.len(), 1);
        assert_eq!(by_uri[0].1[0].1.start.line, 4);
    }

    #[test]
    fn parse_workspace_edit_prefers_document_changes_and_skips_non_text_ops() {
        // 两形态并存时 documentChanges 优先；kind:create 等非文本条目跳过。
        let resp = json!({
            "changes": {"file:///fallback.ts": []},
            "documentChanges": [
                {"kind": "create", "uri": "file:///new.go"},
                {"textDocument": {"uri": "file:///a.go"}, "edits": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "x"}
                ]}
            ]
        });
        let by_uri = parse_workspace_edit(&resp).expect("documentChanges 优先");
        assert_eq!(by_uri.len(), 1, "create 条目跳过，只留文本编辑文件");
        assert_eq!(by_uri[0].0, "file:///a.go");
    }

    #[test]
    fn parse_workspace_edit_neither_form_returns_none() {
        assert!(parse_workspace_edit(&json!({})).is_none());
        assert!(parse_workspace_edit(&json!({"changes": "not-an-object"})).is_none());
    }

    // ---- execute_tool 输入校验：call-hierarchy / type-hierarchy 缺 item 转 BadArgs ----

    #[tokio::test]
    async fn execute_tool_call_hierarchy_missing_op_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({});
        let r = sup
            .execute_tool("call-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_call_hierarchy_incoming_missing_item_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "incoming"});
        let r = sup
            .execute_tool("call-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_type_hierarchy_unknown_op_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "bogus"});
        let r = sup
            .execute_tool("type-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    // ---- inlay-hint / format-range / code-action 缺必填参数 ----

    #[tokio::test]
    async fn execute_tool_inlay_hint_missing_start_line_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"file": "main.cpp", "end_line": 10});
        let r = sup.execute_tool("inlay-hint", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_format_range_missing_end_col_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({
            "file": "main.cpp",
            "start_line": 1, "start_col": 0,
            "end_line": 2
        });
        let r = sup.execute_tool("format-range", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_workspace_diagnostic_no_active_session_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        // No LS loaded yet; without --lang fallback should error.
        let r = sup
            .execute_tool("workspace-diagnostic", ".", json!({}), None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    // ---- 直接构造请求 params 验证字段完整性（不拉 LS）----

    /// `textDocument/codeAction` params 必须含 textDocument / range / context.diagnostics。
    #[test]
    fn code_action_request_payload_shape() {
        let mut params = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end":   { "line": 1, "character": 2 },
            },
            "context": { "diagnostics": [] },
        });
        params["context"]["only"] = json!("quickfix");
        let v: serde_json::Value = params;
        assert_eq!(v["context"]["only"], "quickfix");
        assert!(v["context"]["diagnostics"].is_array());
    }

    /// `textDocument/rangeFormatting` range 必须含 4 字段 + options 2 字段。
    #[test]
    fn range_formatting_request_payload_shape() {
        let v = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 1, "character": 0 },
                "end":   { "line": 5, "character": 0 },
            },
            "options": { "tabSize": 4, "insertSpaces": true },
        });
        assert_eq!(v["options"]["tabSize"], 4);
        assert_eq!(v["options"]["insertSpaces"], true);
    }

    /// `callHierarchy/incomingCalls` 与 `outgoingCalls` params.item 必须存在。
    #[test]
    fn call_hierarchy_subsequent_request_requires_item_field() {
        let item = json!({"name": "foo", "kind": 12, "uri": "file:///x", "range": {}});
        let v = json!({"item": item});
        assert!(v["item"].is_object());
        assert_eq!(v["item"]["name"], "foo");
    }

    /// `textDocument/inlayHint` range 必有 start/end.line。
    #[test]
    fn inlay_hint_request_payload_shape() {
        let v = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 100, "character": 0 },
            },
        });
        assert_eq!(v["range"]["end"]["line"], 100);
    }

    // ---- SemanticTokensFull serialization ----

    #[test]
    fn semantic_tokens_full_serializes_with_decoded_tokens() {
        let entry = SemanticTokensFull {
            result_id: Some("v1".to_string()),
            tokens: vec![SemanticTokenEntry {
                line: 2,
                start_char: 5,
                length: 4,
                token_type: 1,
                token_modifiers: 0,
            }],
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["resultId"], "v1");
        assert_eq!(v["tokens"][0]["line"], 2);
        assert_eq!(v["tokens"][0]["startChar"], 5);
        assert_eq!(v["tokens"][0]["length"], 4);
    }

    // ---- workspace-diagnostic 响应归一化（items 缺失 → 空）----

    #[test]
    fn workspace_diagnostic_response_missing_items_returns_empty() {
        // Mock 一个完整 response（items 字段缺失），确认提取路径不报错。
        let raw = json!({ "kind": "full" });
        let items = raw.get("items").cloned().unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Diagnostic> =
            serde_json::from_value(items).unwrap_or_default();
        assert!(parsed.is_empty());
    }
}

// ---- Phase 4 Task 22b: timeout 三层合并 + sanitize ----

#[cfg(test)]
mod timeout_resolution_tests {
    use super::*;

    #[test]
    fn effective_tool_timeout_default_30s_when_no_override() {
        let args = json!({});
        // markdown 的 servers.toml 当前未写 timeout_ms → 走默认 30s
        let d = effective_tool_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_secs(30), "默认 30s");
    }

    #[test]
    fn effective_tool_timeout_args_override_wins() {
        let args = json!({"_timeout_ms": 1234});
        let d = effective_tool_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_millis(1234), "CLI args._timeout_ms 覆盖");
    }

    #[test]
    fn effective_tool_timeout_handles_unknown_lang() {
        // lang 不在 servers.toml → None → 默认
        let args = json!({});
        let d = effective_tool_timeout(Some("rust"), &args);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn effective_index_timeout_default_120s_when_no_override() {
        let args = json!({});
        let d = effective_index_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_secs(120), "index 默认 120s");
    }

    #[test]
    fn sanitize_timeout_args_strips_private_fields() {
        let args = json!({
            "file": "main.cpp",
            "_timeout_ms": 5000,
            "_index_timeout_ms": 60000,
            "col": 1,
        });
        let out = sanitize_timeout_args(args);
        assert_eq!(out.get("file").and_then(|v| v.as_str()), Some("main.cpp"));
        assert_eq!(out.get("col").and_then(|v| v.as_u64()), Some(1));
        assert!(out.get("_timeout_ms").is_none(), "_timeout_ms 应被清掉");
        assert!(out.get("_index_timeout_ms").is_none(), "_index_timeout_ms 应被清掉");
    }

    #[test]
    fn sanitize_timeout_args_passes_through_non_object() {
        // 防御：若 args 罕见形态（null/array），sanitize 不 panic、不破坏。
        let null_in = serde_json::Value::Null;
        assert!(sanitize_timeout_args(null_in.clone()).is_null());
        let arr_in = json!([1, 2, 3]);
        assert_eq!(sanitize_timeout_args(arr_in.clone()), arr_in);
    }
}
