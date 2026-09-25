//! bash-language-server adapter 测试（Wave 1）。
//!
//! 覆盖：id / languages / launch_info PATH 探测 + `start` flag / 缓存回退不可注入时
//! missing 路径用 LOCALAPPDATA(HOME) 注入屏蔽。

mod common;

use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::bash::BashAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(BashAdapter, "bash-language-server", &[LanguageId::Bash]);
    // bash-language-server 无 textDocument/implementation 能力（默认 false）。
    assert!(!BashAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_bash_ls_on_path_with_start_flag() {
    // 探测到 binary 后必须加 `start` 子命令（上游 _create_launch_command）。
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "bash-language-server.cmd"
    } else {
        "bash-language-server"
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
            let info = BashAdapter.launch_info(&common::dummy_ctx()).await;
            *info_holder_c.lock().unwrap() = Some(info);
            unsafe { std::env::set_var("PATH", original) };
        }
    })
    .await;
    let info = info_holder.lock().unwrap().take().unwrap();
    let info = info.expect("PATH 含 fake bash-language-server 时 launch_info 必须成功");
    assert!(
        info.cmd.iter().any(|a| a == "start"),
        "bash-language-server 必须带 `start` 子命令, cmd={:?}",
        info.cmd
    );
    let first = info.cmd[0].to_string_lossy();
    assert!(
        first.contains("bash-language-server"),
        "cmd[0] 应指向 fake binary, cmd={:?}",
        info.cmd
    );
}

#[tokio::test]
async fn launch_errors_when_path_and_cache_missing() {
    // PATH = 空目录 + 缓存根注入空 tempdir（default_cache_root 读 LOCALAPPDATA/HOME，
    // 持同一把 PATH_LOCK 串行注入，屏蔽真机缓存命中干扰本测试）。
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
            let result = BashAdapter.launch_info(&common::dummy_ctx()).await;
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
    let err = err.expect_err("PATH 与缓存皆无 bash-language-server 时必须报错");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("install bash") || msg.to_lowercase().contains("not found in path"),
        "错误信息必须含安装指引, 实际: {msg}"
    );
}
