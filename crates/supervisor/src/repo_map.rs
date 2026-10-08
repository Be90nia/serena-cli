//! E: repo-map 全库符号地图（ai-token-features-design §10-E）。
//!
//! bd serena-rust-fj17 降级版：**按文件 + 顶层符号列表全展开，不依赖 refs 计数**。
//! 原简化 PageRank（per-symbol `tool_referencing_symbols` 计数）对 direct_refs=0
//! 的小项目全空，且 `tool_symbol_tree` 的 `session_for` 冷启动失败会把整工具传播成
//! `{budget_bytes:0, top:[], total_symbols:0}`（实测全 7 crate 0 信息响应）。
//!
//! 现算法：主源 = `tool_symbol_tree`（documentSymbol 语法级，3.1 缓存兜底）拉平；
//! LS 层零符号（冷窗口 / 会话失败 / 空缓存）→ 纯文本顶层定义行扫描兜底，保证任何
//! 有源码的项目非空。符号清单本身已足够指示 API 面。
//!
//! ponytail: 不做引用计数/热度排序——等 LS 语义层稳定后再加回来。

use serde::Serialize;

use std::path::Path;

use crate::Supervisor;

/// repo-map 单条：一个符号的定位画像。
#[derive(Debug, Serialize)]
pub struct RepoMapEntry {
    pub name: String,
    pub container: Option<String>,
    pub file: String,
    pub kind: String,
}

/// repo-map 工具响应。
#[derive(Debug, Serialize)]
pub struct RepoMapReport {
    pub total_symbols: usize,
    pub top: Vec<RepoMapEntry>,
    pub budget_bytes: usize,
}

/// 文件数保险丝（万级文件目录不拖垮扫描；与 symbol-tree 同量级）。
const CANDIDATE_SCAN_LIMIT: usize = 5000;

/// repo-map 主入口。`lang = None` → 走 multi-lang 自动探测。
pub async fn build(
    sup: &Supervisor,
    root: &Path,
    lang: Option<&str>,
    top_n: usize,
) -> RepoMapReport {
    // 1) LS documentSymbol 全库树（3.1 缓存兜底）。Err 不再整工具失败——降级路径
    //    接管（fj17：`session_for().await?` 曾把 LS 失败传播成全空响应）。
    let tree = sup
        .tool_symbol_tree(root, ".", lang, CANDIDATE_SCAN_LIMIT)
        .await
        .ok();
    let mut top = flatten_symbols(tree);
    // 2) LS 层零符号 → 纯文本顶层定义扫描兜底（保证任何项目非空）。
    if top.is_empty() {
        top = text_scan_top_level(root);
    }
    let total_symbols = top.len();
    top.truncate(top_n);
    let budget_bytes = serde_json::to_vec(&top).map(|v| v.len()).unwrap_or(0);
    RepoMapReport {
        total_symbols,
        top,
        budget_bytes,
    }
}

/// 拉平 symbol-tree `entries[].symbols[]` 为符号列表（保持文件扫描顺序；children
/// 以 container 标注嵌套，全展开）。
fn flatten_symbols(tree: Option<serde_json::Value>) -> Vec<RepoMapEntry> {
    let Some(entries) = tree
        .as_ref()
        .and_then(|t| t.get("entries"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let Some(file) = entry.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(syms) = entry.get("symbols").and_then(|v| v.as_array()) else {
            continue;
        };
        for s in syms {
            let Some(name) = s
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            out.push(RepoMapEntry {
                name: name.to_string(),
                container: s.get("container").and_then(|v| v.as_str()).map(String::from),
                file: file.to_string(),
                kind: s
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    out
}

/// 顶层定义行启发式：零缩进 + 修饰符前缀剥离 + 定义关键字 + 标识符。覆盖
/// rust/py/ts/go/java/c++ 常见形态；缩进行（嵌套项/方法）与注释行不算顶层。
fn top_level_def(line: &str) -> Option<(&'static str, String)> {
    let rest = line.trim_end();
    if rest.starts_with(' ') || rest.starts_with('\t') {
        return None;
    }
    // 带括号形态必须排在裸词前（否则 "pub" 先剥掉 "pub(crate)" 的前缀）。
    const MODIFIERS: [&str; 10] = [
        "pub(crate)", "pub(super)", "pub", "export", "default", "public", "private",
        "protected", "async", "abstract",
    ];
    let mut rest = rest;
    while let Some(r) = MODIFIERS
        .iter()
        .find_map(|m| rest.strip_prefix(m).and_then(|r| r.strip_prefix(' ')))
    {
        rest = r;
    }
    const DEFS: [(&str, &str); 12] = [
        ("fn ", "function"),
        ("func ", "function"),
        ("fun ", "function"),
        ("def ", "function"),
        ("function ", "function"),
        ("struct ", "struct"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("impl ", "impl"),
        ("type ", "type"),
    ];
    DEFS.iter().find_map(|(kw, kind)| {
        let tail = rest.strip_prefix(kw)?;
        let name: String = tail
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!name.is_empty()).then_some((*kind, name))
    })
}

/// LS 层零符号时的纯文本兜底：逐源码文件扫顶层定义行（无 LS 依赖）。
/// 注释行（`/// fn x` / `# def x`）因行首前缀字符不命中关键字，天然跳过。
fn text_scan_top_level(root: &Path) -> Vec<RepoMapEntry> {
    let mut out = Vec::new();
    for entry in crate::fs_tools::filtered_walker(root).build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if crate::resolve_lang_for_file(&rel, None).is_err() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            if let Some((kind, name)) = top_level_def(line) {
                out.push(RepoMapEntry {
                    name,
                    container: None,
                    file: rel.clone(),
                    kind: kind.to_string(),
                });
            }
        }
    }
    out
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

    /// fj17：direct_refs=0 小 fixture 非空——LS 层零符号（tree=None）时文本兜底接管。
    #[test]
    fn text_scan_captures_top_level_defs_and_skips_nested() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            tmp.path().join("a.rs"),
            "pub fn alpha() {}\n\
             struct Beta;\n\
             fn outer() {\n    fn nested() {}\n}\n\
             // fn documented() {}\n\
             async fn gamma() {}\n\
             pub(crate) fn delta() {}\n\
             use std::fmt;\n",
        )
        .expect("write fixture");
        std::fs::write(tmp.path().join("notes.txt"), "fn not_source() {}\n").expect("write txt");

        let out = text_scan_top_level(tmp.path());
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        for expected in ["alpha", "Beta", "gamma", "delta"] {
            assert!(names.contains(&expected), "缺 {expected}，实际 {names:?}");
        }
        assert!(!names.contains(&"nested"), "缩进嵌套不算顶层: {names:?}");
        assert!(
            !names.contains(&"documented"),
            "注释行不算定义: {names:?}"
        );
        assert!(
            !names.contains(&"not_source"),
            "非源码扩展名不入扫: {names:?}"
        );
        assert!(out.iter().all(|e| e.container.is_none()));
        let alpha = out.iter().find(|e| e.name == "alpha").unwrap();
        assert_eq!((alpha.file.as_str(), alpha.kind.as_str()), ("a.rs", "function"));
    }

    /// flatten：entries[].symbols[] 拉平、空名剔除、tree=None 空。
    #[test]
    fn flatten_symbols_pairs_file_with_symbols() {
        let tree = serde_json::json!({
            "entries": [
                {"file": "a.rs", "symbols": [
                    {"name": "alpha", "kind": "function", "container": null},
                    {"name": "meth", "kind": "method", "container": "alpha"},
                ]},
                {"file": "b.rs", "symbols": [{"name": "", "kind": "unknown"}]},
            ]
        });
        let out = flatten_symbols(Some(tree));
        assert_eq!(out.len(), 2, "空名剔除: {out:?}");
        assert_eq!(out[0].file, "a.rs");
        assert_eq!(out[0].name, "alpha");
        assert!(out[0].container.is_none());
        assert_eq!(out[1].container.as_deref(), Some("alpha"));
        assert!(flatten_symbols(None).is_empty());
        assert!(flatten_symbols(Some(serde_json::json!({"entries": []}))).is_empty());
    }

    /// 修饰符剥离顺序：带括号形态先于裸词（否则 pub 先剥掉 pub(crate) 前缀）。
    #[test]
    fn top_level_def_strips_parenthesized_modifier_first() {
        let (kind, name) = top_level_def("pub(crate) fn delta() {}").expect("pub(crate) fn");
        assert_eq!((kind, name.as_str()), ("function", "delta"));
        let (kind, name) = top_level_def("export default class Foo {").expect("export class");
        assert_eq!((kind, name.as_str()), ("class", "Foo"));
        assert!(top_level_def("    fn indented() {}").is_none());
        assert!(top_level_def("use std::fmt;").is_none());
    }
}
