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

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__pyright_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

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

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 探针必须用 root 下真实文件：虚拟 URI 不触发 pyright 的 workspace lazy-load，
        // 首个真实工具请求就得独自承担全量分析（cold-start hang 同根因，
        // 见 local/cold-start-hang-diagnosis.md）。失败也返回 Ok 让 supervisor 放行。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": self.probe_uri() } }),
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

impl PyrightAdapter {
    /// `on_server_ready` 将发出的探针 URI：root 下真实小文件的 file URI；root 未设置
    /// 或无候选文件时退虚拟 URI。
    fn probe_uri(&self) -> String {
        let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
        match root {
            Some(root) => crate::probe_uri_for_root(&root, PROBE_FALLBACK),
            None => PROBE_FALLBACK.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = PyrightAdapter;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(uri.ends_with(".gitignore"), "应指向真实文件: {uri}");

        let empty = tempfile::tempdir().unwrap();
        adapter.set_project_root(empty.path());
        assert_eq!(adapter.probe_uri(), PROBE_FALLBACK);
    }
}
