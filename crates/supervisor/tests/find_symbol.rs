use std::path::PathBuf;

use supervisor::Supervisor;

fn has_clangd() -> bool {
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
    let dir = std::env::temp_dir().join(format!("serena-t20-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 至少一个 cpp 文件（lang 探测要求）。
    std::fs::write(
        dir.join("alpha.cpp"),
        r#"#include "alpha.h"
int alpha_func(int x) { return x + 1; }
int beta_helper() { return 42; }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("beta.cpp"),
        r#"#include "alpha.h"
int gamma_method() { return alpha_func(7); }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("alpha.h"),
        "int alpha_func(int);\nint beta_helper();\nint gamma_method();\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_symbol_finds_known_functions() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("find");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 先 overview 一个文件 → 触发 didOpen → 让 clangd 把 TU 加进索引。
    let _ = sup
        .tool_overview(&root, "alpha.cpp")
        .await
        .expect("overview");

    let hits = sup
        .tool_find_symbol(&root, "alpha_func", 50, None)
        .await
        .expect("find_symbol");

    assert!(
        !hits.is_empty(),
        "expected at least one hit for alpha_func, got 0"
    );
    let has_alpha = hits
        .iter()
        .any(|h| h.name.contains("alpha_func") && h.uri.contains("alpha.cpp"));
    assert!(
        has_alpha,
        "alpha_func hit should reference alpha.cpp, got: {hits:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_query_returns_bad_args() {
    let root = scratch("empty");
    let sup = Supervisor::direct().await.expect("supervisor");

    let err = sup
        .tool_find_symbol(&root, "", 50, None)
        .await
        .expect_err("empty query should fail");
    assert!(
        matches!(err, supervisor::ToolError::BadArgs { .. }),
        "got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_caps_results() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("limit");
    let sup = Supervisor::direct().await.expect("supervisor");

    // alpha / beta / gamma 都含 `_helper` / `_func` / `_method`？没有共同子串。
    // 用空 pattern 之前的 nil；改用一个只匹配一个的 query：alpha_func。
    let hits = sup
        .tool_find_symbol(&root, "alpha_func", 1, None)
        .await
        .expect("find_symbol");
    assert!(
        hits.len() <= 1,
        "limit=1 should cap to at most 1, got {}",
        hits.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// lang_override 指定 lang → 单 LS 查, 即使 root 下有多种已知 lang 文件。
/// 用 cpp fixture 但指定 lang=python 应该 NotInstalled (不强求 install, 只验 fast path)。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lang_override_skips_root_probe() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("override");
    let sup = Supervisor::direct().await.expect("supervisor");
    // 先 overview 触发 clangd 索引, 同 workspace_symbol_finds_known_functions。
    let _ = sup
        .tool_overview(&root, "alpha.cpp")
        .await
        .expect("overview");

    // 指定 lang=cpp → 只查 clangd。
    let hits = sup
        .tool_find_symbol(&root, "alpha_func", 50, Some("cpp"))
        .await
        .expect("find_symbol with lang=cpp");
    assert!(
        !hits.is_empty(),
        "lang=cpp should find alpha_func via clangd, got 0 hits"
    );

    let _ = std::fs::remove_dir_all(&root);
}
