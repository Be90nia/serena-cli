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

/// root 下找该语言首个**可作 UTF-8 文本解码**的真实源文件（filtered_walker：
/// gitignore + target/node_modules 等内置 ignore；限深 4 对齐
/// ls-adapters::find_language_source_file）。探针必须打真实源文件 —— 虚拟路径
/// 不触发项目索引（cold-start-hang 根因）。第二返回值 = 被跳过的解码失败文件
/// （相对 root、正斜杠）—— bd serena-rust-sgc0：二进制内容 .py 曾把整次 warm
/// 毒化成 INTERNAL；逐文件跳过继续，跳过清单交调用方出 warning。
fn find_entry_file(root: &Path, lang: &str) -> (Option<PathBuf>, Vec<String>) {
    let mut skipped = Vec::new();
    for entry in crate::fs_tools::filtered_walker(root)
        .max_depth(Some(4))
        .build()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_some_and(|t| t.is_file())
            || !entry_file_matches(entry.path(), lang)
        {
            continue;
        }
        match std::fs::read(entry.path()) {
            Ok(bytes) if std::str::from_utf8(&bytes).is_ok() => {
                return (Some(entry.into_path()), skipped);
            }
            _ => skipped.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/"),
            ),
        }
    }
    (None, skipped)
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
    let (entry, skipped) = find_entry_file(root, &lang);
    let entry = entry.ok_or_else(|| ToolError::BadArgs {
        detail: format!("no readable {lang} source files under {}", root.display()),
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

    let mut out = json!({
        "lang": lang,
        "project": root.display().to_string(),
        "ready": ready,
        "partial": !ready,
        "elapsed_ms": t0.elapsed().as_millis() as u64,
        "probe_file": entry
            .strip_prefix(root)
            .unwrap_or(&entry)
            .to_string_lossy(),
    });
    // bd serena-rust-sgc0：解码失败被跳过的文件显式带出——可见的降级，不静默。
    if !skipped.is_empty() {
        out["warnings"] = json!(skipped
            .iter()
            .map(|f| format!("{f}: not readable as UTF-8 text (binary content?); skipped"))
            .collect::<Vec<_>>());
    }
    Ok(out)
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
        let (entry, skipped) = find_entry_file(&rust_demo_root(), "rust");
        let entry = entry.expect("entry file");
        assert_eq!(entry.extension().unwrap(), "rs");
        assert!(skipped.is_empty(), "rust_demo 无二进制文件: {skipped:?}");
    }

    /// bd serena-rust-sgc0：二进制内容 .py 不毒化 warm —— 跳过 + 记入 skipped，
    /// 入口落到下一个可解码文件（纯 fs，不拉 LS）。
    #[test]
    fn find_entry_file_skips_binary_and_reports() {
        let dir = std::env::temp_dir().join(format!("warm_bin_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        // 4096B 非 UTF-8 字节（0xFF 开头必然解码失败），与票面复现形态一致。
        std::fs::write(dir.join("bin.py"), vec![0xFFu8; 4096]).expect("binary fixture");
        std::fs::write(dir.join("good.py"), "def add(a, b):\n    return a + b\n")
            .expect("text fixture");

        let (entry, skipped) = find_entry_file(&dir, "python");
        let entry = entry.expect("good.py must be picked");
        assert_eq!(entry.file_name().unwrap(), "good.py");
        assert_eq!(skipped, vec!["bin.py".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部候选解码失败 → (None, skipped 非空)，warm 侧转 BadArgs（不 INTERNAL）。
    #[test]
    fn find_entry_file_all_binary_yields_none_with_skipped() {
        let dir = std::env::temp_dir().join(format!("warm_bin_all_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        std::fs::write(dir.join("bin.py"), vec![0xFFu8; 64]).expect("binary fixture");

        let (entry, skipped) = find_entry_file(&dir, "python");
        assert!(entry.is_none());
        assert_eq!(skipped, vec!["bin.py".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
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
        if std::env::var_os("SERENA_SKIP_LS_E2E")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            // 真 LS fixture 测试：CI 门禁外（runner 语义就绪窗口不可控），真机/nightly 覆盖。
            return;
        }
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
        if std::env::var_os("SERENA_SKIP_LS_E2E")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            // 真 LS fixture 测试：CI 门禁外（runner 语义就绪窗口不可控），真机/nightly 覆盖。
            return;
        }
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
