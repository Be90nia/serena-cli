//! vscode-css-language-server 适配器（bd 56a 后续批）。
//!
//! Δ 自有设计：上游 serena（7a296833）无 css 适配器可镜像；本文件按 html.rs 同构
//! 形态接线同包 `vscode-langservers-extracted` 的 css 双入口（bin 名来自 npm
//! registry bin 字段，2026-09-28 查证，包名≠bin 名）。
//!
//! 启动命令 = `[vscode-css-language-server, "--stdio"]`（vscode-langservers 族
//! 统一 stdio 形态，同族 html/json 适配器先例）。
//!
//! quirk：
//! - css LS hover/completion 为 schema/mdn 驱动（内置数据，无需外部服务）；
//! - 无索引等待语义（didOpen 即用），`workspace/configuration` 轻探针捕获启动
//!   即崩溃（json 适配器同款）。
//! - Windows 安装形态为 `node_modules/.bin/vscode-css-language-server.cmd`
//!   （`npm_bin_path` 已按此处理）。
//!
//! exe 解析两级：全局 PATH → serena npm 缓存（`serena-cli install css`），
//! 缓存 id/版本对齐 servers.toml pin（与 html 同包不同 id，缓存目录分落，
//! sqls-mysql 先例）。

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

/// 就绪探针超时（css LS 无索引期，短超时足够）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// npm 缓存目录 id + 版本 pin（对齐 servers.toml [servers.css]，禁随意改）。
const CACHE_ID: &str = "css";
const CACHE_VERSION: &str = "4.10.0";

/// `serena-cli install css` 产物里的 bin 名（= servers.toml bin_rel）。
const BIN_REL: &str = "vscode-css-language-server";

#[derive(Debug, Default, Clone, Copy)]
pub struct CssAdapter;

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
        "run `serena-cli install css` (npm, pinned 4.10.0)  OR  `npm i -g vscode-langservers-extracted` (requires node on PATH)",
    ))
}

#[async_trait]
impl LanguageServerAdapter for CssAdapter {
    fn id(&self) -> &'static str {
        BIN_REL
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Css];
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

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // Δ：css-language-server 默认配置即可用（schema/mdn 内置驱动），
        // 无 html 侧 embeddedLanguages 类初始化选项。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 轻探针捕获启动即崩溃；css LS 无索引等待语义（didOpen 即用）。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "css"}]}),
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
