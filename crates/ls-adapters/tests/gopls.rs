//! gopls adapter 测试（PLAN M3）。

mod common;
use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::gopls::GoplsAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(GoplsAdapter, "gopls", &[LanguageId::Go]);
    assert!(GoplsAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_gopls_in_path() {
    let bin = if cfg!(windows) { "gopls.exe" } else { "gopls" };
    common::assert_launch_finds_binary(GoplsAdapter, bin).await;
}

#[tokio::test]
async fn launch_errors_when_missing() {
    common::assert_launch_missing_binary(GoplsAdapter, "gopls").await;
}
