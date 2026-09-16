//! gopls 适配器（PLAN M3 / Task T2 第 3 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/gopls_language_server.py`
//!
//! gopls 是 Go 官方 LSP server。它需要 `GOPATH` / `GOMODCACHE` 等环境，但通常
//! 这些已通过 `go env` 设置好；不强求注入。
//!
//! 启动 quirk：gopls 启动比 rust-analyzer 慢（~1-3s 解析 GOPATH）。无 file_associations
//! 需要补 —— 默认 `*.go` 即可。

use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Copy)]
pub struct GoplsAdapter;

#[async_trait]
impl LanguageServerAdapter for GoplsAdapter {
    fn id(&self) -> &'static str {
        "gopls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Go];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("gopls").ok_or_else(|| {
            not_installed_error(
                "gopls",
                "install gopls (`go install golang.org/x/tools/gopls@latest`) and ensure `gopls` is on PATH",
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
        // gopls 不需要 quirk。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": "file:///__gopls_ready_probe__"}}),
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
        // gopls 支持 `textDocument/implementation`（interface → struct 方法跳转）。
        true
    }
}
