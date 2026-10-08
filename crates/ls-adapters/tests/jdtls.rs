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
async fn launch_errors_when_snapshot_mode_and_no_java() {
    // VSIX 默认模式会 auto-install（~100MB）——"未安装"语义只能在 snapshot
    // 模式测：SERENA_JDTLS_SNAPSHOT=1 + PATH 清空 → java 硬前置先于缓存
    // 判定（src jdtls.rs 步骤 2），NotInstalled 可确定性触达且零下载。
    let _lock = common::with_path_lock(|| async {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = std::env::var_os("PATH").unwrap_or_default();
        // SAFETY: 持 common PATH_LOCK 串行。
        unsafe { std::env::set_var("PATH", dir.path()) };
        unsafe { std::env::set_var("SERENA_JDTLS_SNAPSHOT", "1") };
        let result = JdtlsAdapter.launch_info(&common::dummy_ctx()).await;
        // SAFETY: 同上。
        unsafe { std::env::set_var("PATH", original) };
        unsafe { std::env::remove_var("SERENA_JDTLS_SNAPSHOT") };
        let err = result.expect_err("snapshot 模式 + PATH 空 + 无 java 必报 NotInstalled");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("jdtls") || msg.to_lowercase().contains("not installed"),
            "错误信息应说明 jdtls/java 缺失，实际：{msg}"
        );
    })
    .await;
}
