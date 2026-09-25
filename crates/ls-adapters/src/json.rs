//! vscode-json-languageserver 适配器（Wave 1）。
//!
//! ↖ mirror: oraios/serena@43ae021 `solidlsp/language_servers/json_language_server.py`
//!
//! 启动命令 = `[vscode-json-languageserver, "--stdio"]`（上游 `_create_launch_command`）。
//!
//! quirk（抄译上游类 docstring）：
//! - JSON 常以单文件出现，无 workspace/项目概念 —— didOpen 即用，无索引等待；
//! - 仅提供 document symbols 与 hover，**无跨文件 references**（上游明示不支持），
//!   价值在 JSON 结构化 overview 与内容导航。
//! - Windows 安装形态为 `node_modules/.bin/vscode-json-languageserver.cmd`（上游
//!   `json_executable_path += ".cmd"` 同款约束；`npm_bin_path` 已按此处理）。
//!
//! exe 解析两级：全局 PATH → serena npm 缓存（`serena-cli install json`），
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

/// 就绪探针超时（json LS 无索引期，短超时足够）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// npm 缓存目录 id + 版本 pin（对齐 servers.toml [servers.json]，禁随意改）。
const CACHE_ID: &str = "json";
const CACHE_VERSION: &str = "1.3.4";

/// `serena-cli install json` 产物里的 bin 名（= servers.toml bin_rel）。
const BIN_REL: &str = "vscode-json-languageserver";

#[derive(Debug, Default, Clone, Copy)]
pub struct JsonAdapter;

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
        "run `serena-cli install json` (npm, pinned 1.3.4)  OR  `npm i -g vscode-json-languageserver` (requires node on PATH)",
    ))
}

#[async_trait]
impl LanguageServerAdapter for JsonAdapter {
    fn id(&self) -> &'static str {
        BIN_REL
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Json];
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
        // 上游只声明 client capabilities；单文件形态无需 root/workspace 特殊 patch。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 轻探针捕获启动即崩溃；json LS 无索引等待语义（didOpen 即用）。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "json"}]}),
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
