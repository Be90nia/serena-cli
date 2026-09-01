//! Task 17 M1 验收：3 并发读 + 1 写同文件（PLAN §314）。
//!
//! 断言：
//! - 写串行化（写完后才看到新 body）
//! - 读不阻塞（3 个并发读都成功，无错）
//! - 最终一致性（所有读最终看到的 body 与最后一次写一致）

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

fn has_clangd() -> bool {
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
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

fn scratch_fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-t17-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    for f in ["main.cpp", "math.h"] {
        std::fs::copy(fixtures_root().join(f), dir.join(f)).expect("copy fixture");
    }
    std::fs::write(
        dir.join("impl.cpp"),
        "#include \"math.h\"\n\nint add(int a, int b) {\n    return a + b;\n}\n",
    )
    .expect("write impl.cpp");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_concurrent_reads_and_one_write_serialize() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch_fixture("concurrency");
    let sup = Arc::new(Supervisor::direct().await.expect("supervisor"));

    // 一次成功 baseline，固化 LS 状态。
    sup.tool_replace_body(
        &root,
        "impl.cpp",
        "add",
        "int add(int a, int b) {\n    return 100;\n}\n",
    )
    .await
    .expect("baseline");

    // 3 reader + 1 writer。
    let sup_r1 = sup.clone();
    let root_r1 = root.clone();
    let r1 =
        tokio::spawn(async move { sup_r1.tool_symbol_body(&root_r1, "impl.cpp", "add").await });
    let sup_r2 = sup.clone();
    let root_r2 = root.clone();
    let r2 =
        tokio::spawn(async move { sup_r2.tool_symbol_body(&root_r2, "impl.cpp", "add").await });
    let sup_r3 = sup.clone();
    let root_r3 = root.clone();
    let r3 =
        tokio::spawn(async move { sup_r3.tool_symbol_body(&root_r3, "impl.cpp", "add").await });

    let sup_w = sup.clone();
    let root_w = root.clone();
    let w = tokio::spawn(async move {
        sup_w
            .tool_replace_body(
                &root_w,
                "impl.cpp",
                "add",
                "int add(int a, int b) {\n    return 999;\n}\n",
            )
            .await
    });

    let r1 = r1.await.expect("r1 join");
    let r2 = r2.await.expect("r2 join");
    let r3 = r3.await.expect("r3 join");
    let w = w.await.expect("w join");

    // 写一定成功（写门串行保证最终写完）。
    let _ = w.expect("writer should succeed");

    // 3 读都成功（不阻塞）—— 各自看到 100 或 999 都合理（写时序）。
    let bodies = vec![
        r1.expect("r1 body"),
        r2.expect("r2 body"),
        r3.expect("r3 body"),
    ];
    for b in &bodies {
        assert!(
            b.contains("return 100") || b.contains("return 999"),
            "reader got: {b}"
        );
    }

    // 最终一致性：再读一次 = 写完状态（999）。
    let final_body = sup
        .tool_symbol_body(&root, "impl.cpp", "add")
        .await
        .expect("final read");
    assert!(
        final_body.contains("return 999"),
        "最终应看到写后的 999；实际:\n{final_body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
