//! cf54869a mirror 集成测试：`$/progress` → `IndexProgressTracker` → drain 等待全链路。
//!
//! mock_ls 设 `MOCK_LS_PROGRESS_TOKEN` + `MOCK_LS_PROGRESS_END_ON_OPEN`：initialize 后发
//! begin，收到首个 didOpen 才发 end —— 模拟「didOpen 触发的后台索引在请求方等待期间
//! 才完成」。上游语义锚：TS 适配器跨文件查询等 $/progress drain
//! （typescript_language_server.py@cf54869a）。

use std::time::Duration;

use ls_runtime::process::{Child, ChildHandle, LaunchInfo, TransportKind};
use lsp_core::init_params::base_initialize_params;
use lsp_core::session::Session;

fn spawn_mock_ls_with_delayed_end() -> ChildHandle {
    let mock_ls: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    Child::spawn(LaunchInfo {
        cmd: vec![mock_ls.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![
            (
                "MOCK_LS_PROGRESS_TOKEN".to_string(),
                "cf54869a-token".to_string(),
            ),
            ("MOCK_LS_PROGRESS_END_ON_OPEN".to_string(), "1".to_string()),
        ],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls")
}

async fn start_session() -> std::sync::Arc<Session> {
    let child = spawn_mock_ls_with_delayed_end();
    tokio::time::timeout(Duration::from_secs(10), Session::start(Some(child), base_initialize_params()))
        .await
        .expect("session start within 10s")
        .expect("session start Ok")
}

/// 全链路：`$/progress` begin 通知（initialize 后）→ handler track → active=1；
/// didOpen 触发 end → `wait_indexing_drain` 被唤醒返回 true、active 归零。
/// 证明通知分发 → 在飞集合 → watch 唤醒真实接通（非仅单测直调 track）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_progress_tracks_begin_and_drains_on_did_open_end() {
    let session = start_session().await;

    // begin 通知在握手响应之后由泵异步处理，start 返回不保证已 track —— 轮询等待。
    let mut saw_active = false;
    for _ in 0..40 {
        if session.index_active_progress() == 1 {
            saw_active = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saw_active, "mock_ls 的 begin 应在 2s 内被 handler 计入在飞 token");

    // didOpen → mock_ls 补发 end → drain 完成。
    session
        .notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": "file:///tmp/e2e.ts",
                    "languageId": "typescript",
                    "version": 1,
                    "text": "export const x = 1;",
                }
            }),
        )
        .await
        .expect("notify didOpen");

    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        session.wait_indexing_drain(Duration::from_secs(5)).await
    })
    .await
    .expect("drain wait within 5s wall clock");
    assert!(drained, "end 到达后 drain 应完成");
    assert_eq!(session.index_active_progress(), 0, "end 后在飞集合应清空");

    session.shutdown().await;
}

/// 首查 start-or-completion 路径：begin 已在飞 → grace 内观察到活动 → 转 drain →
/// didOpen 的 end 到达后返回 true。证明上游 `_wait_for_indexing_start_or_completion`
/// 的「观察到开始才放行」语义在集成层成立。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_cross_file_query_waits_for_start_then_drains() {
    let session = start_session().await;

    assert!(
        session.take_cross_file_first_query(),
        "首次调用应是首查 latch 赢家"
    );

    // 200ms 后 didOpen → mock_ls 补发 end：drain 应在 end 到达时完成（而非等满超时）。
    let notify_session = std::sync::Arc::clone(&session);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = notify_session
            .notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": "file:///tmp/e2e.ts",
                        "languageId": "typescript",
                        "version": 1,
                        "text": "export const x = 1;",
                    }
                }),
            )
            .await;
    });

    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        session
            .wait_indexing_start_or_completion(Duration::from_secs(5), Duration::from_secs(5))
            .await
    })
    .await
    .expect("start-or-completion within 10s wall clock");
    assert!(completed, "begin 在飞 → 转 drain → end 到达应返 true");
    assert_eq!(session.index_active_progress(), 0);

    session.shutdown().await;
}
