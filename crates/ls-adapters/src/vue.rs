//! `@vue/language-server`（Volar）适配器（Wave 2，hybrid 编排）。
//!
//! ↖ mirror: oraios/serena@43ae021 `solidlsp/language_servers/vue_language_server.py`
//!
//! ## hybrid 编排（最简可行版）
//!
//! Vue LS 自身只解析 SFC 骨架，类型语义全部经 tsserver 桥完成——主 LS（vue）+
//! 伴生 TS LS（typescript-language-server 挂 `@vue/typescript-plugin`）两进程：
//!
//! 1. `on_server_ready` 起伴生 TS LS（init options 注入 vue 插件 + tsserver 路径，
//!    ↖ mirror 上游 `VueTypeScriptServer._create_base_initialize_params`）。
//! 2. 桥接主 LS 的 `tsserver/request` 通知 → 伴生 `workspace/executeCommand`
//!    (`typescript.tsserverRequest`) → `tsserver/response` 回主 LS（↖ mirror
//!    `tsserver_request_notification_handler`；`_vue:projectInfo` 本地找 tsconfig
//!    应答、不进转发，↖ mirror `_find_tsconfig_for_file`）。
//! 3. 生命周期绑定（两向不留孤儿）：伴生进程退出 → 主 session `shutdown`（监视任务
//!    `try_wait` 轮询）；主 session drop → 监视任务退出——它是伴生 `Arc<Session>`
//!    的唯一强引用持有者，drop 即伴生 Job（KILL_ON_JOB_CLOSE）灭树。
//! 4. 会话建立时把 root 下 `.vue`（限深 4，跳依赖/构建目录）在伴生上 `ensure_open`
//!    （↖ mirror `_ensure_vue_files_indexed_on_ts_server`，跨文件引用依赖）。
//!
//! 初始化形态（↖ mirror `_create_base_initialize_params` 两份）：
//! - 主 LS：`vue.hybridMode = true` + `typescript.tsdk = <install>/node_modules/typescript/lib`；
//! - 伴生：`plugins = [@vue/typescript-plugin @ <install>/node_modules/@vue/typescript-plugin]`
//!   + `tsserver.path = <tsdk>` + `workspace.executeCommand.dynamicRegistration`。
//!
//! 启动一律 `node <入口 js>` 直跑、不走 npm `.cmd` shim —— shim 会 cd 到自身目录，
//! 丢项目 cwd 与同目录 node_modules 解析（typescript.rs 同因同修）。
//!
//! ponytail: `tsserver/response` 转发失败静默回 null（上游 log.error 后同样回 None）——
//! 主 LS 自带降级；未抄客户端本地特判（仅 `_vue:projectInfo`）与低优先级队列。

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

/// 主 LS 就绪探针超时（documentSymbol 对真实 .vue）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// tsserver 桥转发单请求超时（重查询在 tsserver 侧秒级；30s 与 supervisor 工具门同量级）。
const TS_FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

/// 伴生进程退出监视轮询间隔。
const WATCH_POLL: Duration = Duration::from_millis(500);

/// npm 缓存目录 id + 版本 pin（= servers.toml [servers.vue] npm 段，禁随意改）。
const CACHE_ID: &str = "vue";
const CACHE_VERSION: &str = "3.1.5";

/// root 未设置时的退路：虚拟探针 URI（仅保底，真实 .vue 才触发项目 lazy-load）。
const PROBE_FALLBACK: &str = "file:///__vue_ls_ready_probe__";

/// 单会话伴生预打开的 `.vue` 文件上限。
///
/// ponytail: 上游无上限全扫；200 之后放弃（跳过文件仅影响 references 召回，不崩），
/// 懒式按需打开等真撞上大仓库再说。
const MAX_VUE_FILES: usize = 200;

/// 当前会话项目 root。adapter 是零字段单例存不了实例状态 —— 会话级数据放静态槽，
/// 由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
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
pub struct VueAdapter;

/// serena npm 缓存安装目录（`{cache}/vue/3.1.5`）——三包同装一处，tsdk / 插件 /
/// 两个入口 js 都从这里拼（↖ mirror 上游 `vue-lsp` 单目录形态）。
fn install_dir() -> PathBuf {
    default_cache_root().join(CACHE_ID).join(CACHE_VERSION)
}

/// typescript `lib/`（tsdk）：tsserver 程序与内置 lib 的根。
fn tsdk_path(install: &Path) -> PathBuf {
    install.join("node_modules/typescript/lib")
}

/// `@vue/typescript-plugin` 包目录（伴生 init options `plugins[].location`）。
fn plugin_path(install: &Path) -> PathBuf {
    install.join("node_modules/@vue/typescript-plugin")
}

/// vue-language-server 入口 js（绕 .cmd shim 直跑）。
fn vue_entry(install: &Path) -> PathBuf {
    install.join("node_modules/@vue/language-server/bin/vue-language-server.js")
}

/// 伴生 typescript-language-server 入口 js（cli.mjs，typescript.rs 同款路径形态）。
fn ts_ls_entry(install: &Path) -> PathBuf {
    install.join("node_modules/typescript-language-server/lib/cli.mjs")
}

/// 安装完整性校验：node_modules + 两个入口 js 齐才可用；否则标准未安装错误。
fn resolve_install() -> anyhow::Result<PathBuf> {
    let dir = install_dir();
    if dir.join("node_modules").is_dir() && vue_entry(&dir).is_file() && ts_ls_entry(&dir).is_file()
    {
        return Ok(dir);
    }
    Err(not_installed_error(
        "vue-language-server",
        "run `serena-cli install vue` (npm: @vue/language-server 3.1.5 + typescript 5.9.3 \
         + typescript-language-server 5.1.3)",
    ))
}

fn node_on_path() -> anyhow::Result<PathBuf> {
    which_no_unc("node").ok_or_else(|| {
        anyhow::anyhow!("node not on PATH; required to run vue-language-server and its companion typescript-language-server")
    })
}

/// 主 LS 初始化补丁：hybridMode + tsdk（↖ mirror `_create_base_initialize_params`）。
fn patch_main_options(base: &mut InitializeParams, install: &Path) {
    let opts = base
        .initialization_options
        .get_or_insert_with(Value::default);
    if !opts.is_object() {
        *opts = json!({});
    }
    opts["vue"] = json!({ "hybridMode": true });
    opts["typescript"] = json!({ "tsdk": tsdk_path(install) });
}

/// 伴生 TS LS 初始化参数（↖ mirror `VueTypeScriptServer._create_base_initialize_params`）。
fn companion_init_params(root: &Path, install: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        // rootUri 即使 deprecated 也设：tsserver 靠它定位 workspace 的项目（supervisor
        // 对 typescript-language-server 同理由，typescript.rs 会话路径共享该形态）。
        "rootUri": path_to_uri_str(root),
        "capabilities": {
            // TLS 会经 client/registerCapability 动态注册 executeCommand；声明支持
            // 让注册走通（未注册 handler 的默认 null 成功应答即可满足）。
            "workspace": { "executeCommand": { "dynamicRegistration": true } },
        },
        "initializationOptions": {
            "plugins": [{
                "name": "@vue/typescript-plugin",
                "location": plugin_path(install),
                "languages": ["vue"],
            }],
            "tsserver": { "path": tsdk_path(install) },
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// server→client `workspace/configuration` 应答：每个 item 回一个空配置对象
/// （↖ mirror 上游 `configuration_handler`；LS 拿到 {} 即回落 init options）。
fn configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    Some(Value::Array(vec![json!({}); items]))
}

/// 从 `file` 所在目录向上找最近的 `tsconfig.json`（root 内），找不到回 root 下
/// tsconfig.json；都没有 → None（↖ mirror `_find_tsconfig_for_file`）。
fn find_tsconfig_for_file(root: &Path, file: Option<&Path>) -> Option<PathBuf> {
    let mut dir = match file {
        None => None,
        Some(f) => f.parent().map(|p| p.to_path_buf()),
    };
    while let Some(d) = dir {
        if !d.starts_with(root) {
            break;
        }
        let p = d.join("tsconfig.json");
        if p.is_file() {
            return Some(p);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    let p = root.join("tsconfig.json");
    p.is_file().then_some(p)
}

/// 递归收集 root 下 `.vue`（限深 [`MAX_VUE_FILES`] 描述的策略；跳隐藏目录与
/// 依赖/构建目录 —— 目录名单对齐上游 `is_ignored_dirname`）。返回路径升序。
fn find_vue_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        if depth > 4 || out.len() >= MAX_VUE_FILES {
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
                        "node_modules" | "dist" | "build" | "coverage" | ".nuxt" | ".output"
                    );
                if !skip {
                    walk(&p, depth + 1, out);
                }
            } else if name.ends_with(".vue") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort();
    out
}

/// `tsserver/request` → 伴生 executeCommand → `tsserver/response` 桥。
///
/// handler 在 stdout 泵线程同步执行（不可 await），转发逻辑整段 `tokio::spawn`。
/// params 形态 = `[[request_id, method, method_params?]]`；回帧同构 `[[id, result]]`。
fn bridge_tsserver_requests(
    session: &std::sync::Arc<lsp_core::session::Session>,
    companion: &Arc<lsp_core::session::Session>,
    root: PathBuf,
) {
    let main = Arc::downgrade(session);
    let comp = Arc::downgrade(companion);
    session
        .client()
        .on_notification("tsserver/request", move |msg| {
            let Some(params) = msg.params else { return };
            let Some(item) = params.get(0).and_then(Value::as_array) else {
                return;
            };
            let Some(id) = item.first().cloned() else {
                return;
            };
            let Some(method) = item.get(1).and_then(Value::as_str).map(str::to_string) else {
                return;
            };
            let mp = item.get(2).cloned().unwrap_or(Value::Null);

            if method == "_vue:projectInfo" {
                // ↖ mirror `_vue:projectInfo` 特判：本地找 tsconfig 直接应答（伴生没有
                // tsserver 项目概念，Volar 靠它确定 configFileName）。应答必须是
                // `{configFileName}` 包裹或 null（裸字符串会让 Volar 项目初始化挂起）。
                let Some(m) = main.upgrade() else { return };
                let file = mp.get("file").and_then(Value::as_str).map(PathBuf::from);
                let cfg = find_tsconfig_for_file(&root, file.as_deref())
                    .map(|p| json!({ "configFileName": p.to_string_lossy() }));
                tokio::spawn(async move {
                    let _ = m.notify("tsserver/response", json!([[id, cfg]])).await;
                });
                return;
            }

            let (Some(m), Some(c)) = (main.upgrade(), comp.upgrade()) else {
                return; // 任一侧已亡：丢弃（请求方随进程退场）
            };
            tokio::spawn(async move {
                let result = c
                    .request::<Value>(
                        "workspace/executeCommand",
                        json!({
                            "command": "typescript.tsserverRequest",
                            "arguments": [method, mp, { "isAsync": true, "lowPriority": true }],
                        }),
                        TS_FORWARD_TIMEOUT,
                    )
                    .await;
                // 转发失败 → null（↖ mirror 上游 log.error + return None）；成功取
                // `body`（tsserver 响应信封，↖ mirror `if "body" in result`）。
                let body = match result {
                    Ok(v) if v.get("body").is_some() => v["body"].clone(),
                    Ok(v) => v,
                    Err(_) => Value::Null,
                };
                let _ = m.notify("tsserver/response", json!([[id, body]])).await;
            });
        });
}

#[async_trait]
impl LanguageServerAdapter for VueAdapter {
    fn id(&self) -> &'static str {
        "vue-language-server"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Vue];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let install = resolve_install()?;
        let node = node_on_path()?;
        Ok(LaunchInfo {
            cmd: vec![
                node.into_os_string(),
                vue_entry(&install).into_os_string(),
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
        // 伴生也要 didOpen .vue 文件（跨文件引用），languageId 必须是 "vue"。
        companion.set_language_id(LanguageId::Vue.as_str());

        // 2. 双向 configuration 应答 + 主 LS 的 tsserver 桥。
        companion
            .client()
            .on_server_request("workspace/configuration", configuration_reply);
        session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);
        bridge_tsserver_requests(session, &companion, root.clone());

        // 3. 登记伴生为语义会话（supervisor 语义类请求路由到这里）。覆盖旧条目：
        // 旧 Arc 归零 → 旧伴生 Job 关句柄灭树，重启场景自动清场。
        if let Ok(mut slot) = COMPANION.lock() {
            *slot = Some((root.clone(), Arc::clone(&companion)));
        }

        // 3. `.vue` 预打开在伴生上（跨文件引用索引，↖ mirror
        //    `_ensure_vue_files_indexed_on_ts_server`；单个失败不阻断）。
        for f in find_vue_files(&root) {
            let _ = companion.ensure_open(&f).await;
        }

        // 4. 监视任务：本任务持伴生 Arc 唯一强引用（bridge/第 3 步已改持 Weak）。
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

        // 5. 主 LS 就绪探针：真实 .vue 优先（触发项目 lazy-load），失败只 warn 放行。
        let probe_uri = crate::probe_uri_for_root(&root, &[LanguageId::Vue], PROBE_FALLBACK);
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
        // hybrid 模式下 Volar 将 implementation 转发 tsserver（3.x 全转发形态）。
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_writes_hybrid_mode_and_tsdk() {
        let mut p = InitializeParams::default();
        patch_main_options(&mut p, Path::new("Z:/install"));
        let opts = p.initialization_options.expect("options written");
        assert_eq!(opts["vue"]["hybridMode"], json!(true));
        assert_eq!(
            opts["typescript"]["tsdk"],
            json!(tsdk_path(Path::new("Z:/install")))
        );
    }

    #[test]
    fn companion_params_inject_plugin_and_tsserver() {
        let install = Path::new("Z:/install");
        let p = companion_init_params(Path::new("Z:/proj"), install).expect("params");
        let raw = serde_json::to_value(&p).expect("serialize");
        let plugin = &raw["initializationOptions"]["plugins"][0];
        assert_eq!(plugin["name"], json!("@vue/typescript-plugin"));
        assert_eq!(plugin["location"], json!(plugin_path(install)));
        assert_eq!(plugin["languages"][0], json!("vue"));
        assert_eq!(
            raw["initializationOptions"]["tsserver"]["path"],
            json!(tsdk_path(install))
        );
        assert_eq!(
            raw["capabilities"]["workspace"]["executeCommand"]["dynamicRegistration"],
            json!(true)
        );
        assert!(
            raw["rootUri"]
                .as_str()
                .expect("rootUri string")
                .starts_with("file://")
        );
    }

    #[test]
    fn tsconfig_lookup_walks_up_within_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).expect("dirs");
        std::fs::write(root.join("tsconfig.json"), "{}").expect("root tsconfig");

        // 无文件路径 → root 下 tsconfig。
        assert_eq!(
            find_tsconfig_for_file(root, None),
            Some(root.join("tsconfig.json"))
        );
        // a/b 无 tsconfig → 上溯命中 root（root 内不越界）。
        assert_eq!(
            find_tsconfig_for_file(root, Some(&root.join("a/b/c.vue"))),
            Some(root.join("tsconfig.json"))
        );
        // root 外路径：不越界上溯，但兜底仍回 root/tsconfig（↖ mirror 上游
        // `_find_tsconfig_for_file` 尾部无条件兜底语义）。
        assert_eq!(
            find_tsconfig_for_file(root, Some(Path::new("Z:/elsewhere/x.vue"))),
            Some(root.join("tsconfig.json"))
        );
        // root 无 tsconfig 且文件在 root 外 → None。
        std::fs::remove_file(root.join("tsconfig.json")).expect("remove");
        assert_eq!(
            find_tsconfig_for_file(root, Some(Path::new("Z:/elsewhere/x.vue"))),
            None
        );
    }

    #[test]
    fn tsconfig_lookup_prefers_nearest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).expect("dirs");
        std::fs::write(root.join("a/b/tsconfig.json"), "{}").expect("nested tsconfig");
        assert_eq!(
            find_tsconfig_for_file(root, Some(&root.join("a/b/c.vue"))),
            Some(root.join("a/b/tsconfig.json"))
        );
    }

    #[test]
    fn vue_scan_skips_deps_hidden_and_sorts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("App.vue"), "").expect("App");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("nm");
        std::fs::write(root.join("node_modules/pkg/x.vue"), "").expect("dep vue");
        std::fs::create_dir_all(root.join(".hidden")).expect("hidden");
        std::fs::write(root.join(".hidden/y.vue"), "").expect("hidden vue");
        std::fs::create_dir_all(root.join("src/deep")).expect("src");
        std::fs::write(root.join("src/deep/z.vue"), "").expect("src vue");
        assert_eq!(
            find_vue_files(root),
            vec![root.join("App.vue"), root.join("src/deep/z.vue")]
        );
    }

    #[test]
    fn configuration_reply_mirrors_items() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [{ "section": "vue" }, { "section": "typescript" }] }),
        );
        assert_eq!(configuration_reply(msg), Some(json!([{}, {}])));
        // 缺 items → None（默认 null 成功应答路径在 client 层）。
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(configuration_reply(msg), None);
    }
}
