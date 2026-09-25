//! M：`warm <lang>` 预热命令（ai-token-features-design §13-M / local/plan-m-warm.md）。
//!
//! 本质 = `session_for`（LS spawn + on_server_ready 根探针）→ `ensure_open` 入口源
//! 文件触发项目索引 → busy-retry documentSymbol 直到非空（K 特性实测：索引窗口内
//! 首调空、就绪后出数）。超时返 `partial: true` 不阻塞 —— 会话已留在 daemon，后台
//! 索引继续，后续真实调用吃到「已启动」的大部分收益。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ls_adapters::LanguageId;
use serde_json::json;

use crate::{Supervisor, ToolError};

/// busy-retry 探针间隔（对齐 ls-adapters::RETRY_PAUSE 节奏）。
const RETRY_PAUSE: Duration = Duration::from_millis(250);
/// 等待上限 1h：防 wire 侧超大 timeout_secs 让 `Instant + Duration` 溢出 panic。
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// 源文件与语言匹配：内置语言走 `LanguageId` 扩展名表（探针拒收 Cargo.toml 等
/// 工程标记 —— 对 rust workspace 探非源文件会让 RA 报 -32603）；T0/external 语言
/// 回落 servers.toml / external-servers.toml 声明的 `extensions`（与
/// `resolve_lang_for_file` 的「内置优先 → 配置兜底」方向对称）。
fn entry_file_matches(path: &Path, lang: &str) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    if let Some(want) = LanguageId::from_str_opt(lang)
        && let Some(got) = LanguageId::from_extension(&ext)
    {
        return got == want;
    }
    ls_registry::config::spec_for(lang)
        .map(|(_, s)| {
            s.extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
        })
        .unwrap_or(false)
}

/// root 下找该语言首个真实源文件（filtered_walker：gitignore + target/node_modules
/// 等内置 ignore；限深 4 对齐 ls-adapters::find_language_source_file）。探针必须打
/// 真实源文件 —— 虚拟路径不触发项目索引（cold-start-hang 根因）。
fn find_entry_file(root: &Path, lang: &str) -> Option<PathBuf> {
    crate::fs_tools::filtered_walker(root)
        .max_depth(Some(4))
        .build()
        .filter_map(Result::ok)
        .find(|e| e.file_type().is_some_and(|t| t.is_file()) && entry_file_matches(e.path(), lang))
        .map(|e| e.into_path())
}

/// warm 主入口（execute_tool `"warm"` 分支调用）。
pub(crate) async fn warm(
    sup: &Supervisor,
    root: &Path,
    lang: &str,
    timeout: Duration,
) -> Result<serde_json::Value, ToolError> {
    let t0 = Instant::now();
    let lang = lang.to_ascii_lowercase();
    let timeout = timeout.min(MAX_WAIT);

    // 1) 入口文件先探：无源文件 = warm 无意义，且防对错误 lang 白白 spawn 一个 LS。
    //    BadArgs 先于 session_for —— 确定性用法错不烧启动成本。
    let entry = find_entry_file(root, &lang).ok_or_else(|| ToolError::BadArgs {
        detail: format!("no {lang} source files under {}", root.display()),
    })?;

    // 2) 触发 LS 启动（T2 含 on_server_ready 根探针；未装走 NotInstalled 原样上抛）。
    let session = sup.session_for(root, &lang).await?;

    // 3) didOpen 入口文件 → 触发该项目索引（探针必须用真实文件，见 cold-start-hang）。
    session.ensure_open(&entry).await.map_err(ToolError::Core)?;

    // 4) busy-retry 真语义探针：documentSymbol 非空 = 索引就绪。
    //    直接打 session 裸请求不走 tool_overview —— 探针不参与符号缓存读写路径，
    //    就绪窗口的空结果不影响首调缓存语义。
    let uri = crate::path_to_uri_str(&entry);
    let params = json!({ "textDocument": { "uri": uri } });
    let deadline = t0 + timeout;
    let mut ready = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match session
            .request::<serde_json::Value>("textDocument/documentSymbol", params.clone(), remaining)
            .await
        {
            Ok(v) if v.as_array().is_some_and(|a| !a.is_empty()) => {
                ready = true;
                break;
            }
            _ => tokio::time::sleep(RETRY_PAUSE).await,
        }
    }

    Ok(json!({
        "lang": lang,
        "project": root.display().to_string(),
        "ready": ready,
        "partial": !ready,
        "elapsed_ms": t0.elapsed().as_millis() as u64,
        "probe_file": entry
            .strip_prefix(root)
            .unwrap_or(&entry)
            .to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    //! 真实就绪判定需 rust-analyzer + fixtures/rust_demo（项目惯例，同 repo_map tests）。
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    fn rust_demo_root() -> PathBuf {
        workspace_root().join("fixtures/rust_demo")
    }

    fn rust_analyzer_available() -> bool {
        let exe = if cfg!(windows) {
            "rust-analyzer.exe"
        } else {
            "rust-analyzer"
        };
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                if dir.join(exe).is_file() {
                    return true;
                }
            }
        }
        false
    }

    /// 扩展名匹配：LanguageId 表命中 / 大小写不敏感 / javascript 别名归一 /
    /// 工程标记拒绝（探针必须是源文件）。
    #[test]
    fn entry_file_matches_by_language_id_table() {
        assert!(entry_file_matches(Path::new("a.rs"), "rust"));
        assert!(entry_file_matches(Path::new("b.JS"), "typescript"));
        assert!(entry_file_matches(Path::new("c.jsx"), "javascript"));
        assert!(!entry_file_matches(Path::new("a.rs"), "python"));
        assert!(!entry_file_matches(Path::new("Cargo.toml"), "rust"));
    }

    /// rust_demo 能找到 .rs 入口（纯 fs，不拉 LS）。
    #[test]
    fn find_entry_file_finds_rust_source() {
        let entry = find_entry_file(&rust_demo_root(), "rust").expect("entry file");
        assert_eq!(entry.extension().unwrap(), "rs");
    }

    /// 无该语言源文件 → BadArgs，且先于 session_for（不 spawn LS）。
    #[tokio::test]
    async fn warm_rejects_lang_without_source_files() {
        let sup = crate::Supervisor::direct().await.expect("supervisor");
        let root = workspace_root().join("fixtures/typescript_demo");
        if !root.exists() {
            eprintln!("skipped: fixtures/typescript_demo missing");
            return;
        }
        let err = warm(&sup, &root, "rust", Duration::from_secs(1))
            .await
            .expect_err("must reject");
        assert!(matches!(err, ToolError::BadArgs { .. }), "got: {err:?}");
    }

    /// 零超时：不进等待环 → ready:false + partial:true（LS 已启动的降级路径）。
    #[tokio::test]
    async fn warm_partial_on_zero_timeout() {
        if !rust_analyzer_available() {
            eprintln!("skipped: rust-analyzer not on PATH");
            return;
        }
        let sup = crate::Supervisor::direct().await.expect("supervisor");
        let v = warm(&sup, &rust_demo_root(), "rust", Duration::ZERO)
            .await
            .expect("warm");
        assert_eq!(v["ready"], json!(false));
        assert_eq!(v["partial"], json!(true));
        assert_eq!(v["lang"], "rust");
        assert!(v["probe_file"].as_str().unwrap().ends_with(".rs"));
    }

    /// §13-M 核心验收：warm ready:true 后，首个 overview 立即有数据（热路径）。
    #[tokio::test]
    async fn warm_ready_then_first_overview_nonempty() {
        if !rust_analyzer_available() {
            eprintln!("skipped: rust-analyzer not on PATH");
            return;
        }
        let root = rust_demo_root();
        let sup = crate::Supervisor::direct().await.expect("supervisor");
        let v = warm(&sup, &root, "rust", Duration::from_secs(120))
            .await
            .expect("warm");
        assert_eq!(v["ready"], json!(true), "warm report: {v}");
        let probe = v["probe_file"].as_str().expect("probe_file").to_string();
        let hits = sup
            .tool_overview(&root, &probe, Some("rust"))
            .await
            .expect("overview");
        assert!(
            !hits.is_empty(),
            "post-warm first overview must be non-empty"
        );
    }
}
