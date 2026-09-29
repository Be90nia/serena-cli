//! Some Sass（`some-sass-language-server`）适配器（W2 批次）。
//!
//! ↖ mirror: oraios/serena@7a296833
//! `solidlsp/language_servers/some_sass_language_server.py`
//!
//! ## 要点
//!
//! - SCSS / Sass indented 双语法的专用 LS（@use/@forward 跨文件导航、SassDoc）。
//!   `.sass`/`.scss` 扩展名归本门；`.css` 归 css 门（Δ 上游：上游把 .css 也路由给
//!   Some Sass，我们的 css 门已由 vscode-css-language-server 接管，不抢）。
//! - 初始化即注入完整 `somesass` 配置片（↖ mirror `SOMESASS_INIT_OPTIONS`）：css
//!   特性全开但 lint 关（上游注释：lint 规则过于主观，噪音大）——css 片在本工程路由
//!   下不会被消费（.css 不进本门），保留以对齐上游初始化面；`workspace/configuration`
//!   应答同享该片（↖ mirror `_handle_workspace_configuration`：somesass 节回片、
//!   editor/未知节回空对象）。
//! - didOpen languageId 基础值 "scss"（lsp_language_id 换算），per-file 覆盖
//!   ↖ mirror `_get_language_id_for_file`：.sass→"sass"——发错 id 会把缩进语法
//!   路由到 scss 解析器。
//! - 上游未 override supports_implementation_request → false。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_core::framing::JsonRpc;
use lsp_types::InitializeParams;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 主 LS 就绪探针超时（documentSymbol 对真实 .scss；兼触发 some-sass 的 workspace 扫描）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// npm 缓存目录 id + 版本 pin（= servers.toml [servers.scss] npm 段，禁随意改）。
/// 条目 id 保持 scss（缓存目录键），路由语言名是 sass。
const CACHE_ID: &str = "scss";
const CACHE_VERSION: &str = "2.3.8";

/// root 未设置时的退路：虚拟探针 URI（仅保底，真实 .scss/.sass 才触发扫描）。
const PROBE_FALLBACK: &str = "file:///__some_sass_ready_probe__";

/// 当前会话项目 root（零字段单例存不了实例状态 —— 会话级数据放静态槽，由
/// supervisor::session_for 在 `on_session_ready` 前经 `set_project_root` 写入）。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct SassAdapter;

/// serena npm 缓存安装目录（`{cache}/scss/2.3.8`）。
fn install_dir() -> PathBuf {
    default_cache_root().join(CACHE_ID).join(CACHE_VERSION)
}

/// some-sass-language-server 入口 js（bin 字段 = bin/some-sass-language-server，
/// 无扩展名 node 脚本；npm registry 2.3.8）。
fn sass_entry(install: &Path) -> PathBuf {
    install.join("node_modules/some-sass-language-server/bin/some-sass-language-server")
}

fn node_on_path() -> anyhow::Result<PathBuf> {
    which_no_unc("node").ok_or_else(|| {
        anyhow::anyhow!("node not on PATH; required to run some-sass-language-server")
    })
}

/// 安装完整性校验（node_modules + 入口 js），否则标准未安装错误。
fn resolve_install() -> anyhow::Result<PathBuf> {
    let dir = install_dir();
    if dir.join("node_modules").is_dir() && sass_entry(&dir).is_file() {
        return Ok(dir);
    }
    Err(not_installed_error(
        "some-sass-language-server",
        "run `serena-cli install sass` (npm: some-sass-language-server 2.3.8)",
    ))
}

/// `somesass.css.*` 特性开关全开、lint 关（↖ mirror `SOMESASS_CSS_FEATURES`；lint
/// 规则主观噪音大，上游刻意保持关闭——本工程路由下 css 片不被消费，纯对齐上游）。
fn somesass_css_features() -> Value {
    json!({
        "codeAction": { "enabled": true },
        "colors": { "enabled": true },
        "completion": { "enabled": true },
        "definition": { "enabled": true },
        "diagnostics": { "enabled": true, "lint": { "enabled": false } },
        "documentSymbols": { "enabled": true },
        "foldingRanges": { "enabled": true },
        "highlights": { "enabled": true },
        "hover": { "enabled": true },
        "links": { "enabled": true },
        "references": { "enabled": true },
        "rename": { "enabled": true },
        "selectionRanges": { "enabled": true },
        "signatureHelp": { "enabled": true },
        "workspaceSymbol": { "enabled": true },
    })
}

/// 初始化 `somesass` 配置片（↖ mirror `SOMESASS_INIT_OPTIONS`）。
fn somesass_section() -> Value {
    json!({
        "somesass": {
            "css": somesass_css_features(),
            "workspace": { "loadPaths": [] },
            "suggest": { "suggestFromUseOnly": false },
        }
    })
}

/// 初始化补丁：initializationOptions = somesass 片（↖ mirror
/// `_create_base_initialize_params`）。
fn patch_main_options(base: &mut InitializeParams) {
    let opts = base
        .initialization_options
        .get_or_insert_with(Value::default);
    if !opts.is_object() {
        *opts = json!({});
    }
    let section = somesass_section();
    for (k, v) in section.as_object().expect("somesass object") {
        opts[k] = v.clone();
    }
}

/// server→client `workspace/configuration` 应答：somesass 节回配置片，其余（editor /
/// 未知）回空对象（↖ mirror `_handle_workspace_configuration`）。
fn configuration_reply(msg: JsonRpc) -> Option<Value> {
    let section_slice = somesass_section();
    let somesass = section_slice.get("somesass").cloned().expect("somesass key");
    let items = msg
        .params
        .as_ref()?
        .get("items")?
        .as_array()?
        .iter()
        .map(|item| {
            let section = item.get("section").and_then(Value::as_str).unwrap_or("");
            if section == "somesass" {
                somesass.clone()
            } else {
                json!({})
            }
        })
        .collect::<Vec<_>>();
    Some(Value::Array(items))
}

#[async_trait]
impl LanguageServerAdapter for SassAdapter {
    fn id(&self) -> &'static str {
        "some-sass-language-server"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Sass];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let install = resolve_install()?;
        let node = node_on_path()?;
        Ok(LaunchInfo {
            cmd: vec![
                node.into_os_string(),
                sass_entry(&install).into_os_string(),
                "--stdio".into(),
            ],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        patch_main_options(base);
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        // didOpen languageId per-file 覆盖（↖ mirror `_get_language_id_for_file`；
        // .scss 用基础值 "scss"，.css 不进本门故不设映射——Δ 上游同位）。
        session.set_language_id_for_extensions(&[("sass", "sass")]);
        session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);

        // 就绪探针：真实 .scss/.sass 优先（some-sass 初始化后自扫 workspace，
        // documentSymbol 兼作预热），失败只放行。
        let root = PROBE_ROOT
            .lock()
            .expect("PROBE_ROOT poisoned")
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let probe_uri = crate::probe_uri_for_root(&root, &[LanguageId::Sass], PROBE_FALLBACK);
        let probe = session
            .request::<Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": probe_uri } }),
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
        // 上游静态先验：SomeSassLanguageServer 未 override supports_implementation_request。
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_injects_somesass_slice() {
        let mut p = InitializeParams::default();
        patch_main_options(&mut p);
        let opts = p.initialization_options.expect("options written");
        assert_eq!(opts["somesass"]["suggest"]["suggestFromUseOnly"], json!(false));
        assert_eq!(opts["somesass"]["css"]["hover"]["enabled"], json!(true));
        // lint 关（上游刻意）：诊断开着但主观 lint 规则不上。
        assert_eq!(
            opts["somesass"]["css"]["diagnostics"],
            json!({ "enabled": true, "lint": { "enabled": false } })
        );
    }

    #[test]
    fn configuration_reply_serves_somesass_section() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [
                { "section": "somesass" },
                { "section": "editor" },
                {},
            ] }),
        );
        let somesass = somesass_section()["somesass"].clone();
        assert_eq!(
            configuration_reply(msg),
            Some(json!([somesass, {}, {}]))
        );
        // 缺 items → None（默认 null 成功应答路径在 client 层）。
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(configuration_reply(msg), None);
    }

    #[tokio::test]
    async fn launch_uses_stdio_entry() {
        // 未装时必须给标准未安装错误（LS_NOT_INSTALLED 语义），不触网。
        let dir = tempfile::tempdir().expect("tempdir");
        // 注入 HOME/LOCALAPPDATA 指向空缓存 + 清 PATH（node 探测必败）→ 报错分支。
        // 仅断言错误形态，不追求启动成功（真启动 = CI 真机门）。
        let result = SassAdapter
            .launch_info(&ProjectCtx {
                project_root: dir.path().to_path_buf(),
            })
            .await;
        match result {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("node") || msg.contains("some-sass"),
                    "错误应指向 node 缺失或 LS 未安装: {msg}"
                );
            }
            Ok(info) => {
                // 本机恰有 node + 缓存：断言 stdio 形态兜底。
                assert!(matches!(info.transport, TransportKind::Stdio));
                assert!(info.cmd.iter().any(|a| a.to_string_lossy().ends_with("--stdio")));
            }
        }
    }

    #[test]
    fn sass_declares_language_closure() {
        assert_eq!(SassAdapter.languages(), &[LanguageId::Sass]);
        assert_eq!(LanguageId::from_str_opt("sass"), Some(LanguageId::Sass));
        assert_eq!(LanguageId::from_extension("sass"), Some(LanguageId::Sass));
        assert_eq!(LanguageId::from_extension("scss"), Some(LanguageId::Sass));
        // .css 归 css 门（不抢）。
        assert_eq!(LanguageId::from_extension("css"), Some(LanguageId::Css));
    }
}
