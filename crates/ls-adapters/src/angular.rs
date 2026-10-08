//! Angular 三服务器编排适配器（上游对拍采纳 W3b 批，bd 69e batchC）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/angular_language_server.py`
//!
//! ## 编排（tri-server，上游模块 docstring 路由表）
//!
//! - **主会话 = 伴生 typescript-language-server**（挂 `@angular/language-service`
//!   tsserver 插件 + tsdk）：承载 .ts/.tsx/.js/.jsx 的 documentSymbol / definition /
//!   hover / rename / implementation / 诊断（↖ mirror 路由表 `.ts` 行）。Δ 我方把
//!   上游「companion TS LS」直接作为 supervisor 主会话——`.ts` 语义全量对齐上游路由；
//!   `.ts` references 上游恒走 ngserver（模板引用聚合，tsls 欠报）——我方走主 tsls，
//!   **已知边界：跨文件 .ts 引用可能漏模板侧使用**（astro `.ts` references 走伴生
//!   同构先例；换会话型 references 路由挂点不存在，不硬造）。
//! - **ngserver 伴生**（`@angular/language-server`）：.html 模板的 definition /
//!   hover / references（模板表达式 @if/@for/{{ }}/[prop]/(event)）——经
//!   [`LanguageServerAdapter::session_for_file`] 按扩展名×方法重路由。
//! - **vscode-html 伴生**：.html documentSymbol（ngserver 对 documentSymbol 恒
//!   -32601，上游用 vscode-html-ls 出结构 outline）。复用 html 门缓存
//!   （vscode-langservers-extracted 4.10.0，`serena-cli install html` 先装）；
//!   未装 → .html documentSymbol 返 None（上游 non-fatal 同款）。
//! - **硬前提**（上游 docstring）：root 上方有 tsconfig.json + `npm install` 过
//!   （@angular/core 可解析）。不满足 → ngserver 对所有文件报 "not in an Angular
//!   project"、模板特性静默空——我方按「探针空 + warning 放行」既定语义（启动时
//!   向上探测 node_modules/@angular/core，缺失 warn）。probe_extensions 只收 .ts
//!   （探针/wait_for_index 用；.html 探针会路由走 ngserver，无意义）。
//! - **扩展名路由**：.ts/.html 归属既有 TypeScript/Html 门，angular 仅
//!   `--lang angular` 显式路由可达（pgsql/mysql 先例），不进 EXT_TABLE。
//! - **版本四元组**（↖ mirror `DEFAULT_*` 常量）：@angular/language-server 与
//!   language-service 21.2.10、typescript 5.9.3、typescript-language-server 5.1.3。
//!   Δ 上游安装目录名编码四版本（任一 bump 换目录）；我方 npm 缓存目录键 = 主包
//!   版本——次包 pin 修改时需同步清理缓存（全 pin 写死，实际不漂）。
//! - **就绪**：主 tsls 走 documentSymbol 探针（astro 同款）；ngserver/html 伴生
//!   握手完成即视为就绪。Δ 上游另等 `angular/projectLoadingFinish`（10s 超时放行）
//!   ——项目加载后台化，首查由工具超时兜底（permissive 同源）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::install_pkg::npm_bin_path;
use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::framing::JsonRpc;
use lsp_types::InitializeParams;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, ProjectRootSlot, RequestHooks,
    not_installed_error,
};

/// 四包 pin（↖ mirror `DEFAULT_ANGULAR_LANGUAGE_SERVER_VERSION` 等，禁随意改）：
/// @angular/language-server 21.2.10 + @angular/language-service 21.2.10 +
/// typescript 5.9.3 + typescript-language-server 5.1.3（后三个 pin 在
/// servers.toml [servers.angular].npm.secondary_packages，pins 单测锁字面量）。
const ANGULAR_LS_VERSION: &str = "21.2.10";

/// npm 缓存目录（= servers.toml [servers.angular] npm 主包版本键，禁随意改）。
const CACHE_ID: &str = "angular";
const CACHE_VERSION: &str = ANGULAR_LS_VERSION;

/// 主 tsls 就绪探针超时（astro 同款）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 伴生握手段内预算（audit P2 #9）：on_session_ready 受 supervisor 30s 包裹，
/// ng/html 握手在此各自封顶——包裹超时取消不再落在「已 spawn 未入槽/未挂 watch」
/// 的半登记窗口（ngserver 孤儿根因）。超时 = 降级放行（缺失同款语义）。
const NG_HANDSHAKE_BUDGET: Duration = Duration::from_secs(20);
const HTML_HANDSHAKE_BUDGET: Duration = Duration::from_secs(5);

/// 伴生进程退出监视轮询间隔（astro 同款）。
const WATCH_POLL: Duration = Duration::from_millis(500);

/// root 未设置时的退路：虚拟探针 URI。
const PROBE_FALLBACK: &str = "file:///__angular_ready_probe__";

/// 当前会话项目 root 表（per-project 键化，bd serena-rust-4y6；零字段单例 →
/// 静态槽，supervisor 经 set_project_root 写入。读侧无键，走 get_last 相邻语义）。
static PROBE_ROOT: ProjectRootSlot = ProjectRootSlot::new();

/// ngserver 伴生会话（.html definition/hover/references）。key = root。
static NG_COMPANION: Mutex<Option<(PathBuf, std::sync::Arc<lsp_core::session::Session>)>> =
    Mutex::new(None);
/// vscode-html 伴生会话（.html documentSymbol）。key = root。
static HTML_COMPANION: Mutex<Option<(PathBuf, std::sync::Arc<lsp_core::session::Session>)>> =
    Mutex::new(None);

fn clear_companions() {
    if let Ok(mut slot) = NG_COMPANION.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = HTML_COMPANION.lock() {
        *slot = None;
    }
}

/// 安装目录：`{cache_root}/angular/{主包版本}`（npm 四包同装一目录，npm hoist
/// 让 ngserver 的 plugin 解析可见全家 —— ↖ mirror 上游单 node_modules 形态）。
fn install_dir() -> PathBuf {
    default_cache_root().join(CACHE_ID).join(CACHE_VERSION)
}

fn node_modules(install: &Path) -> PathBuf {
    install.join("node_modules")
}

/// tsdk（typescript/lib）：ngserver probe 与伴生 tsls 的 tsserver 程序根。
fn tsdk_path(install: &Path) -> PathBuf {
    node_modules(install).join("typescript").join("lib")
}

/// @angular/language-service 包目录（tsserver plugin location）。
fn plugin_path(install: &Path) -> PathBuf {
    node_modules(install)
        .join("@angular")
        .join("language-service")
}

/// npm bin 解析（Windows 严格 .cmd，Unix 裸 shim；spawn 层对 .cmd 自动 cmd /c 包装，
/// html.rs 实证路径）。
fn bin_in(install: &Path, name: &str) -> Option<PathBuf> {
    npm_bin_path(install, name)
}

/// 安装面自检：ngserver / tsls / plugin / tsserverlibrary 四件全在才算装好
/// （↖ mirror `_setup_runtime_dependencies` 尾部 FileNotFoundError 校验清单）。
fn resolve_install() -> anyhow::Result<PathBuf> {
    let dir = install_dir();
    let ok = bin_in(&dir, "ngserver").is_some()
        && bin_in(&dir, "typescript-language-server").is_some()
        && plugin_path(&dir).is_dir()
        && tsdk_path(&dir).join("tsserverlibrary.js").is_file();
    if ok {
        Ok(dir)
    } else {
        Err(not_installed_error(
            "angular",
            "run `serena-cli install angular` (npm installs @angular/language-server \
             + @angular/language-service + typescript + typescript-language-server); \
             requires node/npm on PATH",
        ))
    }
}

/// 向上探测 `node_modules/@angular/core`（monorepo hoist 布局；找到即停）。
/// 缺失 = None（调用方 warn 放行——上游 `_check_angular_core_in_project` 简化版：
/// Δ 未抄 mount-point/workspace 边界检测，向上走到盘根为止）。
fn find_angular_core_install(root: &Path) -> Option<PathBuf> {
    root.ancestors()
        .map(|p| p.join("node_modules").join("@angular").join("core"))
        .find(|c| c.is_dir())
}

/// 主 tsls 初始化参数（↖ mirror `AngularTypeScriptServer._create_base_initialize_params`：
/// plugins 注入 @angular/language-service + tsserver.path tsdk；executeCommand 动态注册）。
fn main_init_params(root: &Path, install: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        "rootUri": path_to_uri_str(root),
        "capabilities": {
            "workspace": { "executeCommand": { "dynamicRegistration": true } },
        },
        "initializationOptions": {
            "plugins": [{
                "name": "@angular/language-service",
                "location": plugin_path(install),
                "languages": ["html"],
            }],
            "tsserver": { "path": tsdk_path(install) },
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// ngserver 初始化参数（↖ mirror `AngularLanguageServer._create_base_initialize_params`：
/// ngProbeLocations/tsProbeLocations 指 install node_modules + forceStrictTemplates:false）。
fn ng_init_params(root: &Path, install: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        "rootUri": path_to_uri_str(root),
        "capabilities": {},
        "initializationOptions": {
            "ngProbeLocations": [node_modules(install)],
            "tsProbeLocations": [node_modules(install)],
            "forceStrictTemplates": false,
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// vscode-html 伴生初始化参数（html.rs 主门同款形态：rootUri + 空对象 options）。
fn html_init_params(root: &Path) -> anyhow::Result<InitializeParams> {
    use lsp_core::docsync::path_to_uri_str;
    let raw = json!({
        "rootUri": path_to_uri_str(root),
        "capabilities": {},
        "initializationOptions": {
            "embeddedLanguages": { "css": true, "javascript": true },
            "handledSchemas": ["file"],
            "provideFormatter": false,
        },
    });
    Ok(serde_json::from_value(raw)?)
}

/// server→client `workspace/configuration` 应答：每 item 一个空对象（↖ mirror
/// `AngularLanguageServer._start_server` 的 `lambda _params: [{}]` 与伴生 tsls
/// `workspace_configuration_handler`；LS 拿到 {} 回落 init options）。
fn configuration_reply(msg: JsonRpc) -> Option<Value> {
    let items = msg.params.as_ref()?.get("items")?.as_array()?.len();
    Some(json!(vec![json!({}); items]))
}

/// .html/.htm 判定（↖ mirror `_is_html_template_file`）。
fn is_html_file(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm"))
}

/// 从静态槽取伴生会话（root 匹配才返回——跨项目残留不清）。
fn companion_of(
    slot: &Mutex<Option<(PathBuf, std::sync::Arc<lsp_core::session::Session>)>>,
    root: &Path,
) -> Option<std::sync::Arc<lsp_core::session::Session>> {
    let guard = slot.lock().expect("companion slot poisoned");
    guard
        .as_ref()
        .filter(|(r, _)| r == root)
        .map(|(_, s)| std::sync::Arc::clone(s))
}

/// spawn 伴生 + 独立 Session 握手（astro.rs 同款三步：Child::spawn → Session::start
/// → language_id 注入）。
async fn spawn_companion(
    launch: LaunchInfo,
    params: InitializeParams,
    language_id: &str,
    label: &str,
) -> anyhow::Result<(
    std::sync::Arc<lsp_core::session::Session>,
    Option<tokio::process::Child>,
)> {
    let mut handle =
        Child::spawn(launch).map_err(|e| anyhow::anyhow!("companion {label} spawn failed: {e}"))?;
    let child = handle.take_child();
    let session = lsp_core::session::Session::start(Some(handle), params)
        .await
        .map_err(|e| anyhow::anyhow!("companion {label} handshake failed: {e}"))?;
    session.set_language_id(language_id);
    Ok((session, child))
}

/// 伴生监视（astro.rs 同款双向灭树：伴生死 → 主 shutdown；主死 → 清槽灭树）。
fn watch_companion(
    mut child: tokio::process::Child,
    session: &std::sync::Arc<lsp_core::session::Session>,
    clear: fn(),
) {
    let main_weak = std::sync::Arc::downgrade(session);
    tokio::spawn(async move {
        loop {
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                if let Some(m) = main_weak.upgrade() {
                    m.shutdown().await;
                }
                clear();
                return;
            }
            if main_weak.upgrade().is_none() {
                clear();
                return;
            }
            tokio::time::sleep(WATCH_POLL).await;
        }
    });
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AngularAdapter;

#[async_trait]
impl LanguageServerAdapter for AngularAdapter {
    fn id(&self) -> &'static str {
        "angular"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Angular];
        LANGS
    }

    /// 主会话 = 伴生 typescript-language-server（含 @angular/language-service 插件）。
    async fn launch_info(&self, _ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let install = resolve_install()?;
        let tsls = bin_in(&install, "typescript-language-server").ok_or_else(|| {
            anyhow::anyhow!("typescript-language-server missing in angular install")
        })?;
        Ok(LaunchInfo {
            cmd: vec![tsls.into_os_string(), "--stdio".into()],
            cwd: _ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        let Ok(install) = resolve_install() else {
            return;
        };
        let Ok(main) = main_init_params(Path::new("."), &install) else {
            return;
        };
        // 只移植 initializationOptions / capabilities 补丁（rootUri 由 supervisor
        // 会话路径管理，伴生参数模板在此仅作形状来源）。
        base.initialization_options = main.initialization_options;
        base.capabilities.workspace = main.capabilities.workspace;
    }

    fn set_project_root(&self, root: &Path) {
        PROBE_ROOT.set(root);
    }

    /// `.html` 语义重路由（↖ mirror 路由表 `.html` 行）：documentSymbol → vscode-html
    /// 伴生（ngserver 恒 -32601）；definition/hover/references → ngserver。
    /// 其余扩展名/伴生缺失 → None（调用方维持原会话）。
    fn session_for_file(
        &self,
        root: &Path,
        file: &Path,
        method: &str,
    ) -> Option<std::sync::Arc<lsp_core::session::Session>> {
        if !is_html_file(file) {
            return None;
        }
        if method == "textDocument/documentSymbol" {
            return companion_of(&HTML_COMPANION, root);
        }
        companion_of(&NG_COMPANION, root)
    }

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        let install = resolve_install()?;
        let root = PROBE_ROOT.get_last().unwrap_or_else(|| PathBuf::from("."));

        // 硬前提探测：@angular/core 缺失 → 模板特性静默空（上游 warn 同款）。
        if find_angular_core_install(&root).is_none() {
            tracing::warn!(
                "angular LS active but @angular/core not found in any node_modules from {} \
                 upward; ngserver will treat files as 'not in an Angular project' and \
                 template features stay empty. Run `npm install` in the workspace root.",
                root.display()
            );
        }

        // 主会话（tsls）languageId：didOpen 官方口径（lsp_language_id("angular") 基础
        // 值 "typescript"），per-file 覆盖 ts/js 家族。.html 不在主会话打开（重路由）。
        session.set_language_id_for_extensions(&[
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("mjs", "javascript"),
            ("cjs", "javascript"),
        ]);

        // 1) ngserver 伴生：--stdio + 双 probe 指向 install node_modules。
        //    ngserver bin 放 cmd[0]（spawn 层对 .cmd 自动 cmd /c 包装——主 tsls/html
        //    同款）。e2e 实锤：原 `[node, ngserver.cmd, ...]` 形态让 node 直接执行
        //    批处理 → SyntaxError 即死，ngserver 在 Windows 上从未真正启动过。
        let ng = bin_in(&install, "ngserver")
            .ok_or_else(|| anyhow::anyhow!("ngserver missing in angular install"))?;
        let mut ng_cmd = vec![ng.into_os_string()];
        ng_cmd.push("--stdio".into());
        ng_cmd.push("--tsProbeLocations".into());
        ng_cmd.push(node_modules(&install).into_os_string());
        ng_cmd.push("--ngProbeLocations".into());
        ng_cmd.push(node_modules(&install).into_os_string());
        let ng_launch = LaunchInfo {
            cmd: ng_cmd,
            cwd: root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        };

        // 2) vscode-html 伴生：复用 html 门缓存（serena-cli install html 先装；
        //    未装 → .html documentSymbol 空，non-fatal —— 上游 _start_html_server
        //    try/except 同款；Δ 不挂退出监视——html 伴生死只降级 documentSymbol，
        //    不连坐主会话（上游 _stop_html_server 同语义）。
        let html_dir = default_cache_root()
            .join(crate::html::CACHE_ID)
            .join(crate::html::CACHE_VERSION);
        let html_bin = bin_in(&html_dir, crate::html::BIN_REL);

        // 1) ngserver 伴生：握手段内预算封顶（audit P2 #9）。超时降级放行 =
        //    ngserver 缺失同款语义（.html 语义空）；降级路径 Session::start future
        //    drop → Job 句柄 drop → KILL_ON_JOB_CLOSE 灭树，无进程残留。
        let (ng_session, ng_child) = match tokio::time::timeout(
            NG_HANDSHAKE_BUDGET,
            spawn_companion(
                ng_launch,
                ng_init_params(&root, &install)?,
                "html",
                "ngserver",
            ),
        )
        .await
        {
            Ok(v) => v?,
            Err(_) => {
                tracing::warn!(
                    "ngserver companion handshake exceeded {NG_HANDSHAKE_BUDGET:?}; \
                     .html semantics stay empty"
                );
                return Ok(());
            }
        };

        // 2) 立即登记 + 挂监视（本段无 await 点——外层 30s 包裹的取消不可能再落进
        //    「已 spawn 未登记」窗口产生孤儿）。
        //
        //    监视传主会话弱引用（astro.rs:390-413 同款；audit P1 #1——原实现
        //    Arc::downgrade(ng_session) 指向伴生自身，NG/HTML 槽内的强引用使
        //    「主死 → 清槽」分支永不可达，主会话被驱逐后 ngserver+html 双 node
        //    进程 + watcher 全部滞留至 daemon 退出）。
        if let Ok(mut slot) = NG_COMPANION.lock() {
            *slot = Some((root.clone(), std::sync::Arc::clone(&ng_session)));
        }
        ng_session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);
        session
            .client()
            .on_server_request("workspace/configuration", configuration_reply);

        // 监视：ngserver 伴生死 → 主 shutdown（.html 语义全灭，无继续价值）；
        // 主会话死（驱逐/shutdown）→ 清槽 → 槽内 Arc 归零 → Job 灭树。
        if let Some(child) = ng_child {
            watch_companion(child, session, clear_companions);
        }

        // 3) vscode-html 伴生：复用 html 门缓存（serena-cli install html 先装；
        //    未装/握手慢 → .html documentSymbol 空，non-fatal —— 上游
        //    _stop_html_server 同语义；不挂退出监视——html 生死只降级 outline，
        //    由上方主会话监视的清槽路径统一回收）。
        if let Some(html_exe) = html_bin {
            let html_launch = LaunchInfo {
                cmd: vec![html_exe.into_os_string(), "--stdio".into()],
                cwd: root.clone(),
                env: vec![],
                transport: TransportKind::Stdio,
            };
            match tokio::time::timeout(
                HTML_HANDSHAKE_BUDGET,
                spawn_companion(
                    html_launch,
                    html_init_params(&root)?,
                    "html",
                    "vscode-html-ls",
                ),
            )
            .await
            {
                Ok(Ok((html_session, _html_child))) => {
                    html_session
                        .client()
                        .on_server_request("workspace/configuration", configuration_reply);
                    if let Ok(mut slot) = HTML_COMPANION.lock() {
                        *slot = Some((root.clone(), html_session));
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        "html companion unavailable; .html documentSymbol will be empty: {e}"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "html companion handshake exceeded {HTML_HANDSHAKE_BUDGET:?}; \
                         .html documentSymbol will be empty"
                    );
                }
            }
        } else {
            tracing::warn!(
                "vscode-html-language-server not installed (serena-cli install html); \
                 .html documentSymbol will be empty"
            );
        }

        // 5) 主 tsls 就绪探针：真实 .ts 优先（astro 同款），失败只 warn 放行。
        let probe_uri = crate::probe_uri_for_root(&root, &[LanguageId::Angular], PROBE_FALLBACK);
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

    /// 主 tsls 支持 implementation（↖ mirror `AngularLanguageServer.supports_implementation_
    /// request` → True：ngserver 委托 tsserver，伴生 tsls 亦然）。
    fn supports_implementation(&self) -> bool {
        true
    }

    /// `.ts` 跨文件首查等 tsls 的 `$/progress` 索引（typescript.rs 同款两段等待，
    /// astro.rs 先例形态）。
    async fn wait_for_cross_file_index(&self, session: &lsp_core::session::Session) {
        const START_GRACE: Duration = Duration::from_secs(5);
        const PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);
        let completed = if session.take_cross_file_first_query() {
            session
                .wait_indexing_start_or_completion(PROGRESS_TIMEOUT, START_GRACE)
                .await
        } else {
            session.wait_indexing_drain(PROGRESS_TIMEOUT).await
        };
        if !completed {
            tracing::debug!("angular tsls index wait timed out; proceeding");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_match_upstream_defaults() {
        // ↖ mirror DEFAULT_* 常量（angular_language_server.py@7a296833 模块头）；
        // 次包三 pin 与 servers.toml [servers.angular].npm.secondary_packages 对账
        //（servers_toml_coverage 侧另有条目断言）。
        assert_eq!(ANGULAR_LS_VERSION, "21.2.10");
        assert_eq!(CACHE_VERSION, "21.2.10");
        // 次包三 pin（language-service 21.2.10 / typescript 5.9.3 / tsls 5.1.3）的
        // 对账在 servers_toml_coverage::w3b_pyrefly_angular_doors_wired（toml 侧）。
    }

    #[test]
    fn main_init_params_inject_plugin_and_tsdk() {
        let install = Path::new("Z:/install");
        let p = main_init_params(Path::new("Z:/proj"), install).expect("params");
        let raw = serde_json::to_value(&p).expect("serialize");
        let plugin = &raw["initializationOptions"]["plugins"][0];
        assert_eq!(plugin["name"], json!("@angular/language-service"));
        assert_eq!(
            plugin["location"],
            json!(plugin_path(install).to_string_lossy().to_string())
        );
        assert_eq!(plugin["languages"], json!(["html"]));
        assert_eq!(
            raw["initializationOptions"]["tsserver"]["path"],
            json!(tsdk_path(install).to_string_lossy().to_string())
        );
        // executeCommand 动态注册（↖ mirror capabilities 补丁）。
        assert_eq!(
            raw["capabilities"]["workspace"]["executeCommand"]["dynamicRegistration"],
            json!(true)
        );
    }

    #[test]
    fn ng_init_params_declare_probe_locations() {
        let install = Path::new("Z:/install");
        let p = ng_init_params(Path::new("Z:/proj"), install).expect("params");
        let raw = serde_json::to_value(&p).expect("serialize");
        let nm = node_modules(install).to_string_lossy().to_string();
        assert_eq!(
            raw["initializationOptions"]["ngProbeLocations"],
            json!([nm])
        );
        assert_eq!(
            raw["initializationOptions"]["tsProbeLocations"],
            json!([nm.clone()])
        );
        assert_eq!(
            raw["initializationOptions"]["forceStrictTemplates"],
            json!(false)
        );
    }

    #[test]
    fn configuration_reply_mirrors_items() {
        let msg = JsonRpc::notification(
            "workspace/configuration",
            json!({ "items": [{ "section": "a" }, {}] }),
        );
        assert_eq!(configuration_reply(msg), Some(json!([{}, {}])));
        let msg = JsonRpc::notification("workspace/configuration", json!({}));
        assert_eq!(configuration_reply(msg), None);
    }

    #[test]
    fn angular_core_probe_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("apps").join("web");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        // 无 @angular/core → None。
        assert!(find_angular_core_install(&sub).is_none());
        // hoist 到 workspace root → 命中。
        let core = dir
            .path()
            .join("node_modules")
            .join("@angular")
            .join("core");
        std::fs::create_dir_all(&core).unwrap();
        assert_eq!(find_angular_core_install(&sub), Some(core));
    }

    #[test]
    fn session_for_file_routes_html_by_method() {
        // 伴生未启动 → 全 None（调用方维持主会话，不新增失败模式）。
        let adapter = AngularAdapter;
        assert!(
            adapter
                .session_for_file(Path::new("."), Path::new("a.ts"), "textDocument/hover")
                .is_none()
        );
        // .html 判定独立于伴生存在性（路由决策纯函数面）。
        assert!(is_html_file(Path::new("src/app/app.component.html")));
        assert!(is_html_file(Path::new("index.HTM")));
        assert!(!is_html_file(Path::new("src/main.ts")));
    }

    #[test]
    fn angular_declares_language_closure() {
        assert_eq!(AngularAdapter.languages(), &[LanguageId::Angular]);
        assert_eq!(
            LanguageId::from_str_opt("angular"),
            Some(LanguageId::Angular)
        );
        // 扩展名不抢：.ts/.html 归既有门（--lang 显式路由可达）。
        assert_eq!(
            LanguageId::from_extension("ts"),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(LanguageId::from_extension("html"), Some(LanguageId::Html));
    }
}
