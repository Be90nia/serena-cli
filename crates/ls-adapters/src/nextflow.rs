//! Nextflow 官方 LS（fat JAR `java -jar`）适配器（上游对拍采纳 W3 批）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/nextflow_language_server.py`
//!
//! ## 要点
//!
//! - **无配置变更不扫描**：LS 在 `initialized` 后不索引任何东西，只在新配置异于其
//!   默认值时才扫描工作区（"Without this notification, every symbol request answers
//!   with an empty result"）。推送通道 = servers.toml `did_change_config`（W1 T0 五通道），
//!   supervisor 在 initialized 后对 T0/T2 一体推送——本适配器不重复发。
//! - **$/progress 扫描等待**（180s 上限，↖ mirror `_WORKSPACE_SCAN_TIMEOUT`）：扫描是
//!   异步的，token 未清空前符号请求看到空 AST 缓存。等待挂在 `on_session_ready`
//!   （Δ 上游在 `_start_server` 尾部阻塞等满 180s；supervisor 对 ready 钩子有 30s
//!   包裹超时，超时 warn 放行=上游超时分支语义；references 路径不受此限——flush
//!   显式同步，见下）。
//! - **references 前 flush**（↖ mirror `_send_references_request` / `_flush_deferred_
//!   workspace_scan`）：扫描被 LS 推迟到会话首个请求之后（1s debounce），references 是
//!   唯一不等它的请求。`completion` 触发 `updateNow` 同步跑更新，两连发强制扫描
//!   （第一发排空 pending change、第二发执行扫描）；`documentSymbol` 先行同步阻塞
//!   到本文件更新落地。上游 2×completion 有一次性 latch，我们逐次重发（成本 = 3 个
//!   轻请求；Δ 换取 LS 重启后语义不变、无进程级状态）。上游
//!   `_get_wait_time_for_cross_file_referencing` 返 0 正因 flush 显式同步。
//! - **符号名前缀剥离**（↖ mirror `_normalize_symbol_name` / `_SYMBOL_NAME_PREFIXES`）：
//!   LS 给符号名带声明关键字前缀（"process GREET"）。[`strip_symbol_prefix`] 按上游
//!   语义还原源码书写名；per-LS 符号归一挂点为后续批次（本单非目标），函数先行落地。
//! - **安装面**：JAR 缓存 = `serena-cli install nextflow`（servers.toml download 条目
//!   钉 26.04.3 + sha）；java 解析 JAVA_HOME → PATH（↖ mirror `_resolve_java`，
//!   Δ 未抄 JDK ≥17 版本审讯——环境预检归 doctor 层，bsl GAP 同款裁决）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;
use serde_json::json;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, declare_work_done_progress,
    not_installed_error, which_no_unc,
};

/// npm/download 缓存目录 pin（= servers.toml [servers.nextflow] download 段，禁随意改）。
const CACHE_ID: &str = "nextflow";
const CACHE_VERSION: &str = "26.04.3";
const JAR_NAME: &str = "language-server-all.jar";

/// 初始工作区扫描（编译全部 .nf 文件）超时上限。
/// ↖ mirror: `_WORKSPACE_SCAN_TIMEOUT`（180.0s）
const WORKSPACE_SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// 单请求超时（flush 用；flush 是 best-effort，失败 debug 后放行——上游 log.debug
/// 同款 permissive 语义，references 请求自身超时兜底）。
const FLUSH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// LS 给符号名带的声明关键字前缀。
/// ↖ mirror: `_SYMBOL_NAME_PREFIXES`（ScriptSymbolProvider.getSymbolName）
pub const SYMBOL_NAME_PREFIXES: &[&str] = &["process", "workflow", "function", "record", "enum"];

/// 剥离符号名的声明关键字前缀（"process GREET" → "GREET"），使符号可按源码书写名
/// 寻址。隐式入口 workflow 保持 "<entry>" 占位名（LS 就这么叫它）。
///
/// ↖ mirror: `_normalize_symbol_name`。per-LS 符号归一挂点为后续批次，本函数先以
/// 纯函数形态落地（单测锁行为）。
pub fn strip_symbol_prefix(name: &str) -> &str {
    match name.split_once(' ') {
        Some((prefix, rest)) if SYMBOL_NAME_PREFIXES.contains(&prefix) => rest,
        _ => name,
    }
}

/// 缓存产物：`{cache}/nextflow/26.04.3/language-server-all.jar`（raw 下载即 bin_path
/// 同名落盘，install.rs DownloadInstaller 同源）。
fn cached_jar() -> PathBuf {
    default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join(JAR_NAME)
}

/// java 解析：JAVA_HOME/bin/java → PATH（↖ mirror `_resolve_java` 前两级；
/// Δ java_home 自定义设置项无通道，环境预检归 doctor）。
fn resolve_java() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        let exe =
            PathBuf::from(home)
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" });
        if exe.is_file() {
            return Some(exe);
        }
    }
    which_no_unc("java")
}

/// exec 装配（纯函数，测试锁形状）：`java -jar <jar>`。
fn launch_cmd(java: PathBuf, jar: PathBuf) -> Vec<OsString> {
    vec![java.into_os_string(), "-jar".into(), jar.into_os_string()]
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NextflowAdapter;

#[async_trait]
impl LanguageServerAdapter for NextflowAdapter {
    fn id(&self) -> &'static str {
        "nextflow"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Nextflow];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let jar = cached_jar();
        if !jar.is_file() {
            return Err(not_installed_error(
                "nextflow",
                "run `serena-cli install nextflow` (GitHub nextflow-io/language-server \
                 v26.04.3 fat JAR; JDK 17+ required)",
            ));
        }
        let Some(java) = resolve_java() else {
            return Err(not_installed_error(
                "nextflow",
                "no java executable found (JAVA_HOME or PATH); JDK 17+ required to run the \
                 Nextflow language server JAR",
            ));
        };
        Ok(LaunchInfo {
            cmd: launch_cmd(java, jar),
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // $/progress 上报门：扫描进度只在客户端声明 workDoneProgress 后才上报
        // （↖ mirror `_create_base_initialize_params` 的 window 声明）。
        declare_work_done_progress(base);
    }

    fn set_project_root(&self, _root: &Path) {}

    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        // did_change_config 推送已由 supervisor（T0 通道，initialized 后、本钩子前）
        // 发出——扫描由它触发，此处等扫描完成（begin → drain，总时长 180s 上限）。
        // 无 progress 活动（老版本 LS / 推送未触发）= 上游 scan_complete.wait 超时
        // 分支：耗满宽限后放行。
        wait_workspace_scan(session).await;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    async fn pre_references(&self, session: &lsp_core::session::Session, file: &Path) {
        flush_deferred_workspace_scan(session, file).await;
    }
}

/// 等初始工作区扫描完成：等 progress 开始 → drain 到清空，总时长 180s 封顶。
/// ↖ mirror: `_start_server` 尾部 `_scan_complete.wait(timeout=180)`（active 集空即
/// set）；Δ supervisor 对 on_session_ready 的 30s 包裹会提前截断——大工作区残余等待
/// 由 references flush 兜底（显式同步，不依赖本等待）。
async fn wait_workspace_scan(session: &lsp_core::session::Session) {
    let started = tokio::time::Instant::now();
    // 扫描由配置推送触发、必然开始；不开始 = 触发失效，耗满上限放行（上游
    // wait(timeout) 同语义——事件不来就等满超时）。
    let _ = session
        .wait_indexing_start_or_completion(WORKSPACE_SCAN_TIMEOUT, WORKSPACE_SCAN_TIMEOUT)
        .await;
    // drain 用满剩余时长（start_or_completion 内部 drain 上限是完整 timeout 参数，
    // 此处再封一次顶保证总时长 ≤ 180s）。
    let _ = session
        .wait_indexing_drain(WORKSPACE_SCAN_TIMEOUT.saturating_sub(started.elapsed()))
        .await;
}

/// references 前强制扫描 + 同步（↖ mirror `_flush_deferred_workspace_scan` +
/// `_send_references_request` 前半）。两连发 `completion`（第一发排空 pending
/// change，第二发触发扫描）+ `documentSymbol`（阻塞到本文件更新落地）。失败
/// debug 记录后继续（上游 log.debug 同款；请求自身超时兜底）。
async fn flush_deferred_workspace_scan(session: &lsp_core::session::Session, file: &Path) {
    let uri = lsp_core::docsync::path_to_uri_str(file);
    let completion = json!({
        "textDocument": { "uri": uri },
        "position": { "line": 0, "character": 0 },
    });
    for _ in 0..2 {
        if let Err(e) = session
            .request::<serde_json::Value>(
                "textDocument/completion",
                completion.clone(),
                FLUSH_REQUEST_TIMEOUT,
            )
            .await
        {
            tracing::debug!("nextflow flush completion failed: {e}");
        }
    }
    let symbol = json!({ "textDocument": { "uri": lsp_core::docsync::path_to_uri_str(file) } });
    if let Err(e) = session
        .request::<serde_json::Value>("textDocument/documentSymbol", symbol, FLUSH_REQUEST_TIMEOUT)
        .await
    {
        tracing::debug!("nextflow flush documentSymbol failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_symbol_prefix_matches_upstream() {
        // ↖ mirror `_normalize_symbol_name`：五关键字前缀剥离。
        for (raw, want) in [
            ("process GREET", "GREET"),
            ("workflow SAY_HELLO", "SAY_HELLO"),
            ("function foo", "foo"),
            ("record Sample", "Sample"),
            ("enum Params", "Params"),
        ] {
            assert_eq!(strip_symbol_prefix(raw), want, "{raw}");
        }
        // 无前缀 / 非声明首词 / 空串不动。
        assert_eq!(strip_symbol_prefix("<entry>"), "<entry>");
        assert_eq!(strip_symbol_prefix("my process X"), "my process X");
        assert_eq!(strip_symbol_prefix("plain"), "plain");
        assert_eq!(strip_symbol_prefix(""), "");
        // 剥离恰好一层（首词是关键字、其余是名字的一部分）。
        assert_eq!(strip_symbol_prefix("process workflow"), "workflow");
    }

    #[test]
    fn launch_cmd_shape_is_java_jar() {
        let cmd = launch_cmd(
            PathBuf::from("java"),
            PathBuf::from("x/language-server-all.jar"),
        );
        assert_eq!(cmd[0], "java");
        assert_eq!(cmd[1], "-jar");
        assert!(cmd[2].to_string_lossy().ends_with(JAR_NAME));
    }

    #[test]
    fn nextflow_declares_language_closure() {
        assert_eq!(NextflowAdapter.languages(), &[LanguageId::Nextflow]);
        assert_eq!(
            LanguageId::from_str_opt("nextflow"),
            Some(LanguageId::Nextflow)
        );
        assert_eq!(LanguageId::from_extension("nf"), Some(LanguageId::Nextflow));
    }
}
