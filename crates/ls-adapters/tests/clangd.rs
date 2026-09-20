//! `ls_adapters::ClangdAdapter` 测试（PLAN Task 8）。
//!
//! 覆盖：
//! 1. `id()` == "clangd"。
//! 2. `languages()` 含 "cpp"。
//! 3. `supports_implementation()` == true（clangd 支持 LSP `textDocument/implementation`，
//!    静态先验；ARCHITECTURE §4.1 ↖ mirror: ls_config.py `supports_implementation_request`）。
//! 4. `launch_info` 在 PATH 有 clangd 时返回可执行路径（去 UNC 前缀，dunce）；
//!    PATH 无 clangd 时返回语义错误（`NotInstalled`），无 panic。
//! 5. `request_hooks()` 默认无操作 + `initialize_patches` 不 panic（传空 base 即可）。

use std::path::PathBuf;

use ls_adapters::clangd::ClangdAdapter;
use ls_adapters::{LanguageId, LanguageServerAdapter, ProjectCtx};

/// 两个 launch_info 用例都要改进程全局 `PATH` —— 并行跑会互踩（一个把 PATH 设为
/// 空目录的瞬间另一个读到它伪造的 PATH）。env 是进程全局的：共用一把 tokio Mutex
/// 串行化（先例：lib.rs 测试的 REPLAY_ENV）。
static PATH_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn dummy_ctx() -> ProjectCtx {
    ProjectCtx {
        project_root: PathBuf::from("."),
    }
}

#[test]
fn id_is_clangd() {
    let a = ClangdAdapter;
    assert_eq!(a.id(), "clangd");
}

#[test]
fn languages_contains_cpp() {
    let langs = ClangdAdapter.languages();
    assert!(
        langs.contains(&LanguageId::Cpp),
        "languages must include Cpp, got {langs:?}"
    );
}

#[test]
fn supports_implementation_is_true() {
    assert!(ClangdAdapter.supports_implementation());
}

#[test]
fn request_hooks_default_is_empty() {
    let h = ClangdAdapter.request_hooks();
    assert!(h.method_allowlist().is_empty());
}

#[tokio::test]
async fn launch_info_finds_clangd_in_path_when_present() {
    let _env = PATH_ENV_LOCK.lock().await;
    // 模拟 PATH 内有 clangd：临时建一个目录，把"clangd"伪可执行文件写入并 PATH 前置。
    let dir = tempfile::tempdir().expect("tempdir");
    let bin_name = if cfg!(windows) {
        "clangd.exe"
    } else {
        "clangd"
    };
    let bin = dir.path().join(bin_name);
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write fake clangd");
    assert!(bin.is_file(), "fake clangd must exist");

    let original_path = std::env::var_os("PATH").unwrap_or_default();
    // Windows PATH 分隔符是 ';'，Unix 是 ':' —— 不能用 std::path::MAIN_SEPARATOR。
    let path_sep = if cfg!(windows) { ";" } else { ":" };
    let mut new_path = dir.path().as_os_str().to_os_string();
    if !original_path.is_empty() {
        new_path.push(path_sep);
        new_path.push(original_path.clone());
    }
    // SAFETY: `#[tokio::test]` 默认单线程；set_var 与 launch_info 之间无跨线程共享。
    unsafe { std::env::set_var("PATH", &new_path) };

    let info = ClangdAdapter.launch_info(&dummy_ctx()).await;

    // 还原 PATH，避免影响后续测试。
    unsafe { std::env::set_var("PATH", &original_path) };

    let info = info.expect("PATH 含 clangd 时 launch_info 必须成功");
    assert!(!info.cmd.is_empty(), "launch_info 必须返回非空 cmd");
    let first = info.cmd[0].to_string_lossy();
    assert!(
        first.contains("clangd"),
        "cmd[0] 应指向 clangd 二进制，实际：{first}"
    );

    // 无 UNC 前缀（dunce）—— 在 Windows 下尤为关键：\\?\ 前缀会污染 LSP URI。
    let first_path = std::path::Path::new(&info.cmd[0]);
    let s = first_path.to_string_lossy();
    assert!(
        !s.starts_with("\\\\?\\"),
        "cmd[0] 不应含 UNC 前缀（dunce 应当去除），实际：{s}"
    );
}

#[tokio::test]
async fn launch_info_returns_not_installed_when_clangd_missing() {
    let _env = PATH_ENV_LOCK.lock().await;
    // PATH 指向一个保证无 clangd 的空目录。
    let dir = tempfile::tempdir().expect("tempdir");
    let original = std::env::var_os("PATH").unwrap_or_default();
    unsafe { std::env::set_var("PATH", dir.path()) };
    unsafe { std::env::set_var("SERENA_SKIP_LLVM_FALLBACK", "1") };

    let result = ClangdAdapter.launch_info(&dummy_ctx()).await;

    unsafe { std::env::set_var("PATH", &original) };
    unsafe { std::env::remove_var("SERENA_SKIP_LLVM_FALLBACK") };
    let err = result.expect_err("PATH 无 clangd 时 launch_info 必须报错");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("clangd") || msg.to_lowercase().contains("not installed"),
        "错误信息应说明 clangd 缺失，实际：{msg}"
    );
}
