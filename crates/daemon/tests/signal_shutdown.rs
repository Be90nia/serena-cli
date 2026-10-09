//! bd 1cv2：Ctrl-C / SIGINT 收尾路径（`serve::signal_shutdown`）集成测试。
//!
//! 真实 OS 信号（SIGINT / Windows SetConsoleCtrlHandler）无法在 CI 里向
//! detached 子进程跨平台注入，故测到 `signal_shutdown` 这层协议：draining
//! 置位、排空窗口尊重 in-flight、广播停机通知。OS 信号 → ctrl_c 的接线由
//! tokio 三平台实现承担（serve.rs 内 select! 分支，人工核对的编译期边界）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use daemon::http::AppState;
use daemon::serve::signal_shutdown;
use supervisor::SupervisorTrait;

struct NoopSup;

#[async_trait::async_trait]
impl SupervisorTrait for NoopSup {
    async fn execute_tool(
        &self,
        _tool: &str,
        _root: &str,
        _args: serde_json::Value,
        _lang: Option<&str>,
    ) -> Result<serde_json::Value, supervisor::ToolError> {
        Ok(serde_json::json!(null))
    }
}

fn test_state(drain_window: Duration) -> AppState {
    AppState {
        supervisor: Arc::new(NoopSup),
        token: Arc::new("t".into()),
        start_ts: Instant::now(),
        loaded_ls: Arc::new(Mutex::new(vec![])),
        draining: Arc::new(AtomicBool::new(false)),
        active_project: Arc::new(Mutex::new(None)),
        switch_reported: Arc::new(Mutex::new(std::collections::HashSet::new())),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        in_flight: Arc::new(AtomicUsize::new(0)),
        invocation_log_path: std::path::PathBuf::new(),
        drain_window,
        no_token_estimate: false,
        obs: daemon::http::ObsState::default(),
    }
}

#[tokio::test]
async fn signal_shutdown_sets_draining_and_notifies() {
    let st = test_state(Duration::from_millis(100));
    let notify = st.shutdown_notify.clone();
    let waiter = tokio::spawn(async move {
        notify.notified().await;
    });
    signal_shutdown(&st).await;
    assert!(
        st.draining.load(Ordering::Acquire),
        "signal 后必须置 draining"
    );
    // 通知在排空窗口后到达；窗口 100ms，给 3s 余量。
    tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .expect("waiter")
        .expect("notify 已广播");
}

#[tokio::test]
async fn signal_shutdown_waits_for_in_flight() {
    let st = test_state(Duration::from_millis(400));
    st.in_flight.store(1, Ordering::SeqCst);
    let started = Instant::now();
    let s = st.clone();
    let task = tokio::spawn(async move { signal_shutdown(&s).await });
    // in-flight 保持 200ms 后释放；排空判据要求 quiet 归零。
    tokio::time::sleep(Duration::from_millis(200)).await;
    st.in_flight.store(0, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("signal_shutdown 返回")
        .expect("join");
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "in-flight 未归零前不得提前完成（实测 {:?}）",
        started.elapsed()
    );
}

#[tokio::test]
async fn signal_shutdown_idempotent_when_already_draining() {
    let st = test_state(Duration::from_millis(100));
    st.draining.store(true, Ordering::SeqCst);
    // 已 draining：直接返回，不重复排空、不重复 notify（原路径负责收尾）。
    let started = Instant::now();
    signal_shutdown(&st).await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "已 draining 时不得再等排空窗口"
    );
}
