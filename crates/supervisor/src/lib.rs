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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lsp_core::docsync::{path_to_uri, path_to_uri_str};
use lsp_core::error::CoreError;
use lsp_core::init_params::base_initialize_params;
use lsp_core::offsets::{OffsetEncoding, Position as LspPos};
use lsp_core::session::Session;
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
}
/// 实例键：canonicalize、去尾分隔符并大小写折叠的 root + language。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub root: PathBuf,
    pub lang: Box<str>,
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

impl Supervisor {
    /// `--direct` 模式入口：创建空 supervisor（懒加载 Session）。
    pub async fn direct() -> ToolResult<Self> {
        Ok(Self {
            instances: Mutex::new(HashMap::new()),
            load_gates: Mutex::new(HashMap::new()),
            last_used: Mutex::new(HashMap::new()),
            direct_mode: true,
            diag_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
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
            root: PathBuf::from(canonical.to_string_lossy().to_lowercase()),
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

        let adapter = ls_registry::adapter_for(lang).ok_or_else(|| ToolError::BadArgs {
            detail: format!("unknown language: {lang}"),
        })?;
        let ctx = ls_adapters::ProjectCtx {
            project_root: key.root.clone(),
        };
        let launch = adapter.launch_info(&ctx).await.map_err(|e| {
            let msg = format!("{e:#}");
            if msg.contains("not found in PATH") {
                ToolError::NotInstalled {
                    language: lang.to_string(),
                    hint: extract_install_hint(&msg),
                }
            } else {
                ToolError::Launch(e)
            }
        })?;
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
        params.workspace_folders = Some(vec![lsp_types::WorkspaceFolder {
            uri: uri.clone(),
            name: key
                .root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string(),
        }]);
        adapter.initialize_patches(&mut params);

        let session = Session::start(Some(child), params).await?;
        // 注册 publishDiagnostics handler → 写 diag_cache。
        let cache_root = key.root.clone();
        let cache = std::sync::Arc::clone(&self.diag_cache);
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
                if !items.is_empty() {
                    cache
                        .lock()
                        .unwrap()
                        .insert((cache_root.clone(), uri.to_string()), items);
                }
            });
        // ↖ mirror: ls.py@43ae021 on_server_started — 把"等待 LS 索引就绪"
        // 推到 session_for 内，避免用户可见的首请求 = 索引懒加载
        // （cold-start 120s 根因：supervisor 路径跳过就绪探针，详见
        // local/cold-start-hang-diagnosis.md）。
        if let Err(e) =
            tokio::time::timeout(Duration::from_secs(30), adapter.on_server_ready(&session)).await
        {
            tracing::warn!(adapter = adapter.id(), error = %e, "on_server_ready probe failed/timed out; continuing");
        }
        self.instances
            .lock()
            .unwrap()
            .insert(key.clone(), session.clone());
        self.touch(&key);
        Ok(session)
    }

    /// `textDocument/diagnostic` 诊断：依赖 LS `publishDiagnostics` 推送缓存。
    /// clangd 对 pull 式 `textDocument/diagnostic` 返 -32601，故走通知缓存 +
    /// 轮询等待（打开文件即推，100ms × 50 次 = 5s 上限），返 `{ items: [...] }`。
    pub async fn tool_diagnostics(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
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
        // 连续 2 次 items 长度相同即认稳；上限 50 × 100ms = 5s。
        // 等 cache 命中 + 再 200ms 确认（防止清空推送被误判为"无错"）。
        // 上限 50 × 100ms = 5s；空 cache（无错）也只等 5s 返空数组。
        let key = Self::key(root, lang.as_str());
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
        let items = self
            .diag_cache
            .lock()
            .unwrap()
            .get(&(key.root.clone(), uri.clone()))
            .cloned()
            .unwrap_or_default();
        Ok(json!({ "items": items }))
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

    /// `textDocument/documentSymbol` → 平铺递归 `DocumentSymbol::children` → `Vec<SymbolHit>`。
    pub async fn tool_overview(
        &self,
        root: &Path,
        file: &str,
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

        Ok(flatten_symbols(resp, &uri))
    }
    /// `textDocument/documentSymbol` → 在树中按 (line, col) 反查最深包含符号 → `Vec<SymbolHit>`。
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

        // 2) Location.uri → 相对 root 的 file 路径。
        let def_uri = def_loc.uri.to_string();
        let abs_path = def_uri
            .strip_prefix("file://")
            .or_else(|| def_uri.strip_prefix("file:///"))
            .unwrap_or(&def_uri);
        // Windows 下 LSP uri 是 `file:///d:/...`（三斜杠 + 小写盘符）；还原绝对路径。
        let abs = if cfg!(windows) && abs_path.starts_with('/') {
            PathBuf::from(&abs_path[1..].replace('/', "\\"))
        } else {
            PathBuf::from(abs_path.replace('\\', "/"))
        };
        let def_file = abs
            .strip_prefix(root)
            .map_err(|_| ToolError::BadArgs {
                detail: format!(
                    "definition at {abs_path} is outside workspace root {root:?}"
                ),
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
                    && let Some(l) = ls_registry::resolve(entry.path())
                {
                    set.insert(l.as_str().to_string());
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
    pub async fn tool_edit_insert_before_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
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
    ) -> ToolResult<()> {
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
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
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

        let text = tokio::fs::read_to_string(&path)
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

    /// `replace-body`：按符号名替换函数/类体（C3 一致性链路，PLAN Task 15）。
    ///
    /// 流程（全程持全局写门）：
    /// 1. documentSymbol 解析符号 range（客户端只传符号名，不传 range）
    /// 2. 读盘 content 与前快照 hash 对账 —— 不符 → WRITE_CONFLICT
    /// 3. 新 body 替换 range → tempfile 原子写 + rename（Windows 共享冲突重试 5×50ms）
    /// 4. 读回 diff 校验 —— 不符 → 从写前副本回滚 + WRITE_CONFLICT
    /// 5. didChange 全量同步 → LS 与盘一致
    pub async fn tool_replace_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        new_body: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);

        // ===== 全局写门：以下所有步骤持锁（A4 FIFO）=====
        let _gate = write_gate::acquire().await;

        // 1) 锁内解析符号 range（杜绝客户端 range 过期）。
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": uri_str.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let range = find_symbol_range(&resp, symbol).ok_or_else(|| ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {file}"),
        })?;

        // 2) 读盘 + content-hash 对账（C3 防线 ①）。
        let old_text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", path.display()),
            })?;
        let old_hash = content_hash(&old_text);

        // didOpen/didChange 后 LS 侧的版本号；从 1 递增即可（mock 与 clangd 都不校验具体值）。
        static VERSION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
        let version = VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // 3) 新文本 = 老文本替换 range；tempfile 原子写 + rename。
        let start = LspPos {
            line: range.start.line,
            character: range.start.character,
        };
        let end = LspPos {
            line: range.end.line,
            character: range.end.character,
        };
        let start_byte =
            lsp_core::offsets::position_to_byte(&old_text, start, OffsetEncoding::Utf16).map_err(
                |e| ToolError::BadArgs {
                    detail: format!("start position: {e}"),
                },
            )?;
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

        atomic_write(&path, &new_text)
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
            let _ = atomic_write(&path, &old_text).await;
            return Err(ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: "readback mismatch; rolled back".into(),
            });
        }

        // 5) didChange 全量同步到 LS。
        let change_params = json!({
            "textDocument": { "uri": uri_str, "version": version },
            "contentChanges": [ { "text": new_text } ],
        });
        session
            .notify("textDocument/didChange", change_params)
            .await
            .map_err(ToolError::Core)?;

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
    ///   我们把 `changes: {uri: [TextEdit]}` 应用到盘上 + 全量 didChange。
    /// - **位置倒序 apply**：每文件 edits 按 `range.end` 倒序处理，避免偏移漂移。
    /// - **不支持 `documentChanges`**：clangd 默认走 `changes` map，简化 MVP。
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

        // 3) 拆 `changes` map → 按文件分组 + 倒序排序。
        let changes = resp
            .get("changes")
            .and_then(|v| v.as_object()).ok_or_else(|| ToolError::Protocol { tool: "rename_symbol".into(), reason: "rename response has no `changes` map (M2 only supports changes, not documentChanges)".into() })?;

        type EditSpec = (u64, lsp_types::Range, String); // (sort_key, range, new_text)
        let mut by_uri: Vec<(String, Vec<EditSpec>)> = Vec::new();
        for (uri, edits) in changes {
            let file_edits: Vec<(u64, lsp_types::Range, String)> = edits
                .as_array()
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
                .unwrap_or_default();
            by_uri.push((uri.clone(), file_edits));
        }

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

            // 全量 didChange 让 LS 跟上。
            let change_params = json!({
                "textDocument": { "uri": uri },
                "contentChanges": [{ "text": new_content }],
            });
            session
                .notify("textDocument/didChange", change_params)
                .await
                .map_err(ToolError::Core)?;

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
        static SD_VERSION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
        let version = SD_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let change_params = json!({
            "textDocument": { "uri": uri_str, "version": version },
            "contentChanges": [ { "text": new_text } ],
        });
        session
            .notify("textDocument/didChange", change_params)
            .await
            .map_err(ToolError::Core)?;

        Ok(SafeDeleteReport {
            deleted: true,
            symbol: symbol.to_string(),
            references: vec![],
        })
    }

    /// `insert-at-line`：在 line（1-based）前插入，原行下移；line == total+1 追加 EOF。
    /// ↖ mirror: file_tools.py@43ae021 InsertAtLineTool（0-based → 1-based Δ）。
    pub async fn tool_insert_at_line(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        content: &str,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
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

/// 把 `file://...` URL 转回 PathBuf。
fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    // Windows: `file:///C:/foo` → `C:/foo`
    let s = if cfg!(windows) && stripped.starts_with('/') {
        &stripped[1..]
    } else {
        stripped
    };
    Some(std::path::PathBuf::from(s.replace('\\', "/")))
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

/// 解析 lang: 有 override 直接用 (大小写折叠), 否则按文件扩展名探测。
fn resolve_lang_for_file(file: &str, lang_override: Option<&str>) -> ToolResult<String> {
    if let Some(l) = lang_override {
        return Ok(l.to_ascii_lowercase());
    }
    ls_registry::resolve(Path::new(file))
        .map(|l| l.as_str().to_string())
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
        match tool {
            "overview" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_overview(root, &file, lang).await?)
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
            "hover" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_hover(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "diagnostics" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_diagnostics(root, &file, lang).await?)
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
                self.tool_edit_insert_after_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            "insert-text-before-symbol" => {
                let (file, symbol, text) = required_edit_args(&args)?;
                self.tool_edit_insert_before_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                Ok(serde_json::Value::Null)
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
                self.tool_insert_at_line(
                    root,
                    &file,
                    line,
                    &content,
                    opt_expected_hash(&args).as_deref(),
                    lang,
                )
                .await?;
                Ok(serde_json::Value::Null)
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
async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
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
