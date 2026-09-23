//! E: repo-map 全库符号地图（ai-token-features-design §10-E）。
//!
//! Aider PageRank 在 LSP 之上：1KB JSON 给 AI 全库 API 面 — top N 符号按调用
//! 热度排序，省 AI 反复 find-symbol。
//!
//! 算法简化版（plan-e-repo-map.md §1）：
//! 1) `tool_symbol_tree` 拉所有文件 symbols（Phase 3.1 缓存命中免 LS 往返）。
//! 2) 对候选（top_n*2 上限）的每个 symbol 走 `tool_find_symbol` 拿 def range →
//!    `tool_referencing_symbols` 数直接引用。
//! 3) sort by direct_refs desc, truncate top_n。
//!
//! ponytail: 不引入 page-rank 库 — 直方 + 同文件热度足够指示 API 中心度；
//! 万级文件再换库。

use serde::Serialize;
use std::path::Path;

use crate::Supervisor;

/// repo-map 单条：单个符号的 API 中心度画像。
#[derive(Debug, Serialize)]
pub struct RepoMapEntry {
    pub name: String,
    pub container: Option<String>,
    pub file: String,
    pub kind: String,
    pub weight: f64,
    pub direct_refs: usize,
}

/// repo-map 工具响应。
#[derive(Debug, Serialize)]
pub struct RepoMapReport {
    pub total_symbols: usize,
    pub top: Vec<RepoMapEntry>,
    pub budget_bytes: usize,
}

/// 候选扫描上限（防止 RA/clangd 大项目炸内存 / 拉太久）。
const CANDIDATE_SCAN_LIMIT: usize = 5000;

/// repo-map 主入口。`lang = None` → 走 multi-lang 自动探测。
pub async fn build(
    sup: &Supervisor,
    root: &Path,
    lang: Option<&str>,
    top_n: usize,
) -> RepoMapReport {
    // 1) 拿全 workspace 符号树（Phase 3.1 缓存兜底 + 5k 文件保险丝）。
    let tree = match sup
        .tool_symbol_tree(root, ".", lang, CANDIDATE_SCAN_LIMIT)
        .await
    {
        Ok(v) => v,
        Err(_) => {
            return RepoMapReport {
                total_symbols: 0,
                top: vec![],
                budget_bytes: 0,
            };
        }
    };

    // 2) 拉平 tree.entries[].symbols[] → (file, name, container, kind)。
    // 预算 = top_n*2：超过即停止 refs 查询，控制 LS 往返。
    let candidate_cap = top_n.saturating_mul(2);
    let entries_val = tree.get("entries").and_then(|v| v.as_array());
    let mut all_syms: Vec<(String, String, Option<String>, String)> = Vec::new();
    if let Some(entries) = entries_val {
        for entry in entries {
            let Some(file) = entry.get("file").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(syms) = entry.get("symbols").and_then(|v| v.as_array()) else {
                continue;
            };
            for s in syms {
                let name = s
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let container = s
                    .get("container")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
                all_syms.push((file.to_string(), name, container, kind));
                if all_syms.len() >= candidate_cap {
                    break;
                }
            }
            if all_syms.len() >= candidate_cap {
                break;
            }
        }
    }
    let total = all_syms.len();

    // 3) 对每个候选查 refs 数（走 find_symbol 拿 def range → referencing_symbols）。
    // find_symbol 缓存 + symbol-tree 都吃 Phase 3.1 缓存层，二次调用零成本。
    let mut ranked: Vec<RepoMapEntry> = Vec::with_capacity(all_syms.len());
    for (file, name, container, kind) in all_syms {
        let direct_refs = count_refs(sup, root, &file, &name, lang).await;
        ranked.push(RepoMapEntry {
            name,
            container,
            file,
            kind,
            weight: direct_refs as f64,
            direct_refs,
        });
    }

    // 4) 排序 + 截断。
    ranked.sort_by_key(|e| std::cmp::Reverse(e.direct_refs));
    ranked.truncate(top_n);

    let budget_bytes = serde_json::to_vec(&ranked).map(|v| v.len()).unwrap_or(0);
    RepoMapReport {
        total_symbols: total,
        top: ranked,
        budget_bytes,
    }
}

/// 走 `find_symbol(query=name)` 拿候选 SymbolHit，再用 `range.start` 作为
/// `tool_referencing_symbols` 的 line/col 锚点。返回直接引用计数。
///
/// 失败 = 0（静默跳过：单符号 refs 查询失败不影响整体排序；这条契约与
/// search 工具的「失败 → 字段缺失而非整工具失败」一致）。
async fn count_refs(
    sup: &Supervisor,
    root: &Path,
    file: &str,
    name: &str,
    lang: Option<&str>,
) -> usize {
    let hits = match sup.tool_find_symbol(root, name, 50, lang).await {
        // warnings（失败 lang）在 repo_map 计数场景无挂载点，忽略 —— 计数尽力而为。
        Ok((h, _)) => h,
        Err(_) => return 0,
    };
    // 在 hits 里挑 file 路径一致的第一个（uri → file_path 同款 URI 解码逻辑
    // 这里走 file 名 suffix 匹配：symbol-tree 给的是相对 path，find_symbol
    // 给的是绝对 uri；落宽松匹配 — 同名同 basename 即视为同一符号）。
    let target_basename = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let hit = hits.iter().find(|h| {
        let hit_basename = h.uri.rsplit(['/', '\\']).next().unwrap_or(&h.uri);
        hit_basename == target_basename
    });
    let Some(hit) = hit else { return 0 };
    let line = hit.range.start.line;
    let col = hit.range.start.character;
    match sup
        .tool_referencing_symbols(root, file, line, col, lang)
        .await
    {
        Ok(refs) => refs.len(),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    //! 真实集成验证需 rust-analyzer + fixtures/rust_demo（项目惯例）。
    //! 见 plan-e-repo-map.md §Task 4。

    use super::*;

    fn rust_demo_root() -> std::path::PathBuf {
        let crate_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        crate_root
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("fixtures/rust_demo")
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

    /// E: 聚合 top N + budget ≤ 4096（1KB token 预算的 4 倍软上限）。
    /// ponytail: RA cold-start 索引窗口用 busy-retry 而非 sleep（避免 flake；上限 5s）。
    #[tokio::test]
    async fn build_aggregates_top_n_for_rust_demo() {
        if !rust_analyzer_available() {
            eprintln!("skipped: rust-analyzer not on PATH");
            return;
        }
        let root = rust_demo_root();
        if !root.exists() {
            eprintln!("skipped: fixtures/rust_demo missing");
            return;
        }
        let sup = crate::Supervisor::direct().await.expect("supervisor");

        // 预热 + busy-retry 直到 total 非零（RA cold-start 索引就绪）。
        let mut report = build(&sup, &root, Some("rust"), 10).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if report.total_symbols >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            report = build(&sup, &root, Some("rust"), 10).await;
        }

        assert!(
            report.total_symbols >= 2,
            "rust_demo 至少 add/multiply/main 等符号；got {}",
            report.total_symbols
        );
        assert!(report.top.len() <= 10, "top 长度不超过 top_n");
        assert!(
            report.budget_bytes <= 4096,
            "1KB token 预算软上限（4x 安全余量）；got {}",
            report.budget_bytes
        );
    }

    /// E: 不存在的 root → 全空（不 panic、不返 Err）。
    #[tokio::test]
    async fn build_returns_empty_on_error() {
        let sup = crate::Supervisor::direct().await.expect("supervisor");
        let report = build(&sup, Path::new("/nonexistent_xyz_qq"), Some("rust"), 20).await;
        assert_eq!(report.total_symbols, 0);
        assert!(report.top.is_empty());
    }
}