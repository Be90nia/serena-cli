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
    let resp = sup.tool_search_for_pattern(&root, "TODO", None, 100, false, &[], false)
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
    let resp_cs = sup.tool_search_for_pattern(&root, "foo", None, 100, true, &[], false)
        .await
        .unwrap();
    assert_eq!(resp_cs.hits.len(), 1, "cs should only match lowercase foo");

    let resp_ci = sup.tool_search_for_pattern(&root, "foo", None, 100, false, &[], false)
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
    let resp = sup.tool_search_for_pattern(&root, "TODO", Some("*.cpp"), 100, false, &[], false)
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
    let resp = sup.tool_search_for_pattern(&root, "match_", None, 10, false, &[], false)
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
    let err = sup.tool_search_for_pattern(&root, "[unclosed", None, 100, false, &[], false)
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
    let resp = sup.tool_search_for_pattern(&root, "match", None, 100, false, &[], false)
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
    let resp = sup.tool_search_for_pattern(&root, "needle", None, 100, false, &[], false)
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
    let resp = sup.tool_search_for_pattern(&root, "hello", None, 100, false, &[], false)
        .await
        .unwrap();
    assert_eq!(resp.hits.len(), 1);
    let h = &resp.hits[0];
    assert_eq!(h.match_start, 4);
    assert_eq!(h.match_end, 9);
    assert_eq!(h.text, "say hello world");
}

// ==== 批1-B：search 默认防噪 ====

/// 批1-B③：合法 UTF-8 但含 NUL 字节 = 二进制，跳过（ripgrep 同款判定）。
#[tokio::test]
async fn nul_byte_utf8_file_skipped() {
    let root = scratch("nulutf8");
    std::fs::write(root.join("keep.txt"), "needle here\n").unwrap();
    // UTF-8 合法但藏 NUL —— read_to_string 成功，必须靠 NUL 检测排除。
    std::fs::write(root.join("nul.bin"), "needle\x00here\n").unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "needle", None, 50, false, &[], false)
        .await
        .unwrap();
    assert_eq!(resp.hits.len(), 1, "NUL 文件必须跳过: {resp:?}");
    assert!(resp.hits[0].file.ends_with("keep.txt"));
}

/// 批1-B③：>1MB 大文件跳过（盲测实锤大文件是字节坑）。
#[tokio::test]
async fn big_file_skipped() {
    let root = scratch("bigfile");
    std::fs::write(root.join("small.txt"), "needle\n").unwrap();
    // 1.2MB 全是 needle 的文件 —— 修前会贡献海量命中。
    let big: String = std::iter::repeat_n("needle\n", 200_000).collect();
    assert!(big.len() > 1024 * 1024);
    std::fs::write(root.join("big.txt"), &big).unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "needle", None, 50, false, &[], false)
        .await
        .unwrap();
    assert_eq!(
        resp.hits.iter().filter(|h| h.file.ends_with("small.txt")).count(),
        1
    );
    assert!(
        !resp.hits.iter().any(|h| h.file.ends_with("big.txt")),
        ">1MB 文件必须跳过"
    );
}

/// 批1-B②：默认上限语义 —— max_results=50 截断时 truncated + hint 同现。
#[tokio::test]
async fn default_cap_truncates_with_hint() {
    let root = scratch("cap50");
    let lines: String = (0..60).map(|i| format!("match_{i}\n")).collect();
    std::fs::write(root.join("a.txt"), &lines).unwrap();

    let sup = new_sup().await;
    let resp = sup
        .tool_search_for_pattern(&root, "match_", None, 50, false, &[], false)
        .await
        .unwrap();
    assert_eq!(resp.hits.len(), 50, "默认上限 50");
    assert!(resp.truncated);
    assert_eq!(
        resp.hint.as_deref(),
        Some("add --path-glob / --max-results"),
        "截断必须带降噪 hint"
    );
    // 未截断时 hint 不出现（wire 零扰动）。
    let clean = sup
        .tool_search_for_pattern(&root, "nomatch", None, 50, false, &[], false)
        .await
        .unwrap();
    assert!(clean.hint.is_none(), "未截断不得带 hint: {clean:?}");
}
