//! `which_no_unc` Windows npm shim 回归（Wave 1 npm MISS 修复）。
//!
//! 根因：PATH 上的 npm 有三种形态——无扩展名（sh 脚本）/ `.cmd` / `.ps1`。旧 exts
//! 顺序 `["", ".exe", ".cmd", ".bat"]` 先命中 sh 脚本，`Command::spawn` 无法执行它
//! → doctor 报 npm MISS 而 `where.exe npm` 命中。修复后 Windows 只认可执行后缀，
//! `.cmd` 必须胜出裸名。

mod common;

#[cfg(windows)]
#[tokio::test]
async fn which_prefers_cmd_shim_over_extensionless_sh_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 裸名 = npm 全局 shim 的 sh 脚本形态（spawn 不可执行）。
    std::fs::write(dir.path().join("npm"), b"#!/bin/sh\necho sh-script\n").unwrap();
    // .cmd = Windows 真实可执行 shim。
    std::fs::write(dir.path().join("npm.cmd"), b"@echo 99.99.9\r\n").unwrap();

    let found = common::which_with_dir_on_path(dir.path(), "npm").await;
    let found = found.expect("PATH 含 fake npm 时必须命中");
    assert_eq!(
        found.extension().and_then(|e| e.to_str()),
        Some("cmd"),
        "Windows 必须解析到 .cmd shim 而非裸名 sh 脚本, got {}",
        found.display()
    );
}

/// 非 Windows：裸名命中语义不变（防修复误伤 Unix）。
#[cfg(not(windows))]
#[tokio::test]
async fn which_still_finds_extensionless_binary_on_unix() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("npm"), b"#!/bin/sh\n").unwrap();
    let found = common::which_with_dir_on_path(dir.path(), "npm").await;
    assert!(found.is_some(), "Unix 裸名命中语义不变");
}
