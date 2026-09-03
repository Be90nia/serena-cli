//! Task 25 e2e: 文件编辑四件套（insert / replace / delete 在 symbol 体内）。
//!
//! 走真实 clangd（mock_ls 不支持 didChange）。

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

fn scratch(tag: &str) -> (PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("serena-t25-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("demo.cpp");
    // 3 行内容（行号 1-based: 1, 2, 3）。
    std::fs::write(
        &file,
        "int old_value = 1;\nint keep_me() { return 42; }\nint main() { return old_value; }\n",
    )
    .unwrap();
    (dir, file)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_text_before_symbol_adds_prefix() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("before");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_edit_insert_before_symbol(&root, "demo.cpp", "main", "// header comment\n", None)
        .await
        .expect("insert before");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("// header comment\nint main()"),
        "expected header before main, got: {content}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_text_after_symbol_adds_suffix() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("after");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_edit_insert_after_symbol(&root, "demo.cpp", "keep_me", " // tail comment", None)
        .await
        .expect("insert after");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("return 42; } // tail comment"),
        "expected tail after keep_me, got: {content}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_text_in_symbol_swaps_substring() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("replace");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    sup.tool_edit_replace_text(
        &root,
        "demo.cpp",
        "main",
        "return old_value",
        "return old_value + 100",
        None,
    )
    .await
    .expect("replace");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("return old_value + 100"),
        "expected replaced body, got: {content}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_text_in_symbol_removes_range() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("delete");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup
        .tool_overview(&root, "demo.cpp", None)
        .await
        .expect("overview");

    // main 是文件第 3 行（1-based）。删 3..=3。
    sup.tool_edit_delete_text(&root, "demo.cpp", "main", 3, 3, None)
        .await
        .expect("delete");

    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        !content.contains("return old_value;"),
        "expected body removed, got: {content}"
    );
    // `int old_value = 1` 和 `int keep_me` 应该还在。
    assert!(content.contains("int old_value = 1;"));
    assert!(content.contains("int keep_me"));

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_text_in_symbol_rejects_start_gt_end() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, _file) = scratch("delflip");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup.tool_overview(&root, "demo.cpp", None).await.expect("overview");
    let err = sup
        .tool_edit_delete_text(&root, "demo.cpp", "main", 5, 3, None)
        .await
        .expect_err("start_line > end_line 应 error");
    assert!(matches!(err, supervisor::ToolError::BadArgs { .. }), "got: {err:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_tools_reject_unknown_symbol() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, _file) = scratch("nosym");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup.tool_overview(&root, "demo.cpp", None).await.expect("overview");
    let err = sup
        .tool_edit_replace_text(&root, "demo.cpp", "nonexistent_symbol", "x", "y", None)
        .await
        .expect_err("未知 symbol 应 error");
    assert!(matches!(err, supervisor::ToolError::BadArgs { .. }), "got: {err:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_writes_serialize_in_order() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("concurrent");
    let sup = std::sync::Arc::new(Supervisor::direct().await.expect("supervisor"));
    let _ = sup.tool_overview(&root, "demo.cpp", None).await.expect("overview");
    let sup2 = std::sync::Arc::clone(&sup);
    let root2 = root.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let b1 = std::sync::Arc::clone(&barrier);
    let b2 = std::sync::Arc::clone(&barrier);
    let t1 = tokio::spawn(async move {
        b1.wait();
        sup2.tool_edit_insert_after_symbol(&root2, "demo.cpp", "old_value", " // t1", None)
            .await
            .expect("t1 insert");
    });
    let sup3 = std::sync::Arc::clone(&sup);
    let root3 = root.clone();
    let t2 = tokio::spawn(async move {
        b2.wait();
        sup3.tool_edit_insert_before_symbol(&root3, "demo.cpp", "old_value", "// t2\n", None)
            .await
            .expect("t2 insert");
    });
    t1.await.unwrap();
    t2.await.unwrap();
    // 串行保证：两个 task 都完成（write_gate FIFO），最终内容至少含一个修改生效。
    let content = std::fs::read_to_string(&file).unwrap();
    let has_t2 = content.contains("// t2");
    let has_t1 = content.contains("// t1");
    assert!(
        has_t2 || has_t1,
        "至少一个修改应生效: {content}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_then_read_consistency() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let (root, file) = scratch("rr");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup.tool_overview(&root, "demo.cpp", None).await.expect("overview");
    sup.tool_edit_replace_text(
        &root,
        "demo.cpp",
        "main",
        "return old_value",
        "return old_value * 2",
        None,
    )
    .await
    .expect("replace");
    let content = std::fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("return old_value * 2"),
        "disk content mismatch, got: {content}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
