//! csharp-ls adapter 测试（PLAN M3）。

mod common;
use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::csharp_ls::CsharpLsAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(CsharpLsAdapter, "csharp-ls", &[LanguageId::CSharp]);
    assert!(CsharpLsAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_csharp_ls_in_path() {
    let bin = if cfg!(windows) {
        "csharp-ls.exe"
    } else {
        "csharp-ls"
    };
    common::assert_launch_finds_binary(CsharpLsAdapter, bin).await;
}

#[tokio::test]
async fn launch_errors_when_missing() {
    common::assert_launch_missing_binary(CsharpLsAdapter, "csharp-ls").await;
}
