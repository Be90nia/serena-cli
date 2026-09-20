//! pyright adapter 测试（PLAN M3）。

mod common;

use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::pyright::PyrightAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(PyrightAdapter, "pyright", &[LanguageId::Python]);
    // pyright 不支持 `textDocument/implementation`。
    assert!(!PyrightAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_pyright_langserver_in_path() {
    let bin = if cfg!(windows) {
        "pyright-langserver.exe"
    } else {
        "pyright-langserver"
    };
    common::assert_launch_finds_binary(PyrightAdapter, bin).await;
}

#[tokio::test]
async fn launch_finds_pyright_in_path_when_pyright_langserver_absent() {
    // fallback 探测 pyright；fake binary 名为 pyright，locate_pyright 应找到 + 加 --stdio。
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "pyright.exe"
    } else {
        "pyright"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake pyright");
    let info = common::launch_info_with_dir_on_path(PyrightAdapter, dir.path()).await;
    let info = info.expect("PATH 含 fake pyright 时 launch_info 必须成功");
    let has_stdio = info.cmd.iter().any(|a| a == "--stdio");
    assert!(
        has_stdio,
        "pyright fallback 必须加 --stdio flag, cmd={:?}",
        info.cmd
    );
}

#[tokio::test]
async fn launch_adds_stdio_flag_for_pyright_langserver() {
    // pyright-langserver 无 flag 会立即退出（Connection input stream is not set）——
    // 两种二进制都必须显式 --stdio。
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "pyright-langserver.exe"
    } else {
        "pyright-langserver"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake langserver");
    let info = common::launch_info_with_dir_on_path(PyrightAdapter, dir.path()).await;
    let info = info.expect("PATH 含 fake pyright-langserver 时 launch_info 必须成功");
    let has_stdio = info.cmd.iter().any(|a| a == "--stdio");
    assert!(
        has_stdio,
        "pyright-langserver 必须加 --stdio flag, cmd={:?}",
        info.cmd
    );
}

#[tokio::test]
async fn launch_errors_when_both_missing() {
    common::assert_launch_missing_binary(PyrightAdapter, "pyright").await;
}
