//! Deno 官方 LS（`deno lsp` 子命令）适配器（W2 批次）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/deno_language_server.py`
//!
//! ## 要点
//!
//! - **入口是子命令不是 --stdio flag**：`deno lsp`（裸跑 `deno` = CLI 帮助即退）；
//!   servers.toml [servers.deno] exec = ["{bin}", "lsp"] 同源（pgls `lsp-proxy` 先例）。
//! - **experimental 显式路由门**：上游标注 "overlaps the TypeScript server on file
//!   extensions... must be selected explicitly"——本适配器同样仅 `--lang deno` 显式
//!   路由可达，TS 家族扩展名一律归 typescript 门（pgsql/mysql 先例；
//!   `probe_extensions(Deno)` 为空表，by_shebang 的 deno 解释器也仍归 TypeScript）。
//! - **initializationOptions 即设置面**（↖ mirror `_create_base_initialize_params`：
//!   "deno lsp reads its settings from initializationOptions; enabling the server and
//!   the linter mirrors the defaults of the official VS Code Deno extension"——裸启动
//!   enable=false 是死服务器，enable/lint 必须显式注入）。
//! - didOpen languageId 基础值 "typescript"（lsp_language_id 换算），per-file 覆盖
//!   ↖ mirror `_get_language_id_for_file`：.tsx→"typescriptreact"、.jsx→
//!   "javascriptreact"、js/mjs/cjs→"javascript"——发错值符号 range 会在 JSX 处截断。
//! - 二进制解析：serena download 缓存（install deno 钉版）优先 → PATH（上游
//!   `shutil.which` 语义）→ 标准未安装错误。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_core::framing::JsonRpc;
use lsp_types::InitializeParams;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// npm/download 缓存目录 pin（= servers.toml [servers.deno] download 段，禁随意改）。
const CACHE_ID: &str = "deno";
const CACHE_VERSION: &str = "2.9.7";

#[derive(Debug, Default, Clone, Copy)]
pub struct DenoAdapter;

/// 缓存产物名：zip 根 `deno.exe`（windows）/ `deno`（unix）——
/// servers.toml bin_path 单值 + bin_path_per_platform 覆盖同源。
fn cached_binary() -> PathBuf {
    let name = if cfg!(windows) { "deno.exe" } else { "deno" };
    default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join(name)
}

/// deno 二进制解析：download 缓存优先 → PATH（↖ mirror `DependencyProvider
/// ._get_or_install_core_dependency` 的 `shutil.which`）→ 未安装错误。
fn resolve_deno() -> anyhow::Result<PathBuf> {
    let cached = cached_binary();
    if cached.is_file() {
        return Ok(cached);
    }
    which_no_unc("deno").ok_or_else(|| {
        not_installed_error(
            "deno",
            "run `serena-cli install deno` (GitHub release denoland/deno v2.9.7) \
             or install deno (https://docs.deno.com/runtime/) and ensure it is on PATH",
        )
    })
}

/// server→client `workspace/configuration` 应答：每个 item 回一个空配置对象
/// （↖ mirror `configuration_handler`；deno lsp 启动期即拉取，拿 {} 回落
/// initializationOptions）。
fn configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    Some(Value::Array(vec![json!({}); items]))
}

#[async_trait]
impl LanguageServerAdapter for DenoAdapter {
    fn id(&self) -> &'static str {
        "deno lsp"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Deno];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let deno = resolve_deno()?;
        Ok(LaunchInfo {
            // 入口 = `deno lsp` 子命令（非 --stdio flag；pgls `lsp-proxy` 同款形态）。
            cmd: vec![deno.into_os_string(), "lsp".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        let opts = base
            .initialization_options
            .get_or_insert_with(Value::default);
        if !opts.is_object() {
            *opts = json!({});
        }
        // ↖ mirror `_create_base_initialize_params`：enable + lint 对齐 VS Code 扩展
        // 默认值（裸 deno lsp 的 enable=false 会让所有请求静默降级），unstable 关。
        opts["enable"] = json!(true);
        opts["lint"] = json!(true);
        opts["unstable"] = json!(false);
    }

    fn set_project_root(&self, _root: &Path) {}

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        // didOpen languageId per-file 覆盖（↖ mirror `_get_language_id_for_file`；
        // .ts/.mts/.cts 用基础值 "typescript"，由 supervisor 经 lsp_language_id 注入）。
        session.set_language_id_for_extensions(&[
            ("tsx", "typescriptreact"),
            ("jsx", "javascriptreact"),
            ("js", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ]);
        session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);
        // 就绪探针省略（Δ astro）：probe_extensions(Deno) = 空表（TS 家族扩展名不抢，
        // 探针候选名单是非源码文件），无有效探针 URI；deno lsp 无 lazy-load 语义，
        // 冷启动差异由首个真实请求吸收。
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // ↖ mirror `supports_implementation_request() -> True`（上游显式 override）。
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_enables_server_and_linter() {
        let mut p = InitializeParams::default();
        DenoAdapter.initialize_patches(&mut p);
        let opts = p.initialization_options.expect("options written");
        assert_eq!(opts["enable"], json!(true));
        assert_eq!(opts["lint"], json!(true));
        assert_eq!(opts["unstable"], json!(false));
    }

    #[tokio::test]
    async fn launch_uses_lsp_subcommand() {
        // bd 83f：环境变量改动全程持 crate 级共享锁——并发 set_var("PATH")
        // 与 powershell/vts 测试互踩（9ai 假红同根因）。
        let _env = crate::ENV_TEST_LOCK.lock().await;
        // 本机零安装铁律：PATH 注入 fake deno（cache 优先、PATH 兜底的兜底分支）。
        let deno_name = if cfg!(windows) { "deno.exe" } else { "deno" };
        let deno_dir = tempfile::tempdir().expect("tempdir");
        let fake_deno = deno_dir.path().join(deno_name);
        std::fs::write(&fake_deno, b"fake").expect("fake deno");
        // unix which() 走 X_OK：fs::write 产物 0644 无执行位会被判不存在。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_deno, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let path_original = std::env::var_os("PATH").unwrap_or_default();
        // SAFETY: ENV_TEST_LOCK 保证进程内独占；函数尾恢复原值。
        unsafe {
            let new_path = std::env::join_paths(
                std::iter::once(deno_dir.path().to_path_buf())
                    .chain(std::env::split_paths(&path_original)),
            )
            .expect("join path");
            std::env::set_var("PATH", &new_path);
        }
        let info = DenoAdapter
            .launch_info(&ProjectCtx {
                project_root: std::env::temp_dir(),
            })
            .await;
        // SAFETY: 恢复先于任何断言（panic 路径除外；ENV_TEST_LOCK 持有中）。
        unsafe {
            std::env::set_var("PATH", path_original);
        }
        let info = info.expect("launch_info with fake deno on PATH");
        assert_eq!(
            info.cmd[1].to_string_lossy(),
            "lsp",
            "入口必须是 deno lsp 子命令"
        );
        assert!(
            info.cmd[0].to_string_lossy().contains(deno_name),
            "cmd[0] 应为解析到的 deno 二进制: {}",
            info.cmd[0].to_string_lossy()
        );
    }

    #[test]
    fn configuration_reply_mirrors_items() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [{ "section": "deno" }, {}] }),
        );
        assert_eq!(configuration_reply(msg), Some(json!([{}, {}])));
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(configuration_reply(msg), None);
    }

    #[test]
    fn deno_declares_language_closure() {
        assert_eq!(DenoAdapter.languages(), &[LanguageId::Deno]);
        assert_eq!(LanguageId::from_str_opt("deno"), Some(LanguageId::Deno));
        // 显式路由门：TS 家族扩展名不得路由到 Deno。
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"] {
            assert_ne!(
                LanguageId::from_extension(ext),
                Some(LanguageId::Deno),
                "{ext}: deno 不抢 TS 家族扩展名"
            );
        }
    }
}
