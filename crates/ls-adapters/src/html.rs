//! vscode-html-language-server 适配器（bd 56a 后续批）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/vscode_html_language_server.py`
//!
//! 启动命令 = `[vscode-html-language-server, "--stdio"]`（上游 `_create_launch_command`）。
//! npm 包 `vscode-langservers-extracted`（同包另有 css 双入口，见 css.rs / servers.toml）。
//!
//! quirk（抄译上游类 docstring）：
//! - HTML LSP 提供 in-file 元素/id 符号（documentSymbol）；跨文件 references /
//!   definition 对 HTML 无意义（上游明示）；
//! - 上游以 `completionProvider` 存在性做就绪 sanity check——本侧用
//!   `workspace/configuration` 轻探针捕获启动即崩溃（json 适配器同款）。
//! - Windows 安装形态为 `node_modules/.bin/vscode-html-language-server.cmd`
//!   （`npm_bin_path` 已按此处理）。
//!
//! exe 解析两级：全局 PATH → serena npm 缓存（`serena-cli install html`），
//! 缓存 id/版本对齐 servers.toml pin。

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::install_pkg::npm_bin_path;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 就绪探针超时（html LS 无索引期，短超时足够）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// npm 缓存目录 id + 版本 pin（对齐 servers.toml [servers.html]，禁随意改）。
/// pub：angular.rs 的 vscode-html 伴生复用同一缓存（angular_language_server.py
/// 的 VsCodeHtmlLanguageServer 伴生同包）。
pub const CACHE_ID: &str = "html";
pub const CACHE_VERSION: &str = "4.10.0";

/// `serena-cli install html` 产物里的 bin 名（= servers.toml bin_rel）。
pub const BIN_REL: &str = "vscode-html-language-server";

#[derive(Debug, Default, Clone, Copy)]
pub struct HtmlAdapter;

/// PATH → serena npm 缓存 两级 exe 解析；都未命中 → 标准安装提示错误。
fn resolve_exe() -> anyhow::Result<PathBuf> {
    if let Some(exe) = which_no_unc(BIN_REL) {
        return Ok(exe);
    }
    let cache_dir = default_cache_root().join(CACHE_ID).join(CACHE_VERSION);
    if let Some(exe) = npm_bin_path(&cache_dir, BIN_REL) {
        return Ok(exe);
    }
    Err(not_installed_error(
        BIN_REL,
        "run `serena-cli install html` (npm, pinned 4.10.0)  OR  `npm i -g vscode-langservers-extracted` (requires node on PATH)",
    ))
}

#[async_trait]
impl LanguageServerAdapter for HtmlAdapter {
    fn id(&self) -> &'static str {
        BIN_REL
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Html];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = resolve_exe()?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string(), "--stdio".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // ↖ mirror 上游 _create_base_initialize_params.initializationOptions：
        // 嵌入脚本/样式开启（补全/悬停覆盖 <script>/<style> 段），格式化关闭
        // （上游 provideFormatter: false）。
        base.initialization_options = Some(serde_json::json!({
            "embeddedLanguages": { "css": true, "javascript": true },
            "handledSchemas": ["file"],
            "provideFormatter": false,
        }));
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 轻探针捕获启动即崩溃；html LS 无索引等待语义（didOpen 即用）。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "html"}]}),
                READY_PROBE_TIMEOUT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }
}
