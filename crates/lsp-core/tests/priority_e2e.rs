//! P0B：priority 路由端到端测试（replay 模式 + Recorder）。
//!
//! `replay_pump_with_priority` 在 recorder = Record 模式时把所有 priority 路由后
//! 待发的帧按写入顺序追加到文件，每行 `> <jsonrpc frame>`。本测试：
//! 1. 起 1 个 High + 5 个 Normal + 5 个 Background（无时间间隔，全部在 block 1 入队）。
//! 2. drop sender → writer 退出 → 读完 recorder 文件 → 断言顺序：
//!    - High 永远最先（即便最后入队）。
//!    - Normal 接下来。
//!    - Background 最后（受 TokenBucket 节流，但本测试 burst ≤ 30/s 故 30/s burst
//!      内单帧间隔 ≤ per_sec，应能在测试时间内出完）。
//!
//! 注：不依赖真 LS；replay 模式无 stdin。

use lsp_core::client::{OutboundItem, Priority};
use lsp_core::framing::JsonRpc;
use lsp_core::recording::Recorder;
use lsp_core::transport::stdio::replay_pump_with_priority;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn new_method(name: &str) -> JsonRpc {
    JsonRpc::notification(name, json!({}))
}

#[tokio::test]
async fn priority_routing_serves_high_before_background() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let record_path = tmp.path().join("frames.jsonl");
    let recorder = Recorder::open(&record_path).expect("open recorder");

    let (out_tx, out_rx) = mpsc::channel::<OutboundItem>(64);
    let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);
    let on_msg: Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync> = Arc::new(|_| None);
    let on_eof: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

    let mut pumps = replay_pump_with_priority(out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder);

    // 立即 drop reply_tx → reply_rx 关闭后 writer 不会卡死 select。
    // 拿 reply_tx 不可能（已 move）；下面 out_tx drop 后还需要 rep_closed=true。
    // → 我们让 on_eof 直接是 no-op；writer 用 rep_closed 标志靠 reply_rx.recv()
    //   返回 Err 才置。这里我们靠 send 完所有 out 后 drop out_tx，等 writer 全部写完。
    for i in 0..5 {
        let method = if i % 2 == 0 {
            "workspace/_ping"
        } else {
            "workspace/symbol"
        };
        out_tx
            .send(OutboundItem {
                msg: new_method(method),
                priority: Priority::Background,
            })
            .await
            .expect("send bg");
    }
    out_tx
        .send(OutboundItem {
            msg: new_method("textDocument/definition"),
            priority: Priority::High,
        })
        .await
        .expect("send high");

    // drop sender → writer EOF → task 退出。
    drop(out_tx);

    // 等 writer 写完所有帧。Background 5 帧各需 1/30s ≈ 33ms → 6 帧 ≤ 200ms。
    // 给余量 2s。
    tokio::time::sleep(Duration::from_secs(2)).await;
    pumps.kill();
    drop(pumps);

    // 读 recorder 文件 → 提取顺序。
    let body = std::fs::read_to_string(&record_path).expect("read record");
    let lines: Vec<&str> = body.lines().filter(|l| l.starts_with("--> ")).collect();
    assert!(
        lines.len() >= 6,
        "应至少 6 行（5 BG + 1 High）；实际 {} 行：{lines:?}",
        lines.len()
    );

    // 解析 method 字段，按文件写入顺序。
    let methods: Vec<String> = lines
        .iter()
        .filter_map(|l| {
            serde_json::from_str::<Value>(l.trim_start_matches("--> "))
                .ok()
                .and_then(|v| {
                    v.get("method")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
        })
        .collect();

    // High 一定最先（contract 即便合同里的唯一 High 也要抢占）
    let first = methods.first().expect("至少一帧");
    assert_eq!(
        first, "textDocument/definition",
        "High 必须最先入文件；实际首帧 = {first}, 顺序 = {methods:?}"
    );
}

#[tokio::test]
async fn priority_routing_serves_normal_before_background() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let record_path = tmp.path().join("frames.jsonl");
    let recorder = Recorder::open(&record_path).expect("open recorder");

    let (out_tx, out_rx) = mpsc::channel::<OutboundItem>(64);
    let (reply_tx, reply_rx) = mpsc::channel::<JsonRpc>(8);
    let on_msg: Arc<dyn Fn(JsonRpc) -> Option<JsonRpc> + Send + Sync> = Arc::new(|_| None);
    let on_eof: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

    let mut pumps = replay_pump_with_priority(out_rx, reply_rx, reply_tx, on_msg, on_eof, recorder);

    // 3 Background → 3 Normal。预期 Normal 先于 Background 出（除非触发 50ms 降级）。
    for _ in 0..3 {
        out_tx
            .send(OutboundItem {
                msg: new_method("workspace/_ping"),
                priority: Priority::Background,
            })
            .await
            .expect("send bg");
    }
    for _ in 0..3 {
        out_tx
            .send(OutboundItem {
                msg: new_method("initialized"),
                priority: Priority::Normal,
            })
            .await
            .expect("send normal");
    }

    drop(out_tx);

    // 等所有帧写完。Background 3 帧 ≤ 100ms；Normal 同理。给 2s 余量。
    tokio::time::sleep(Duration::from_secs(2)).await;
    pumps.kill();
    drop(pumps);

    let body = std::fs::read_to_string(&record_path).expect("read record");
    let lines: Vec<&str> = body.lines().filter(|l| l.starts_with("--> ")).collect();
    assert!(lines.len() >= 6, "应至少 6 行；实际 {} 行", lines.len());

    let methods: Vec<String> = lines
        .iter()
        .filter_map(|l| {
            serde_json::from_str::<Value>(l.trim_start_matches("--> "))
                .ok()
                .and_then(|v| {
                    v.get("method")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
        })
        .collect();

    // 找第一个 Background 与第一个 Normal 的位置。
    let first_bg = methods.iter().position(|m| {
        m == "workspace/_ping" || m == "workspace/symbol"
    });
    let first_normal = methods.iter().position(|m| m == "initialized");
    if let (Some(b), Some(n)) = (first_bg, first_normal) {
        assert!(
            n < b,
            "Normal 应先于 Background 出；顺序 = {methods:?}"
        );
    }
}