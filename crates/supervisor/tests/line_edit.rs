//! 编辑闭环补全 e2e：行级三件套（insert-at-line / replace-lines / delete-lines）
//! + safe-delete-symbol。
//!
//! 走真实 clangd（mock_ls 不支持 didChange / references）。

use std::path::PathBuf;

use supervisor::ToolError;

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

/// 三行 scratch：1=`int old_value = 1;` 2=`int keep_me() {...}` 3=`int main() {...}`。
fn scratch(tag: &str) -> (PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("serena-t27-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("demo.cpp");
    std::fs::write(
        &file,
        "int old_value = 1;\nint keep_me() { return 42; }\nint main() { return old_value; }\n",
    )
    .unwrap();
    (dir, file)
}

/// safe-delete fixture：orphan 无引用；used 被 wrapper 引用。
fn scratch_safe_delete(tag: &str) -> (PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("serena-t27-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("demo.cpp");
    std::fs::write(
        &file,
        "int orphan_fn() { return 1; }\nint used_fn() { return 2; }\nint wrapper() { return used_fn(); }\n",
    )
    .unwrap();
    (dir, file)
}

// ============ 行级三件套 ============

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_at_line_pushes_lines_down() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("ins");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_insert_at_line(&root, "demo.cpp", 2, "int inserted = 7;", None, None)
        .await
        .expect("insert_at_line");

    let content = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        content,
        "int old_value = 1;\nint inserted = 7;\nint keep_me() { return 42; }\nint main() { return old_value; }\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_lines_swaps_range() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("rep");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_replace_lines(
        &root,
        "demo.cpp",
        2,
        2,
        "int swapped() { return 0; }",
        None,
        None,
    )
    .await
    .expect("replace_lines");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("int swapped() { return 0; }") && !content.contains("keep_me"),
        "line 2 应被替换: {content}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_lines_removes_range() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("del");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_delete_lines(&root, "demo.cpp", 1, 1, None, None)
        .await
        .expect("delete_lines");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        !content.contains("old_value = 1") && content.starts_with("int keep_me()"),
        "第 1 行应被删除: {content}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_edit_out_of_bounds_rejected() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, _file) = scratch("oob");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    // 3 行文件：insert 允许到 4（EOF 追加），5 越界；delete 4 越界。
    let err = sup
        .tool_insert_at_line(&root, "demo.cpp", 5, "x", None, None)
        .await
        .expect_err("insert 越界应报错");
    assert!(matches!(err, ToolError::BadArgs { .. }), "{err}");

    let err = sup
        .tool_delete_lines(&root, "demo.cpp", 2, 4, None, None)
        .await
        .expect_err("delete 越界应报错");
    assert!(matches!(err, ToolError::BadArgs { .. }), "{err}");

    let err = sup
        .tool_replace_lines(&root, "demo.cpp", 0, 1, "x", None, None)
        .await
        .expect_err("replace 0 行应报错");
    assert!(matches!(err, ToolError::BadArgs { .. }), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_edit_hash_mismatch_rejects_write() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, _file) = scratch("hash");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    // read-file 拿真 hash 作对照；用过期/伪造 hash 写必拒（C3 对账拒写）。
    let report = supervisor::fs_tools::read_file(&root, "demo.cpp", None, None)
        .await
        .expect("read_file");
    assert!(!report.hash.is_empty(), "read-file 应返回 hash");
    let stale_hash = "deadbeefdeadbeef";
    assert_ne!(stale_hash, report.hash);
    let err = sup
        .tool_delete_lines(&root, "demo.cpp", 1, 1, Some(stale_hash), None)
        .await
        .expect_err("hash 失配应拒写");
    assert!(
        matches!(err, ToolError::WriteConflict { .. }),
        "期望 WriteConflict，实际: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ============ safe-delete ============

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_delete_removes_unreferenced_symbol() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch_safe_delete("sd1");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    let report = sup
        .tool_safe_delete_symbol(&root, "demo.cpp", "orphan_fn", None)
        .await
        .expect("safe_delete orphan");
    assert!(report.deleted, "无引用符号应被删除");
    assert!(report.references.is_empty());

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        !content.contains("orphan_fn"),
        "orphan_fn 应已删除: {content}"
    );
    assert!(content.contains("used_fn"), "其他符号不应受影响: {content}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_delete_refuses_referenced_symbol_with_locations() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch_safe_delete("sd2");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    let report = sup
        .tool_safe_delete_symbol(&root, "demo.cpp", "used_fn", None)
        .await
        .expect("safe_delete used_fn");
    assert!(!report.deleted, "有引用符号应拒删");
    assert!(
        report
            .references
            .iter()
            .any(|r| r.file == "demo.cpp" && r.line == 3),
        "应报告 demo.cpp:3 的引用，实际: {:?}",
        report.references
    );

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(content.contains("used_fn"), "拒删时文件不应变化");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_delete_unknown_symbol_rejected() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, _file) = scratch_safe_delete("sd3");
    let sup = supervisor::Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    let err = sup
        .tool_safe_delete_symbol(&root, "demo.cpp", "no_such_symbol", None)
        .await
        .expect_err("未知符号应报错");
    assert!(matches!(err, ToolError::BadArgs { .. }), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}
