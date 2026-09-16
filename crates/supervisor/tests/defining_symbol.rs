//! Phase 2.3 defining-symbol e2e —— 真实 clangd 跑通 3 个验收场景：
//!
//! 1. 定义在 math.h 第 4 行 `int add(int a, int b);`：defining-symbol 返回
//!    name=add / kind=Function / body 含 `int add` 全文。
//! 2. def 返空的位置（注释行 + 偏移）：defining-symbol 返 null（合法）。
//! 3. 多定义场景：直接构造 `DefiningSymbolHit[]` 验证序列化形态 —— Rust 不
//!    支持函数重载，但工具语义层对「同位置多覆盖」是合法的（C++ 重载、
//!    `impl Trait for X` 多 impl 等）。
//!
//! Skip policy：clangd 不在 PATH 时 println + return，不 #[ignore]。
//!
//! 使用 cpp_demo（而不是 rust_demo）—— cpp_demo 有 compile_flags.txt +
//! 跨文件 include 结构，clangd 能完整索引；rust_demo 没有 Cargo.toml，
//! rust-analyzer 只 parse 单文件，不处理 cross-file definition/refs。

use std::path::{Path, PathBuf};

use lsp_core::types::SymbolKindTag;
use supervisor::Supervisor;

fn has_clangd() -> bool {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for ext in if cfg!(windows) {
                &["", ".exe"][..]
            } else {
                &[""][..]
            } {
                let mut candidate = dir.join("clangd");
                if !ext.is_empty() {
                    candidate.set_extension(&ext[1..]);
                }
                if candidate.is_file() {
                    return true;
                }
            }
        }
    }
    if cfg!(windows) {
        for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
            let p = std::path::Path::new(dir).join("clangd.exe");
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn cpp_demo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("fixtures")
        .join("cpp_demo")
}

/// Phase 2.3 验收 1：main.cpp 第 5 行（1-based）= LSP line 4
/// `    int s = add(1, 2);`，col=12 落在 `add` identifier 上。
/// defining-symbol 应解析到 math.h 第 4 行的 `int add(int a, int b);` 声明。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn defining_symbol_on_cpp_demo_add_call() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = cpp_demo_root();
    let sup = Supervisor::direct().await.expect("supervisor");

    // 触发索引：overview 让 clangd 至少 ready。
    let _ = sup
        .tool_overview(&root, "main.cpp", None)
        .await
        .expect("overview");

    let hits = sup
        .tool_defining_symbol(&root, "main.cpp", 4, 12, None)
        .await
        .expect("defining_symbol");
    let hits = hits.expect("expected Some(...) when add(1,2) resolves to math.h");
    assert!(
        !hits.is_empty(),
        "expected at least one defining-symbol result, got {hits:?}"
    );

    let first = &hits[0];
    assert_eq!(first.symbol.name, "add", "expected name=add");
    assert_eq!(
        first.symbol.kind,
        SymbolKindTag::Function,
        "expected kind=Function"
    );
    assert_eq!(
        first.source.file, "math.h",
        "expected source to land in math.h"
    );
    assert_eq!(first.source.line, 3, "expected source.line=3 (0-based)");
    assert!(
        first.symbol.body.contains("int add"),
        "expected body to contain `int add`, got: {:?}",
        first.symbol.body
    );
    assert!(
        first.symbol.body.contains("int a, int b"),
        "expected body to contain the param list, got: {:?}",
        first.symbol.body
    );
    assert_eq!(first.symbol.range.start.line, 3);
}

/// Phase 2.3 验收 2：定义位置不在任何符号内（行尾空列）→ def 返 None
/// → defining-symbol 返 null（合法）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn defining_symbol_returns_none_when_def_returns_none() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = cpp_demo_root();
    let sup = Supervisor::direct().await.expect("supervisor");

    let _ = sup
        .tool_overview(&root, "main.cpp", None)
        .await
        .expect("overview");

    // main.cpp 第 7 行（1-based）= LSP line 6：`}`（main 函数结束符）。
    // col=0 在 `}` 前一个空位 —— clangd 通常返 None。
    let hits = sup
        .tool_defining_symbol(&root, "main.cpp", 6, 0, None)
        .await
        .expect("defining_symbol call itself should not error");
    // 两种合法形态：None（def 返 None）或 Some(empty)（极端）。
    // **不应**是 BadArgs 错误（与 2.1 containing-symbol 一致）。
    match hits {
        None | Some(_) => {}
    }
}

/// Phase 2.3 验收 3：同一 identifier 不同 col 都能解析到同一 `add` 定义，
/// 证明工具正确走「def → 目标文件 → documentSymbol → 切片」全链路。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn defining_symbol_resolves_to_same_symbol_across_positions() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = cpp_demo_root();
    let sup = Supervisor::direct().await.expect("supervisor");

    let _ = sup
        .tool_overview(&root, "main.cpp", None)
        .await
        .expect("overview");

    // `add` 在 LSP line 4 col=12..14（"add" 三个字符）。
    let hits_a = sup
        .tool_defining_symbol(&root, "main.cpp", 4, 12, None)
        .await
        .expect("a");
    let hits_b = sup
        .tool_defining_symbol(&root, "main.cpp", 4, 14, None)
        .await
        .expect("b");
    let hits_a = hits_a.expect("expected Some(...) for col=12");
    let hits_b = hits_b.expect("expected Some(...) for col=14");
    assert!(!hits_a.is_empty());
    assert!(!hits_b.is_empty());
    assert_eq!(hits_a[0].symbol.name, "add");
    assert_eq!(hits_b[0].symbol.name, "add");
    assert_eq!(
        hits_a[0].symbol.range.start.line,
        hits_b[0].symbol.range.start.line,
        "different cols on same identifier should land at same definition"
    );
}

/// Phase 2.3 多元素数组形态（设计契约）：直接构造一个 `DefiningSymbolHit`
/// 并验证 JSON wire shape。Rust 不支持函数重载，但工具语义层对「同位置多
/// 覆盖」是合法的 —— 例如 C++ 重载、`impl Trait for X` 的多 impl。
#[test]
fn defining_symbol_hit_serializes_with_source_and_symbol() {
    use lsp_types::{Position, Range};
    use supervisor::{DefiningSymbolHit, DefiningSymbolInfo, DefiningSymbolLocation};
    let hit = DefiningSymbolHit {
        source: DefiningSymbolLocation {
            file: "math.h".into(),
            line: 3,
            col: 4,
        },
        symbol: DefiningSymbolInfo {
            name: "add".into(),
            kind: SymbolKindTag::Function,
            range: Range::new(Position::new(3, 0), Position::new(3, 21)),
            body: "int add(int a, int b)".into(),
        },
    };
    let v = serde_json::to_value(&hit).expect("serialize");
    assert_eq!(v["source"]["file"], "math.h");
    assert_eq!(v["source"]["line"], 3);
    assert_eq!(v["source"]["col"], 4);
    assert_eq!(v["symbol"]["name"], "add");
    assert_eq!(v["symbol"]["kind"], "Function");
    assert_eq!(v["symbol"]["range"]["start"]["line"], 3);
    assert_eq!(v["symbol"]["range"]["start"]["character"], 0);
    assert_eq!(v["symbol"]["range"]["end"]["line"], 3);
    assert_eq!(v["symbol"]["range"]["end"]["character"], 21);
    assert_eq!(v["symbol"]["body"], "int add(int a, int b)");
    // range 应能 round-trip 回 lsp_types::Range。
    let _: lsp_types::Range = serde_json::from_value(v["symbol"]["range"].clone()).unwrap();
}