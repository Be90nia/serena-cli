//! AI 编辑主路径聚合：body + callers + doc + tests。
//!
//! 设计（ai-token-features-design.md §10-B）：4 工具串接，任一段失败 → 对应字段
//! `None`（区别于"无结果"空数组）；其余字段正常输出，单次调用省 3-4 次 round-trip。
//!
//! 实现要点：
//! - body 走 `tool_symbol_body`（已自带 documentSymbol 缓存，命中免 LS 往返）；
//!   范围 line 由同文件 `symbol_cache` 找 `SymbolHit.range` 提供（与 tool_symbol_body 共用缓存层）。
//! - callers / tests 走 `tool_referencing_symbols`（ref_tools 内已把路径归一为相对 root）；
//!   tests 子集 = callers 中 file 路径走 `looks_like_test_file` 过滤。
//! - doc 走 `tool_hover`（`HoverContents` 三 variant：Scalar / Array / Markup）。
//!
//! 锁纪律：纯函数串接，零跨 await；不持 supervisor 内部任何 Mutex。
use crate::ref_tools::RefSymbolHit;
use crate::{Supervisor, ToolError};
use lsp_types::{HoverContents, MarkedString};
use serde::Serialize;
use std::path::Path;

/// 符号体的 LSP Range 切片结果（1-based line，AI token 友好）。
#[derive(Debug, Serialize)]
pub struct BodyRange {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// `edit-context` 工具聚合响应。任一字段 `None` = 失败；空 Vec / 空 String = 无结果。
#[derive(Debug, Serialize)]
pub struct EditContextReport {
    pub file: String,
    pub symbol: String,
    pub body: Option<BodyRange>,
    pub callers: Option<Vec<RefSymbolHit>>,
    pub doc: Option<String>,
    pub tests: Option<Vec<RefSymbolHit>>,
}

/// 测试文件识别（callers 中筛 tests/ 子集用）。覆盖主流约定：
/// - `tests/` / `test/` 目录（Python/JS/Go/Rust）—— 含项目根下的 `tests/foo.rs` 形态
/// - `_test.` 后缀（Go：`foo_test.go`）和 `.test.` / `.spec.` 后缀（TS/JS/Rust 集成测试）
/// - `test_` 前缀（Python pytest 风格）
/// - JUnit Java：`文件名 Test 开头 或 Test.java / Tests.java 后缀`
fn looks_like_test_file(file: &str) -> bool {
    let f = file.replace('\\', "/");
    // 路径前缀（项目根的 tests/） OR 嵌套目录（`/tests/`、`/test/`）。
    f.starts_with("tests/")
        || f.starts_with("test/")
        || f.contains("/tests/")
        || f.contains("/test/")
        || f.contains("_test.")
        || f.contains(".test.")
        || f.contains(".spec.")
        || f.starts_with("test_")
        // Java JUnit：`TestFoo.java`（前缀 Test）或 `FooTest.java`（后缀 Test.java）。
        || (f.ends_with("Test.java") || f.ends_with("Tests.java"))
        // 取最后一个 path segment 后判定前缀 Test（限 .java）。
        || {
            let last = f.rsplit('/').next().unwrap_or(&f);
            last.ends_with(".java")
                && (last.starts_with("Test") || last.starts_with("Tests"))
        }
}

/// 从 `tool_symbol_body` 缓存层找匹配符号的 range（line 0-based），为 BodyRange 提供
/// 起止行号。缓存命中/miss 都能命中——`tool_symbol_body` 写缓存时已把全部符号平铺入库。
fn range_from_symbol_cache(
    sup: &Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
) -> Option<(u32, u32)> {
    let key = crate::doc_symbol_cache_key(root, file);
    let hits = sup.symbol_cache_get(&key)?;
    hits.iter()
        .find(|h| h.name == symbol)
        .map(|h| (h.range.start.line, h.range.end.line))
}

/// 找符号名在 body 第一行的列偏移（0-based）。RA `references` 要求光标在符号名
/// 自身上才有结果。ponytail: 直接字符串扫描，未走 LSP。失败 fallback 0。
fn column_of_symbol_on_line(body_text: &str, line_0based: u32, symbol: &str) -> u32 {
    body_text
        .lines()
        .nth(line_0based as usize)
        .and_then(|line| line.find(symbol).map(|c| c as u32))
        .unwrap_or(0)
}

/// `edit-context` 聚合入口。任一段失败不影响其他字段。
pub async fn collect(
    sup: &Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    lang: Option<&str>,
) -> EditContextReport {
    let mut report = EditContextReport {
        file: file.into(),
        symbol: symbol.into(),
        body: None,
        callers: None,
        doc: None,
        tests: None,
    };

    // 1) body：symbol-body + 缓存里取 range。
    let mut body_line_0based: u32 = 0;
    if let Ok(text) = sup.tool_symbol_body(root, file, symbol, lang).await {
        let (s0, e0) = range_from_symbol_cache(sup, root, file, symbol).unwrap_or((0, 0));
        body_line_0based = s0;
        report.body = Some(BodyRange {
            start_line: s0 + 1, // 1-based 输出给 AI
            end_line: e0 + 1,
            text,
        });
    }

    // 2) callers + tests：refs 反查。RA `references` 要求光标在符号名上才返回真引用，
    //    用 column_of_symbol_on_line 找 body 第一行 symbol 列偏移作为查点。
    if let Some(body) = report.body.as_ref() {
        let query_col = column_of_symbol_on_line(&body.text, body_line_0based, symbol);
        if let Ok(callers) = sup
            .tool_referencing_symbols(root, file, body_line_0based, query_col, lang)
            .await
        {
            let tests: Vec<RefSymbolHit> = callers
                .iter()
                .filter(|c| looks_like_test_file(&c.file))
                .map(|c| RefSymbolHit {
                    file: c.file.clone(),
                    line: c.line,
                    col: c.col,
                    container_name: c.container_name.clone(),
                })
                .collect();
            report.callers = Some(callers);
            report.tests = Some(tests);
        }
    }

    // 3) doc：hover 同位置拿 doc_string。
    if let Some(body) = report.body.as_ref() {
        let query_col = column_of_symbol_on_line(&body.text, body_line_0based, symbol);
        if let Ok(Some(hover)) = sup
            .tool_hover(root, file, body_line_0based, query_col, lang)
            .await
        {
            report.doc = extract_hover_doc(&hover.contents);
        }
    }

    report
}

/// 把 `HoverContents` 三 variant 折叠为人类可读 doc 字符串：
/// - `Markup(MarkupContent)` → `.value`（多数 LS 走这条，含 doc 注释）
/// - `Array(Vec<MarkedString>)` → 拼接每项渲染
/// - `Scalar(MarkedString)` → 同上
fn extract_hover_doc(contents: &HoverContents) -> Option<String> {
    match contents {
        HoverContents::Markup(m) => Some(m.value.clone()),
        HoverContents::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(marked_string_to_string).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        HoverContents::Scalar(ms) => Some(marked_string_to_string(ms)),
    }
}

fn marked_string_to_string(ms: &MarkedString) -> String {
    match ms {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(ls) => ls.value.clone(),
    }
}

// Suppress unused-imports for ToolError if trait surface changes later.
#[allow(dead_code)]
fn _tool_error_marker(_: ToolError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tests_filter_recognizes_test_files() {
        // 标准 tests/ 目录
        assert!(looks_like_test_file("tests/integration.rs"));
        assert!(looks_like_test_file("crates/x/tests/foo.rs"));
        // Python test_ 前缀
        assert!(looks_like_test_file("test_foo.py"));
        // Go _test 后缀
        assert!(looks_like_test_file("internal/foo_test.go"));
        // TS _spec 后缀（无论是否在 tests/ 目录）
        assert!(looks_like_test_file("src/foo.spec.ts"));
        assert!(looks_like_test_file("foo.spec.ts"));
        // JUnit Java：TestFoo.java（前缀 Test） + FooTest.java（后缀 Test.java）
        assert!(looks_like_test_file("TestFoo.java"));
        assert!(looks_like_test_file("src/FooTest.java"));
        // Windows 反斜杠路径也认（ref_tools 内部已替换）
        assert!(looks_like_test_file("crates\\x\\tests\\foo.rs"));
        // 否定
        assert!(!looks_like_test_file("src/main.rs"));
        assert!(!looks_like_test_file("lib.rs"));
        assert!(!looks_like_test_file("src/foo.rs"));
        assert!(!looks_like_test_file("foo.ts"));
        assert!(!looks_like_test_file("FooBar.java")); // 普通 Java 类（不以 Test 开头 / 结尾）
    }

    #[test]
    fn tests_filter_does_not_match_unrelated_keywords() {
        // "test" 是子串但不构成测试文件（无 /tests/、_test.、_spec. 等）
        assert!(!looks_like_test_file("src/contest.rs"));
        assert!(!looks_like_test_file("src/protest.rs"));
    }

    #[test]
    fn marked_string_to_string_handles_both_variants() {
        let plain = MarkedString::String("hello".into());
        assert_eq!(marked_string_to_string(&plain), "hello");
        let lang = MarkedString::LanguageString(lsp_types::LanguageString {
            language: "rust".into(),
            value: "fn x() {}".into(),
        });
        assert_eq!(marked_string_to_string(&lang), "fn x() {}");
    }

    /// fixtures/rust_demo 的 root；workspace 根到 crate 根相对定位。
    fn rust_demo_root() -> PathBuf {
        // CARGO_MANIFEST_DIR 是 crates/supervisor；往上两级到项目根。
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        crate_root
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("fixtures/rust_demo")
    }

    /// r-a 是否可达。false 时所有集成测试 skip（不构成 false failure）。
    /// ponytail: 不用 `which` crate（新增依赖），手工 PATH 查找 + Windows .exe 后缀。
    fn rust_analyzer_available() -> bool {
        let exe = if cfg!(windows) { "rust-analyzer.exe" } else { "rust-analyzer" };
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                if dir.join(exe).is_file() {
                    return true;
                }
            }
        }
        false
    }

    /// B: 4 字段各自填充（fixture 里有 `add` 函数 + `main.rs` 调用）。
    /// `add` 函数体必非空；callers 必≥1（lib.rs 自身声明）+ doc 看 hover 实现 + tests 必为 Some。
    /// ponytail: RA cold-start 索引窗口用 busy-retry 而非 sleep（避免 flake；满载机实测 >30s，上限 45s）。
    #[tokio::test]
    async fn edit_context_collects_all_four_fields() {
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
        // 预热 + busy-retry 直到 callers 非空且 doc Some（RA cold-start 索引就绪；
        // refs 与 hover 就绪时间不同步，只盯 callers 会在 hover 仍冷时漏出循环）。
        let mut report = collect(&sup, &root, "lib.rs", "add", Some("rust")).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        while std::time::Instant::now() < deadline {
            let callers_ok = report.callers.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
            if callers_ok && report.doc.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            report = collect(&sup, &root, "lib.rs", "add", Some("rust")).await;
        }

        assert!(report.body.is_some(), "body 必须有内容");
        let body = report.body.as_ref().unwrap();
        assert!(
            !body.text.is_empty() && body.text.contains("add"),
            "body.text 必须含 add 函数体；got: {}",
            body.text
        );
        assert!(body.start_line >= 1 && body.end_line >= body.start_line);
        assert_eq!(report.symbol, "add");
        // callers：lib.rs 的 add 至少有声明自身（includeDeclaration: true）。
        let callers = report.callers.as_ref().expect("callers must be Some");
        assert!(
            !callers.is_empty(),
            "callers 必须非空（至少含声明自身）；raw_count={}",
            callers.len()
        );
        // doc：hover 拿到 pub fn 签名（任一字符串）。
        assert!(report.doc.is_some(), "doc 必须 Some（hover 必有响应）");
        // tests：rust_demo 无 tests/ → 空 Vec（合法语义）。
        let tests = report.tests.as_ref().expect("tests field must be Some");
        assert!(tests.is_empty(), "rust_demo 无 tests/ 文件 → 空 Vec");
    }

    /// B: 故意传不存在的符号 → body 失败，callers/doc/tests 也都 None（短路）。
    #[tokio::test]
    async fn edit_context_failed_body_yields_others_null() {
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
        let report = collect(
            &sup,
            &root,
            "lib.rs",
            "nonexistent_symbol_xyz_qq",
            Some("rust"),
        )
        .await;

        // 失败隔离契约：body 失败 → callers/doc/tests 也都 None（短路在 if let Some(body)）。
        assert!(report.body.is_none(), "body 失败必须 None");
        assert!(report.callers.is_none(), "callers 必须 None（短路）");
        assert!(report.doc.is_none(), "doc 必须 None（短路）");
        assert!(report.tests.is_none(), "tests 必须 None（短路）");
    }
}