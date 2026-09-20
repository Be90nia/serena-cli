//! ty (Astro Python type checker LSP) 适配器壳（PLAN Phase 2 / Task 24）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/ty_language_server.py`
//!
//! ty 是 Astral（Ruff 团队）的新 Python 类型检查器，2026 年起提供 LSP server。
//! 与 pyright / basedpyright 同样消费 `python.pythonPath` 字段（ty 协议对齐 pyright）。
//!
//! 启动：探测 `ty`（uv tool install ty）。ty 默认走 stdio LSP，无 --stdio flag。
//!
//! 已知限制（M2 壳）：
//! - 不实现 ty 特异 CLI flag；M3+ 按需加。
//! - 不写 ty.toml 自动生成；用户自管。

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
pub struct TyServerAdapter;

/// 候选可执行名（依次尝试）。
fn locate_ty() -> Option<std::ffi::OsString> {
    which_no_unc("ty").map(|p| p.into_os_string())
}

#[async_trait]
impl LanguageServerAdapter for TyServerAdapter {
    fn id(&self) -> &'static str {
        "ty"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Python];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = locate_ty().ok_or_else(|| {
            not_installed_error(
                "ty",
                "install ty (`uv tool install ty` or `pip install ty`) and ensure `ty` is on PATH",
            )
        })?;
        // ty 默认走 stdio LSP。
        Ok(LaunchInfo {
            cmd: vec![exe],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // 与 pyright 同款 venv 探测 → 注入 python.pythonPath。
        let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
        let Some(interp) = root.as_deref().and_then(crate::pyright::find_python_interpreter) else {
            return;
        };
        let opts = base
            .initialization_options
            .get_or_insert_with(serde_json::Value::default);
        if !opts.is_object() {
            *opts = serde_json::json!({});
        }
        opts["python"]["pythonPath"] =
            serde_json::Value::String(interp.to_string_lossy().into_owned());
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        use serde_json::json;
        let uri = crate::probe_uri_for_root(
            PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").as_deref().unwrap_or(Path::new(".")),
            "file:///__ty_ready_probe__",
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