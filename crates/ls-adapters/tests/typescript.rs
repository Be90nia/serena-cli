//! typescript-language-server adapter 测试（PLAN M3）。
//!
//! 覆盖：id / languages / supports_implementation / launch_info PATH 探测 + --stdio flag。

mod common;

use ls_adapters::LanguageServerAdapter;
use ls_adapters::LanguageId;
use ls_adapters::typescript::TypescriptLanguageServerAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(
        TypescriptLanguageServerAdapter,
        "typescript-language-server",
        &[LanguageId::TypeScript],
    );
    assert!(TypescriptLanguageServerAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_typescript_ls_in_path_with_stdio_flag() {
    // 探测到 binary 后必须加 --stdio flag。
    // 用 with_path_lock 串行化 PATH 修改。
    use ls_adapters::LanguageServerAdapter;
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "typescript-language-server.exe"
    } else {
        "typescript-language-server"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake");
    let dir_path = dir.path().to_path_buf();
    let info_holder: std::sync::Arc<std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
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
            let info = TypescriptLanguageServerAdapter.launch_info(&common::dummy_ctx()).await;
            *info_holder_c.lock().unwrap() = Some(info);
            unsafe { std::env::set_var("PATH", original) };
        }
    })
    .await;
    let info = info_holder.lock().unwrap().take().unwrap();
    let info = info.expect("PATH 含 fake typescript-language-server 时 launch_info 必须成功");
    let has_stdio = info.cmd.iter().any(|a| a == "--stdio");
    assert!(has_stdio, "typescript-language-server 必须加 --stdio flag, cmd={:?}", info.cmd);
}

#[tokio::test]
async fn launch_errors_when_missing() {
    common::assert_launch_missing_binary(TypescriptLanguageServerAdapter, "typescript-language-server").await;
}
