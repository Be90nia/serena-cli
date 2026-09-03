//! pyright 适配器（PLAN M3 / Task T2 第 2 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/pyright_language_server.py`
//!
//! pyright 是 Microsoft 的 Python 类型检查 + LSP 实现。**它本身不可执行** —— 实际
//! 是 `pyright-langserver`（pip 包）或 `@pyright/langserver`（npm 包）。我们优先
//! 探测 `pyright-langserver`（pip 安装即可用），其次 `pyright`（新版直接当 LSP server 启动）。
//!
//! 启动方式：探测到的可执行直接 stdio。pyright 不需要 project_root 初始化文件（无
//! tsconfig/Cargo.toml 等价物）；`pyrightconfig.json` 仅影响检查策略不影响 LSP。

use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default, Clone, Copy)]
pub struct PyrightAdapter;

/// 候选可执行名：依次尝试。
/// ponytail: 不抽 install_hint 模板 —— `not_installed_error` 已封装常见 hint。
fn locate_pyright() -> Option<std::ffi::OsString> {
    // pyright-langserver 是 pip 包装的 entry point；优先找。
    if let Some(p) = which_no_unc("pyright-langserver") {
        return Some(p.into_os_string());
    }
    // 新版 pyright 直接支持 --stdio / --langserver。
    if let Some(p) = which_no_unc("pyright") {
        return Some(p.into_os_string());
    }
    None
}

#[async_trait]
impl LanguageServerAdapter for PyrightAdapter {
    fn id(&self) -> &'static str {
        "pyright"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Python];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = locate_pyright().ok_or_else(|| {
            not_installed_error(
                "pyright",
                "install pyright (`pip install pyright` or `npm i -g pyright`) and ensure `pyright-langserver` or `pyright` is on PATH",
            )
        })?;
        let cmd = if exe.to_string_lossy().contains("pyright-langserver") {
            // pyright-langserver 自动进入 LSP 模式（无额外 flag）。
            vec![exe]
        } else {
            // pyright（npm/Python 包）走 --stdio 进入 LSP 模式。
            vec![exe, "--stdio".into()]
        };
        Ok(LaunchInfo {
            cmd,
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // pyright 不需要 client capability quirk。
    }

    async fn on_server_ready(
        &self,
        session: &lsp_core::session::Session,
    ) -> anyhow::Result<()> {
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": "file:///__pyright_ready_probe__"}}),
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
        false
    }
}
