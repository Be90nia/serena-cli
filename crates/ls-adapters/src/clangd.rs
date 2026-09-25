//! clangd 适配器（PLAN Task 8 / 首个 T2）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/clangd_language_server.py`
//!
//! 本任务（M0）落地的最小可用面：
//! - `id / languages`：稳定标识 + Cpp 一项。
//! - `launch_info`：PATH 查找 `clangd`（dunce 去 UNC），无则报 `LS_NOT_INSTALLED`。
//! - `initialize_patches`：声明 `general.position_encodings = [utf-16, utf-8]`
//!   （补强 base init_params 的 utf-16 优先选择 —— clangd 默认 utf-16）。
//!   以及 `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport = true`
//!   （base 已声明；本任务重复声明便于 clangd 单适配器独立可见）。
//!   **不**在此实现 OffsetEncoding 协商 —— Session 由 `lsp_core::offsets` 内部
//!   解析 capabilities.positionEncoding；M0 init_params 默认 utf-16 即 clangd 默认。
//! - `on_server_ready`：发起一次 `textDocument/documentSymbol` 等待首次成功或 30s 超时
//!   （基础就绪信号）。clangd 还要求 index 完整加载才稳，完整索引就绪等待事件由 M3
//!   `$/progress` 接入（M0 不必）。
//! - `logmap`（stderr 分级）—— M0 不在 trait 上；ls-runtime stderr 泵 task 用通用
//!   tracing 默认分级即可。clangd 的 `I[..]/E[..]` 特异前缀是 M3 + T2 深度 quirk 时
//!   再补（ARCH §4.1 ↖ mirror `_determine_log_level`）。
//!
//! ## 深度（M2 落地）
//!
//! - `launch_info`：root 上下找 `compile_commands.json`（BFS 限深 5），命中则追加
//!   `--compile-commands-dir=<dir>`（dir = 含 compile_commands.json 的目录）；未命中
//!   维持原参数，不报错 —— 纯头文件项目仍可用。
//! - UE project detection、`--query-driver` / `--clang-tidy` 等其它参数仍不做。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lsp_types::{
    ClientCapabilities, DocumentSymbolClientCapabilities, GeneralClientCapabilities,
    InitializeParams, TextDocumentClientCapabilities,
};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// `on_server_ready` 等就绪上限（30s）—— clangd 真实项目多在 5s 内返回首次 documentSymbol。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__clangd_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 适配器本体：零字段，单例即可（`&'static str` 返回 `Send + Sync`）。
#[derive(Debug, Default, Clone, Copy)]
pub struct ClangdAdapter;

#[async_trait]
impl LanguageServerAdapter for ClangdAdapter {
    fn id(&self) -> &'static str {
        "clangd"
    }

    fn languages(&self) -> &'static [LanguageId] {
        // ARCH §4.1 实例键维度 —— C++ 全家桶（cpp/hpp/c/cc/cxx）。
        // 扩展名 → LanguageId 的解析在 ls-registry Task 9 落地。
        const LANGS: &[LanguageId] = &[LanguageId::Cpp];
        LANGS
    }

    async fn launch_info(
        &self,
        ctx: &ProjectCtx,
    ) -> anyhow::Result<ls_runtime::process::LaunchInfo> {
        let exe = which_no_unc("clangd").ok_or_else(|| {
            not_installed_error(
                "clangd",
                "install LLVM clangd (https://clangd.llvm.org/installation) and ensure `clangd` is on PATH",
            )
        })?;
        let mut cmd: Vec<std::ffi::OsString> = vec![
            exe.into_os_string(),
            "--background-index".into(),
            "--limit-results=500".into(),
        ];
        // M2 深度：探测 compile_commands.json。命中 → 追加 --compile-commands-dir=<dir>。
        if let Some(dir) = find_compile_commands_dir(&ctx.project_root) {
            let mut arg = std::ffi::OsString::from("--compile-commands-dir=");
            arg.push(dir.as_os_str());
            cmd.push(arg);
        }
        Ok(ls_runtime::process::LaunchInfo {
            cmd,
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: ls_runtime::process::TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // 与 base init_params 对齐：hierarchicalDocumentSymbolSupport、position_encodings。
        // 这里**增强**声明：clangd 默认 utf-16，但若 base 未声明 position_encodings，
        // 我们强制加上 —— 让 capabilities 始终一致（否则 base 默认空 caps 可能漏声明）。
        let caps = &mut base.capabilities;
        let text_doc = caps
            .text_document
            .get_or_insert(TextDocumentClientCapabilities {
                document_symbol: Some(DocumentSymbolClientCapabilities {
                    hierarchical_document_symbol_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        // 即便 base 已有 document_symbol，也强制 hierarchical=true。
        let ds = text_doc
            .document_symbol
            .get_or_insert(DocumentSymbolClientCapabilities::default());
        ds.hierarchical_document_symbol_support = Some(true);

        let general = caps
            .general
            .get_or_insert(GeneralClientCapabilities::default());
        // 强制声明 utf-16 优先（与 base 一致；这里再保险一次）。
        if general.position_encodings.is_none() {
            general.position_encodings = Some(vec![
                lsp_types::PositionEncodingKind::UTF16,
                lsp_types::PositionEncodingKind::UTF8,
            ]);
        }
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 基础就绪：用 root 下真实文件发一次 documentSymbol —— 虚拟 URI 不触发 clangd
        // 的 index lazy load，首个真实工具请求就得独自承担全量索引（cold-start hang
        // 根因，见 local/cold-start-hang-diagnosis.md）。完整索引就绪事件由 M3 接入
        // $/progress。失败也返回 Ok 让 supervisor 放行。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": self.probe_uri() } }),
                READY_PROBE_TIMEOUT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        // clangd M0 不注入特异改写；M3+ 可加 textDocument/definition 参数注入（macro aware）。
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // clangd 支持 `textDocument/implementation`（goto implementation）。
        true
    }
}

impl ClangdAdapter {
    /// `on_server_ready` 将发出的探针 URI：root 下真实小文件的 file URI；root 未设置
    /// 或无候选文件时退虚拟 URI。
    fn probe_uri(&self) -> String {
        let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
        match root {
            Some(root) => crate::probe_uri_for_root(&root, self.languages(), PROBE_FALLBACK),
            None => PROBE_FALLBACK.to_string(),
        }
    }
}

/// 暴露给测试 / supervisor 的辅助：`launch_info` 找可执行时返回找到的路径（无则 None）。
///
/// 测试与未来「download fallback」（M2）会用到。`launch_info` 用本函数查找 + 报错。
#[allow(dead_code)]
pub(crate) fn locate_clangd() -> Option<PathBuf> {
    which_no_unc("clangd")
}

/// 从 `root` 向下 BFS 找 `compile_commands.json`，命中返回所在目录（dir 即
/// --compile-commands-dir= 的取值）。BFS 限深 5 避免大型 monorepo 扫穿。
///
/// ↖ mirror: clangd_language_server.py@43ae021 `find_compile_commands`（向上找版本不同；
/// clangd 自身默认向上找，但项目根不确定时向下更稳）。
///
/// ponytail: 不递归遍历子目录做 symlink 检查 —— 绝大多数构建系统产物目录是常规目录，
/// 真实 symlink-loop 项目极少；后续真撞上再加。
pub(crate) fn find_compile_commands_dir(root: &Path) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    const MAX_DEPTH: usize = 5;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        let probe = dir.join("compile_commands.json");
        if probe.is_file() {
            return Some(dir);
        }
        if depth < MAX_DEPTH
            && let Ok(read) = std::fs::read_dir(&dir)
        {
            for entry in read.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    queue.push((p, depth + 1));
                }
            }
        }
    }
    None
}

/// 客户端能力声明 helper（供 future tests / supervisor 直读）。
#[allow(dead_code)]
pub(crate) fn clangd_client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        text_document: Some(TextDocumentClientCapabilities {
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                lsp_types::PositionEncodingKind::UTF16,
                lsp_types::PositionEncodingKind::UTF8,
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = ClangdAdapter;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(uri.ends_with(".gitignore"), "应指向真实文件: {uri}");

        let empty = tempfile::tempdir().unwrap();
        adapter.set_project_root(empty.path());
        assert_eq!(adapter.probe_uri(), PROBE_FALLBACK);
    }

    /// compile_commands.json 命中：root 直下 build/compile_commands.json → 返 build dir。
    #[test]
    fn find_compile_commands_dir_in_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("compile_commands.json"), "[]").unwrap();
        let found = find_compile_commands_dir(dir.path()).expect("命中子目录");
        assert_eq!(
            dunce::canonicalize(&found).unwrap_or(found.clone()),
            dunce::canonicalize(&build).unwrap_or(build.clone()),
            "返回 dir 应等于 build 目录: got={found:?} expected={build:?}"
        );
    }

    /// compile_commands.json 未命中：root + 子目录都无 → None（不报错）。
    #[test]
    fn find_compile_commands_dir_absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.cpp"), "int main(){}\n").unwrap();
        assert!(
            find_compile_commands_dir(dir.path()).is_none(),
            "无 compile_commands.json 必须返 None"
        );
    }
}
