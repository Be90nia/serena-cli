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
    use ls_adapters::LanguageServerAdapter;
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "pyright.exe"
    } else {
        "pyright"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake pyright");
    let dir_path = dir.path().to_path_buf();
    let info_holder: std::sync::Arc<
        std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let info_holder_c = info_holder.clone();
    common::with_path_lock(move || {
        let info_holder_c = info_holder_c.clone();
        let dir_path = dir_path.clone();
        async move {
            let original = std::env::var_os("PATH").unwrap_or_default();
            let mut new_path = dir_path.as_os_str().to_os_string();
            if !original.is_empty() {
                new_path.push(if cfg!(windows) { ";" } else { ":" });
                new_path.push(original.clone());
            }
            unsafe { std::env::set_var("PATH", &new_path) };
            let info = PyrightAdapter.launch_info(&common::dummy_ctx()).await;
            *info_holder_c.lock().unwrap() = Some(info);
            unsafe { std::env::set_var("PATH", original) };
        }
    })
    .await;
    let info = info_holder.lock().unwrap().take().unwrap();
    let info = info.expect("PATH 含 fake pyright 时 launch_info 必须成功");
    let has_stdio = info.cmd.iter().any(|a| a == "--stdio");
    assert!(
        has_stdio,
        "pyright fallback 必须加 --stdio flag, cmd={:?}",
        info.cmd
    );
}

#[tokio::test]
async fn launch_errors_when_both_missing() {
    common::assert_launch_missing_binary(PyrightAdapter, "pyright").await;
}
