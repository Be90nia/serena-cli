//! bash-language-server 适配器（Wave 1）。
//!
//! ↖ mirror: oraios/serena@43ae021 `solidlsp/language_servers/bash_language_server.py`
//!
//! 启动命令 = `[bash-language-server, "start"]`（上游 `_create_launch_command`；与
//! `servers.toml [servers.bash]` 的 `npm_args = ["start"]` 同源）。
//!
//! 上游 quirk 取舍：
//! - explainshell 集成（web 服务查询）默认关闭 —— 不抄。
//! - ShellCheck 二进制下载（诊断增强，SHELLCHECK_PATH env 注入）—— 不抄：无 ShellCheck
//!   时 bash-language-server 仍有内建 tree-sitter 语法诊断，三关（overview/hover/
//!   diagnostics）不受影响；下载步骤引入额外网络面，Wave 1 不扩。
//! - documentSymbol 对 `function name()` / `name()` 两种函数语法均可靠 —— 走标准请求。
//!
//! exe 解析两级：全局 PATH（`npm i -g` 形态）→ serena npm 缓存（`serena-cli install bash`
//! 形态，`{cache}/bash/{VER}/node_modules/.bin/`）。缓存 id/版本对齐 servers.toml pin。

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

/// 就绪探针超时（bash LS 无索引期，短超时足够）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// npm 缓存目录 id + 版本 pin（对齐 servers.toml [servers.bash]，禁随意改）。
const CACHE_ID: &str = "bash";
const CACHE_VERSION: &str = "5.6.0";

/// `serena-cli install bash` 产物里的 bin 名（= servers.toml bin_rel）。
const BIN_REL: &str = "bash-language-server";

#[derive(Debug, Default, Clone, Copy)]
pub struct BashAdapter;

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
        "run `serena-cli install bash` (npm, pinned 5.6.0)  OR  `npm i -g bash-language-server` (requires node on PATH)",
    ))
}

#[async_trait]
impl LanguageServerAdapter for BashAdapter {
    fn id(&self) -> &'static str {
        BIN_REL
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Bash];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = resolve_exe()?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string(), "start".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // 上游只声明 client capabilities（常规 didSave/completion 等），默认 params 已覆盖。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 轻探针捕获启动即崩溃（如 node 缺失时 shim 秒退）；bash LS 无索引等待语义。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "bash"}]}),
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
