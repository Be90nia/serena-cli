//! Task 23 e2e: read_file / list_dir / find_file
//!
//! 纯 fs 工具——不需要 clangd。验证：
//! - read_file 行切片 + 越界检查
//! - list_dir max_depth + 排除 ignore
//! - find_file glob 模式 + path_glob

use std::fs;

use supervisor::fs_tools;

fn dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("serena-t23-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "current_thread")]
async fn read_file_full_returns_all_lines() {
    let d = dir("full");
    let p = d.join("a.txt");
    fs::write(&p, "l1\nl2\nl3\nl4\nl5\n").unwrap();
    let r = fs_tools::read_file(&d, "a.txt", None, None).await.unwrap();
    assert_eq!(r.content, "l1\nl2\nl3\nl4\nl5");
    assert_eq!(r.total_lines, 5);
    assert_eq!(r.start_line, 1);
    assert_eq!(r.end_line, 5);
}

#[tokio::test(flavor = "current_thread")]
async fn read_file_slice_by_line_range() {
    let d = dir("slice");
    let p = d.join("a.txt");
    fs::write(&p, "l1\nl2\nl3\nl4\nl5\n").unwrap();
    let r = fs_tools::read_file(&d, "a.txt", Some(2), Some(4))
        .await
        .unwrap();
    assert_eq!(r.content, "l2\nl3\nl4");
    assert_eq!(r.start_line, 2);
    assert_eq!(r.end_line, 4);
}

#[tokio::test(flavor = "current_thread")]
async fn read_file_out_of_range_returns_bad_args() {
    let d = dir("oor");
    fs::write(d.join("a.txt"), "l1\nl2\n").unwrap();
    let err = fs_tools::read_file(&d, "a.txt", Some(1), Some(99))
        .await
        .unwrap_err();
    assert!(
        matches!(err, fs_tools::FsError::BadArgs { .. }),
        "got {err:?}"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn read_file_escape_root_rejected() {
    let d = dir("escape");
    fs::write(d.join("a.txt"), "x").unwrap();
    // 路径必须解析后落在 root 外（用绝对路径跨 root）。
    let other = std::env::temp_dir().join("serena-t23-other-not-exist");
    let _ = fs::create_dir_all(&other);
    let err = fs_tools::read_file(&d, other.to_str().unwrap(), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, fs_tools::FsError::BadArgs { .. }),
        "expected BadArgs, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_dir_recursive_with_max_depth() {
    let d = dir("list");
    fs::create_dir_all(d.join("a/b/c")).unwrap();
    fs::write(d.join("a/file1.txt"), "x").unwrap();
    fs::write(d.join("a/b/file2.txt"), "x").unwrap();
    fs::write(d.join("a/b/c/file3.txt"), "x").unwrap();
    fs::write(d.join("top.txt"), "x").unwrap();

    // max_depth=1：root + 顶层第一层 = `a/ + top.txt`，不含 a/file1.txt。
    let r = fs_tools::list_dir(&d, ".", Some(1), 100).unwrap();
    let paths: Vec<_> = r.iter().map(|e| e.path.as_str()).collect();

    assert!(paths.contains(&"top.txt"), "got {paths:?}");
    assert!(paths.contains(&"a"), "got {paths:?}");
    assert!(
        !paths.contains(&"a/file1.txt"),
        "depth 1 should NOT include a/file1.txt (depth 2), got {paths:?}"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn list_dir_respects_ignore() {
    let d = dir("ignore");
    fs::create_dir_all(d.join("build")).unwrap();
    fs::write(d.join("build/ignored.txt"), "x").unwrap();
    fs::write(d.join("keep.txt"), "x").unwrap();
    fs::write(d.join(".ignore"), "build/\n").unwrap();

    let r = fs_tools::list_dir(&d, ".", None, 100).unwrap();
    let paths: Vec<_> = r.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"keep.txt"), "got {paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("build/")),
        "build/ should be excluded, got {paths:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn find_file_glob_matches_names() {
    let d = dir("find");
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("src/main.rs"), "").unwrap();
    fs::write(d.join("src/lib.rs"), "").unwrap();
    fs::write(d.join("src/test.py"), "").unwrap();
    fs::write(d.join("README.md"), "").unwrap();

    let hits = fs_tools::find_file(&d, "*.rs", None, 100).unwrap();
    assert_eq!(hits.len(), 2, "got {hits:?}");
    assert!(hits.iter().any(|p| p.ends_with("main.rs")), "got {hits:?}");
    assert!(hits.iter().any(|p| p.ends_with("lib.rs")), "got {hits:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn find_file_invalid_glob_returns_glob_err() {
    let d = dir("find-bad");
    let err = fs_tools::find_file(&d, "[invalid", None, 100).unwrap_err();
    assert!(matches!(err, fs_tools::FsError::Glob { .. }), "got {err:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn find_file_respects_max_results() {
    let d = dir("cap");
    for i in 0..10 {
        fs::write(d.join(format!("f{i}.txt")), "").unwrap();
    }
    let hits = fs_tools::find_file(&d, "*.txt", None, 3).unwrap();
    assert_eq!(hits.len(), 3, "should cap at 3, got {hits:?}");
}

#[test]
fn should_ignore_builtin_dirs() {
    for name in [
        "node_modules",
        "target",
        "venv",
        ".venv",
        ".git",
        "__pycache__",
        "dist",
        "build",
        ".mypy_cache",
    ] {
        assert!(fs_tools::should_ignore(name), "{name} should be ignored");
    }
    for name in ["src", "main.rs", "output", "coverage-report", ".github"] {
        assert!(
            !fs_tools::should_ignore(name),
            "{name} should NOT be ignored"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn list_dir_skips_builtin_ignore_dirs() {
    let d = dir("ignore33-list");
    fs::create_dir_all(d.join("node_modules/pkg")).unwrap();
    fs::write(d.join("node_modules/pkg/index.js"), "x").unwrap();
    fs::write(d.join("keep.txt"), "x").unwrap();

    let r = fs_tools::list_dir(&d, ".", None, 100).unwrap();
    let paths: Vec<_> = r.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"keep.txt"), "got {paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("node_modules")),
        "node_modules should be excluded, got {paths:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn find_file_skips_builtin_ignore_dirs() {
    let d = dir("ignore33-find");
    fs::create_dir_all(d.join("target/debug")).unwrap();
    fs::create_dir_all(d.join("src")).unwrap();
    fs::write(d.join("target/debug/foo.rs"), "x").unwrap();
    fs::write(d.join("src/foo.rs"), "x").unwrap();

    let hits = fs_tools::find_file(&d, "foo.rs", None, 100).unwrap();
    assert_eq!(hits.len(), 1, "only src/foo.rs expected, got {hits:?}");
    assert!(
        hits[0].replace('\\', "/").contains("src/foo.rs"),
        "got {hits:?}"
    );
}
