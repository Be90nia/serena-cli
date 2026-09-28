//! `@astrojs/language-server` 适配器（Wave 2，hybrid 双服务器编排）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/astro_language_server.py`
//!
//! ## 双服务器编排（vue.rs 同构，上游 AstroLanguageServer + AstroTypeScriptServer）
//!
//! 主 LS（astro-ls）承载 .astro 结构与模板语义；伴生 TS LS（typescript-language-server
//! 挂 `@astrojs/ts-plugin`）承载 ts/js/tsx/jsx 语义与跨文件（.ts↔.astro 经 plugin）：
//!
//! 1. `on_session_ready` 起伴生 TS LS（init options 注入 astro 插件 + tsserver 路径，
//!    ↖ mirror `AstroTypeScriptServer._create_base_initialize_params`）。
//! 2. 无 tsserver 桥 —— 上游 astro 主 LS 不走 `tsserver/request` 转发机制（vue 特有），
//!    主 LS 语义自带（Δ vue.rs：不抄 `bridge_tsserver_requests`）。
//! 3. 生命周期绑定（两向不留孤儿）：伴生进程退出 → 主 session `shutdown`（监视任务
//!    `try_wait` 轮询）；主 session drop → 监视任务退出 —— 它是伴生 `Arc<Session>`
//!    的唯一强引用持有者，drop 即伴生 Job（KILL_ON_JOB_CLOSE）灭树。
//! 4. 会话建立时把 root 下 `.astro`（限深 4，跳依赖/构建目录）在伴生上 `ensure_open`
//!    （↖ mirror `_ensure_astro_files_indexed_on_ts_server`：plugin 要见到 .astro
//!    消费者才能算对 .ts 的引用）。
//! 5. references 跨文件等待：伴生是同一 lsp-core `Session` 机制（`$/progress`
//!    跟踪），typescript.rs 的 override 逻辑原样适用 —— astro 自带同款 override
//!    （契约二选一之「astro 也 override」，不动 typescript.rs）。
//!
//! 初始化形态（↖ mirror 两份 `_create_base_initialize_params`）：
//! - 主 LS：`initializationOptions = { typescript: { tsdk: <install>/node_modules/typescript/lib } }`；
//! - 伴生：`plugins = [@astrojs/ts-plugin @ <install>/node_modules/@astrojs/ts-plugin,
//!   languages: ["astro"]]` + `tsserver.path = <tsdk>`。
//!
//! 启动一律 `node <入口 js>` 直跑、不走 npm `.cmd` shim —— shim 会 cd 到自身目录，
//! 丢项目 cwd 与同目录 node_modules 解析（typescript.rs/vue.rs 同因同修）。
//!
//! 主 LS 侧 `workspace/configuration` 保留上游 `.customData` 特判（section 以
//! `.customData` 结尾回空数组，其余空对象）。

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
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 主 LS 就绪探针超时（documentSymbol 对真实 .astro）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 伴生进程退出监视轮询间隔。
const WATCH_POLL: Duration = Duration::from_millis(500);

/// npm 缓存目录 id + 主包版本 pin（= servers.toml [servers.astro] npm 段，禁随意改）。
/// 幂等语义照抄上游 `.installed_version` 四元组：目录名 pin 主包版本 + servers.toml
/// 钉死全部四版本（astro-ls 2.17.0 / ts-plugin 1.10.10 / typescript 5.9.3 / tsls 5.1.3），
/// 缓存目录存在 = 该版本组合已装，任何升版都换目录名重装。
const CACHE_ID: &str = "astro";
const CACHE_VERSION: &str = "2.17.0";

/// root 未设置时的退路：虚拟探针 URI（仅保底，真实 .astro 才触发项目 lazy-load）。
const PROBE_FALLBACK: &str = "file:///__astro_ls_ready_probe__";

/// 首查等待 tsserver *开始*发 `$/progress` 的宽限窗（typescript.rs 同源常量）。
/// ↖ mirror: typescript_language_server.py@43ae021 `INDEXING_START_GRACE`（5.0s）
const INDEXING_START_GRACE: Duration = Duration::from_secs(5);

/// `$/progress` 索引等待兜底超时（typescript.rs 同源常量；上游 astro 伴生为 120s，
/// 我们对齐 typescript.rs 会话路径的 30s —— supervisor 工具门同量级）。
const INDEXING_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

/// 单会话伴生预打开的 `.astro` 文件上限。
///
/// ponytail: 上游无上限全扫；200 之后放弃（跳过文件仅影响 references 召回，不崩），
/// 懒式按需打开等真撞上大仓库再说。
const MAX_ASTRO_FILES: usize = 200;

/// 当前会话项目 root。adapter 是零字段单例存不了实例状态 —— 会话级数据放静态槽，
/// 由 supervisor::session_for 在 `on_session_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 当前伴生 TS LS 会话（hybrid 语义通道，见 trait `semantic_session`）。key = root。
/// 持有一份强引用；清理时机：`on_session_ready` 覆盖 / 监视任务两分支（伴生死 →
/// 主 shutdown 时清；主 session drop 任务退出时清 —— 否则该引用单独保活伴生树）。
static COMPANION: Mutex<Option<(PathBuf, Arc<lsp_core::session::Session>)>> = Mutex::new(None);

fn clear_companion() {
    if let Ok(mut slot) = COMPANION.lock() {
        *slot = None;
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AstroAdapter;

/// serena npm 缓存安装目录（`{cache}/astro/2.17.0`）——四包同装一处，tsdk / 插件 /
/// 两个入口 js 都从这里拼（↖ mirror 上游 `astro-lsp-{version}` 单目录形态）。
fn install_dir() -> PathBuf {
    default_cache_root().join(CACHE_ID).join(CACHE_VERSION)
}

/// typescript `lib/`（tsdk）：tsserver 程序与内置 lib 的根。
fn tsdk_path(install: &Path) -> PathBuf {
    install.join("node_modules/typescript/lib")
}

/// `@astrojs/ts-plugin` 包目录（伴生 init options `plugins[].location`）。
fn plugin_path(install: &Path) -> PathBuf {
    install.join("node_modules/@astrojs/ts-plugin")
}

/// astro-ls 入口 js（绕 .cmd shim 直跑）。
fn astro_entry(install: &Path) -> PathBuf {
    install.join("node_modules/@astrojs/language-server/bin/nodeServer.js")
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
        && astro_entry(&dir).is_file()
        && ts_ls_entry(&dir).is_file()
        && tsdk_path(&dir).is_dir()
    {
        return Ok(dir);
    }
    Err(not_installed_error(
        "astro-ls",
        "run `serena-cli install astro` (npm: @astrojs/language-server 2.17.0 \
         + @astrojs/ts-plugin 1.10.10 + typescript 5.9.3 + typescript-language-server 5.1.3)",
    ))
}

fn node_on_path() -> anyhow::Result<PathBuf> {
    which_no_unc("node").ok_or_else(|| {
        anyhow::anyhow!("node not on PATH; required to run astro-ls and its companion typescript-language-server")
    })
}

/// 主 LS 初始化补丁：tsdk（↖ mirror `_create_base_initialize_params`；
/// Δ vue：astro 无 hybridMode 选项）。
fn patch_main_options(base: &mut InitializeParams, install: &Path) {
    let opts = base
        .initialization_options
        .get_or_insert_with(Value::default);
    if !opts.is_object() {
        *opts = json!({});
    }
    opts["typescript"] = json!({ "tsdk": tsdk_path(install) });
}

/// 伴生 TS LS 初始化参数（↖ mirror `AstroTypeScriptServer._create_base_initialize_params`）。
fn companion_init_params(root: &Path, install: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        // rootUri 即使 deprecated 也设：tsserver 靠它定位 workspace 的项目（vue.rs
        // 伴生同理由，typescript.rs 会话路径共享该形态）。
        "rootUri": path_to_uri_str(root),
        // InitializeParams 必填；astro 伴生不需要 executeCommand 动态注册（无
        // tsserver 桥，Δ vue），空对象即可。
        "capabilities": {},
        "initializationOptions": {
            "plugins": [{
                "name": "@astrojs/ts-plugin",
                "location": plugin_path(install),
                "languages": ["astro"],
            }],
            "tsserver": { "path": tsdk_path(install) },
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// server→client `workspace/configuration` 应答（伴生侧）：每个 item 回一个空配置
/// 对象（↖ mirror 上游 `AstroTypeScriptServer.workspace_configuration_handler`；
/// LS 拿到 {} 即回落 init options）。
fn companion_configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    Some(Value::Array(vec![json!({}); items]))
}

/// server→client `workspace/configuration` 应答（主 LS 侧）：section 以 `.customData`
/// 结尾回空数组（astro LS 据此拉 HTML/组件数据，空 = 用内置数据），其余回空对象
/// （↖ mirror 上游 `AstroLanguageServer.configuration_handler` 特判语义）。
fn main_configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg
        .params
        .as_ref()?
        .get("items")?
        .as_array()?
        .iter()
        .map(|item| {
            let section = item.get("section").and_then(Value::as_str).unwrap_or("");
            if section.ends_with(".customData") {
                json!([])
            } else {
                json!({})
            }
        })
        .collect::<Vec<_>>();
    Some(Value::Array(items))
}

/// 递归收集 root 下 `.astro`（限深 4；跳隐藏目录与依赖/构建目录 —— 目录名单对齐
/// 上游 `is_ignored_dirname` 增补的 dist/build/coverage/.astro）。返回路径升序。
fn find_astro_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        if depth > 4 || out.len() >= MAX_ASTRO_FILES {
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
                    || matches!(
                        name,
                        "node_modules" | "dist" | "build" | "coverage"
                    );
                if !skip {
                    walk(&p, depth + 1, out);
                }
            } else if name.ends_with(".astro") {
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
impl LanguageServerAdapter for AstroAdapter {
    fn id(&self) -> &'static str {
        "astro-ls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Astro];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let install = resolve_install()?;
        let node = node_on_path()?;
        Ok(LaunchInfo {
            cmd: vec![
                node.into_os_string(),
                astro_entry(&install).into_os_string(),
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
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
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
        let root = PROBE_ROOT
            .lock()
            .expect("PROBE_ROOT poisoned")
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));

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
        // didOpen languageId 按扩展名分派（↖ mirror `_get_language_id_for_file`）：
        // .astro 以 "astro" 激活 plugin；.ts/.tsx/.js/.jsx 以标准语言打开（languageId
        // 错了 plugin 会把 .ts 当 astro 模板破解析 —— 真机帧录制实证）。
        companion.set_language_id(LanguageId::Astro.as_str());
        companion.set_language_id_for_extensions(&[
            ("astro", "astro"),
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ]);

        // 2. 双向 configuration 应答 + 主 LS 的 applyEdit 拒绝（rename 时 astro LS
        //    发 workspace/applyEdit，回 {applied:false} ↖ mirror 上游 handler）。
        companion
            .client()
            .on_server_request("workspace/configuration", companion_configuration_reply);
        session
            .client()
            .on_server_request("workspace/configuration", main_configuration_reply);
        session
            .client()
            .on_server_request("workspace/applyEdit", |_| {
                Some(json!({ "applied": false }))
            });

        // 3. 登记伴生为语义会话（supervisor 语义类请求路由到这里）。覆盖旧条目：
        // 旧 Arc 归零 → 旧伴生 Job 关句柄灭树，重启场景自动清场。
        if let Ok(mut slot) = COMPANION.lock() {
            *slot = Some((root.clone(), Arc::clone(&companion)));
        }

        // 4. `.astro` 预打开在伴生上（跨文件引用索引，↖ mirror
        //    `_ensure_astro_files_indexed_on_ts_server`；单个失败不阻断）。
        for f in find_astro_files(&root) {
            let _ = companion.ensure_open(&f).await;
        }

        // 5. 监视任务：活跃强引用只剩 COMPANION 槽与本任务（companion 局部变量
        //    函数尾 drop；登记/桥接其余持有者皆 Weak）。两分支都经 clear_companion
        //    清槽归零：
        //    - 伴生退出（try_wait Some）→ 主 session shutdown（伴生死 → 主退出）；
        //    - 主 session drop（Weak upgrade 失败）→ 任务退出 → Arc 归零 → 伴生
        //      Job 关句柄 → KILL_ON_JOB_CLOSE 灭树（主死 → 伴生死）。
        let main_weak = Arc::downgrade(session);
        if let Some(mut child) = companion_child.take() {
            tokio::spawn(async move {
                loop {
                    // try_wait Err（句柄失效等 IO 异常）也按退出处理。
                    if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                        if let Some(m) = main_weak.upgrade() {
                            m.shutdown().await;
                        }
                        clear_companion();
                        return;
                    }
                    if main_weak.upgrade().is_none() {
                        clear_companion(); // drop 伴生 Arc → Job 灭树
                        return;
                    }
                    tokio::time::sleep(WATCH_POLL).await;
                }
            });
        }

        // 6. 主 LS 就绪探针：真实 .astro 优先（触发项目 lazy-load），失败只 warn 放行。
        let probe_uri = crate::probe_uri_for_root(&root, &[LanguageId::Astro], PROBE_FALLBACK);
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
        // 上游静态先验：AstroLanguageServer 类未 override supports_implementation_request
        // （SolidLanguageServer 基类默认 false；implementation 能力走伴生会话路径）。
        false
    }

    /// 跨文件引用查询前的索引等待（伴生 TS 会话与 typescript.rs 是同一 lsp-core
    /// `Session` `$/progress` 机制 —— override 逻辑同源复制）。
    ///
    /// ↖ mirror: typescript_language_server.py@43ae021
    ///          `_wait_for_cross_file_references_if_needed`
    ///          ↖ mirror: @cf54869a 修订 —— 首查 latch 后，后续查询若仍有在飞
    ///          `$/progress` token 则继续 drain。超时 warn 后放行（上游 permissive）。
    async fn wait_for_cross_file_index(&self, session: &lsp_core::session::Session) {
        let completed = if session.take_cross_file_first_query() {
            // 首查：等索引开始并 drain，或 grace 内证明无需索引。
            session
                .wait_indexing_start_or_completion(
                    INDEXING_PROGRESS_TIMEOUT,
                    INDEXING_START_GRACE,
                )
                .await
        } else if session.index_active_progress() > 0 {
            // 后续查询：有在飞 token 就 drain。
            session.wait_indexing_drain(INDEXING_PROGRESS_TIMEOUT).await
        } else {
            return;
        };
        if completed {
            tracing::debug!("astro companion cross-file indexing complete");
        } else {
            tracing::warn!(
                "astro companion cross-file indexing did not complete within {}s; proceeding (active tokens: {})",
                INDEXING_PROGRESS_TIMEOUT.as_secs(),
                session.index_active_progress()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_writes_tsdk() {
        let mut p = InitializeParams::default();
        patch_main_options(&mut p, Path::new("Z:/install"));
        let opts = p.initialization_options.expect("options written");
        assert_eq!(
            opts["typescript"]["tsdk"],
            json!(tsdk_path(Path::new("Z:/install")))
        );
        // Δ vue：astro 主 LS 无 hybridMode 选项。
        assert!(opts.get("vue").is_none());
    }

    #[test]
    fn companion_params_inject_plugin_and_tsserver() {
        let install = Path::new("Z:/install");
        let p = companion_init_params(Path::new("Z:/proj"), install).expect("params");
        let raw = serde_json::to_value(&p).expect("serialize");
        let plugin = &raw["initializationOptions"]["plugins"][0];
        assert_eq!(plugin["name"], json!("@astrojs/ts-plugin"));
        assert_eq!(plugin["location"], json!(plugin_path(install)));
        assert_eq!(plugin["languages"][0], json!("astro"));
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
    fn astro_scan_skips_deps_build_and_sorts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("index.astro"), "").expect("index");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("nm");
        std::fs::write(root.join("node_modules/pkg/x.astro"), "").expect("dep astro");
        std::fs::create_dir_all(root.join(".astro")).expect("generated dir");
        std::fs::write(root.join(".astro/y.astro"), "").expect("generated astro");
        std::fs::create_dir_all(root.join("dist")).expect("dist");
        std::fs::write(root.join("dist/z.astro"), "").expect("dist astro");
        std::fs::create_dir_all(root.join("src/pages")).expect("src");
        std::fs::write(root.join("src/pages/about.astro"), "").expect("src astro");
        assert_eq!(
            find_astro_files(root),
            vec![root.join("index.astro"), root.join("src/pages/about.astro")]
        );
    }

    #[test]
    fn main_configuration_reply_handles_custom_data() {
        // `.customData` section → 空数组（上游主 LS 特判）；其余 → 空对象。
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [
                { "section": "css" },
                { "section": "html.customData" },
                { "section": "astro" },
            ] }),
        );
        assert_eq!(
            main_configuration_reply(msg),
            Some(json!([{}, [], {}]))
        );
        // 缺 items → None（默认 null 成功应答路径在 client 层）。
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(main_configuration_reply(msg), None);
    }

    #[test]
    fn companion_configuration_reply_mirrors_items() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [{ "section": "typescript" }, {}] }),
        );
        assert_eq!(
            companion_configuration_reply(msg),
            Some(json!([{}, {}]))
        );
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(companion_configuration_reply(msg), None);
    }
}
