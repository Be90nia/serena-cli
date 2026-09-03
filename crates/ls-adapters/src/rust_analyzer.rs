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

    async fn on_server_ready(
        &self,
        session: &lsp_core::session::Session,
    ) -> anyhow::Result<()> {
        // 探测：首次请求触发 rust-analyzer 索引加载；失败也返回 Ok 让 supervisor 放行。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": "file:///__rust_analyzer_ready_probe__"}}),
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
