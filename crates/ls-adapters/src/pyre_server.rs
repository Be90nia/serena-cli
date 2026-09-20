//! pyre (Meta Python type checker LSP) 适配器壳（PLAN Phase 2 / Task 24）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/pyre_language_server.py`
//!
//! pyre 是 Meta 的 Python 类型检查器，自带 Pysa（静态分析）。LSP server 通过
//! `pyre --command start` 启动（或 `pyre-lsp` 旧 wrapper）。
//!
//! 启动：探测 `pyre`（pip install pyre）。pyre 启动需 watchman 配套；不强制，
//! fallback 走 polling。
//!
//! 已知限制（M2 壳）：
//! - 不注入 pyre 启动所需 --search-path / --source-directory 等参数；用户 .pyre_config 自管。
//! - 不解析 .pyre_configuration.toml；同基于 LSP 标准 auto-discovery。

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
pub struct PyreServerAdapter;

/// 候选可执行名（依次尝试）。
fn locate_pyre() -> Option<std::ffi::OsString> {
    which_no_unc("pyre").map(|p| p.into_os_string())
}

#[async_trait]
impl LanguageServerAdapter for PyreServerAdapter {
    fn id(&self) -> &'static str {
        "pyre"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Python];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = locate_pyre().ok_or_else(|| {
            not_installed_error(
                "pyre",
                "install pyre (`pip install pyre-check`) and ensure `pyre` is on PATH",
            )
        })?;
        // pyre 通过 `pyre start` 进入 LSP server 模式（2024+ 默认进入 LSP）。
        Ok(LaunchInfo {
            cmd: vec![exe, "start".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // pyre 不消费 pyright 风格的 python.pythonPath，但初始化层仍扫 venv 用于
        // 未来切换；M2 保持空（探测到即打 hint 写日志，M3+ 真接时补 wire）。
        let _ = base;
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        use serde_json::json;
        let uri = crate::probe_uri_for_root(
            PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").as_deref().unwrap_or(Path::new(".")),
            "file:///__pyre_ready_probe__",
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
        false
    }
}