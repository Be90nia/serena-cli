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
pub(crate) fn range_from_symbol_cache(
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

/// 在 body 文本内定位符号名 token（相对 body 首行的行偏移 + 0-based 列）。
///
/// bd syra：symbol range 常把 `///` doc 注释包进 body（RA documentSymbol range 从
/// doc 行起）——旧实现只在 body 第一行找名字，找不到就 fallback 列 0，hover/references
/// 落在 doc 注释/`pub` 关键字上，doc 字段抓到的是关键字的 hover。改为扫描全文首个
/// 含名字的行（= 签名行），hover/ref 查询点即 fn name token。找不到 fallback (0,0)。
fn locate_name_in_body(body_text: &str, symbol: &str) -> (u32, u32) {
    body_text
        .lines()
        .enumerate()
        .find_map(|(i, line)| line.find(symbol).map(|c| (i as u32, c as u32)))
        .unwrap_or((0, 0))
}

/// `edit-context` 聚合入口。body 失败 = 四段全不可得（callers/doc/tests 均以
/// body 为前提）→ 硬错与 symbol-body 对齐（bd serena-rust-37nl：全 null +
/// rc=0 是静默失败，AI 会误判"无 callers/tests"）；body 成功后 callers/doc
/// 段失败仍按字段降级（任一段失败不影响其他字段）。
/// 第二返回值 = 降级警示（bd serena-rust-e0hi/8vo9）：callers 空且语义层未证就绪
/// 时由 [`Supervisor::referencing_empty_warnings`] 给出，dispatch 层 attach 到 wire。
pub async fn collect(
    sup: &Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    lang: Option<&str>,
) -> Result<(EditContextReport, Vec<String>), ToolError> {
    let mut report = EditContextReport {
        file: file.into(),
        symbol: symbol.into(),
        body: None,
        callers: None,
        doc: None,
        tests: None,
    };
    let mut warnings = Vec::new();

    // 1) body：symbol-body + 缓存里取 range。
    let text = match sup.tool_symbol_body(root, file, symbol, lang).await {
        Ok(t) => t,
        // bd serena-rust-37nl：嵌套名（A.b / A::b）在 documentSymbol 里按平铺
        // 短名检索，not-found 补短名提示；瞬态错（LS_TIMEOUT 等）原样上抛。
        Err(ToolError::BadArgs { detail }) => {
            let nested = symbol.contains('.') || symbol.contains("::");
            return Err(ToolError::BadArgs {
                detail: if nested {
                    format!("{detail}; nested names are searched flat — retry with the short name (part after the last `.`)")
                } else {
                    detail
                },
            });
        }
        Err(e) => return Err(e),
    };
    let (s0, e0) = range_from_symbol_cache(sup, root, file, symbol).unwrap_or((0, 0));
    let body_line_0based = s0;
    report.body = Some(BodyRange {
        start_line: s0 + 1, // 1-based 输出给 AI
        end_line: e0 + 1,
        text,
    });

    // 2) callers + tests：refs 反查。RA `references` 要求光标在符号名上才返回真引用，
    //    用 locate_name_in_body 找 body 内符号名 token（签名行）作为查点 —— bd syra：
    //    旧实现查 body 第一行（doc 注释行）会落在关键字上。
    if let Some(body) = report.body.as_ref() {
        let (name_off, name_col) = locate_name_in_body(&body.text, symbol);
        let name_line = body_line_0based + name_off;
        if let Ok((callers, _raw_snip)) = sup
            .tool_referencing_symbols(root, file, name_line, name_col, lang)
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
            // 空 callers = 真无 caller 或语义未就绪（类型分析窗口 refs 静默返空），
            // 警示让 AI 可分；refs 失败（callers=null）不警示，failure 已由 null 表达。
            if callers.is_empty() {
                warnings = sup.referencing_empty_warnings(root);
            } else {
                sup.mark_semantic_ready(root);
            }
            report.callers = Some(callers);
            report.tests = Some(tests);
        }
    }

    // 3) doc：hover 同位置（fn name token）拿 doc_string —— bd syra：hover 必须
    //    打在符号名上，打在 doc 注释/`pub` 关键字上会抓到关键字的文档。
    if let Some(body) = report.body.as_ref() {
        let (name_off, name_col) = locate_name_in_body(&body.text, symbol);
        if let Ok(Some(hover)) = sup
            .tool_hover(root, file, body_line_0based + name_off, name_col, lang)
            .await
        {
            report.doc = extract_hover_doc(&hover.contents);
        }
    }

    Ok((report, warnings))
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
    fn locate_name_in_body_skips_doc_comment_first_line() {
        // bd syra：body 首行是 doc 注释（不含符号名）→ 必须落到签名行，不能 fallback 列 0。
        let body = "/// Compute the sum.\n///\npub fn sum_slice(xs: &[i64]) -> i64 {\n    0\n}\n";
        let (off, col) = locate_name_in_body(body, "sum_slice");
        assert_eq!(off, 2, "name token lives on the signature line");
        assert_eq!(col, 7, "after `pub fn `");
        // 名字就在首行（无 doc）→ (0, 列)。
        let (off, col) = locate_name_in_body("fn main() {}", "main");
        assert_eq!((off, col), (0, 3));
        // 全文无名字 → fallback (0,0)。
        assert_eq!(locate_name_in_body("fn other() {}", "missing"), (0, 0));
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

    /// B: 4 字段各自填充（fixture 里有 `add` 函数 + `main.rs` 调用）。
    /// `add` 函数体必非空；callers 必≥1（lib.rs 自身声明）+ doc 看 hover 实现 + tests 必为 Some。
    /// ponytail: RA cold-start 索引窗口用 busy-retry 而非 sleep（避免 flake；满载机实测 >30s，上限 45s）。
    #[tokio::test]
    async fn edit_context_collects_all_four_fields() {
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
        // 预热 + busy-retry 直到 callers 非空且 doc Some（RA cold-start 索引就绪；
        // refs 与 hover 就绪时间不同步，只盯 callers 会在 hover 仍冷时漏出循环）。
        let (mut report, _) = collect(&sup, &root, "lib.rs", "add", Some("rust"))
            .await
            .expect("collect ok");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        while std::time::Instant::now() < deadline {
            let callers_ok = report
                .callers
                .as_ref()
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if callers_ok && report.doc.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            report = collect(&sup, &root, "lib.rs", "add", Some("rust"))
                .await
                .expect("collect ok")
                .0;
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
        // callers：demo() 内的真实调用（bd nl2w/A3b #1：定义点自身已被
        // drop_self_reference 过滤——旧断言"至少含声明自身"是在 Windows 过滤器
        // no-op 状态下写的，过滤器生效后 add 无真实调用即恒空，fixture 已补 demo）。
        // RefSymbolHit.line 0-based：add 定义在 lib.rs 0-based 第 0 行。
        let callers = report.callers.as_ref().expect("callers must be Some");
        assert!(
            !callers.is_empty(),
            "callers 必须含 demo 内的真实调用；raw_count={}",
            callers.len()
        );
        assert!(
            callers.iter().all(|c| c.line != 0),
            "定义点自身必须被过滤；got: {callers:?}"
        );
        // doc：hover 拿到 pub fn 签名（任一字符串）。
        assert!(report.doc.is_some(), "doc 必须 Some（hover 必有响应）");
        // tests：rust_demo 无 tests/ → 空 Vec（合法语义）。
        let tests = report.tests.as_ref().expect("tests field must be Some");
        assert!(tests.is_empty(), "rust_demo 无 tests/ 文件 → 空 Vec");
    }

    /// bd serena-rust-37nl：未知符号 → 硬错与 symbol-body 对齐（不再全 null +
    /// rc=0 静默失败）；嵌套名（含 `.`/`::`）额外带短名提示，短名查询不带。
    #[tokio::test]
    async fn edit_context_unknown_symbol_errors_like_symbol_body() {
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

        // 短名未知符号：not found，无嵌套提示。
        let err = collect(
            &sup,
            &root,
            "lib.rs",
            "nonexistent_symbol_xyz_qq",
            Some("rust"),
        )
        .await
        .expect_err("unknown symbol must error");
        let ToolError::BadArgs { detail } = err else {
            panic!("expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("not found"), "{detail}");
        assert!(!detail.contains("nested names"), "{detail}");

        // 嵌套形态未知符号：not found + 短名提示（与 symbol-body 行为对齐处）。
        let err = collect(
            &sup,
            &root,
            "lib.rs",
            "NoSuchType.nonexistent_symbol_xyz_qq",
            Some("rust"),
        )
        .await
        .expect_err("nested unknown symbol must error");
        let ToolError::BadArgs { detail } = err else {
            panic!("expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("not found"), "{detail}");
        assert!(detail.contains("nested names"), "{detail}");
    }
}
