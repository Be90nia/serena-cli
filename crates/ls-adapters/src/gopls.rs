//! gopls 适配器（PLAN M3 / Task T2 第 3 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/gopls_language_server.py`
//!
//! gopls 是 Go 官方 LSP server。它需要 `GOPATH` / `GOMODCACHE` 等环境，但通常
//! 这些已通过 `go env` 设置好；不强求注入。
//!
//! 启动 quirk：gopls 启动比 rust-analyzer 慢（~1-3s 解析 GOPATH）。无 file_associations
//! 需要补 —— 默认 `*.go` 即可。
//!
//! ## 深度（M2 落地）
//!
//! - `initialize_patches`：root 下探测 `go.work`（多 module workspace），命中则
//!   `workspaceFolders = [root, ...modules]` —— gopls 据此一次性解析多个 module。
//!   探测走 `lsp_core::workspace_folders::discover_additional_workspace_folders`
//!   （同步覆盖 Cargo workspace + git submodules；gopls 视角下 go.work 命中即生效，
//!   其他 marker 命中不影响 gopls —— 它会忽略无关 folder；URL/等后续 monorepo）。

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
const PROBE_FALLBACK: &str = "file:///__gopls_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct GoplsAdapter;

#[async_trait]
impl LanguageServerAdapter for GoplsAdapter {
    fn id(&self) -> &'static str {
        "gopls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Go];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("gopls").ok_or_else(|| {
            not_installed_error(
                "gopls",
                "install gopls (`go install golang.org/x/tools/gopls@latest`) and ensure `gopls` is on PATH",
            )
        })?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // M2 深度：探测 go.work / Cargo workspace / git submodules → 追加到 workspaceFolders。
        // supervisor 已设 root 为唯一 folder；这里 extend 多 module。
        // 探测未命中（单 module 项目）→ 不动 workspaceFolders。
        let root = PROBE_ROOT
            .lock()
            .expect("PROBE_ROOT poisoned")
            .clone();
        let Some(root) = root else {
            return;
        };
        let extra = lsp_core::workspace_folders::discover_additional_workspace_folders(&root);
        if extra.is_empty() {
            return;
        }
        let folders = base
            .workspace_folders
            .get_or_insert_with(Vec::new);
        folders.extend(extra);
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 探针必须用 root 下真实文件：虚拟 URI 不触发 gopls 的 module lazy-load，
        // 首个真实工具请求就得独自承担全量加载（cold-start hang 同根因，
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
        // gopls 支持 `textDocument/implementation`（interface → struct 方法跳转）。
        true
    }
}

impl GoplsAdapter {
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
        let adapter = GoplsAdapter;

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

    /// go.work 多 module：root 下 .gitignore + go.work（use 2 个 module）→ 命中。
    /// initialize_patches 后 workspaceFolders 至少 2 个（root + 至少 1 extra）。
    #[test]
    fn initialize_patches_appends_workspace_folders_on_go_work() {
        use std::str::FromStr;
        let dir = tempfile::tempdir().unwrap();
        // 造 go.work: 2 个 module（带 `./` 前缀，真实项目最常见形态）
        std::fs::write(
            dir.path().join("go.work"),
            "go 1.22\n\nuse (\n\t./mod-a\n\t./mod-b\n)\n",
        )
        .unwrap();
        // mod-a 与 mod-b 各放一个目录（确保存在 — discover 过滤不存在的）
        std::fs::create_dir_all(dir.path().join("mod-a")).unwrap();
        std::fs::create_dir_all(dir.path().join("mod-b")).unwrap();
        let adapter = GoplsAdapter;
        adapter.set_project_root(dir.path());
        let mut params = lsp_types::InitializeParams::default();
        // supervisor 已设 root 为唯一 folder；模拟之。
        let uri_str = lsp_core::docsync::path_to_uri_str(dir.path());
        params.workspace_folders = Some(vec![lsp_types::WorkspaceFolder {
            uri: lsp_types::Uri::from_str(&uri_str).unwrap(),
            name: "root".to_string(),
        }]);
        adapter.initialize_patches(&mut params);
        let folders = params
            .workspace_folders
            .expect("initialize_patches 应保留 workspaceFolders");
        assert!(
            folders.len() >= 3,
            "go.work 多 module 必须追加 2 个 folder: got {} folders",
            folders.len()
        );
    }

    /// 未命中 monorepo marker：workspaceFolders 不变。
    #[test]
    fn initialize_patches_no_op_when_no_monorepo_marker() {
        use std::str::FromStr;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.go"), "package main\n").unwrap();
        let adapter = GoplsAdapter;
        adapter.set_project_root(dir.path());
        let mut params = lsp_types::InitializeParams::default();
        let uri_str = lsp_core::docsync::path_to_uri_str(dir.path());
        params.workspace_folders = Some(vec![lsp_types::WorkspaceFolder {
            uri: lsp_types::Uri::from_str(&uri_str).unwrap(),
            name: "root".to_string(),
        }]);
        let before = params.workspace_folders.clone();
        adapter.initialize_patches(&mut params);
        assert_eq!(
            params.workspace_folders, before,
            "未命中 marker 必须不动 workspaceFolders"
        );
    }
}
