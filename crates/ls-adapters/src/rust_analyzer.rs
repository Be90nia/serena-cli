//! rust-analyzer 适配器（PLAN M3 / Task T2 第 1 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/rust_analyzer.py`
//!
//! rust-analyzer 是 LSP 实现（不基于另一 LSP），本身启动快 + 索引靠项目 root 的
//! `Cargo.toml`/`rust-project.json` 自动发现；无 quirk 需要额外补。`cargo` 不必前置，
//! 因为 rust-analyzer 不调 cargo —— 它读 `target/` 索引但懒加载。
//!
//! 已知限制：
//! - 不处理 rust-project.json 显式模式（非 cargo 项目）；M3+ 用户少，不预抽。
//! - 不实现 `rust-analyzer --help`/query-db 等 admin 接口。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, ProjectRootSlot, RequestHooks,
    not_installed_error, which_no_unc,
};

/// 30s 探活上限。rust-analyzer 启动 <1s，但首次 `textDocument/documentSymbol` 触发
/// 索引加载时可能慢；保守给 30s。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__rust_analyzer_ready_probe__";

/// RA Indexing progress token（RA `main_loop.rs` 硬编码 `"rustAnalyzer/cachePriming"`，
/// title="Indexing"，0.3.x→0.5.x 五年未变）。bd 0vj1 B 修：`$/progress` 该 token 的
/// begin→end 完成 = prime caches 收敛 = hover/def/goto 走的路径真就绪。
const INDEXING_TOKEN: &str = "rustAnalyzer/cachePriming";

/// 当前会话项目 root 表（per-project 键化，bd serena-rust-4y6）。adapter 是零字段
/// 单例（`Copy`）存不了实例状态 —— 会话级数据放静态槽，由 supervisor::session_for
/// 在 `on_server_ready` 前经 `set_project_root` 写入（读侧无键，走 get_last 相邻语义）。
static PROBE_ROOT: ProjectRootSlot = ProjectRootSlot::new();

#[derive(Debug, Default, Clone, Copy)]
pub struct RustAnalyzerAdapter;

#[async_trait]
impl LanguageServerAdapter for RustAnalyzerAdapter {
    fn id(&self) -> &'static str {
        "rust-analyzer"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Rust];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = Self::locate_rust_analyzer()
            .await
            .ok_or_else(|| {
                not_installed_error(
                    "rust-analyzer",
                    "install rust-analyzer (`rustup component add rust-analyzer` or https://rust-analyzer.github.io) and ensure it is on PATH",
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
        // rust-analyzer 不需要 quirk patches；base init_params 默认声明已足够。
        // 它的 semanticTokensProvider / inlayHintsProvider 是 server-side capabilities，
        // client capability 留默认即可。
    }

    fn set_project_root(&self, root: &Path) {
        PROBE_ROOT.set(root);
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // bd 0vj1 B 修：等 RA Indexing progress（token cachePriming）end。documentSymbol
        // 走 Salsa 不依赖 prime caches —— 旧探针返 Ok ≠ hover/def/goto 真就绪，30s 超时
        // 放行后语义层永不收敛。
        // 兜底链：(a) Indexing end = 真就绪；(b/c) token 缺失（cachePriming 关 / 老 RA /
        // capability 未生效）、RA 进程死亡或预算耗尽 → 退回旧 documentSymbol 探针 1 次
        // （失败也 Ok，保持放行契约，不阻塞 ls_registry 启动链）。
        let indexed = Self::wait_indexing(session, Self::indexing_wait_for_root()).await;
        if indexed {
            return Ok(());
        }
        // A 兜底：root 下真实文件 documentSymbol 探针（虚拟 URI 不触发 rust-analyzer
        // 的 workspace lazy-load，首个真实工具请求就得独自承担全量索引 —— cold-start
        // 87s 根因，见 local/cold-start-hang-diagnosis.md）。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": self.probe_uri() } }),
                READY_PROBE_TIMEOUT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    /// bd serena-rust-62z：外层包裹预算必须 ≥ 内部最坏路径（Indexing 等待 T + 兜底
    /// 探针 30s），5s 余量保证内部先超时 —— 否则长预算被外层默认 30s 截断（jdtls 同款）。
    fn ready_probe_budget(&self) -> Duration {
        Self::indexing_wait_for_root() + READY_PROBE_TIMEOUT + Duration::from_secs(5)
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // rust-analyzer 支持 `textDocument/implementation`（trait → impl 跳转）。
        true
    }
}

impl RustAnalyzerAdapter {
    /// Indexing 等待预算：root 的 Cargo.lock 包数分档（0vj1 契约）。包数含全部
    /// 依赖（含 dev/build），是 RA 全量索引规模的廉价代理 —— 偏大取档更安全。
    fn indexing_wait_for_root() -> Duration {
        indexing_wait(PROBE_ROOT.get_last().and_then(|root| cargo_lock_crates(&root)))
    }

    /// Indexing begin→end 等待主体。切片等待（200ms）间轮询会话态：RA 进程死亡
    /// （OOM / 被杀 → stdout EOF → Failed）时 `$/progress` end 永不到达，白等满档
    /// 预算毫无意义 —— 立即退出走兜底，supervisor 对 Failed 会话自愈换新
    /// （调研 rust-analyzer-progress-protocol.md Q4 #3「探活」边界）。
    async fn wait_indexing(session: &lsp_core::session::Session, budget: Duration) -> bool {
        const ALIVE_POLL: Duration = Duration::from_millis(200);
        let deadline = std::time::Instant::now() + budget;
        // 段 1：等 Indexing begin（早到通知在 resolved，切片重复调用幂等消费）。
        loop {
            let Some(slice) = Self::alive_slice(session, deadline, ALIVE_POLL) else {
                return false;
            };
            if session.wait_for_progress(INDEXING_TOKEN, slice).await.is_ok() {
                break;
            }
        }
        // 段 2：等在飞 progress 清空（Indexing end = 真就绪）。
        loop {
            let Some(slice) = Self::alive_slice(session, deadline, ALIVE_POLL) else {
                return false;
            };
            if session.wait_indexing_drain(slice).await {
                return true;
            }
        }
    }

    /// 下一个等待切片；预算耗尽或会话已 Failed → None（等待终止）。
    fn alive_slice(
        session: &lsp_core::session::Session,
        deadline: std::time::Instant,
        poll: Duration,
    ) -> Option<Duration> {
        if matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        Some(poll.min(remaining))
    }

    /// `on_server_ready` 将发出的探针 URI：root 下真实小文件的 file URI；root 未设置
    /// 或无候选文件时退虚拟 URI。
    fn probe_uri(&self) -> String {
        let root = PROBE_ROOT.get_last();
        match root {
            Some(root) => crate::probe_uri_for_root(&root, self.languages(), PROBE_FALLBACK),
            None => PROBE_FALLBACK.to_string(),
        }
    }

    /// ↖ mirror: rust_analyzer.py@43ae021 `_ensure_rust_analyzer_installed`
    /// 查找链：`rustup which`（版本匹配 toolchain，上游首选）→ PATH（--version 功能
    /// 校验，防 rustup proxy 断链）→ `~/.cargo/bin` 兜底。
    /// Δ 上游：不做 `rustup component add` 自动装（网络 + 写操作，CLI 侧副作用大，
    /// 失败路径报 not_installed 带指引即可）。
    async fn locate_rust_analyzer() -> Option<PathBuf> {
        // 1. rustup which：优先 —— 保证与项目 toolchain 版本一致
        if let Some(rustup) = which_no_unc("rustup") {
            let out = tokio::process::Command::new(rustup)
                .args(["which", "rust-analyzer"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .creation_flags_safe()
                .output()
                .await;
            if let Ok(out) = out
                && out.status.success()
            {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if !s.is_empty() {
                    let p = PathBuf::from(s);
                    if p.is_file() {
                        return Some(dunce::canonicalize(&p).unwrap_or(p));
                    }
                }
            }
        }
        // 2. PATH —— rustup 生态下 PATH 里的可能是 rustup proxy，组件没装时是坏的；
        //    用 --version 校验功能（2s 超时护栏，慢机器不拖启动）。
        if let Some(p) = which_no_unc("rust-analyzer")
            && Self::binary_functional(&p).await
        {
            return Some(p);
        }
        // 3. ~/.cargo/bin 兜底（cargo install / rustup 标准位置）
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            for name in ["rust-analyzer.exe", "rust-analyzer"] {
                let p = PathBuf::from(&home).join(".cargo/bin").join(name);
                if p.is_file() && Self::binary_functional(&p).await {
                    return Some(dunce::canonicalize(&p).unwrap_or(p));
                }
            }
        }
        None
    }

    /// `--version` 能正常退出即认为功能可用（2s 超时护栏）。
    async fn binary_functional(path: &Path) -> bool {
        let fut = tokio::process::Command::new(path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags_safe()
            .status();
        matches!(
            tokio::time::timeout(Duration::from_secs(2), fut).await,
            Ok(Ok(status)) if status.success()
        )
    }
}

/// Indexing 等待分档（0vj1 契约）：<500 crates → 60s，<2000 → 120s，≥2000 → 300s；
/// 拿不到包数（无 Cargo.lock，rust-project.json 形态）→ 120s 默认。
fn indexing_wait(crates: Option<usize>) -> Duration {
    match crates {
        Some(n) if n < 500 => Duration::from_secs(60),
        Some(n) if n < 2000 => Duration::from_secs(120),
        Some(_) => Duration::from_secs(300),
        None => Duration::from_secs(120),
    }
}

/// Cargo.lock `[[package]]` 段计数 —— 无子进程、无网络的规模估计。文件缺失/不可读
/// → None（回默认档）。
fn cargo_lock_crates(root: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(root.join("Cargo.lock")).ok()?;
    Some(text.lines().filter(|l| l.starts_with("[[package]]")).count())
}

/// Windows 隐藏子进程窗口（CREATE_NO_WINDOW）；非 Windows 无操作。
trait CreationFlagsSafe {
    fn creation_flags_safe(&mut self) -> &mut Self;
}

#[cfg(windows)]
impl CreationFlagsSafe for tokio::process::Command {
    fn creation_flags_safe(&mut self) -> &mut Self {
        // 与 ls-runtime process.rs 同款语义（CREATE_NO_WINDOW）。tokio Command 在
        // Windows 有固有 creation_flags 方法，无需 std CommandExt。
        self.creation_flags(0x0800_0000)
    }
}

#[cfg(not(windows))]
impl CreationFlagsSafe for tokio::process::Command {
    fn creation_flags_safe(&mut self) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = RustAnalyzerAdapter;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(uri.ends_with(".gitignore"), "应指向真实文件: {uri}");

        let empty = tempfile::tempdir().unwrap();
        adapter.set_project_root(empty.path());
        assert_eq!(adapter.probe_uri(), PROBE_FALLBACK);
    }

    /// 查找链：本机 rust-analyzer 应能定位（rustup which 或 PATH 之一命中）且功能可用。
    #[tokio::test]
    async fn locate_finds_functional_rust_analyzer() {
        if which_no_unc("rustup").is_none() && which_no_unc("rust-analyzer").is_none() {
            // 无 rust 生态的 CI 跳过（fixture 门，非逻辑断言）。
            return;
        }
        let p = RustAnalyzerAdapter::locate_rust_analyzer()
            .await
            .expect("rust 生态存在时应能定位 rust-analyzer");
        assert!(p.is_file(), "定位结果必须是存在的文件: {p:?}");
        assert!(
            RustAnalyzerAdapter::binary_functional(&p).await,
            "定位结果必须通过 --version 功能校验"
        );
    }

    /// binary_functional 对垃圾路径必须返回 false（2s 内失败，不误报可用）。
    #[tokio::test]
    async fn binary_functional_rejects_garbage_path() {
        let bogus = std::env::temp_dir().join("__definitely_not_rust_analyzer__.exe");
        assert!(!RustAnalyzerAdapter::binary_functional(&bogus).await);
    }

    /// Indexing 等待分档边界（0vj1 契约：None→120s，<500→60s，<2000→120s，≥2000→300s）。
    #[test]
    fn indexing_wait_tiers_by_crate_count() {
        assert_eq!(indexing_wait(None), Duration::from_secs(120));
        assert_eq!(indexing_wait(Some(0)), Duration::from_secs(60));
        assert_eq!(indexing_wait(Some(499)), Duration::from_secs(60));
        assert_eq!(indexing_wait(Some(500)), Duration::from_secs(120));
        assert_eq!(indexing_wait(Some(1999)), Duration::from_secs(120));
        assert_eq!(indexing_wait(Some(2000)), Duration::from_secs(300));
    }

    /// Cargo.lock `[[package]]` 计数（含依赖），缺失文件 → None。
    #[test]
    fn cargo_lock_crates_counts_packages_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            "# generated by cargo\nversion = 4\n\n[[package]]\nname = \"a\"\n\n[[package]]\nname = \"b\"\n",
        )
        .unwrap();
        assert_eq!(cargo_lock_crates(dir.path()), Some(2));
        let missing = dir.path().join("nope");
        assert_eq!(cargo_lock_crates(&missing), None);
    }

    /// 62z 契约：外层 ready_probe_budget 必须覆盖内部最坏路径（Indexing 等待 T +
    /// 兜底探针），否则长预算被外层默认 30s 截断 —— 慢索引永远走不完自己的等待。
    #[test]
    fn ready_probe_budget_covers_indexing_wait_and_probe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "[[package]]\nname = \"a\"\n").unwrap();
        let adapter = RustAnalyzerAdapter;
        adapter.set_project_root(dir.path());
        let t = indexing_wait(cargo_lock_crates(dir.path()));
        assert_eq!(t, Duration::from_secs(60));
        assert!(
            adapter.ready_probe_budget() > t + READY_PROBE_TIMEOUT,
            "外层预算必须严格大于内部最坏路径"
        );
    }
}
