//! pyrefly（Meta Python 类型检查器 LSP）适配器（上游对拍采纳 W3b 批）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/pyrefly_server.py`
//!
//! ## 要点（batchA 四 quirk）
//!
//! - **workspace 边界**（↖ mirror `_ensure_workspace_pyrefly_config` :59-80）：root 无
//!   `pyrefly.toml`/`pyproject.toml` 时创建空 `pyrefly.toml`——否则 pyrefly 搜刮父目录
//!   配置、误解析 import 根。空文件 touch 失败仅 warn 放行（上游 log.warning 同款）。
//! - **configuration 必答**（↖ mirror `workspace_configuration_handler` :470-490）：
//!   pyrefly 收到 `workspace/configuration` 应答才开始索引；应答
//!   `{pythonPath, pyrefly.diagnosticMode: "workspace"}`（workspace 模式 = VSCode
//!   project mode 对齐，上游注释）。注册挂 `on_session_ready`（sass 先例；LS 惯例在
//!   initialized 通知后才发 configuration 请求——注册窗口与 sass 同构）。
//! - **变更取消重试**（↖ mirror `_install_mutation_retry` :82-130）：上游对
//!   -32800/-32800 透明重试 5×0.2s。-32801 ContentModified 已由 lsp-core client 层
//!   全局消化（`Session::start` 注册 `RETRY_ON_CONTENT_MODIFIED` 白名单，bd s3u）；
//!   -32800 RequestCancelled 无 per-LS 挂点（`RequestHooks` 是占位），**已知边界**：
//!   后台重索引窗口内的取消错误会外泄为 TOOL_TIMEOUT 语义的失败，重试即可。
//! - **索引进度等待**（↖ mirror `_start_server` 尾 `_indexing_complete.wait(30)`）：
//!   $/progress 全部 end 后视为索引完成；复用 `Session` 的 progress drain 工具
//!   （nextflow `wait_workspace_scan` 同款），30s 封顶超时 warn 放行。
//! - **didOpen languageId**：恒 "python"（↖ mirror `_get_language_id_for_file`
//!   :226-228；`lsp_language_id` 已显式映射，不赌 LS 对自名的宽容）。
//! - **launch**：`uvx --from pyrefly==1.2.0 pyrefly lsp`（↖ mirror
//!   `LanguageServerDependencyProviderUvx` + `extra_args=["lsp"]`；servers.toml
//!   `[servers.python_pyrefly].uvx` 留 install/doctor 面，版本 pin 同源）。
//! - 上游 `request_defining_symbol` 的 `__init__` → enclosing class 校正与
//!   `is_ignored_dirname` venv/__pycache__ 未镜像（defining-symbol 路径特化、目录
//!   忽略挂点均不存在——已知边界，跟随上游对拍横切裁决）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_core::framing::JsonRpc;
use lsp_types::InitializeParams;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, declare_work_done_progress,
    not_installed_error, which_no_unc,
};

/// 版本 pin（= servers.toml [servers.python_pyrefly].uvx，禁随意改）。
/// ↖ mirror: `PYREFLY_VERSION`（pyrefly_server.py@7a296833 L36）
const PYREFLY_VERSION: &str = "1.2.0";

/// 初始 workspace 索引等待上限。
/// ↖ mirror: `_indexing_complete.wait(timeout=30.0)`（`_start_server` 尾）
const INDEX_WAIT: Duration = Duration::from_secs(30);

/// root 未设置时的退路：虚拟探针 URI（ty/pyright 同款）。
const PROBE_FALLBACK: &str = "file:///__pyrefly_ready_probe__";

/// 当前会话项目 root（零字段单例存不了实例状态 —— 会话级数据放静态槽，由
/// supervisor::session_for 在 `on_session_ready` 前经 `set_project_root` 写入）。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct PyreflyServerAdapter;

/// quirk①：root 无 `pyrefly.toml`/`pyproject.toml` → touch 空 `pyrefly.toml`。
/// ↖ mirror: `_ensure_workspace_pyrefly_config`（pyrefly_server.py@7a296833 :59-80）
fn ensure_workspace_pyrefly_config(root: &Path) {
    if !root.is_dir() {
        return;
    }
    let pyrefly_toml = root.join("pyrefly.toml");
    let pyproject_toml = root.join("pyproject.toml");
    if pyrefly_toml.exists() || pyproject_toml.exists() {
        return;
    }
    match std::fs::File::create(&pyrefly_toml) {
        Ok(_) => tracing::warn!(
            "no config in repository root ({}): neither `pyrefly.toml` nor `pyproject.toml` \
             exists; created empty `pyrefly.toml` so pyrefly won't mis-resolve imports via \
             parent directories",
            root.display()
        ),
        Err(e) => tracing::warn!(
            "failed to create fallback pyrefly.toml at {}: {e}",
            pyrefly_toml.display()
        ),
    }
}

/// quirk②：server→client `workspace/configuration` 应答——每项回
/// `{pythonPath, pyrefly: {diagnosticMode: "workspace"}}`（pyrefly 收到才开始索引）。
/// ↖ mirror: `workspace_configuration_handler`（pyrefly_server.py@7a296833 :470-490）
fn configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
    let python_path = root
        .as_deref()
        .and_then(crate::pyright::find_python_interpreter)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python".to_string());
    let config = json!({
        "pythonPath": python_path,
        "pyrefly": { "diagnosticMode": "workspace" },
    });
    Some(json!(vec![config; items]))
}

/// uvx 缓存内入口解析：PATH 有 `pyrefly`（uv tool install 形态）优先，否则
/// `uvx --from pyrefly==<ver> pyrefly lsp`（↖ mirror Uvx provider argv 形态）。
fn resolve_launch() -> anyhow::Result<Vec<OsString>> {
    if let Some(entry) = which_no_unc("pyrefly") {
        let mut cmd = vec![entry.into_os_string()];
        cmd.push("lsp".into());
        return Ok(cmd);
    }
    let uvx = which_no_unc("uvx").ok_or_else(|| {
        not_installed_error(
            "pyrefly",
            "install pyrefly (`uv tool install pyrefly` or `pip install pyrefly`) or the uv runtime (`uvx` on PATH)",
        )
    })?;
    Ok(vec![
        uvx.into_os_string(),
        "--from".into(),
        format!("pyrefly=={PYREFLY_VERSION}").into(),
        "pyrefly".into(),
        "lsp".into(),
    ])
}

#[async_trait]
impl LanguageServerAdapter for PyreflyServerAdapter {
    fn id(&self) -> &'static str {
        "pyrefly"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::PythonPyrefly];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        // quirk①：launch 前保证 workspace 边界（上游在 __init__，即 start 前）。
        ensure_workspace_pyrefly_config(&ctx.project_root);
        Ok(LaunchInfo {
            cmd: resolve_launch()?,
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // ↖ mirror `_create_base_initialize_params`：`window.workDoneProgress = true`
        //（索引进度经 $/progress 上报，quirk④ 的等待依赖它）。
        declare_work_done_progress(base);
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        // quirk②：configuration 应答注册（sass 先例——LS 在 initialized 后才发请求）。
        session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);

        // quirk④：等初始索引（progress begin → drain 清空，30s 封顶）。
        // 超时仍在飞 → warn 放行（上游 `_indexing_complete.wait(30)` 超时分支
        // "proceeding anyway" 同款）。Δ progress 恒不来时上游耗满 30s，我方经
        // grace 分支提前放行——日志差异，行为等价。
        let _ = session
            .wait_indexing_start_or_completion(INDEX_WAIT, INDEX_WAIT)
            .await;
        if session.index_active_progress() > 0 {
            tracing::warn!("timeout waiting for pyrefly indexing completion, proceeding anyway");
        }

        // 就绪探针：真实 .py 优先（ty/pyright 同款；兼触发 pyrefly 文档装载），失败只放行。
        let root = PROBE_ROOT
            .lock()
            .expect("PROBE_ROOT poisoned")
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let probe_uri = crate::probe_uri_for_root(&root, &[LanguageId::Python], PROBE_FALLBACK);
        let probe = session
            .request::<Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": probe_uri } }),
                INDEX_WAIT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // ↖ mirror `_create_base_initialize_params`：capabilities 声明 implementation ✓
        //（solidlsp 基类按 capabilities 静态先验返回 true）。
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_reply_serves_python_path_and_workspace_mode() {
        // ↖ mirror workspace_configuration_handler：每 item 一份 config 拷贝。
        let msg = JsonRpc::request(
            1,
            "workspace/configuration",
            json!({ "items": [{ "section": "python" }, { "section": "pyrefly" }] }),
        );
        let reply = configuration_reply(msg).expect("reply for items");
        let arr = reply.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        for item in arr {
            assert!(item["pythonPath"].is_string(), "pythonPath required");
            assert_eq!(item["pyrefly"]["diagnosticMode"], json!("workspace"));
        }
        // 缺 items → None（默认 null 成功应答路径在 client 层）。
        let msg = JsonRpc::request(2, "workspace/configuration", json!({}));
        assert_eq!(configuration_reply(msg), None);
    }

    #[test]
    fn ensure_config_touches_only_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 无任何配置 → 创建空 pyrefly.toml。
        ensure_workspace_pyrefly_config(root);
        assert!(root.join("pyrefly.toml").is_file());
        // 已有 pyproject.toml → 不再动（删掉 pyrefly.toml 重验）。
        std::fs::remove_file(root.join("pyrefly.toml")).unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\n").unwrap();
        ensure_workspace_pyrefly_config(root);
        assert!(!root.join("pyrefly.toml").exists());
    }

    #[test]
    fn launch_prefers_entry_then_uvx_from() {
        // 环境门：无 pyrefly/uvx 的机器（CI runner 裸 env）跳过——resolve 依赖 PATH。
        if std::env::var_os("PATH").is_none_or(|p| {
            !["pyrefly", "pyrefly.exe", "uvx", "uvx.exe"]
                .iter()
                .any(|b| std::env::split_paths(&p).any(|d| d.join(b).is_file()))
        }) {
            return;
        }
        // 形状锁：uvx 路径四段 argv（resolve 结果依赖环境 PATH，锁 --from 分支形状）。
        let cmd = resolve_launch().expect("launch resolvable (pyrefly or uvx on PATH)");
        if cmd[0].to_string_lossy().ends_with("uvx")
            || cmd[0].to_string_lossy().ends_with("uvx.exe")
        {
            assert_eq!(cmd[1], "--from");
            assert_eq!(cmd[2].to_string_lossy(), "pyrefly==1.2.0");
            assert_eq!(cmd[3].to_string_lossy(), "pyrefly");
            assert_eq!(cmd[4].to_string_lossy(), "lsp");
        }
    }

    #[test]
    fn pyrefly_declares_language_closure() {
        assert_eq!(
            PyreflyServerAdapter.languages(),
            &[LanguageId::PythonPyrefly]
        );
        assert_eq!(
            LanguageId::from_str_opt("python_pyrefly"),
            Some(LanguageId::PythonPyrefly)
        );
        // .py 扩展名归主 Python 门——变体门仅 --lang 显式路由可达（deno/pgsql 先例）。
        assert_eq!(LanguageId::from_extension("py"), Some(LanguageId::Python));
    }
}
