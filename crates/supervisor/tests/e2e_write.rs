//! Task 15 write_gate + symbol-body + replace-body e2e（PLAN §300）。
//!
//! 真 clangd + fixture 副本（scratch 目录），同 e2e_direct 策略：无 clangd 优雅 skip。
//! fixture 增加 impl.cpp 提供 `add` 的**定义**（math.h 只有声明——声明上 replace 无意义）。
//!
//! 测试 ×4：
//! 1. symbol-body 正常切片（impl.cpp 里 add 定义体）
//! 2. replace-body 正常替换 + 读回一致 + LS 同步
//! 3. 并发外部改盘 → 成功替换或明确 WRITE_CONFLICT，文件不半写
//! 4. 并发两写串行化（写门 FIFO，不混写）

use std::path::{Path, PathBuf};
use std::sync::Arc;

use supervisor::Supervisor;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn fixtures_root() -> PathBuf {
    workspace_root().join("fixtures").join("cpp_demo")
}

/// 与 e2e_direct.rs 同款 PATH + LLVM 标准路径探测。
fn has_clangd() -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    if let Some(path_var) = std::env::var_os("PATH") {
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

/// fixture 副本 + impl.cpp（add 的定义体），不污染仓库 fixture。
fn scratch_fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-t15-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    for f in ["main.cpp", "math.h"] {
        std::fs::copy(fixtures_root().join(f), dir.join(f)).expect("copy fixture");
    }
    std::fs::write(
        dir.join("compile_flags.txt"),
        std::fs::read_to_string(fixtures_root().join("compile_flags.txt")).unwrap_or_default(),
    )
    .ok();
    std::fs::write(
        dir.join("impl.cpp"),
        "#include \"math.h\"\n\nint add(int a, int b) {\n    return a + b;\n}\n",
    )
    .expect("write impl.cpp");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_body_returns_add_definition_body() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch_fixture("body");
    let sup = Supervisor::direct().await.expect("supervisor");

    let body = sup
        .tool_symbol_body(&root, "impl.cpp", "add", None)
        .await
        .expect("symbol-body add");
    assert!(
        body.contains("return a + b"),
        "add 定义体应含 return a + b；实际:\n{body}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_body_updates_file_and_ls_syncs() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch_fixture("replace");
    let sup = Supervisor::direct().await.expect("supervisor");

    let new_body = "int add(int a, int b) {\n    // replaced by test\n    return a + b + 0;\n}\n";
    sup.tool_replace_body(&root, "impl.cpp", "add", new_body, None)
        .await
        .expect("replace-body");

    let on_disk = std::fs::read_to_string(root.join("impl.cpp")).expect("read back");
    assert!(
        on_disk.contains("// replaced by test"),
        "盘上应有替换后的标记；实际:\n{on_disk}"
    );

    // 再取 symbol-body —— LS 状态应与盘一致（didChange 全量已同步）。
    let body_after = sup
        .tool_symbol_body(&root, "impl.cpp", "add", None)
        .await
        .expect("symbol-body after replace");
    assert!(
        body_after.contains("// replaced by test"),
        "LS 侧符号体应同步更新；实际:\n{body_after}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_body_handles_external_disk_write() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch_fixture("conflict");
    let sup = Arc::new(Supervisor::direct().await.expect("supervisor"));

    // baseline：一次成功替换。
    sup.tool_replace_body(
        &root,
        "impl.cpp",
        "add",
        "int add(int a, int b) {\n    return a + b;\n}\n",
        None,
    )
    .await
    .expect("baseline replace");

    // 模拟另一客户端直接改盘（绕过 LS）。
    let impl_cpp = root.join("impl.cpp");
    let mutated = std::fs::read_to_string(&impl_cpp)
        .unwrap()
        .replace("return a + b;", "return a + b + 1;");
    std::fs::write(&impl_cpp, mutated).expect("external write");

    // 再次 replace：LS 侧 range 已过期。链路正确时：didChange 先同步老内容 →
    let result = sup
        .tool_replace_body(
            &root,
            "impl.cpp",
            "add",
            "int add(int a, int b) {\n    return 42;\n}\n",
            None,
        )
        .await;
    match result {
        Ok(()) => {
            let on_disk = std::fs::read_to_string(&impl_cpp).unwrap();
            assert!(on_disk.contains("return 42;"), "成功替换后盘上应含 42");
            let body = sup
                .tool_symbol_body(&root, "impl.cpp", "add", None)
                .await
                .expect("body after");
            assert!(body.contains("return 42;"), "LS 侧应同步");
        }
        Err(supervisor::ToolError::WriteConflict { .. }) => {
            let on_disk = std::fs::read_to_string(&impl_cpp).unwrap();
            assert!(
                on_disk.contains("int add("),
                "冲突回滚后文件应完整；实际:\n{on_disk}"
            );
        }
        Err(e) => panic!("应只返回 Ok 或 WriteConflict；实际 {e:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_replace_serializes_via_gate() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch_fixture("concurrent");
    let sup = Arc::new(Supervisor::direct().await.expect("supervisor"));

    let sup1 = sup.clone();
    let root1 = root.clone();
    let t1 = tokio::spawn(async move {
        sup1.tool_replace_body(
            &root1,
            "impl.cpp",
            "add",
            "int add(int a, int b) {\n    return a + b + 100;\n}\n",
            None,
        )
        .await
    });
    let sup2 = sup.clone();
    let root2 = root.clone();
    let t2 = tokio::spawn(async move {
        sup2.tool_replace_body(
            &root2,
            "impl.cpp",
            "add",
            "int add(int a, int b) {\n    return a + b + 200;\n}\n",
            None,
        )
        .await
    });
    let (r1, r2) = tokio::join!(t1, t2);
    let _ = r1.expect("task1 join");
    let _ = r2.expect("task2 join");

    // 不变量：盘上内容自洽（100 或 200 之一，不混写）。
    let on_disk = std::fs::read_to_string(root.join("impl.cpp")).unwrap();
    let has100 = on_disk.contains("+ 100;");
    let has200 = on_disk.contains("+ 200;");
    assert!(!(has100 && has200), "并发两写后不应混写；实际:\n{on_disk}");
    assert!(
        has100 || has200,
        "至少一次替换生效（写门保证串行，两次都应成功）；实际:\n{on_disk}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
