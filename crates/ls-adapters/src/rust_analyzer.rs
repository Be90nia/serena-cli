//! rust-analyzer 适配器（PLAN M3 / Task T2 第 1 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/rust_analyzer.py`
//!
//! rust-analyzer 是 LSP 实现（不基于另一 LSP），本身启动快 + 索引靠项目 root 的
//! `Cargo.toml`/`rust-project.json` 自动发现；无 quirk 需要额外补。`cargo` 不必前置，
//! 因为 rust-analyzer 不调 cargo —— 它读 `target/` 索引但懒加载。
//!
//! 已知限制：
//! - 不处理 rust-project.json 显式模式（非 cargo 项目）；M3+ 用户少，不预抽。
//! - 不实现 `rust-analyzer --help`/query-db 等 admin 接口。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 30s 探活上限。rust-analyzer 启动 <1s，但首次 `textDocument/documentSymbol` 触发
/// 索引加载时可能慢；保守给 30s。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__rust_analyzer_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct RustAnalyzerAdapter;

#[async_trait]
impl LanguageServerAdapter for RustAnalyzerAdapter {
    fn id(&self) -> &'static str {
        "rust-analyzer"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Rust];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("rust-analyzer").ok_or_else(|| {
            not_installed_error(
                "rust-analyzer",
                "install rust-analyzer (https://rust-analyzer.github.io) and ensure `rust-analyzer` is on PATH",
            )
        })?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // rust-analyzer 不需要 quirk patches；base init_params 默认声明已足够。
        // 它的 semanticTokensProvider / inlayHintsProvider 是 server-side capabilities，
        // client capability 留默认即可。
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 探针必须用 root 下真实文件：虚拟 URI 不触发 rust-analyzer 的 workspace
        // lazy-load，首个真实工具请求就得独自承担全量索引（cold-start 87s 根因，
        // 见 local/cold-start-hang-diagnosis.md）。探针返回时索引已完成。
        // 失败也返回 Ok 让 supervisor 放行。
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
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // rust-analyzer 支持 `textDocument/implementation`（trait → impl 跳转）。
        true
    }
}

impl RustAnalyzerAdapter {
    /// `on_server_ready` 将发出的探针 URI：root 下真实小文件的 file URI；root 未设置
    /// 或无候选文件时退虚拟 URI。
    fn probe_uri(&self) -> String {
        let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
        match root {
            Some(root) => crate::probe_uri_for_root(&root, PROBE_FALLBACK),
            None => PROBE_FALLBACK.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = RustAnalyzerAdapter;

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
}
