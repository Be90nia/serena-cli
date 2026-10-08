//! `svelte-language-server`（svelteserver）适配器（W2 批次，hybrid 双服务器编排）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/svelte_language_server.py`
//!
//! ## hybrid 编排（astro.rs 同构，上游 `SvelteLanguageServer` + `SvelteTypeScriptServer`）
//!
//! 主 LS（svelteserver）承载 .svelte 结构与模板语义；伴生 TS LS
//! （typescript-language-server 挂 `typescript-svelte-plugin`）承载 ts/js 语义与
//! 跨文件（.ts↔.svelte 经 plugin，↖ mirror 类文档 "cross-file rename, find-references,
//! and go-to-definition from .ts/.js files into .svelte consumers"）：
//!
//! 1. `on_session_ready` 起伴生 TS LS（init options 注入 svelte 插件 + tsserver 路径，
//!    ↖ mirror `SvelteTypeScriptServer._create_base_initialize_params`）。
//! 2. 无 tsserver 桥（Δ vue）——上游 svelte 主 LS 不走 `tsserver/request` 转发机制。
//! 3. 生命周期绑定（两向不留孤儿）：伴生进程退出 → 主 session `shutdown`（监视任务
//!    `try_wait` 轮询）；主 session drop → 监视任务退出 —— 它是伴生 `Arc<Session>`
//!    的唯一强引用持有者，drop 即伴生 Job（KILL_ON_JOB_CLOSE）灭树。
//! 4. 会话建立时把 root 下 `.svelte`（限深 4，跳依赖/构建目录）在伴生上 `ensure_open`
//!    （↖ mirror `_ensure_svelte_files_indexed_on_ts_server`：plugin 的 getExternalFiles
//!    只在首个文件打开后生效，预打开 .svelte 才能进 TS program 图）。
//! 5. references 跨文件等待：伴生是同一 lsp-core `Session` `$/progress` 机制，
//!    typescript.rs/astro.rs 的 override 逻辑同源复制。
//!
//! 初始化形态（↖ mirror 两份 `_create_base_initialize_params`）：
//! - 主 LS：`initializationOptions = {isTrusted, dontFilterIncompleteCompletions,
//!   configuration: {…插件节表…, javascript/typescript/js-ts: {tsdk}}}`；
//! - 伴生：`plugins = [typescript-svelte-plugin @ <install>/node_modules/typescript-svelte-plugin,
//!   languages: ["svelte"]]` + `tsserver.path = <tsdk>`。
//!
//! 启动一律 `node <入口 js>` 直跑、不走 npm `.cmd` shim —— shim 会 cd 到自身目录，
//! 丢项目 cwd 与同目录 node_modules 解析（typescript.rs/vue.rs 同因同修）。
//!
//! Δ 上游（已记录的省略）：
//! - `request_references` 的 `$/getFileReferences`（ts/js）与 `$/getComponentReferences`
//!   （.svelte）主 LS 自定义方法增补未抄 —— 主+伴生两路合并已覆盖跨文件召回，
//!   自定义方法属增量召回，等真有漏报再补；
//! - `_wrap_notify_send_for_ts_js_mirror`（didChange 附加 `$/onDidChangeTsOrJsFile`）
//!   未抄 —— 需要 lsp-core 出站通知包装钩子（RequestHooks 仅入站占位）；主 LS 内部
//!   TS 快照仅影响上述未抄的自定义方法，与省略项同批。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::framing::JsonRpc;
use lsp_types::InitializeParams;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, ProjectRootSlot, RequestHooks,
    not_installed_error, which_no_unc,
};

/// 主 LS 就绪探针超时（documentSymbol 对真实 .svelte）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 伴生进程退出监视轮询间隔。
const WATCH_POLL: Duration = Duration::from_millis(500);

/// npm 缓存目录 id + 主包版本 pin（= servers.toml [servers.svelte] npm 段，禁随意改）。
/// 幂等语义照抄上游 `.installed_version` 四元组：目录名 pin 主包版本 + servers.toml
/// 钉死全部四版本（svelteserver 0.18.0 / typescript 6.0.3 / tsls 5.1.3 /
/// typescript-svelte-plugin 0.3.52），缓存目录存在 = 该版本组合已装。
const CACHE_ID: &str = "svelte";
const CACHE_VERSION: &str = "0.18.0";

/// root 未设置时的退路：虚拟探针 URI（仅保底，真实 .svelte 才触发项目 lazy-load）。
const PROBE_FALLBACK: &str = "file:///__svelte_ls_ready_probe__";

/// 首查等待 tsserver *开始*发 `$/progress` 的宽限窗（typescript.rs/astro.rs 同源常量）。
const INDEXING_START_GRACE: Duration = Duration::from_secs(5);

/// `$/progress` 索引等待兜底超时（astro.rs 同源：上游伴生 120s，对齐会话路径 30s）。
const INDEXING_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

/// 单会话伴生预打开的 `.svelte` 文件上限。
///
/// ponytail: 上游无上限全扫；200 之后放弃（跳过文件仅影响 references 召回，不崩），
/// 懒式按需打开等真撞上大仓库再说。
const MAX_SVELTE_FILES: usize = 200;

/// 当前会话项目 root 表（per-project 键化，bd serena-rust-4y6）。adapter 是零字段
/// 单例存不了实例状态 —— 会话级数据放静态槽，由 supervisor::session_for 在
/// `on_server_ready` 前经 `set_project_root` 写入（读侧无键，走 get_last 相邻语义）。
static PROBE_ROOT: ProjectRootSlot = ProjectRootSlot::new();

/// 当前伴生 TS LS 会话（hybrid 语义通道，见 trait `semantic_session`）。key = root。
/// 持有一份强引用；清理时机与 astro.rs 同构（on_session_ready 覆盖 / 监视任务两分支）。
static COMPANION: Mutex<Option<(PathBuf, Arc<lsp_core::session::Session>)>> = Mutex::new(None);

fn clear_companion() {
    if let Ok(mut slot) = COMPANION.lock() {
        *slot = None;
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SvelteAdapter;

/// serena npm 缓存安装目录（`{cache}/svelte/0.18.0`）——四包同装一处，tsdk / 插件 /
/// 两个入口 js 都从这里拼（↖ mirror 上游 `svelte-lsp-{version}` 单目录形态）。
fn install_dir() -> PathBuf {
    default_cache_root().join(CACHE_ID).join(CACHE_VERSION)
}

/// typescript `lib/`（tsdk）：tsserver 程序与内置 lib 的根。
fn tsdk_path(install: &Path) -> PathBuf {
    install.join("node_modules/typescript/lib")
}

/// `typescript-svelte-plugin` 包目录（伴生 init options `plugins[].location`）。
fn plugin_path(install: &Path) -> PathBuf {
    install.join("node_modules/typescript-svelte-plugin")
}

/// svelteserver 入口 js（绕 .cmd shim 直跑；bin 字段 = bin/server.js，npm registry 0.18.0）。
fn svelte_entry(install: &Path) -> PathBuf {
    install.join("node_modules/svelte-language-server/bin/server.js")
}

/// 伴生 typescript-language-server 入口 js（cli.mjs，typescript.rs/vue.rs 同款路径形态）。
fn ts_ls_entry(install: &Path) -> PathBuf {
    install.join("node_modules/typescript-language-server/lib/cli.mjs")
}

/// 安装完整性校验：node_modules + 两个入口 js + tsdk 齐才可用；否则标准未安装错误。
/// tsdk 检查 ↖ mirror 上游 `_get_tsdk_path`（缺 typescript = 安装失败或版本错位）。
fn resolve_install() -> anyhow::Result<PathBuf> {
    let dir = install_dir();
    if dir.join("node_modules").is_dir()
        && svelte_entry(&dir).is_file()
        && ts_ls_entry(&dir).is_file()
        && tsdk_path(&dir).is_dir()
    {
        return Ok(dir);
    }
    Err(not_installed_error(
        "svelteserver",
        "run `serena-cli install svelte` (npm: svelte-language-server 0.18.0 \
         + typescript 6.0.3 + typescript-language-server 5.1.3 + typescript-svelte-plugin 0.3.52)",
    ))
}

fn node_on_path() -> anyhow::Result<PathBuf> {
    which_no_unc("node").ok_or_else(|| {
        anyhow::anyhow!(
            "node not on PATH; required to run svelteserver and its companion typescript-language-server"
        )
    })
}

/// 主 LS `initializationOptions.configuration` 节表（↖ mirror `_create_base_initialize_params`
/// 的 `lsp_config`：svelte/prettier/emmet/css/less/scss/html 空对象 + 三个 tsdk 节）。
fn lsp_configuration(install: &Path) -> Value {
    let tsdk = tsdk_path(install);
    json!({
        "svelte": {},
        "prettier": {},
        "emmet": {},
        "javascript": { "tsdk": tsdk },
        "typescript": { "tsdk": tsdk },
        "js/ts": { "tsdk": tsdk },
        "css": {},
        "less": {},
        "scss": {},
        "html": {},
    })
}

/// 主 LS 初始化补丁（↖ mirror `_create_base_initialize_params` 的 initializationOptions）。
fn patch_main_options(base: &mut InitializeParams, install: &Path) {
    let opts = base
        .initialization_options
        .get_or_insert_with(Value::default);
    if !opts.is_object() {
        *opts = json!({});
    }
    opts["isTrusted"] = json!(true);
    opts["dontFilterIncompleteCompletions"] = json!(true);
    opts["configuration"] = lsp_configuration(install);
}

/// 伴生 TS LS 初始化参数（↖ mirror `SvelteTypeScriptServer._create_base_initialize_params`）。
fn companion_init_params(root: &Path, install: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        // rootUri 即使 deprecated 也设：tsserver 靠它定位 workspace 的项目（astro 伴生同理）。
        "rootUri": path_to_uri_str(root),
        // InitializeParams 必填；svelte 伴生无 tsserver 桥（Δ vue），空对象即可。
        "capabilities": {},
        "initializationOptions": {
            "plugins": [{
                "name": "typescript-svelte-plugin",
                "location": plugin_path(install),
                "languages": ["svelte"],
            }],
            "tsserver": { "path": tsdk_path(install) },
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// server→client `workspace/configuration` 应答（伴生侧）：每个 item 回一个空配置
/// 对象（↖ mirror `SvelteTypeScriptServer.workspace_configuration_handler`；
/// LS 拿到 {} 即回落 init options）。
fn companion_configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    Some(Value::Array(vec![json!({}); items]))
}

/// server→client `workspace/configuration` 应答（主 LS 侧）：逐 item 按 section 查
/// [`lsp_configuration`] 节表，未命中回空对象（↖ mirror `configuration_handler` 的
/// `_lsp_configuration.get(section, {})`——tsdk 节是 svelteserver 走 tsserver 的前提）。
fn main_configuration_reply(msg: JsonRpc, install: &Path) -> Option<Value> {
    let config = lsp_configuration(install);
    let items = msg
        .params
        .as_ref()?
        .get("items")?
        .as_array()?
        .iter()
        .map(|item| {
            let section = item.get("section").and_then(Value::as_str).unwrap_or("");
            config.get(section).cloned().unwrap_or_else(|| json!({}))
        })
        .collect::<Vec<_>>();
    Some(Value::Array(items))
}

/// 递归收集 root 下 `.svelte`（限深 4；跳隐藏目录与依赖/构建目录 —— 目录名单对齐
/// 上游 `is_ignored_dirname` 增补的 dist/build/coverage + `_find_all_svelte_files`
/// 的 node_modules/点目录排除）。返回路径升序。
fn find_svelte_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        if depth > 4 || out.len() >= MAX_SVELTE_FILES {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if p.is_dir() {
                let skip = name.starts_with('.')
                    || matches!(name, "node_modules" | "dist" | "build" | "coverage");
                if !skip {
                    walk(&p, depth + 1, out);
                }
            } else if name.ends_with(".svelte") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort();
    out
}

#[async_trait]
impl LanguageServerAdapter for SvelteAdapter {
    fn id(&self) -> &'static str {
        "svelte-ls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Svelte];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let install = resolve_install()?;
        let node = node_on_path()?;
        Ok(LaunchInfo {
            cmd: vec![
                node.into_os_string(),
                svelte_entry(&install).into_os_string(),
                "--stdio".into(),
            ],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        patch_main_options(base, &install_dir());
    }

    fn set_project_root(&self, root: &Path) {
        PROBE_ROOT.set(root);
    }

    fn semantic_session(&self, root: &Path) -> Option<Arc<lsp_core::session::Session>> {
        let guard = COMPANION.lock().expect("COMPANION poisoned");
        guard
            .as_ref()
            .filter(|(r, _)| r == root)
            .map(|(_, s)| Arc::clone(s))
    }

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        let install = resolve_install()?;
        let node = node_on_path()?;
        let root = PROBE_ROOT.get_last().unwrap_or_else(|| PathBuf::from("."));

        // 1. 伴生 TS LS：spawn（保留 child 供监视）→ 独立 Session 握手。
        let launch = LaunchInfo {
            cmd: vec![
                node.into_os_string(),
                ts_ls_entry(&install).into_os_string(),
                "--stdio".into(),
            ],
            cwd: root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        };
        let mut handle = Child::spawn(launch).map_err(|e| {
            anyhow::anyhow!("companion typescript-language-server spawn failed: {e}")
        })?;
        let mut companion_child = handle.take_child();
        let companion = lsp_core::session::Session::start(
            Some(handle),
            companion_init_params(&root, &install)?,
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!("companion typescript-language-server handshake failed: {e}")
        })?;
        // didOpen languageId 按扩展名分派（↖ mirror `SvelteTypeScriptServer
        // ._get_language_id_for_file`）：.svelte 以 "svelte" 打开激活 plugin（错值 =
        // plugin 不生效，跨文件语义静默缺失）；JS 系以 "javascript"；ts/tsx 系用基础
        // 值 "typescript"（上游无 typescriptreact 分支，Δ astro）。
        companion.set_language_id("typescript");
        companion.set_language_id_for_extensions(&[
            ("svelte", "svelte"),
            ("js", "javascript"),
            ("jsx", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ]);
        // 主会话同样按扩展名分派（↖ mirror 主 LS `_get_language_id_for_file`：
        // TS_EXT→"typescript"、JS_EXT→"javascript"，基础值 "svelte" 由 supervisor 注入
        // —— 主 LS 会打开用户查询的 ts/js 文档，宿主语言 id 打开会被插件当模板破解析）。
        session.set_language_id_for_extensions(&[
            ("ts", "typescript"),
            ("tsx", "typescript"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascript"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ]);

        // 2. 双向 configuration 应答 + 主 LS 的 applyEdit 拒绝（↖ mirror
        //    `workspace_apply_edit_handler` 回 {applied:false}）。
        companion
            .client()
            .on_server_request("workspace/configuration", companion_configuration_reply);
        session
            .client()
            .on_server_request("workspace/configuration", move |msg| {
                main_configuration_reply(msg, &install)
            });
        session
            .client()
            .on_server_request("workspace/applyEdit", |_| Some(json!({ "applied": false })));

        // 3. 登记伴生为语义会话（supervisor 语义类请求路由到这里）。覆盖旧条目：
        //    旧 Arc 归零 → 旧伴生 Job 关句柄灭树，重启场景自动清场。
        if let Ok(mut slot) = COMPANION.lock() {
            *slot = Some((root.clone(), Arc::clone(&companion)));
        }

        // 4. `.svelte` 预打开在伴生上（plugin getExternalFiles 依赖，↖ mirror
        //    `_ensure_svelte_files_indexed_on_ts_server`；单个失败不阻断）。
        for f in find_svelte_files(&root) {
            let _ = companion.ensure_open(&f).await;
        }

        // 5. 监视任务（astro.rs 同构）：伴生退出 → 主 shutdown；主 drop → 伴生 Arc
        //    归零 → Job 灭树。
        let main_weak = Arc::downgrade(session);
        if let Some(mut child) = companion_child.take() {
            tokio::spawn(async move {
                loop {
                    if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                        if let Some(m) = main_weak.upgrade() {
                            m.shutdown().await;
                        }
                        clear_companion();
                        return;
                    }
                    if main_weak.upgrade().is_none() {
                        clear_companion();
                        return;
                    }
                    tokio::time::sleep(WATCH_POLL).await;
                }
            });
        }

        // 6. 主 LS 就绪探针：真实 .svelte 优先（触发项目 lazy-load），失败只 warn 放行。
        let probe_uri = probe_uri_for_svelte(&root);
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
        // 上游静态先验：SvelteLanguageServer 未 override supports_implementation_request
        // （基类默认 false；implementation 能力走伴生会话路径）。
        false
    }

    /// 跨文件引用查询前的索引等待（伴生 TS 会话与 typescript.rs 是同一 lsp-core
    /// `Session` `$/progress` 机制 —— astro.rs 同源复制）。
    async fn wait_for_cross_file_index(&self, session: &lsp_core::session::Session) {
        let completed = if session.take_cross_file_first_query() {
            session
                .wait_indexing_start_or_completion(INDEXING_PROGRESS_TIMEOUT, INDEXING_START_GRACE)
                .await
        } else if session.index_active_progress() > 0 {
            session.wait_indexing_drain(INDEXING_PROGRESS_TIMEOUT).await
        } else {
            return;
        };
        if completed {
            tracing::debug!("svelte companion cross-file indexing complete");
        } else {
            tracing::warn!(
                "svelte companion cross-file indexing did not complete within {}s; proceeding (active tokens: {})",
                INDEXING_PROGRESS_TIMEOUT.as_secs(),
                session.index_active_progress()
            );
        }
    }
}

/// root 下选就绪探针 URI：优先真实 `.svelte`（触发项目 lazy-load），退虚拟 URI。
/// probe_extensions(Svelte) 走 [`crate::probe_uri_for_root`] 同源逻辑的本地形态。
fn probe_uri_for_svelte(root: &Path) -> String {
    crate::probe_uri_for_root(root, &[LanguageId::Svelte], PROBE_FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_writes_tsdk_and_svelte_flags() {
        let mut p = InitializeParams::default();
        patch_main_options(&mut p, Path::new("Z:/install"));
        let opts = p.initialization_options.expect("options written");
        assert_eq!(opts["isTrusted"], json!(true));
        assert_eq!(opts["dontFilterIncompleteCompletions"], json!(true));
        let cfg = &opts["configuration"];
        assert_eq!(
            cfg["typescript"]["tsdk"],
            json!(tsdk_path(Path::new("Z:/install")))
        );
        assert_eq!(cfg["javascript"]["tsdk"], cfg["typescript"]["tsdk"]);
        assert_eq!(cfg["js/ts"]["tsdk"], cfg["typescript"]["tsdk"]);
        assert_eq!(cfg["svelte"], json!({}));
        assert_eq!(cfg["scss"], json!({}));
    }

    #[test]
    fn companion_params_inject_svelte_plugin() {
        let install = Path::new("Z:/install");
        let p = companion_init_params(Path::new("Z:/proj"), install).expect("params");
        let raw = serde_json::to_value(&p).expect("serialize");
        let plugin = &raw["initializationOptions"]["plugins"][0];
        assert_eq!(plugin["name"], json!("typescript-svelte-plugin"));
        assert_eq!(plugin["location"], json!(plugin_path(install)));
        assert_eq!(plugin["languages"], json!(["svelte"]));
        assert_eq!(
            raw["initializationOptions"]["tsserver"]["path"],
            json!(tsdk_path(install))
        );
        assert!(
            raw["rootUri"]
                .as_str()
                .expect("rootUri string")
                .starts_with("file://")
        );
    }

    #[test]
    fn main_configuration_reply_maps_sections() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [
                { "section": "svelte" },
                { "section": "typescript" },
                { "section": "javascript" },
                { "section": "unknown.section" },
            ] }),
        );
        let install = Path::new("Z:/install");
        let tsdk = json!(tsdk_path(install));
        assert_eq!(
            main_configuration_reply(msg, install),
            Some(json!([{}, { "tsdk": tsdk }, { "tsdk": tsdk }, {}]))
        );
        // 缺 items → None（默认 null 成功应答路径在 client 层）。
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(main_configuration_reply(msg, install), None);
    }

    #[test]
    fn companion_configuration_reply_mirrors_items() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [{ "section": "typescript" }, {}] }),
        );
        assert_eq!(companion_configuration_reply(msg), Some(json!([{}, {}])));
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(companion_configuration_reply(msg), None);
    }

    #[test]
    fn svelte_scan_skips_deps_build_and_sorts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("app.svelte"), "").expect("app");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("nm");
        std::fs::write(root.join("node_modules/pkg/x.svelte"), "").expect("dep svelte");
        std::fs::create_dir_all(root.join("dist")).expect("dist");
        std::fs::write(root.join("dist/z.svelte"), "").expect("dist svelte");
        std::fs::create_dir_all(root.join("src/routes")).expect("src");
        std::fs::write(root.join("src/routes/about.svelte"), "").expect("src svelte");
        assert_eq!(
            find_svelte_files(root),
            vec![
                root.join("app.svelte"),
                root.join("src/routes/about.svelte")
            ]
        );
    }
}
