//! TG-H2（bd g6yp Q1）：shutdown 时序回归锁。
//!
//! 根因：`Session::start` 里 `stdout_eof` 与泵闭包持有的 clone 之间
//! `Arc::try_unwrap` 恒失败（闭包还活着），`unwrap_or_else` 落一个**全新**
//! `Notify` —— shutdown 步骤 4 等的门与泵 EOF 打的门不是同一个实例，
//! 无论 LS 退多快都等满 `SHUTDOWN_WAIT_TIMEOUT`（5s）→ 每次会话拆除固定 +5s。
//!
//! 修复（session 字段改 `Arc<Notify>` 共享）后：kill → stdout EOF → 泵闭包
//! `notify_waiters()` 命中同一门 → ms 级放行。本测试在修复前必然 ~5s（红），
//! 修复后 <1s（绿）。

use std::ffi::OsString;
use std::time::{Duration, Instant};

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::session::Session;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_completes_under_one_second_after_ls_eof() {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![OsString::from(exe)],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");
    let session = Session::start(Some(child), lsp_core::init_params::base_initialize_params())
        .await
        .expect("Session::start Ready");

    let started = Instant::now();
    session.shutdown().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown 必须在 stdout EOF 放行后 <1s 完成；等满 ~5s = stdout_eof 断链回归。实际 {elapsed:?}"
    );
}
