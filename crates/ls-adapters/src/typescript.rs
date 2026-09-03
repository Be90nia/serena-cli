//! typescript-language-server 适配器（PLAN M3 / Task T2 第 4 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/typescript_language_server.py`
//!
//! typescript-language-server 是 tsserver 的 LSP 包装（`@typescript-language-server/typescript-language-server`）。
//! 同时服务 TS + JS（同名包，区别在内部通过 `filetype` 区分）。
//!
//! 启动 quirk：
//! - 服务 TS 项目：找 `tsconfig.json` / `jsconfig.json`；不在时降级为单文件模式。
//! - `typescript-language-server` 需要 `typescript` + `tsserver` 作为 peer dep —— 装包时一并 npm i。
//!
//! 已知限制：
//! - 不实现 jsx/tsx 自动配置；默认即可。
//! - 不注入 inlayHints / semanticTokens —— M3+ 用户需要再加。

use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Copy)]
pub struct TypescriptLanguageServerAdapter;

#[async_trait]
impl LanguageServerAdapter for TypescriptLanguageServerAdapter {
    fn id(&self) -> &'static str {
        "typescript-language-server"
    }

    fn languages(&self) -> &'static [LanguageId] {
        // 同 adapter 服务 TS + JS（spec 通过 file_extension 走 LanguageId 维度后映射）。
        const LANGS: &[LanguageId] = &[LanguageId::TypeScript];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("typescript-language-server").ok_or_else(|| {
            not_installed_error(
                "typescript-language-server",
                "install TypeScript LS (`npm i -g typescript typescript-language-server`) and ensure `typescript-language-server` is on PATH",
            )
        })?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string(), "--stdio".into()],
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
                json!({"textDocument": {"uri": "file:///__ts_ls_ready_probe__"}}),
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
        // TypeScript LS 支持 `textDocument/implementation`（interface → class）。
        true
    }
}
