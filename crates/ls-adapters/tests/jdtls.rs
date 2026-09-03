//! jdtls adapter 测试（PLAN M3）。
//!
//! jdtls 启动最复杂：探测 `jdtls` 二进制，fallback 探测 `java` + launcher。
use ls_adapters::LanguageServerAdapter;
mod common;

use ls_adapters::LanguageId;
use ls_adapters::jdtls::JdtlsAdapter;

#[test]
fn metadata() {
    common::assert_basic_metadata(JdtlsAdapter, "jdtls", &[LanguageId::Java]);
    assert!(JdtlsAdapter.supports_implementation());
}

#[tokio::test]
async fn launch_finds_jdtls_in_path() {
    // 优先探测 `jdtls` 直接 binary（新版发行版提供）。
    let bin = if cfg!(windows) { "jdtls.exe" } else { "jdtls" };
    common::assert_launch_finds_binary(JdtlsAdapter, bin).await;
}

#[tokio::test]
async fn launch_errors_when_missing() {
    // PATH 空 → jdtls + java 都找不到 → 报 NotInstalled。
    common::assert_launch_missing_binary(JdtlsAdapter, "jdtls").await;
}
