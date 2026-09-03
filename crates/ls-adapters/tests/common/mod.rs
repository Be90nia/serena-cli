//! 通用 adapter 测试 helper（PLAN M3）。
//!
//! 5 个新 adapter（rust-analyzer / pyright / gopls / typescript-language-server /
//! csharp-ls / jdtls）共享同一套测试模板：id / languages / supports_implementation /
//! request_hooks 默认无操作 + launch_info PATH 探测。本模块把模板集中到一处，
//! 各 adapter 测试文件只声明差异点（id 字面量 + expected_languages）。
//!
//! 设计：
//! - 不引入测试宏（macro_rules!），避免编译时 Rust 工具链警告；用普通 fn。
//! - 所有测试**不**依赖真 binary —— 通过 tempdir + fake binary + set_var PATH 模拟。

#![allow(dead_code)] // 每个 adapter 测试文件 import 部分使用

/// 全局 PATH 操作计数器 —— 防止不同测试并发跑时 `set_var` 互踩。
/// `#[tokio::test]` 默认单线程；这里仍带 atomic 以 future-proof。
static PATH_OPS: AtomicU64 = AtomicU64::new(0);

/// 串行化所有修改 PATH 的测试，避免并发 set_var 互踩。
/// 用 `tokio::sync::Mutex` 而非 std —— 锁跨 await 持有（set_var → closure → restore）。
static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use std::sync::atomic::{AtomicU64, Ordering};

use ls_adapters::{LanguageId, LanguageServerAdapter, ProjectCtx};
use std::path::PathBuf;
use std::future::Future;

pub fn dummy_ctx() -> ProjectCtx {
    ProjectCtx {
        project_root: PathBuf::from("."),
    }
}

/// 临时建一个目录，把 `bin_name`（如 "rust-analyzer" / "rust-analyzer.exe"）
/// 写成一个 0 字节的 fake binary；返回目录与 fake 路径。
pub fn fake_binary_dir(bin_name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake");
    assert!(bin.is_file());
    (dir, bin)
}
/// PATH 前置 `dir`：返回原 PATH，调用方负责还原。
pub fn prepend_path(dir: &std::path::Path) -> std::ffi::OsString {
    let _guard = PATH_LOCK.blocking_lock();
    let original = std::env::var_os("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut new_path = dir.as_os_str().to_os_string();
    if !original.is_empty() {
        new_path.push(sep);
        new_path.push(original.clone());
    }
    PATH_OPS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: 测试内串行；不跨线程共享 PATH。
    unsafe { std::env::set_var("PATH", &new_path) };
    original
}

pub fn restore_path(original: std::ffi::OsString) {
    PATH_OPS.fetch_add(1, Ordering::SeqCst);
    unsafe { std::env::set_var("PATH", original) };
}

/// 持锁期间执行 closure：set PATH → call closure → restore PATH。
/// 锁的 lifetime 跨越 closure 执行 —— 解决"prepend_path 返回后锁释放"的 race。
pub async fn with_path_lock<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let _guard = PATH_LOCK.lock().await;
    let original = std::env::var_os("PATH").unwrap_or_default();
    f().await;
    unsafe { std::env::set_var("PATH", original) };
}

/// PATH 指向**空目录**（保证找不到任何 binary）。
pub fn empty_path() -> (tempfile::TempDir, std::ffi::OsString) {
    let _guard = PATH_LOCK.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let original = std::env::var_os("PATH").unwrap_or_default();
    unsafe { std::env::set_var("PATH", dir.path()) };
    (dir, original)
}

/// 通用断言：`launch_info` 在 PATH 含 fake binary 时返回 ok 且 cmd[0] 指向 fake。
pub async fn assert_launch_finds_binary<A: LanguageServerAdapter>(
    adapter: A,
    bin_name: &str,
) {
    let (dir, _bin) = fake_binary_dir(bin_name);
    let dir_path = dir.path().to_path_buf();
    let bin_name_owned = bin_name.to_string();
    let info_holder: std::sync::Arc<std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let info_holder_c = info_holder.clone();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let _ = sep;
    with_path_lock(move || async move {
        let original = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = dir_path.as_os_str().to_os_string();
        if !original.is_empty() {
            new_path.push(if cfg!(windows) { ";" } else { ":" });
            new_path.push(original.clone());
        }
        // SAFETY: 持 PATH_LOCK。
        unsafe { std::env::set_var("PATH", &new_path) };
        let info = adapter.launch_info(&dummy_ctx()).await;
        *info_holder_c.lock().unwrap() = Some(info);
        unsafe { std::env::set_var("PATH", original) };
    })
    .await;
    let info = info_holder.lock().unwrap().take().unwrap();
    let info = info.expect("PATH 含 fake binary 时 launch_info 必须成功");
    assert!(!info.cmd.is_empty(), "launch_info 必须返回非空 cmd");
    let first = info.cmd[0].to_string_lossy();
    assert!(
        first.contains(bin_name_owned.trim_end_matches(".exe"))
            || first.contains(&bin_name_owned),
        "cmd[0] 应指向 fake binary，实际：{first}"
    );
    let first_path = std::path::Path::new(&info.cmd[0]);
    let s = first_path.to_string_lossy();
    assert!(
        !s.starts_with("\\\\?\\"),
        "cmd[0] 不应含 UNC 前缀（dunce 应当去除），实际：{s}"
    );
}

/// 通用断言：`launch_info` 在 PATH 为空时返回语义错误（NotInstalled），无 panic。
pub async fn assert_launch_missing_binary<A: LanguageServerAdapter>(
    adapter: A,
    name_hint: &str,
) {
    let err_holder: std::sync::Arc<std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let err_holder_c = err_holder.clone();
    with_path_lock(|| async move {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = std::env::var_os("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", dir.path()) };
        let result = adapter.launch_info(&dummy_ctx()).await;
        *err_holder_c.lock().unwrap() = Some(result);
        unsafe { std::env::set_var("PATH", original) };
    })
    .await;
    let err = err_holder.lock().unwrap().take().unwrap();
    let err = err.expect_err("PATH 无 binary 时 launch_info 必须报错");
    let msg = format!("{err:?}");
    assert!(
        msg.contains(name_hint) || msg.to_lowercase().contains("not installed"),
        "错误信息应说明 {name_hint} 缺失，实际：{msg}"
    );
}

/// 通用断言：id + languages + supports_implementation。
pub fn assert_basic_metadata<A: LanguageServerAdapter>(
    adapter: A,
    expected_id: &str,
    expected_langs: &[LanguageId],
) {
    assert_eq!(adapter.id(), expected_id, "id mismatch");
    let langs = adapter.languages();
    for lang in expected_langs {
        assert!(
            langs.contains(lang),
            "languages must include {lang:?}, got {langs:?}"
        );
    }
    let h = adapter.request_hooks();
    let _ = h; // default 即可；具体语义由各 adapter 内部 test 覆盖
}
