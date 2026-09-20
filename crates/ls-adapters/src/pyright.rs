//! pyright 适配器（PLAN M3 / Task T2 第 2 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/pyright_language_server.py`
//!
//! pyright 是 Microsoft 的 Python 类型检查 + LSP 实现。**它本身不可执行** —— 实际
//! 是 `pyright-langserver`（pip 包）或 `@pyright/langserver`（npm 包）。我们优先
//! 探测 `pyright-langserver`（pip 安装即可用），其次 `pyright`（新版直接当 LSP server 启动）。
//!
//! 启动方式：探测到的可执行 + `--stdio`。pyright 不需要 project_root 初始化文件（无
//! tsconfig/Cargo.toml 等价物）；`pyrightconfig.json` 仅影响检查策略不影响 LSP。
//!
//! ## 深度（M2 落地）
//!
//! - `initialize_patches`：root 下探测 `.venv/bin/python(.exe)` / `venv/...` / `.python-version`，
//!   命中则 `initializationOptions.python.pythonPath=<path>` —— pyright 用此路径解析 import，
//!   否则会用系统 python（与 venv 不一致 → 类型错乱）。
//!
//! ## 同构壳（T2 placeholder，Task 24 收口）
//!
//! `basedpyright_server` / `ty_server` / `pyre_server` / `jedi_server` 是其它 Python
//! LSP 实现，与 pyright 共享探测链；架构 / M2 探测层保持一致。

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
        // 两种二进制都必须 --stdio 进入 LSP 模式：实测 pyright-langserver 无 flag 时
        // 立即退出（`Connection input stream is not set`）；servers.toml uvx 条目同参。
        let cmd = vec![exe, "--stdio".into()];
        Ok(LaunchInfo {
            cmd,
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // M2 深度：探测 venv interpreter；命中 → 注入 `initializationOptions.python.pythonPath`。
        // pyright 用此路径解析 import 与 .pyi 搜索；缺省走系统 python，venv 项目会错乱。
        let root = PROBE_ROOT
            .lock()
            .expect("PROBE_ROOT poisoned")
            .clone();
        let Some(interp) = root.as_deref().and_then(find_python_interpreter) else {
            return;
        };
        let opts = base
            .initialization_options
            .get_or_insert_with(serde_json::Value::default);
        if !opts.is_object() {
            *opts = serde_json::json!({});
        }
        opts["python"]["pythonPath"] = serde_json::Value::String(interp.to_string_lossy().into_owned());
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
            Some(root) => crate::probe_uri_for_root(&root, self.languages(), PROBE_FALLBACK),
            None => PROBE_FALLBACK.to_string(),
        }
    }
}

/// 在 `root` 下探测 Python venv 解释器路径。命中优先级：
/// 1. `<root>/.venv/bin/python`(.exe) — uv / pip 标准约定
/// 2. `<root>/venv/bin/python`(.exe) — Debian / venv 经典命名
/// 3. `<root>/.python-version` 内容（pyenv 用户，文件首行 = 版本号，不返绝对路径，
///    仅用于文档/调试留 hint，pyright 启动参数仍走系统 python）
///
/// 返回绝对路径（dunce 去 UNC）。未命中返回 None —— 调用方（initialize_patches）
/// 走「不注入 pythonPath」fallback。
///
/// ponytail: 不读 pyenv shims 名（`python3.x` → shim 链）—— shim 解析是 pyenv 域，
/// 我们只兜到 `.venv/venv` 真实 venv，shimless CI 项目无 venv 时维持默认。
pub(crate) fn find_python_interpreter(root: &Path) -> Option<PathBuf> {
    let bin_name = if cfg!(windows) { "python.exe" } else { "python" };
    let candidates: &[&str] = &[".venv", "venv"];
    for venv in candidates {
        let candidate = root.join(venv).join("bin").join(bin_name);
        if candidate.is_file() {
            let canonical = dunce::canonicalize(&candidate).unwrap_or(candidate);
            return Some(canonical);
        }
    }
    None
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

    /// venv 探测：.venv/bin/python(.exe) 命中 → 返该路径。
    #[test]
    fn find_python_interpreter_dotvenv() {
        let dir = tempfile::tempdir().unwrap();
        let bin_name = if cfg!(windows) { "python.exe" } else { "python" };
        let venv_bin = dir.path().join(".venv").join("bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        let py = venv_bin.join(bin_name);
        std::fs::write(&py, "").unwrap();
        let found = find_python_interpreter(dir.path()).expect(".venv 应命中");
        assert!(
            found.ends_with(format!(".venv/bin/{bin_name}").replace('/', std::path::MAIN_SEPARATOR_STR).as_str()),
            "返回路径必须以 .venv/bin/python 结尾: {found:?}"
        );
    }

    /// venv 探测：无 .venv / venv → None（fallback 路径）。
    #[test]
    fn find_python_interpreter_absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.py"), "").unwrap();
        assert!(
            find_python_interpreter(dir.path()).is_none(),
            "无 venv 必须返 None"
        );
    }
}
