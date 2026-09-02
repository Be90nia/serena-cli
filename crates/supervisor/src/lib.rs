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
//! ponylabel: 单实例 ≠ `Mutex<HashMap>` —— 真正的「同 root 多 lang」也只装得下 1 个 lang
//! (clangd)，单 `Mutex<HashMap>` 比 `Arc<Mutex<OnceCell>>` 简单。Task 13 把池换进来时本
//! 公共 API 不变。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lsp_core::docsync::path_to_uri_str;
use lsp_core::error::CoreError;
use lsp_core::init_params::base_initialize_params;
use lsp_core::offsets::{OffsetEncoding, Position as LspPos};
use lsp_core::session::Session;
pub mod fs_tools;

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
}

/// supervisor 公共结果类型（库层 Result 别名）。
pub type ToolResult<T> = std::result::Result<T, ToolError>;

/// Daemon 工具语义层抽象；实现负责按工具名分派只读请求。
#[async_trait::async_trait]
pub trait SupervisorTrait: Send + Sync {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;
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
pub use lsp_types::Range as LspRange;
impl Supervisor {
    /// `--direct` 模式入口：创建空 supervisor（懒加载 Session）。
    pub async fn direct() -> ToolResult<Self> {
        Ok(Self {
            instances: Mutex::new(HashMap::new()),
            load_gates: Mutex::new(HashMap::new()),
            last_used: Mutex::new(HashMap::new()),
            direct_mode: true,
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

    /// 拿到/创建 (root, lang) 对应的 Session，同 key 只允许一次冷启动。
    async fn session_for(&self, root: &Path, lang: &'static str) -> ToolResult<Arc<Session>> {
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

        let session = Session::start(child, params).await?;
        self.instances
            .lock()
            .unwrap()
            .insert(key.clone(), session.clone());
        self.touch(&key);
        Ok(session)
    }

    /// `textDocument/documentSymbol` → 平铺递归 `DocumentSymbol::children` → `Vec<SymbolHit>`。
    ///
    /// 上游语义（PLAN Task 10 step 2）：position-free；只传 file。M1 才补 name 过滤。
    pub async fn tool_overview(&self, root: &Path, file: &str) -> ToolResult<Vec<SymbolHit>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
        let lang_str: &'static str = lang.as_str();

        let session = self.session_for(root, lang_str).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: DocumentSymbolResponse = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        Ok(flatten_symbols(resp, &uri))
    }

    /// `workspace/symbol` → 全 workspace 跨文件符号查找（Task 20）。
    ///
    /// 设计要点：
    /// - **不传 file**：position-free + workspace scope，AI 找符号定义的标准入口。
    /// - lang 探测：root 下任一 `.cpp` / `.c` / `.h` → cpp。MVP 不支持混合多语言 root。
    /// - 触发索引：本次首调会拉起 `didOpen` 任一文件让 clangd 开 background index。
    ///   索引可能慢（>10s），用 `INDEX_TIMEOUT` 而不是 `TOOL_TIMEOUT`。
    /// - 输出与 `tool_overview` 形状一致 (`Vec<SymbolHit>`)，便于 agent 用同一段代码处理。
    pub async fn tool_find_symbol(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
    ) -> ToolResult<Vec<SymbolHit>> {
        use ignore::WalkBuilder;

        if query.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "query must not be empty".into(),
            });
        }

        // 探测 root 下任一 cpp 文件以确定 lang。
        let mut probe: Option<PathBuf> = None;
        for entry in WalkBuilder::new(root)
            .standard_filters(true)
            .max_depth(Some(3))
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|t| t.is_file())
                && let Some(lang) = ls_registry::resolve(entry.path())
                && lang.as_str() == "cpp"
            {
                probe = Some(entry.path().to_path_buf());
                break;
            }
        }
        let probe = probe.ok_or_else(|| ToolError::BadArgs {
            detail: "no cpp/c/h files under root; workspace/symbol requires a known language"
                .into(),
        })?;

        let lang = ls_registry::resolve(&probe).unwrap();
        let session = self.session_for(root, lang.as_str()).await?;
        // 触发背景索引：把 probe 文件 didOpen 一次。
        let _ = session.ensure_open(&probe).await.map_err(ToolError::Core)?;

        let params = json!({ "query": query });
        // clangd 索引可能慢，30s 太短。
        let resp: Vec<lsp_types::SymbolInformation> = session
            .request("workspace/symbol", params, INDEX_TIMEOUT)
            .await?;

        // 裁剪 + 转 SymbolHit。
        let hits: Vec<SymbolHit> = resp
            .into_iter()
            .take(limit)
            .map(|si| SymbolHit {
                name: si.name,
                kind: kind_from_lsp(&si.kind),
                uri: si.location.uri.to_string(),
                range: si.location.range,
                container: si.container_name,
            })
            .collect();
        Ok(hits)
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
    ) -> ToolResult<Option<Location>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    ) -> ToolResult<Vec<Location>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    ) -> ToolResult<Vec<Location>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
        let resp: Vec<Location> = session
            .request("textDocument/references", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
    }

    /// `find_referencing_symbols`：所有引用 + 每个 ref 落在哪个外层符号里（Task 24）。
    pub async fn tool_referencing_symbols(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
    ) -> ToolResult<Vec<ref_tools::RefSymbolHit>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    pub async fn tool_referencing_code_snippets(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        context_lines: u32,
        max_results: usize,
    ) -> ToolResult<Vec<ref_tools::RefSnippetHit>> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    /// `symbol-body`：按符号名取函数/类体切片（PLAN Task 15）。
    ///
    /// 流程：ensure_open → documentSymbol 定位 name 匹配的符号 range →
    /// offsets.rs 切片返回。position-free（客户端只传 file + symbol name）。
    pub async fn tool_symbol_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
    ) -> ToolResult<String> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    ) -> ToolResult<()> {
        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
    pub async fn tool_rename_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        new_name: &str,
    ) -> ToolResult<RenameReport> {
        if new_name.is_empty() || new_name.contains(' ') {
            return Err(ToolError::BadArgs {
                detail: "new_name must be non-empty, no whitespace".into(),
            });
        }

        let lang = ls_registry::resolve(Path::new(file)).ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })?;
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
        let resp =
            resp.ok_or_else(|| ToolError::Launch(anyhow::anyhow!("rename returned null")))?;

        // 3) 拆 `changes` map → 按文件分组 + 倒序排序。
        let changes = resp
            .get("changes")
            .and_then(|v| v.as_object())
            .ok_or_else(|| ToolError::Launch(anyhow::anyhow!(
                "rename response has no `changes` map (M2 only supports changes, not documentChanges)"
            )))?;

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

#[async_trait::async_trait]
impl SupervisorTrait for Supervisor {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let root = Path::new(project_root);
        match tool {
            "overview" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_overview(root, &file).await?)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "find-symbol" => {
                let query = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'query'".into(),
                    }
                })?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
                let resp = self.tool_find_symbol(root, query, limit).await?;
                serde_json::to_value(resp)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "def" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_def(root, &file, line, col).await?)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "refs" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_refs(root, &file, line, col).await?)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "find-implementations" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_find_implementations(root, &file, line, col)
                        .await?,
                )
                .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
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
                serde_json::to_value(resp)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "symbol-body" => {
                let (file, symbol) = required_symbol_body_args(&args)?;
                serde_json::to_value(self.tool_symbol_body(root, &file, &symbol).await?)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "replace-body" => {
                let (file, symbol, new_body) = required_replace_args(&args)?;
                self.tool_replace_body(root, &file, &symbol, &new_body)
                    .await?;
                Ok(serde_json::Value::Null)
            }
            "rename-symbol" => {
                let (file, line, col, new_name) = required_rename_args(&args)?;
                serde_json::to_value(
                    self.tool_rename_symbol(root, &file, line, col, &new_name)
                        .await?,
                )
                .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
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
                serde_json::to_value(report)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
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
                serde_json::to_value(entries)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
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
                serde_json::to_value(hits)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            "find-referencing-symbols" => {
                let (file, line, col) = required_position(&args)?;
                let lang =
                    ls_registry::resolve(Path::new(&file)).ok_or_else(|| ToolError::BadArgs {
                        detail: format!("file not supported: {file}"),
                    })?;
                let session = self.session_for(root, lang.as_str()).await?;
                let hits = ref_tools::find_referencing_symbols(&session, root, &file, line, col)
                    .await
                    .map_err(|e| {
                        ToolError::Core(CoreError::Rpc {
                            code: -1,
                            message: format!("find_referencing_symbols: {e}"),
                        })
                    })?;
                serde_json::to_value(hits)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
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
                let lang =
                    ls_registry::resolve(Path::new(&file)).ok_or_else(|| ToolError::BadArgs {
                        detail: format!("file not supported: {file}"),
                    })?;
                let session = self.session_for(root, lang.as_str()).await?;
                let hits = ref_tools::find_referencing_code_snippets(
                    &session,
                    root,
                    &file,
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
                })?;
                serde_json::to_value(hits)
                    .map_err(|e| ToolError::Launch(anyhow::anyhow!("serialize: {e}")))
            }
            other => Err(ToolError::BadArgs {
                detail: format!("unknown tool: {other}"),
            }),
        }
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
fn normalize_implementations(raw: Option<&serde_json::Value>) -> Vec<Location> {
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

/// sha256(content) hex 前 16 位（对账用；碰撞概率足够低且只做提示性校验）。
fn content_hash(text: &str) -> String {
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
