//! basedpyright 适配器壳（PLAN Phase 2 / Task 24 Python LSP 同构占位）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/basedpyright_language_server.py`
//!
//! basedpyright 是 pyright 的 fork（基于 pylance + 偏严格配置），同样消费
//! `initializationOptions.python.pythonPath` 字段。架构 / 探测与 `pyright.rs`
//! 一致；M2 探测层（venv interpreter）在 `pyright.rs` 落地，本壳走同款 pattern。
//!
//! 启动：探测 `basedpyright-langserver` / `basedpyright-langserver@latest`（uvx
//! 入口由 ls-registry Task 19 铺），无则报 `LS_NOT_INSTALLED`。
//!
//! 已知限制（M2 壳）：
//! - 不实现 basedpyright 特异 CLI flag（--verbose / --strict 等）；按需 M3+ 加。
//! - 不处理 basedpyrightconfig.json；pyrightconfig.json 兼容即可。

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
pub struct BasedpyrightServerAdapter;

/// 候选可执行名（依次尝试）。
///
/// ponytail: 不抽 install_hint 模板 —— `not_installed_error` 已封装常见 hint。
fn locate_basedpyright() -> Option<std::ffi::OsString> {
    which_no_unc("basedpyright-langserver")
        .or_else(|| which_no_unc("basedpyright"))
        .map(|p| p.into_os_string())
}

#[async_trait]
impl LanguageServerAdapter for BasedpyrightServerAdapter {
    fn id(&self) -> &'static str {
        "basedpyright"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Python];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = locate_basedpyright().ok_or_else(|| {
            not_installed_error(
                "basedpyright",
                "install basedpyright (`pip install basedpyright` or `uv tool install basedpyright`) and ensure `basedpyright-langserver` is on PATH",
            )
        })?;
        // 同 pyright：basedpyright-langserver 也必须 --stdio（servers.toml uvx 同参）。
        let cmd = vec![exe, "--stdio".into()];
        Ok(LaunchInfo {
            cmd,
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // 与 pyright 同款 venv 探测 → 注入 python.pythonPath（共享探测函数）。
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
        // 探针逻辑同 pyright：根下真实 .py 文件 URI 触发项目解析。失败返回 Ok 放行。
        use serde_json::json;
        let uri = crate::probe_uri_for_root(
            PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").as_deref().unwrap_or(Path::new(".")),
            self.languages(),
            "file:///__basedpyright_ready_probe__",
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
        // basedpyright 沿袭 pyright，pyright 不实现 textDocument/implementation。
        false
    }
}