//! Task 22 rename_symbol e2e —— C++ 类成员跨文件重命名。

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
    let dir = std::env::temp_dir().join(format!("serena-t22-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // header: 声明 foo。
    std::fs::write(dir.join("foo.h"), "int foo(int x);\n").unwrap();
    // foo.cpp: 定义。
    std::fs::write(
        dir.join("foo.cpp"),
        "#include \"foo.h\"\nint foo(int x) { return x + 1; }\n",
    )
    .unwrap();
    // caller.cpp: 用 foo。
    std::fs::write(
        dir.join("caller.cpp"),
        "#include \"foo.h\"\nint caller() { return foo(5); }\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_foo_to_bar_across_files() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("rename");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 触发索引：开所有相关 cpp/h 文件让 clangd 把 TU 都加进索引。
    for f in ["foo.h", "foo.cpp", "caller.cpp"] {
        let _ = sup.tool_overview(&root, f).await.expect("overview");
    }

    let report = sup
        .tool_rename_symbol(&root, "foo.h", 0, 4, "bar")
        .await
        .expect("rename");
    assert!(
        report.files_modified >= 2,
        "should modify >=2 files, got {report:?}"
    );
    assert!(
        report.edits_applied >= 3,
        "should apply >=3 edits, got {report:?}"
    );

    let header = std::fs::read_to_string(root.join("foo.h")).unwrap();
    let impl_ = std::fs::read_to_string(root.join("foo.cpp")).unwrap();
    let caller = std::fs::read_to_string(root.join("caller.cpp")).unwrap();
    for (name, content) in [
        ("foo.h", &header),
        ("foo.cpp", &impl_),
        ("caller.cpp", &caller),
    ] {
        // 检查 `foo(` 或 `foo ` 应该不存在（`bar(` 取代）。
        assert!(
            !content.contains("foo(") && !content.contains(" foo "),
            "{name} still has foo: {content}"
        );
        assert!(content.contains("bar"), "{name} should have bar: {content}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_new_name_returns_bad_args() {
    let root = scratch("empty");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup.tool_overview(&root, "foo.h").await.expect("overview");

    let err = sup
        .tool_rename_symbol(&root, "foo.h", 0, 4, "")
        .await
        .expect_err("empty new_name");
    assert!(
        matches!(err, supervisor::ToolError::BadArgs { .. }),
        "got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whitespace_new_name_returns_bad_args() {
    let root = scratch("ws");
    let sup = Supervisor::direct().await.expect("supervisor");
    let _ = sup.tool_overview(&root, "foo.h").await.expect("overview");

    let err = sup
        .tool_rename_symbol(&root, "foo.h", 0, 4, "bad name")
        .await
        .expect_err("whitespace new_name");
    assert!(
        matches!(err, supervisor::ToolError::BadArgs { .. }),
        "got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
