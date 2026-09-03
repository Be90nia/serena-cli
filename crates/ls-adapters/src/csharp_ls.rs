//! csharp_ls 适配器（PLAN M3 / Task T2 第 5 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/csharp_language_server.py`
//!
//! csharp_ls（https://github.com/razzmatazz/csharp-language-server）是 Roslyn 的 LSP 包装，
//! 启动快（vs omnisharp-roslyn 慢 ~10x）。默认走 `dotnet tool install -g csharp-ls` 安装。
//!
//! 启动 quirk：
//! - csharp_ls 启动需 ~3s 加载 Roslyn workspaces。
//! - 默认 .sln/.csproj 自动发现；不需要 --solution。
//!
//! 已知限制：
//! - 不实现 omnisharp-roslyn 兼容路径 —— 它单独有 `omnisharp` binary 和协议差异。
//! - 不注入 .editorconfig 读取 —— 用户 workspace 自管。

use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(60); // csharp_ls 启动慢

#[derive(Debug, Default, Clone, Copy)]
pub struct CsharpLsAdapter;

#[async_trait]
impl LanguageServerAdapter for CsharpLsAdapter {
    fn id(&self) -> &'static str {
        "csharp-ls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::CSharp];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("csharp-ls").ok_or_else(|| {
            not_installed_error(
                "csharp-ls",
                "install csharp-ls (`dotnet tool install -g csharp-ls`) and ensure `csharp-ls` is on PATH",
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
        // 无 quirk。
    }

    async fn on_server_ready(
        &self,
        session: &lsp_core::session::Session,
    ) -> anyhow::Result<()> {
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": "file:///__csharp_ls_ready_probe__"}}),
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
        true
    }
}
