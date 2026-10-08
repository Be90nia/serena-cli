//! Scala Metals 适配器（上游对拍采纳 W3 批，findings batchA 缺失[高]）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/scala_language_server.py`
//!
//! ## 要点
//!
//! - **build-root 探测作 workspaceFolders**（↖ mirror `find_build_roots` /
//!   `BUILD_ROOT_MARKER_FILES`）：Metals 一个 workspace folder 服务一个 build，仓库
//!   内含多个 build（或 build 在仓库根之下）时必须把 build 根目录发给它。探测深度 3，
//!   不识别任何 build 时回落仓库根本身（保持 Metals 默认行为）。
//! - **Metals 进度等待**（↖ mirror `MetalsProgressTracker`）：import/index/compile 以
//!   work-done progress 上报，references 类语义依赖 build server 编译产物
//!   SemanticDB——首查等其完成而非固定时长。Metals 阶段交接（import→indexing→compile）
//!   时 token 集会短暂清空，所以"完成"= 连续 `QUIET_PERIOD` 无新 token，不是"此刻为
//!   空"。跟踪复用 lsp-core [`lsp_core::session::Session`] 的 `$/progress` 在飞 token
//!   表（begin/create 插入、end 移除，三路信号含 `window/workDoneProgress/create`）。
//! - **showMessageRequest 自动应答**（↖ mirror `choose_show_message_request_action`）：
//!   只应答站在"未导入工作区→build server"路上的三个动作（Import build / Import
//!   changes / Connect）；其余一律回 None（LSP 规范的"不选择"，即我们的 null 默认
//!   应答）。刻意不应答 ChooseBuildTool（多 build 定义选择——替用户选构建工具是另
//!   一类猜测）与"Don't show again"（Metals 会把该选择持久化进工程状态）。
//! - **安装面**：PATH 探测 `metals`（servers.toml path_only 条目同源；上游同样 PATH
//!   优先，miss 时 coursier bootstrap——托管安装归 `serena-cli install scala` 提示链）。
//! - Δ 未抄：陈旧 H2 锁清理（check_metals_db_status）、多实例提示、ls_specific_settings
//!   全部可调项（我们无 settings 通道，常量取上游默认值）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, declare_work_done_progress,
    not_installed_error, which_no_unc,
};

/// 首查跨文件等待兜底超时。↖ mirror: `DEFAULT_INDEXING_TIMEOUT`（180.0s）
const INDEXING_TIMEOUT: Duration = Duration::from_secs(180);
/// 等待工作*开始*的宽限窗：超窗无任何活动 = Metals 不上报进度，放行。
/// ↖ mirror: `DEFAULT_INDEXING_START_GRACE`（15.0s）
const INDEXING_START_GRACE: Duration = Duration::from_secs(15);
/// "完成"要求的连续静默时长（阶段交接的 token 空窗约 1-2s）。
/// ↖ mirror: `DEFAULT_INDEXING_QUIET_PERIOD`（3.0s）
const INDEXING_QUIET_PERIOD: Duration = Duration::from_secs(3);
/// 活动轮询间隔（↖ mirror 上游 0.05s 轮询；watch 通道无「保持为空 X 时长」原语）。
const POLL: Duration = Duration::from_millis(50);

/// build-root 探测深度。↖ mirror: `DEFAULT_PROJECT_ROOT_SCAN_DEPTH`（3）
pub const DEFAULT_PROJECT_ROOT_SCAN_DEPTH: u8 = 3;

/// 标记 build 根目录的文件（Metals `BuildTools` per-build-tool 探测的有意子集：
/// 需要读文件内容才能判定的探针（scala-cli BSP scope）刻意不收——漏判只是回到
/// 不分派的行为，误判会遮蔽其下真实 build）。
/// ↖ mirror: `BUILD_ROOT_MARKER_FILES`
pub const BUILD_ROOT_MARKER_FILES: &[&str] = &[
    "MODULE.bazel",
    "WORKSPACE",
    "build.gradle",
    "build.gradle.kts",
    "build.mill",
    "build.mill.scala",
    "build.mill.yaml",
    "build.sbt",
    "build.sc",
    "deder.pkl",
    "mill",
    "mill.bat",
    "pom.xml",
    "project.scala",
    "settings.gradle",
    "settings.gradle.kts",
];

/// 目录内含任意 .json 文件即视为已配置的 Metals 工程（空目录是遗留物不是 build）。
/// ↖ mirror: `BUILD_ROOT_MARKER_JSON_DIRS`（`BuildTools.hasJsonFile`）
pub const BUILD_ROOT_MARKER_JSON_DIRS: &[&str] = &[".bloop", ".bsp"];

/// 扫描不向下进入的目录（构建产物/依赖/其父已是 build 的目录）；目录本身仍会被
/// 探测——只跳过下降。
/// ↖ mirror: `BUILD_ROOT_SCAN_SKIP_DIRS`
pub const BUILD_ROOT_SCAN_SKIP_DIRS: &[&str] =
    &["node_modules", "out", "project", "src", "target", "venv"];

/// 自动应答的 build 导入提示动作。↖ mirror: `BUILD_IMPORT_PROMPT_ACTIONS`
pub const BUILD_IMPORT_PROMPT_ACTIONS: &[&str] = &["Import build", "Import changes", "Connect"];

/// 目录内是否含任意 `.json` 文件（↖ mirror `_contains_json_file`；OSError → false）。
fn contains_json_file(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("json"))
    })
}

/// 是否为 Metals 可导入的 build 根。↖ mirror: `_is_build_root`
pub fn is_build_root(path: &Path) -> bool {
    if BUILD_ROOT_MARKER_FILES
        .iter()
        .any(|n| path.join(n).is_file())
    {
        return true;
    }
    if BUILD_ROOT_MARKER_JSON_DIRS
        .iter()
        .any(|d| contains_json_file(&path.join(d)))
    {
        return true;
    }
    // sbt 允许 build 完整定义在 project/ 下而无 build.sbt：
    // project/build.properties 含 sbt.version 行即 build 根。
    let props = path.join("project").join("build.properties");
    if let Ok(bytes) = std::fs::read(&props) {
        return String::from_utf8_lossy(&bytes)
            .lines()
            .any(|l| l.trim_start().starts_with("sbt.version"));
    }
    false
}

/// 找仓库内全部 build 根（↖ mirror `find_build_roots`）。Metals 一个 workspace
/// folder 一个 build：多 build 仓库（或 build 在仓库根之下）必须分派目录而非仓库
/// 根。未识别到任何 build → `[root]`（保持 Metals 默认行为）。深度语义同上游：
/// `scan(root, 1)` 起、`depth > max_depth` 剪枝（根的直接子目录 = 深度 1）。
pub fn find_build_roots(root: &Path, max_depth: u8) -> Vec<PathBuf> {
    if is_build_root(root) {
        return vec![root.to_path_buf()];
    }
    let mut roots = Vec::new();
    let mut visited = HashSet::new();
    scan(root, 1, max_depth, &mut roots, &mut visited);
    if roots.is_empty() {
        vec![root.to_path_buf()]
    } else {
        roots
    }
}

/// 深度优先扫描（↖ mirror `scan`）：符号链接跟随（Metals 自身搜索同款），canonical
/// 路径去环；点目录跳过；是 build 根则收录不再下降，非 skip 目录才继续下降。
fn scan(
    dir: &Path,
    depth: u8,
    max_depth: u8,
    roots: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) {
    if depth > max_depth {
        return;
    }
    let real = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(real) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // 确定性顺序（↖ mirror sorted(scandir, key=name)）。
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Ok(meta) = entry.metadata() else { continue }; // 跟随符号链接
        if !meta.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let child = entry.path();
        if is_build_root(&child) {
            roots.push(child);
        } else if BUILD_ROOT_SCAN_SKIP_DIRS.contains(&name.as_ref()) {
            // skip 目录：直接子层仍探测 build 根（node_modules/<pkg> 形态），
            // 但不下降更深层（↖ mirror "只跳过下降"）。
            if depth < max_depth {
                let Ok(sub) = std::fs::read_dir(&child) else {
                    continue;
                };
                let mut sub: Vec<_> = sub.flatten().collect();
                sub.sort_by_key(|e| e.file_name());
                for e in sub {
                    let p = e.path();
                    if p.is_dir() && is_build_root(&p) {
                        roots.push(p);
                    }
                }
            }
        } else {
            scan(&child, depth + 1, max_depth, roots, visited);
        }
    }
}

/// `window/showMessageRequest` 应答选择（↖ mirror `choose_show_message_request_action`）。
/// 命中 [`BUILD_IMPORT_PROMPT_ACTIONS`] 的首个动作原样返回；否则 None = 不选择
/// （调用方 handler 回 None → lsp-core 默认 null 应答，与上游 return None 同语义）。
pub fn choose_show_message_request_action(params: Option<&Value>) -> Option<Value> {
    let params = params?;
    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
    let actions = params.get("actions")?.as_array()?;
    for action in actions {
        if let Some(title) = action.get("title").and_then(Value::as_str)
            && BUILD_IMPORT_PROMPT_ACTIONS.contains(&title)
        {
            tracing::info!(message, title, "metals showMessageRequest auto-answered");
            return Some(action.clone());
        }
    }
    tracing::info!(message, "metals showMessageRequest dismissed");
    None
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScalaAdapter;

#[async_trait]
impl LanguageServerAdapter for ScalaAdapter {
    fn id(&self) -> &'static str {
        "scala"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Scala];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let metals = which_no_unc("metals").ok_or_else(|| {
            not_installed_error(
                "scala",
                "bootstrap via coursier: cs bootstrap org.scalameta:metals_2.13:1.6.4 -o \
                 metals --java-opt -Xss4m (JDK 11+; upstream scala_language_server.py\
                 @7a296833 DEFAULT_METALS_VERSION)",
            )
        })?;
        Ok(LaunchInfo {
            cmd: vec![metals.into_os_string()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // 进度上报门：不声明则 Metals 从不报告在做什么，首查等待无事可等
        // （↖ mirror `_create_base_initialize_params` 注释原文）。
        declare_work_done_progress(base);
        // initializationOptions 全树（↖ mirror 逐字；常量取上游默认值——我们无
        // settings 通道）。isExitOnShutdown=true：会话 shutdown 时 Metals 自退。
        base.initialization_options = Some(json!({
            "compilerOptions": {
                "completionCommand": null,
                "isCompletionItemDetailEnabled": true,
                "isCompletionItemDocumentationEnabled": true,
                "isCompletionItemResolve": true,
                "isHoverDocumentationEnabled": true,
                "isSignatureHelpDocumentationEnabled": true,
                "overrideDefFormat": "ascli",
                "snippetAutoIndent": false
            },
            "debuggingProvider": true,
            "decorationProvider": false,
            "didFocusProvider": false,
            "doctorProvider": false,
            "executeClientCommandProvider": false,
            "globSyntax": "uri",
            "icons": "unicode",
            "inputBoxProvider": false,
            "isVirtualDocumentSupported": false,
            "isExitOnShutdown": true,
            "isHttpEnabled": true,
            "openFilesOnRenameProvider": false,
            "quickPickProvider": false,
            "renameFileThreshold": 200,
            "statusBarProvider": "false",
            "treeViewProvider": false,
            "testExplorerProvider": false,
            "openNewWindowProvider": false,
            "copyWorksheetOutputProvider": false,
            "doctorVisibilityProvider": false
        }));
        base.locale = Some("en".into());
        // workspaceFolders = build 根（↖ mirror `ScalaInitializeParamsBuilder`：
        // 多 build 仓库必须分派 build 根目录，发仓库根会让单 folder 服务不了任何
        // 一个 build）。探测回落 [root] 时与 supervisor 默认形态一致。
        if let Some(root) = (|| {
            // root_uri 反解（lsp-types 标记 deprecated，supervisor 设置处同款 allow；
            // file:/// 形态与 docsync::path_to_uri_str 同源，仅 PATH_UNSAFE 集需解码）。
            #[allow(deprecated)]
            let raw = base.root_uri.as_ref()?;
            let rest = raw.as_str().strip_prefix("file:///")?;
            percent_decode_str(rest).decode_utf8().ok().map(|decoded| {
                #[cfg(windows)]
                let path = decoded.to_string();
                #[cfg(unix)]
                let path = format!("/{decoded}");
                PathBuf::from(path)
            })
        })() {
            let folders = find_build_roots(&root, DEFAULT_PROJECT_ROOT_SCAN_DEPTH)
                .into_iter()
                .filter_map(|p| {
                    let uri = lsp_core::docsync::path_to_uri_str(&p);
                    let uri = lsp_types::Uri::from_str(&uri).ok()?;
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("build")
                        .to_string();
                    Some(lsp_types::WorkspaceFolder { uri, name })
                })
                .collect::<Vec<_>>();
            if !folders.is_empty() {
                base.workspace_folders = Some(folders);
            }
        }
    }

    fn set_project_root(&self, _root: &Path) {}

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        // import 构建提示自动应答（未注册方法的默认应答是 null=不选择，与上游
        // return None 同语义——见 choose_show_message_request_action 文档）。
        session
            .client()
            .on_server_request("window/showMessageRequest", |msg| {
                choose_show_message_request_action(msg.params.as_ref())
            });
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    async fn wait_for_cross_file_index(&self, session: &lsp_core::session::Session) {
        // 上游单次 latch（`_has_waited_for_cross_file_references`）：仅首查等待；
        // Δ 不套用 cf54869a 的后续查询 drain 修订——metals 阶段交接窗口本就会把
        // token 集清空，逐查 drain 会在交接点提前放行，上游 Scala 即单次语义。
        if !session.take_cross_file_first_query() {
            return;
        }
        wait_metals_idle(
            session,
            INDEXING_TIMEOUT,
            INDEXING_START_GRACE,
            INDEXING_QUIET_PERIOD,
        )
        .await;
    }
}

/// 等 Metals 在飞工作清空并保持静默（↖ mirror `MetalsProgressTracker.wait_until_idle`）。
/// 返回即放行（上游 permissive：NO_WORK / IDLE / TIMEOUT 三结局都 proceed，仅日志
/// 有别）；超时 warn 供排障。
async fn wait_metals_idle(
    session: &lsp_core::session::Session,
    timeout: Duration,
    start_grace: Duration,
    quiet_period: Duration,
) {
    // ① start grace：等工作出现。didOpen（调用方 ensure_open 已发）触发 Metals 连
    // build server 并开工；Δ 上游 expect_work 在 didOpen 前置位——我们 wait 在
    // didOpen 后立刻执行，grace 窗口同样覆盖工作启动，无需前置位。
    let grace_deadline = tokio::time::Instant::now() + start_grace;
    let mut saw_work = session.index_active_progress() > 0;
    while !saw_work {
        let now = tokio::time::Instant::now();
        if now >= grace_deadline {
            break;
        }
        tokio::time::sleep(POLL.min(grace_deadline - now)).await;
        saw_work = session.index_active_progress() > 0;
    }
    if !saw_work {
        tracing::info!(
            "metals reported no work within {start_grace:?}; proceeding (no build server?)"
        );
        return;
    }
    // ② drain + quiet 确认：阶段交接（import→indexing→compile）时 token 集短暂
    // 清空，drain 返回≠完成——连续 quiet_period 无新 token 才算落地。
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                "metals still working after {timeout:?}; proceeding, cross-file results may \
                 be incomplete"
            );
            return;
        }
        let _ = session.wait_indexing_drain(remaining).await;
        let quiet_end = tokio::time::Instant::now() + quiet_period.min(remaining);
        let mut quiet = true;
        while tokio::time::Instant::now() < quiet_end {
            if session.index_active_progress() > 0 {
                quiet = false;
                break;
            }
            let step = quiet_end.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(step.min(POLL)).await;
        }
        if quiet && session.index_active_progress() == 0 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build-root 探测（纯文件系统 fixture）----

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn root_with_marker_is_single_build_root() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("build.sbt"), "scalaVersion := \"3\"\n");
        assert_eq!(
            find_build_roots(dir.path(), 3),
            vec![dir.path().to_path_buf()]
        );
    }

    #[test]
    fn nested_builds_are_found_skip_dirs_are_probed_but_not_descended() {
        let dir = tempfile::tempdir().unwrap();
        // 两个真实 build（深度 1/2）+ node_modules 内 build（探测收录但不下降）
        // + 深度 4 的 build（超出 max_depth 不收）+ 点目录（跳过）。
        write(&dir.path().join("services/api/build.sbt"), "");
        write(&dir.path().join("webapp/pom.xml"), "<project/>");
        write(&dir.path().join("node_modules/pkg/build.sbt"), "");
        write(&dir.path().join("deep/a/b/c/build.sbt"), "");
        write(&dir.path().join(".hidden/build.sbt"), "");
        write(&dir.path().join("services/api/src/Main.scala"), "");
        let roots = find_build_roots(dir.path(), 3);
        let names: Vec<String> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // node_modules 本身探测：其子 pkg/build.sbt 在深度 2 —— is_build_root 命中收录。
        assert!(names.contains(&"api".to_string()), "{names:?}");
        assert!(names.contains(&"webapp".to_string()), "{names:?}");
        assert!(names.contains(&"pkg".to_string()), "{names:?}");
        assert!(!names.contains(&"c".to_string()), "深度 4 不收: {names:?}");
        assert!(
            !names.contains(&"hidden".to_string()),
            "点目录跳过: {names:?}"
        );
        // services/api 是 build 根 → 不再下降（src 下无 build 语义本就无影响，
        // 此处锁「收录即剪枝」）。
    }

    #[test]
    fn no_markers_falls_back_to_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Main.scala"), "object Main");
        assert_eq!(
            find_build_roots(dir.path(), 3),
            vec![dir.path().to_path_buf()]
        );
    }

    #[test]
    fn bloop_or_bsp_json_marks_build_root() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".bsp").join("server.json"), "{}");
        assert!(is_build_root(dir.path()));
        // 空 .bloop（无 .json）不算——遗留目录不是 build。
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir2.path().join(".bloop")).unwrap();
        assert!(!is_build_root(dir2.path()));
    }

    #[test]
    fn sbt_build_properties_marks_build_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("project/build.properties"),
            "sbt.version=1.9.0\n",
        );
        assert!(is_build_root(dir.path()));
        let dir2 = tempfile::tempdir().unwrap();
        write(&dir2.path().join("project/build.properties"), "other=1\n");
        assert!(!is_build_root(dir2.path()));
    }

    // ---- showMessageRequest 应答 ----

    #[test]
    fn import_prompts_answered_other_prompts_dismissed() {
        // 命中：返回动作对象本身。
        let msg = json!({
            "message": "Import build?",
            "actions": [
                { "title": "Import build" },
                { "title": "Don't show again" }
            ]
        });
        assert_eq!(
            choose_show_message_request_action(Some(&msg)),
            Some(json!({ "title": "Import build" }))
        );
        // 不认识的提示（kill 进程/开窗口类）→ None（= 不选择，非错误）。
        let other = json!({
            "message": "Old Bloop version running. Kill it?",
            "actions": [{ "title": "Kill process" }, { "title": "Cancel" }]
        });
        assert_eq!(choose_show_message_request_action(Some(&other)), None);
        // 无 actions / 无 params。
        assert_eq!(choose_show_message_request_action(Some(&json!({}))), None);
        assert_eq!(choose_show_message_request_action(None), None);
        // ChooseBuildTool（构建工具名列表）刻意不应答（上游注释：替用户选构建
        // 工具是另一类猜测）。
        let choose = json!({
            "message": "Multiple build definitions found.",
            "actions": [{ "title": "sbt" }, { "title": "gradle" }]
        });
        assert_eq!(choose_show_message_request_action(Some(&choose)), None);
    }

    // ---- 初始化补丁 ----

    #[test]
    fn patches_declare_progress_options_and_build_root_folders() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("backend/build.sbt"), "");
        let mut base = lsp_core::init_params::base_initialize_params();
        #[allow(deprecated)]
        {
            base.root_uri = Some(
                lsp_types::Uri::from_str(&lsp_core::docsync::path_to_uri_str(dir.path()))
                    .expect("uri"),
            );
        }
        ScalaAdapter.initialize_patches(&mut base);
        // workDoneProgress 声明（不声明则 Metals 不上报进度，首查等待无从等待）。
        assert_eq!(
            base.capabilities
                .window
                .as_ref()
                .and_then(|w| w.work_done_progress),
            Some(true)
        );
        // initializationOptions 关键项（isExitOnShutdown 决定 shutdown 时 Metals 自退）。
        let opts = base.initialization_options.expect("options");
        assert_eq!(opts["isExitOnShutdown"], json!(true));
        assert_eq!(opts["compilerOptions"]["overrideDefFormat"], json!("ascli"));
        // workspaceFolders = build 根（backend），不再是仓库根本身。
        let folders = base.workspace_folders.expect("folders");
        assert_eq!(folders.len(), 1);
        assert!(
            folders[0].uri.as_str().ends_with("backend"),
            "{}",
            folders[0].uri.as_str()
        );
    }

    // ---- 首查等待（SERENA_REPLAY 手写 JSONL 驱动，不依赖外部进程）----

    static REPLAY_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn wait_returns_fast_when_metals_reports_no_work_and_latches() {
        let _env = REPLAY_ENV.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let lines = [
            r#"--> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"<-- {"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
        ];
        let replay = dir.path().join("replay.jsonl");
        std::fs::write(&replay, lines.join("\n") + "\n").unwrap();
        // SAFETY: REPLAY_ENV 保证本进程内独占访问 SERENA_REPLAY（lib.rs tests 同款）。
        unsafe { std::env::set_var("SERENA_REPLAY", &replay) };
        let session = lsp_core::session::Session::start(
            None,
            lsp_core::init_params::base_initialize_params(),
        )
        .await
        .expect("replay session Ready");
        unsafe { std::env::remove_var("SERENA_REPLAY") };

        // NO_WORK 路径：grace 内无任何 progress 活动 → 放行（不熬 timeout）。
        let started = std::time::Instant::now();
        wait_metals_via_trait(
            &session,
            Duration::from_secs(30),
            Duration::from_millis(150),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "NO_WORK 应在 grace 后立即放行，实际 {:?}",
            started.elapsed()
        );

        // latch：首查后第二查立即返回（上游 _has_waited_for_cross_file_references）。
        let started = std::time::Instant::now();
        wait_metals_via_trait(&session, Duration::from_secs(30), Duration::from_secs(15)).await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "latch 后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    /// 测试入口：复刻 trait 方法 latch + 注入短时长（真实路径走 trait 方法 + 上游
    /// 默认值常量）。
    async fn wait_metals_via_trait(
        session: &lsp_core::session::Session,
        timeout: Duration,
        grace: Duration,
    ) {
        if !session.take_cross_file_first_query() {
            return;
        }
        wait_metals_idle(session, timeout, grace, Duration::from_millis(20)).await;
    }
}
