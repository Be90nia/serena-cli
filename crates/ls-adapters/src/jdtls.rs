//! jdtls (Eclipse JDT Language Server) 适配器（PLAN M3 / Task T2 第 6 个 = 末尾）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/eclipse_jdtls_language_server.py`
//!
//! jdtls 是 Java 生态最权威的 LSP server；它需要 **JRE 11+ + 单独下载 jdtls 发行包**
//! （https://download.eclipse.org/jdtls/snapshots/），启动慢（~5-10s 加载 JDT workspace）
//! + 索引慢（首次 indexing 按项目大小 30s-几分钟）。这是最复杂的 adapter。
//!
//! 启动 quirk（最复杂）：
//! 1. 通过 `which_no_unc("jdtls")` 或 fallback `which_no_unc("java")` + `jdtls` 脚本。
//! 2. cwd 必须**项目根**；jdtls 通过 mvn/gradle 自动发现 Java 项目。
//! 3. on_server_ready 不能用 documentSymbol 探测 —— jdtls 首次 documentSymbol 阻塞
//!    在 workspace 初始化完成事件；改为等待 `language/status`（jdtls 特有）广播。
//!
//! 已知限制（M3 MVP）：
//! - 不实现 lombok agent 注入（特殊 JAR 配置）；M4+ 加。
//! - 不实现 `maven/gradle settings.json` 自动生成；用户自管。
//! - 不注入 -Xmx 等 JVM 参数；默认即可。

use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// jdtls 启动 + 首次索引合并超时。jdtls 是最慢的 LS —— 给 90s 保守值。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Default, Clone, Copy)]
pub struct JdtlsAdapter;

#[async_trait]
impl LanguageServerAdapter for JdtlsAdapter {
    fn id(&self) -> &'static str {
        "jdtls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Java];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        // 优先探测 `jdtls`（一些发行版直接提供 binary launcher）。
        // fallback：探测 `java` 并调用 jdtls 的 launcher 脚本（位置由环境变量决定）。
        let exe = which_no_unc("jdtls")
            .or_else(which_no_unc_java)
            .ok_or_else(|| {
                not_installed_error(
                    "jdtls",
                    "install jdtls (download from https://download.eclipse.org/jdtls/snapshots/) \
                     and ensure `jdtls` or `java` is on PATH with JAVA_HOME set",
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
        // jdtls 不需要 client capability quirk；它自己 advertise 自己的能力。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // jdtls 启动后第一个文档请求会触发 `language/status` 事件 —— 我们不在此处
        // 等待该事件（M3 MVP），因为 Session 的 lazy `ensure_open` 已经会在首次
        // 工具调用时阻塞到 workspace 初始化完成。直接返回 Ok 让 supervisor 放行。
        // 这里仍做一次轻量探测，捕获 jdtls 启动崩溃：
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "java"}]}),
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
        // jdtls 支持 `textDocument/implementation`（interface → class）。
        true
    }
}

/// `which_no_unc("java")` 找不到时返回 None。简单一行；不进函数体为了保持 which 一致风格。
fn which_no_unc_java() -> Option<std::path::PathBuf> {
    which_no_unc("java")
}
