//! bd qq3a perf-P1-2：出站积压时 `Client::request` 仍受预算保护。
//!
//! `send_once_at` 的 `outbound.send().await` 若在预算计时之外，LS writer 卡死
//! （stdin 管道满 / LS 假死）→ 出站 mpsc 积压 → 后续所有请求在发送段无界阻塞，
//! 工具级 30s 预算形同虚设。预算必须覆盖「发送 + 等待响应」全程。
//!
//! 纯内存测试：cap=1 的 channel 先占满、receiver 不消费，request 必须在预算内
//! 返回 `CoreError::Timeout` 而非挂死。

use std::time::{Duration, Instant};

use lsp_core::client::{Client, OutboundItem, Priority};
use lsp_core::error::CoreError;
use lsp_core::framing::JsonRpc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_times_out_when_outbound_backpressured() {
    let (tx, _rx) = tokio::sync::mpsc::channel::<OutboundItem>(1);
    // 占满 cap=1 的出站通道（rx 被 _rx 持有不读 → 永不消费）。
    tx.send(OutboundItem {
        msg: JsonRpc::request(9999, "initialized", serde_json::json!({})),
        priority: Priority::High,
    })
    .await
    .expect("首条应入队成功");

    let client = Client::with_name("mock-ls".into(), tx);
    let budget = Duration::from_millis(300);

    let started = Instant::now();
    // 外层 5s 只是防测试自身挂死；断言的是 request 自身在预算量级内返回。
    let res = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .request::<serde_json::Value>(
                "textDocument/hover",
                serde_json::json!({}),
                budget,
            )
            .await
    })
    .await
    .expect("request 必须在预算+余量内返回，而非在发送段无界阻塞");

    let err = res.expect_err("出站积压 + LS 静默必须超时");
    assert!(
        matches!(err, CoreError::Timeout { .. }),
        "应 CoreError::Timeout，实际 {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "超时应在预算量级（300ms）内触发，实测 {:?}",
        started.elapsed()
    );
}
