//! Task 21 find_implementations e2e —— C++ virtual method 跨实现。

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
    let dir = std::env::temp_dir().join(format!("serena-t21-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 接口 + 2 个实现。
    std::fs::write(
        dir.join("shape.h"),
        r#"#pragma once
struct Shape {
    virtual double area() const = 0;
    virtual ~Shape() = default;
};
struct Circle : Shape {
    double r;
    double area() const override { return 3.14 * r * r; }
};
struct Square : Shape {
    double s;
    double area() const override { return s * s; }
};
"#,
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn virtual_method_finds_all_implementations() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("impl");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 触发索引。
    let _ = sup
        .tool_overview(&root, "shape.h", None)
        .await
        .expect("overview");

    // 在 `area` 第一次出现处（virtual 声明那行）请求 implementation。
    // shape.h 里 `virtual double area() const = 0;` 大约在第 3 行 char 12。
    let hits = sup
        .tool_find_implementations(&root, "shape.h", 2, 11, None)
        .await
        .expect("find_implementations");

    // 应至少找到 Circle::area + Square::area 均在同一文件，但 range 不同。
    // 断言：至少 2 个结果，且都引用 shape.h。
    assert!(
        hits.len() >= 2,
        "expected at least 2 implementations of Shape::area, got {}: {hits:?}",
        hits.len()
    );
    assert!(hits.iter().all(|h| h.uri.as_str().contains("shape.h")));

    let _ = std::fs::remove_dir_all(&root);
}
