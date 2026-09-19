//! rust-analyzer adapter 测试（PLAN M3）。
//!
//! 覆盖：id / languages / supports / launch_info PATH 探测。
//! 真二进制 e2e 留 fixture `fixtures/rust_demo/main.rs` + 用户装 rustup 后跑 cli。

mod common;
use ls_adapters::LanguageServerAdapter;

use ls_adapters::LanguageId;
use ls_adapters::rust_analyzer::RustAnalyzerAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(RustAnalyzerAdapter, "rust-analyzer", &[LanguageId::Rust]);
    assert!(RustAnalyzerAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_rust_analyzer_in_path() {
    let bin = if cfg!(windows) {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    };
    common::assert_launch_finds_binary(RustAnalyzerAdapter, bin).await;
}

#[tokio::test]
async fn launch_errors_when_missing() {
    // PATH 清空后 rust-analyzer 仍可能经 ~/.cargo/bin 兜底命中（4.1 查找链设计行为），
    // 上游 _ensure_rust_analyzer_installed 同样有非 PATH 兜底。故本用例只断言：
    // 真找得到时结果功能可用；彻底无 rust 生态（兜底也落空）时报 not_installed。
    let dir = tempfile::tempdir().expect("tempdir");
    let original = std::env::var_os("PATH").unwrap_or_default();
    let result = {
        unsafe { std::env::set_var("PATH", dir.path()) };
        let r = RustAnalyzerAdapter
            .launch_info(&common::dummy_ctx())
            .await
            .map(|i| i.cmd[0].clone());
        unsafe { std::env::set_var("PATH", original) };
        r
    };
    match result {
        Ok(exe) => assert!(
            std::path::Path::new(&exe).is_file(),
            "兜底命中的必须是存在的文件: {exe:?}"
        ),
        Err(e) => {
            let msg = format!("{e:#}").to_lowercase();
            assert!(
                msg.contains("rust-analyzer") || msg.contains("not installed"),
                "失败信息应说明 rust-analyzer 缺失，实际：{msg}"
            );
        }
    }
}
