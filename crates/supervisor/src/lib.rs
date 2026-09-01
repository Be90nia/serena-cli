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
use lsp_core::types::{SymbolHit, SymbolKindTag};
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position};
use serde_json::json;
use thiserror::Error;

/// Read-only tool timeout. overview/def/refs on small files complete in ms; clangd
/// cold-start of a project may take seconds. 30s mirrors `READY_PROBE_TIMEOUT`.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// 适配器启动期错误（anyhow 上抛统一收口）。
    #[error("adapter launch failed: {0}")]
    Launch(#[from] anyhow::Error),
}

/// supervisor 公共结果类型（库层 Result 别名）。
pub type ToolResult<T> = std::result::Result<T, ToolError>;

/// M0 单实例 supervisor。
///
/// - `instances`：`Mutex<HashMap<Key, Arc<Session>>>`。M0 仅 Cpp 一个 lang；同 (root, cpp) 复用 Session。
/// - `direct_mode`：当前 supervisor 由 `--direct` CLI 拉起；M1 daemon 模式不复用本类型
///   （daemon 引入 HTTP + idle reaper），Task 13 会把 `direct()` 拆为 `DirectSupervisor`，
///   此处留模式开关便于未来扩展。
pub struct Supervisor {
    instances: Mutex<HashMap<Key, Arc<Session>>>,
    direct_mode: bool,
}

/// 实例键：dunce 规范化的 root + language 字符串（M0 仅 "cpp"）。
///
/// ponylabel: 不实现完整 `Eq` 比较的 case-folding —— root 走 `dunce::canonicalize` 后
/// Windows 已是大小写无关形态（同盘）。M1 跨平台接入再加 `to_lowercase`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    root: PathBuf,
    lang: &'static str,
}

/// 对外暴露给工具调用方的"位置"结构（直接复用 lsp-types `Location`，
/// 但放本 crate re-export 以避免下游依赖 `lsp-types`）。
///
/// `Location { uri: Uri, range: Range }` —— `Uri`/`Range` 也直接 re-export。
pub use lsp_types::Location;
pub use lsp_types::Range as LspRange;

impl Supervisor {
    /// `--direct` 模式入口：创建空 supervisor（懒加载 Session）。
    pub async fn direct() -> ToolResult<Self> {
        Ok(Self {
            instances: Mutex::new(HashMap::new()),
            direct_mode: true,
        })
    }

    /// 当前是否 direct 模式（保留位，M1 用）。
    #[allow(dead_code)]
    pub fn is_direct(&self) -> bool {
        self.direct_mode
    }

    /// 解析 key（root 走 dunce canonicalize 避免 Windows UNC `\\?\` 污染 LSP URI）。
    fn key(root: &Path, lang: &'static str) -> Key {
        let canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Key { root: canon, lang }
    }

    /// 拿到/创建 (root, lang) 对应的 Session。M0 single-key per (root,lang)。
    async fn session_for(&self, root: &Path, lang: &'static str) -> ToolResult<Arc<Session>> {
        let key = Self::key(root, lang);
        // 快路径：缓存命中。
        if let Some(s) = self.instances.lock().unwrap().get(&key).cloned() {
            return Ok(s);
        }

        // 慢路径：拉起 adapter → spawn → 握手 → Ready。
        let adapter = ls_registry::adapter_for(lang).ok_or_else(|| ToolError::BadArgs {
            detail: format!("unknown language: {lang}"),
        })?;

        let ctx = ls_adapters::ProjectCtx {
            project_root: key.root.clone(),
        };
        let launch = adapter.launch_info(&ctx).await.map_err(|e| {
            // 区分 LS_NOT_INSTALLED（PATH miss）与其它 launch 错误。
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
        // LSP 3.17 deprecates `root_uri` in favor of `workspace_folders`.
        params.workspace_folders = Some(vec![lsp_types::WorkspaceFolder {
            uri: uri.clone(),
            name: key
                .root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string(),
        }]);
        // adapter patch（clangd 加 utf-16 / hierarchical 等）。
        adapter.initialize_patches(&mut params);

        let session = Session::start(child, params).await?;
        self.instances.lock().unwrap().insert(key, session.clone());
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

    /// `textDocument/definition` → 第一个 `Location`（server 可能回 `LocationLink[]` 或
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
        let resp: Option<Location> = session
            .request("textDocument/definition", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
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
