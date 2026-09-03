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
    common::assert_basic_metadata(
        RustAnalyzerAdapter,
        "rust-analyzer",
        &[LanguageId::Rust],
    );
    assert!(RustAnalyzerAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_rust_analyzer_in_path() {
    let bin = if cfg!(windows) { "rust-analyzer.exe" } else { "rust-analyzer" };
    common::assert_launch_finds_binary(RustAnalyzerAdapter, bin).await;
}

#[tokio::test]
async fn launch_errors_when_missing() {
    common::assert_launch_missing_binary(RustAnalyzerAdapter, "rust-analyzer").await;
}
