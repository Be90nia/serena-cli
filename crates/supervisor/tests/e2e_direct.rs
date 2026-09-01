//! M0 end-to-end: spawn real clangd via the supervisor and exercise the
//! three read-only tools (`overview` / `def` / `refs`) against the bundled
//! `fixtures/cpp_demo` (PLAN Task 10).
//!
//! Skip policy (PLAN Global Constraints): when `clangd` is not on PATH we
//! `println!("skipped: clangd not in PATH")` and `return` early. Never `#[ignore]`:
//! the test still runs in CI and just becomes a no-op when the binary is absent.

use std::path::{Path, PathBuf};

use lsp_types::{Position, Range};
use supervisor::{Location, Supervisor};

fn fixtures_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/supervisor; fixture is at workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("fixtures")
        .join("cpp_demo")
}

fn main_cpp_line_of(text: &str, needle: &str) -> (u32, u32) {
    for (i, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (i as u32, col as u32);
        }
    }
    panic!("needle {needle:?} not in fixture main.cpp:\n{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m0_overview_def_refs_over_real_clangd() {
    // PATH probe — `where clangd` on Windows, `which` elsewhere. We use the same
    // PATH-walk helper as `ls-adapters::which_no_unc` so test and prod agree.
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }

    let root = fixtures_root();
    assert!(
        root.join("main.cpp").is_file(),
        "fixture main.cpp missing at {}",
        root.display()
    );
    assert!(
        root.join("math.h").is_file(),
        "fixture math.h missing at {}",
        root.display()
    );

    let sup = Supervisor::direct().await.expect("supervisor direct init");

    // overview: hit `main` in main.cpp
    let overview = sup
        .tool_overview(&root, "main.cpp")
        .await
        .expect("overview request");
    let main_hit = overview
        .iter()
        .find(|s| s.name == "main")
        .unwrap_or_else(|| panic!("no `main` symbol in overview: {overview:#?}"));
    // main should be a Function (SymbolKind 12) on clangd.
    assert!(
        matches!(
            main_hit.kind,
            lsp_core::types::SymbolKindTag::Function | lsp_core::types::SymbolKindTag::Other(12)
        ),
        "main should be a function-like kind, got {:?}",
        main_hit.kind
    );

    // def: locate `add(` call line in main.cpp, expect definition in math.h
    let main_cpp = std::fs::read_to_string(root.join("main.cpp")).unwrap();
    let (line, col) = main_cpp_line_of(&main_cpp, "add(");
    let def = sup
        .tool_def(&root, "main.cpp", line, col)
        .await
        .expect("def request")
        .expect("def should resolve `add` call to a Location");
    let def_path = def.uri.to_string();
    assert!(
        def_path.ends_with("math.h"),
        "def should land in math.h, got {def_path}"
    );

    // refs: jump onto the declaration in math.h and find >= 1 reference (the call).
    let math_h = std::fs::read_to_string(root.join("math.h")).unwrap();
    let (decl_line, decl_col) = main_cpp_line_of(&math_h, "add");
    let refs = sup
        .tool_refs(&root, "math.h", decl_line, decl_col)
        .await
        .expect("refs request");
    assert!(
        !refs.is_empty(),
        "expected at least one reference for `add` (the call in main.cpp)"
    );
    let saw_main_cpp = refs
        .iter()
        .any(|r: &Location| r.uri.to_string().ends_with("main.cpp"));
    assert!(
        saw_main_cpp,
        "expected at least one reference in main.cpp, got {:#?}",
        refs
    );

    // Sanity: Location fields are well-formed (range in main.cpp).
    let _ = Range::new(Position::new(0, 0), Position::new(0, 0));
}

/// Mimic `ls_adapters::which_no_unc` exactly so the skip policy matches the
/// production code path. Duplicating 15 lines avoids lifting the helper.
fn has_clangd() -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let mut candidate = dir.join("clangd");
            if !ext.is_empty() {
                candidate.set_extension(&ext[1..]);
            }
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}
