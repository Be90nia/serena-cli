//! Task 19 search_for_pattern 单元测试（无需 clangd）。

use std::path::PathBuf;

use supervisor::{Supervisor, ToolError};

async fn new_sup() -> Supervisor {
    Supervisor::direct().await.expect("supervisor init")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-t19-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn basic_match_returns_hits_with_correct_columns() {
    let root = scratch("basic");
    std::fs::write(root.join("a.cpp"), "TODO: hello world\nTODO: second line\n").unwrap();
    std::fs::write(root.join("b.cpp"), "func() {}\n").unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "TODO", None, 100, false)
        .await
        .expect("search ok");
    assert_eq!(resp.hits.len(), 2);
    assert!(resp.hits[0].text.contains("TODO: hello world"));
    assert_eq!(resp.hits[0].line, 1);
    assert_eq!(resp.hits[0].col, 1); // 1-based
    assert_eq!(resp.hits[1].line, 2);
}

#[tokio::test]
async fn case_sensitive_flag_filters_correctly() {
    let root = scratch("case");
    std::fs::write(root.join("a.txt"), "Foo foo FOO\n").unwrap();

    let sup = new_sup().await;
    let resp_cs = sup
        .tool_search_for_pattern(&root, "foo", None, 100, true)
        .await
        .unwrap();
    assert_eq!(resp_cs.hits.len(), 1, "cs should only match lowercase foo");

    let resp_ci = sup
        .tool_search_for_pattern(&root, "foo", None, 100, false)
        .await
        .unwrap();
    assert_eq!(resp_ci.hits.len(), 3, "ci matches all 3 cases");
}

#[tokio::test]
async fn path_glob_filters_files() {
    let root = scratch("glob");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.cpp"), "TODO here\n").unwrap();
    std::fs::write(root.join("b.cpp"), "TODO here\n").unwrap();
    std::fs::write(root.join("c.txt"), "TODO here\n").unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "TODO", Some("*.cpp"), 100, false)
        .await
        .unwrap();
    let files: std::collections::HashSet<_> = resp.hits.iter().map(|h| h.file.clone()).collect();
    assert!(files.contains("src/a.cpp"));
    assert!(files.contains("b.cpp"));
    assert!(!files.contains("c.txt"));
}

#[tokio::test]
async fn max_results_truncates() {
    let root = scratch("trunc");
    let lines: String = (0..50).map(|i| format!("match_{i}\n")).collect();
    std::fs::write(root.join("a.txt"), &lines).unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "match_", None, 10, false)
        .await
        .unwrap();
    assert_eq!(resp.hits.len(), 10);
    assert!(resp.truncated);
}

#[tokio::test]
async fn bad_regex_returns_bad_args() {
    let root = scratch("badre");
    std::fs::write(root.join("a.txt"), "ok\n").unwrap();

    let sup = new_sup().await;
    let err = sup
        .tool_search_for_pattern(&root, "[unclosed", None, 100, false)
        .await
        .expect_err("bad regex");
    assert!(matches!(err, ToolError::BadArgs { .. }), "got {err:?}");
}

#[tokio::test]
async fn binary_file_skipped_silently() {
    let root = scratch("binary");
    std::fs::write(root.join("text.txt"), "match here\n").unwrap();
    std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00, 0x01, 0x02]).unwrap();


    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "match", None, 100, false)
        .await
        .unwrap();
    // 只 hit 文本文件。
    assert_eq!(resp.hits.len(), 1);
    assert!(resp.hits[0].file.ends_with("text.txt"));
}

#[tokio::test]
async fn respects_gitignore() {
    let root = scratch("gitignore");
    std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    std::fs::create_dir_all(root.join("ignored")).unwrap();
    std::fs::write(root.join("ignored/skip.txt"), "needle\n").unwrap();
    std::fs::write(root.join("keep.txt"), "needle\n").unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "needle", None, 100, false)
        .await
        .unwrap();
    let files: Vec<_> = resp.hits.iter().map(|h| h.file.as_str()).collect();
    assert!(files.contains(&"keep.txt"));
    assert!(
        !files.iter().any(|f| f.contains("ignored")),
        "gitignored files leaked: {files:?}"
    );
}

#[tokio::test]
async fn match_start_end_offsets_are_correct() {
    let root = scratch("offsets");
    std::fs::write(root.join("a.txt"), "say hello world\n").unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "hello", None, 100, false)
        .await
        .unwrap();
    assert_eq!(resp.hits.len(), 1);
    let h = &resp.hits[0];
    assert_eq!(h.match_start, 4);
    assert_eq!(h.match_end, 9);
    assert_eq!(h.text, "say hello world");
}
