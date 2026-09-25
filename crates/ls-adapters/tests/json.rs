//! vscode-json-languageserver adapter 测试（Wave 1）。
//!
//! 覆盖：id / languages / launch_info PATH 探测 + `--stdio` flag / PATH+缓存皆无时报错。

mod common;

use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::json::JsonAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(
        JsonAdapter,
        "vscode-json-languageserver",
        &[LanguageId::Json],
    );
    // json LS 无跨文件 references / implementation（上游明示）。
    assert!(!JsonAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_json_ls_on_path_with_stdio_flag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "vscode-json-languageserver.cmd"
    } else {
        "vscode-json-languageserver"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake");
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
            // SAFETY: 持 PATH_LOCK。
            unsafe { std::env::set_var("PATH", &new_path) };
            let info = JsonAdapter.launch_info(&common::dummy_ctx()).await;
            *info_holder_c.lock().unwrap() = Some(info);
            unsafe { std::env::set_var("PATH", original) };
        }
    })
    .await;
    let info = info_holder.lock().unwrap().take().unwrap();
    let info = info.expect("PATH 含 fake vscode-json-languageserver 时 launch_info 必须成功");
    assert!(
        info.cmd.iter().any(|a| a == "--stdio"),
        "vscode-json-languageserver 必须带 --stdio flag, cmd={:?}",
        info.cmd
    );
    let first = info.cmd[0].to_string_lossy();
    assert!(
        first.contains("vscode-json-languageserver"),
        "cmd[0] 应指向 fake binary, cmd={:?}",
        info.cmd
    );
}

#[tokio::test]
async fn launch_errors_when_path_and_cache_missing() {
    let err_holder: std::sync::Arc<
        std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let err_holder_c = err_holder.clone();
    common::with_path_lock(move || {
        let err_holder_c = err_holder_c.clone();
        async move {
            let dir = tempfile::tempdir().expect("tempdir");
            let cache_dir = tempfile::tempdir().expect("cache tempdir");
            let path_original = std::env::var_os("PATH").unwrap_or_default();
            let home_key = if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "HOME"
            };
            let home_original = std::env::var_os(home_key);
            // SAFETY: 持 PATH_LOCK 串行；restore 于 closure 末尾。
            unsafe { std::env::set_var("PATH", dir.path()) };
            unsafe { std::env::set_var(home_key, cache_dir.path()) };
            let result = JsonAdapter.launch_info(&common::dummy_ctx()).await;
            *err_holder_c.lock().unwrap() = Some(result);
            // SAFETY: 同上，还原注入。
            unsafe { std::env::set_var("PATH", path_original) };
            match home_original {
                Some(v) => unsafe { std::env::set_var(home_key, v) },
                None => unsafe { std::env::remove_var(home_key) },
            }
        }
    })
    .await;
    let err = err_holder.lock().unwrap().take().unwrap();
    let err = err.expect_err("PATH 与缓存皆无 vscode-json-languageserver 时必须报错");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("install json") || msg.to_lowercase().contains("not found in path"),
        "错误信息必须含安装指引, 实际: {msg}"
    );
}
