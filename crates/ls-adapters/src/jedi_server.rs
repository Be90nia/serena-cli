//! jedi-language-server 适配器壳（PLAN Phase 2 / Task 24）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/jedi_language_server.py`
//!
//! jedi 是 Python 老牌 LSP 实现（无类型检查，仅补全/定义/引用），轻量、单文件；
//! 大型 monorepo 内 jedi 启动 <0.5s，pyright 30s+。适用代码补全 quick wins。
//!
//! 启动：探测 `jedi-language-server`（pip install jedi-language-server）。无
//! 额外 flag，直接 stdio。
//!
//! 已知限制（M2 壳）：
//! - 不注入 jedi 项目路径参数；jedi 启动时从 InitializeParams.rootUri 自动发现。
//! - 不写 `.jedi/` 缓存目录配置；jedi 默认 `<root>/.cache/jedi` 即可。

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 当前会话项目 root。同构于 pyright.rs —— Python LSP 系共享探测链。
static PROBE_ROOT: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct JediServerAdapter;

/// 候选可执行名（依次尝试）。
fn locate_jedi() -> Option<std::ffi::OsString> {
    which_no_unc("jedi-language-server").map(|p| p.into_os_string())
}

#[async_trait]
impl LanguageServerAdapter for JediServerAdapter {
    fn id(&self) -> &'static str {
        "jedi"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Python];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = locate_jedi().ok_or_else(|| {
            not_installed_error(
                "jedi",
                "install jedi-language-server (`pip install jedi-language-server`) and ensure `jedi-language-server` is on PATH",
            )
        })?;
        // jedi-language-server 直接 stdio LSP，无 flag。
        Ok(LaunchInfo {
            cmd: vec![exe],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // jedi 不需要 initializationOptions 注入；项目根由 rootUri 解析。
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        use serde_json::json;
        let uri = crate::probe_uri_for_root(
            PROBE_ROOT
                .lock()
                .expect("PROBE_ROOT poisoned")
                .as_deref()
                .unwrap_or(Path::new(".")),
            self.languages(),
            "file:///__jedi_ready_probe__",
        );
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
                std::time::Duration::from_secs(30),
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // jedi 不实现 textDocument/implementation。
        false
    }
}
