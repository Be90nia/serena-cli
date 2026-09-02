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
        .tool_overview(&root, "demo.cpp")
        .await
        .expect("overview");

    sup.tool_edit_insert_before_symbol(&root, "demo.cpp", "main", "// header comment\n")
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
        .tool_overview(&root, "demo.cpp")
        .await
        .expect("overview");

    sup.tool_edit_insert_after_symbol(&root, "demo.cpp", "keep_me", " // tail comment")
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
        .tool_overview(&root, "demo.cpp")
        .await
        .expect("overview");

    sup.tool_edit_replace_text(
        &root,
        "demo.cpp",
        "main",
        "return old_value",
        "return old_value + 100",
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
        .tool_overview(&root, "demo.cpp")
        .await
        .expect("overview");

    // main 是文件第 3 行（1-based）。删 3..=3。
    sup.tool_edit_delete_text(&root, "demo.cpp", "main", 3, 3)
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
