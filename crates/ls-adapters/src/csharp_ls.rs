//! csharp_ls 适配器（PLAN M3 / Task T2 第 5 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/csharp_language_server.py`
//!
//! csharp_ls（https://github.com/razzmatazz/csharp-language-server）是 Roslyn 的 LSP 包装，
//! 启动快（vs omnisharp-roslyn 慢 ~10x）。默认走 `dotnet tool install -g csharp-ls` 安装。
//!
//! 启动 quirk：
//! - csharp_ls 启动需 ~3s 加载 Roslyn workspaces。
//! - 默认 .sln/.csproj 自动发现；不需要 --solution。
//!
//! ## 深度（M2 落地）
//!
//! - 上游 `oraios/serena@43ae021` 已将 csharp 适配器从 csharp_ls 迁到 Roslyn 官方
//!   `vscode-csharp`（microsoft/vscode-csharp 的 LSP 端 = Roslyn LSP server）。
//!   我们**暂留 csharp_ls 不切**——理由见 `local/csharp-ls-decision.md`。
//! - `prepare_csharp_ls_to_roslyn` stub：占位接口，未来切换 Roslyn LS 时改这一处即可。
//!   当前不调用，仅 doc + compile 验证 trait shape。
//!
//! 已知限制：
//! - 不实现 omnisharp-roslyn 兼容路径 —— 它单独有 `omnisharp` binary 和协议差异。
//! - 不注入 .editorconfig 读取 —— 用户 workspace 自管。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(60); // csharp_ls 启动慢

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__csharp_ls_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct CsharpLsAdapter;

#[async_trait]
impl LanguageServerAdapter for CsharpLsAdapter {
    fn id(&self) -> &'static str {
        "csharp-ls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::CSharp];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("csharp-ls").ok_or_else(|| {
            not_installed_error(
                "csharp-ls",
                "install csharp-ls (`dotnet tool install -g csharp-ls`) and ensure `csharp-ls` is on PATH",
            )
        })?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // 无 quirk。
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 探针必须用 root 下真实文件：虚拟 URI 不触发 Roslyn workspace 加载相关的
        // 项目 lazy-load，首个真实工具请求就得独自承担（cold-start hang 同根因，
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
        true
    }
}

impl CsharpLsAdapter {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = CsharpLsAdapter;

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

    /// Roslyn LS 切换 stub：当前返 None（不切），仅验证函数 shape。
    /// 真实切换将走 microsoft/vscode-csharp 的 LSP 端 = Roslyn LSP server（dotnet
    /// 工具安装 + 启动约定），见 local/csharp-ls-decision.md §3。
    #[test]
    fn prepare_csharp_ls_to_roslyn_stub_returns_none() {
        let resolved = prepare_csharp_ls_to_roslyn();
        assert!(
            resolved.is_none(),
            "M2 stub 必须返 None（决策暂留 csharp_ls）: got {resolved:?}"
        );
    }
}

/// Roslyn LS 切换 stub（`prepare_csharp_ls_to_roslyn`）。
///
/// 当前**不切**：上游 `oraios/serena@43ae021` 的 csharp 适配器已迁到 Roslyn LS
/// （microsoft/vscode-csharp 的 LSP 端），但本地因下述原因暂留 csharp_ls：
/// 1. csharp_ls 启动快（~3s vs omnisharp 30s+），冷启动 UX 占优；
/// 2. Roslyn LS 需 `Microsoft.VisualStudio.Code.Tools.ServiceDefaults` + 项目
///    restore workflow，setup 复杂（dotnet workload install + 手动 MSBuild
///    discovery），M2 不到；
/// 3. 真实用户多用 csharp_ls + .NET SDK 5-8（已稳定 4+ 年）。
///
/// 切 Roslyn 时的改动入口（**M3+**）：
/// - `prepare_csharp_ls_to_roslyn` 返 `Some(RoslynLaunchPlan)`；
/// - `launch_info` 走 Roslyn 分支（vscode-csharp `RoslynLSPServer` 或等价物）；
/// - `initialize_patches` 设 `RoslynLSPServerOptions`（workspace settings path）。
///
/// 返回 `Option<RoslynLaunchPlan>` 让调用方未来能判别实现与否；
/// 当前 `None` 即「M2 暂不实现」的契约。
#[derive(Debug, Clone)]
pub struct RoslynLaunchPlan {
    /// Roslyn LS 可执行路径（vscode-csharp 提供的 dotnet tool）。
    pub exe_path: std::path::PathBuf,
    /// 启动参数（含 dotnet host 入口）。
    pub args: Vec<String>,
    /// Roslyn 特有 init patch（workspace settings path / log level）。
    pub init_patch_keys: Vec<String>,
}

#[allow(dead_code)] // M2 stub 不调用（决策暂留 csharp_ls）
pub(crate) fn prepare_csharp_ls_to_roslyn() -> Option<RoslynLaunchPlan> {
    // M2 stub：决策暂留 csharp_ls（见 local/csharp-ls-decision.md）。
    None
}
