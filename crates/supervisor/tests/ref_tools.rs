//! Task 24 e2e: find_referencing_symbols / find_referencing_code_snippets
//!
//! 走真实 clangd（mock_ls 不支持 references）。

use std::path::PathBuf;

use supervisor::Supervisor;

fn has_clangd() -> bool {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        return false; // CI: skip real-LS e2e (3rd-party LS version drift; covered locally/nightly)
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
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

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-t24-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // foo.h: 声明。
    std::fs::write(dir.join("foo.h"), "int foo(int x);\n").unwrap();
    // foo.cpp: 定义 + 一个 caller（用 foo）。
    std::fs::write(
        dir.join("foo.cpp"),
        "#include \"foo.h\"\nint foo(int x) { return x + 1; }\nint caller_a() { return foo(5); }\n",
    )
    .unwrap();
    // caller.cpp: 另一个 caller。
    std::fs::write(
        dir.join("caller.cpp"),
        "#include \"foo.h\"\nint caller_b() { return foo(7); }\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_referencing_symbols_returns_callers_with_containers() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("refs-syms");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 触发索引：开所有相关 cpp/h。
    for f in ["foo.h", "foo.cpp", "caller.cpp"] {
        let _ = sup.tool_overview(&root, f, None).await.expect("overview");
    }

    // 在 foo.h:1 "foo" 第一次出现处（col=5，0-based=4）。
    let hits = sup
        .tool_referencing_symbols(&root, "foo.h", 0, 4, None)
        .await
        .expect("refs syms");

    assert!(!hits.is_empty(), "expected some refs, got 0");
    // 至少一个 ref 应该落在 caller_a（foo.cpp）或 caller_b（caller.cpp）。
    let has_caller = hits
        .iter()
        .any(|h| h.container_name == "caller_a" || h.container_name == "caller_b");
    assert!(
        has_caller,
        "expected at least one caller_a/caller_b container, got {hits:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_referencing_code_snippets_returns_snippets() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("refs-snips");
    let sup = Supervisor::direct().await.expect("supervisor");

    for f in ["foo.h", "foo.cpp", "caller.cpp"] {
        let _ = sup.tool_overview(&root, f, None).await.expect("overview");
    }

    let hits = sup
        .tool_referencing_code_snippets(&root, "foo.h", 0, 4, 2, 50, None)
        .await
        .expect("refs snips");

    assert!(!hits.is_empty(), "expected some snippets, got 0");
    // 每个 snippet 都应该有非空 text。
    for h in &hits {
        assert!(!h.text.is_empty(), "empty text: {h:?}");
        assert!(h.snippet.lines().count() >= 1);
    }
    // 至少一个 snippet 应该包含 "foo(" 字符串。
    let has_foo = hits.iter().any(|h| h.text.contains("foo"));
    assert!(
        has_foo,
        "expected at least one ref with `foo`, got {hits:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
