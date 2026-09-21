//! lsp-core docsync 集成测试（PLAN Task 7 / ARCHITECTURE §3.2 BUF + §3.4 锁）。
//!
//! 覆盖 PLAN Task 7 的 3 条用例：
//! 1. `ensure_open` 后 mock_ls 收到 `textDocument/didOpen`（version=1）。
//! 2. 外部改文件后再次 `ensure_open` 触发 `didChange`（version=2）。
//! 3. 双 guard 嵌套只发一次 `didOpen`（ref_count++ 路径）。
//!
//! mock_ls 通过 `MOCK_LS_TRACK_FILE_EVENTS=<path>` 追加写日志，测试读完即断言事件序列。

use std::ffi::OsString;
use std::time::Duration;

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::session::Session;
use lsp_types::InitializeParams;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time;

fn launch_mock_ls_track(track_log: &std::path::Path) -> LaunchInfo {
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    LaunchInfo {
        cmd: vec![OsString::from(exe)],
        cwd: std::env::temp_dir(),
        env: vec![(
            "MOCK_LS_TRACK_FILE_EVENTS".into(),
            track_log.to_string_lossy().into_owned(),
        )],
        transport: TransportKind::Stdio,
    }
}

fn dummy_init_params() -> InitializeParams {
    InitializeParams::default()
}

fn file_uri(path: &std::path::Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    let canonical = dunce::canonicalize(&abs).unwrap_or(abs);
    lsp_core::docsync::path_to_uri_str(&canonical)
}

async fn read_track_events(path: &std::path::Path) -> Vec<Value> {
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = tokio::fs::read_to_string(path).await {
            let lines: Vec<Value> = s
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if !lines.is_empty() || time::Instant::now() >= deadline {
                return lines;
            }
        }
        if time::Instant::now() >= deadline {
            return Vec::new();
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

/// 验收（BD serena-rust-81m 次级缺陷）：set_language_id 注入的真实语言必须进
/// didOpen——此前硬编码 "cpp"，rust-analyzer 收到错语言文档直接拒收。
#[tokio::test]
async fn did_open_carries_injected_language_id() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("lib.rs");
    tokio::fs::write(&file, b"fn main() {}\n")
        .await
        .expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");
    session.set_language_id("Rust");

    let _guard = session.ensure_open(&file).await.expect("ensure_open");

    time::sleep(Duration::from_millis(300)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(opens.len(), 1, "应 1 次 didOpen；events={events:?}");
    assert_eq!(
        opens[0].get("languageId").and_then(Value::as_str),
        Some("rust"),
        "didOpen languageId 必须是注入后的真实语言（小写化）"
    );

    session.shutdown().await;
}

/// 未注入时保持旧行为 "cpp"（lsp-core 直连路径的兜底兼容）。
#[tokio::test]
async fn did_open_defaults_to_cpp_when_language_not_injected() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("a.cpp");
    tokio::fs::write(&file, b"int main(){}\n")
        .await
        .expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _guard = session.ensure_open(&file).await.expect("ensure_open");

    time::sleep(Duration::from_millis(300)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(opens.len(), 1, "应 1 次 didOpen；events={events:?}");
    assert_eq!(opens[0].get("languageId").and_then(Value::as_str), Some("cpp"));

    session.shutdown().await;
}

/// 用例 #1：ensure_open 后 mock_ls 收到 didOpen，version=1。
#[tokio::test]
async fn ensure_open_emits_did_open_with_full_text() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("a.cpp");
    tokio::fs::write(&file, b"int main(){}\n")
        .await
        .expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start 应在握手超时内 Ready");

    let _guard = session
        .ensure_open(&file)
        .await
        .expect("ensure_open 首次应成功");

    // 等 mock_ls 把事件落盘（writer task → mock_ls 解码 → track_event 落盘）。
    time::sleep(Duration::from_millis(300)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(opens.len(), 1, "应 1 次 didOpen；events={events:?}");
    assert_eq!(
        opens[0].get("uri").and_then(Value::as_str),
        Some(file_uri(&file).as_str())
    );
    assert_eq!(opens[0].get("version").and_then(Value::as_i64), Some(1));

    session.shutdown().await;
}

/// 用例 #2：外部改文件（mtime 推进）→ 再次 ensure_open 触发 didChange。
#[tokio::test]
async fn ensure_open_after_external_edit_emits_did_change() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("b.cpp");
    tokio::fs::write(&file, b"v1\n")
        .await
        .expect("write fixture v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let g1 = session.ensure_open(&file).await.expect("first ensure_open");

    // 改文件 + sleep 让 mtime 推进（NTFS 100ns 精度）。
    time::sleep(Duration::from_millis(60)).await;
    tokio::fs::write(&file, b"v2 edited\n")
        .await
        .expect("rewrite fixture");
    time::sleep(Duration::from_millis(60)).await;

    let _g2 = session
        .ensure_open(&file)
        .await
        .expect("second ensure_open");

    drop(g1);
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let changes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didChange"))
        .collect();

    assert_eq!(
        opens.len(),
        1,
        "后续 ensure_open 不应再发 didOpen；events={events:?}"
    );
    assert_eq!(
        changes.len(),
        1,
        "mtime 变化后应发一次 didChange；events={events:?}"
    );
    assert_eq!(changes[0].get("version").and_then(Value::as_i64), Some(2));

    session.shutdown().await;
}

/// 用例 #3：嵌套 guard 只发一次 didOpen（ref_count++ 路径）。
#[tokio::test]
async fn nested_guards_emit_did_open_once() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("c.cpp");
    tokio::fs::write(&file, b"x\n")
        .await
        .expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let g1 = session.ensure_open(&file).await.expect("guard 1");
    let g2 = session.ensure_open(&file).await.expect("guard 2");
    let g3 = session.ensure_open(&file).await.expect("guard 3");

    drop(g3);
    drop(g2);
    drop(g1);
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();

    assert_eq!(
        opens.len(),
        1,
        "三嵌套 guard 只应发 1 次 didOpen；events={events:?}"
    );
    assert_eq!(
        closes.len(),
        1,
        "ref_count 归零应发 1 次 didClose；events={events:?}"
    );

    session.shutdown().await;
}

/// P1 修复 #4：连续多次 mtime 推进 → version 必须单调递增（v1, v2, v3, ...）。
/// 修复前：edit_tools 各自维护 `version` 计数器，跨工具调用序列非单调，
/// rust-analyzer 拒收 → 关 channel → 后续所有 LS_TERMINATED。
/// 修复后：统一走 `ensure_open` 的 `content_version` 内部递增。
#[tokio::test]
async fn repeated_mtime_changes_yield_monotonic_versions() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("d.cpp");
    tokio::fs::write(&file, b"v1\n")
        .await
        .expect("write v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _g1 = session.ensure_open(&file).await.expect("first open");
    for i in 2..=4u64 {
        time::sleep(Duration::from_millis(60)).await;
        tokio::fs::write(&file, format!("v{i}\n").as_bytes())
            .await
            .expect("rewrite");
        time::sleep(Duration::from_millis(60)).await;
        let _g = session
            .ensure_open(&file)
            .await
            .expect("subsequent ensure_open");
    }
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let changes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didChange"))
        .collect();
    assert_eq!(opens.len(), 1, "4 次 ensure_open 仅首次 didOpen；events={events:?}");
    assert_eq!(changes.len(), 3, "后 3 次 mtime 变化 → 3 次 didChange；events={events:?}");
    let versions: Vec<i64> = std::iter::once(opens[0].get("version").and_then(Value::as_i64).unwrap())
        .chain(changes.iter().map(|e| e.get("version").and_then(Value::as_i64).unwrap()))
        .collect();
    assert_eq!(versions, vec![1, 2, 3, 4], "version 必须严格单调递增");

    session.shutdown().await;
}

/// P1 修复 #5：单写门跨工具（edit_tools::commit_change → tool_replace_body）场景下，
/// 一次 ensure_open 拿到的 guard 在被替换为下一个 ensure_open 之前释放，
/// 等价于「FileGuard drop → ref_count 归零 → 后续 ensure_open 重新 didOpen」，
/// 不会因 ref_count 残留导致 version 撞车。
#[tokio::test]
async fn guard_drop_then_reopen_restarts_version_at_one() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("e.cpp");
    tokio::fs::write(&file, b"v1\n").await.expect("write v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 周期 1：guard 持有 → drop → ref_count=0 → buffer 清空。
    let g1 = session.ensure_open(&file).await.expect("open #1");
    drop(g1);
    time::sleep(Duration::from_millis(60)).await;

    // 周期 2：buffer 已移除 → 重新走 didOpen（version 重置为 1，非承接上轮 version）。
    let _g2 = session.ensure_open(&file).await.expect("reopen");
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        opens.len(),
        2,
        "guard drop 后再 ensure_open 应再发一次 didOpen；events={events:?}"
    );
    assert_eq!(opens[1].get("version").and_then(Value::as_i64), Some(1));
    assert!(
        !closes.is_empty(),
        "drop 应触发 didClose；events={events:?}"
    );

    session.shutdown().await;
}

/// P1 修复 #6：单 session 跨文件并行 ensure_open —— 不同 file URI 各自维护 version，
/// 互不串扰（与 P1 修复前"全文件共用静态 VERSION 计数器"的关键区别）。
#[tokio::test]
async fn different_files_keep_independent_versions() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file_a = tmp.path().join("a.cpp");
    let file_b = tmp.path().join("b.cpp");
    tokio::fs::write(&file_a, b"a1\n").await.expect("write a1");
    tokio::fs::write(&file_b, b"b1\n").await.expect("write b1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _ga1 = session.ensure_open(&file_a).await.expect("a open");
    let _gb1 = session.ensure_open(&file_b).await.expect("b open");

    time::sleep(Duration::from_millis(60)).await;
    tokio::fs::write(&file_a, b"a2\n").await.expect("rewrite a2");
    tokio::fs::write(&file_b, b"b2\n").await.expect("rewrite b2");
    time::sleep(Duration::from_millis(60)).await;

    let _ga2 = session.ensure_open(&file_a).await.expect("a reopen");
    let _gb2 = session.ensure_open(&file_b).await.expect("b reopen");
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let uri_a = file_uri(&file_a);
    let uri_b = file_uri(&file_b);
    let a_opens: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("didOpen")
                && e.get("uri").and_then(Value::as_str) == Some(uri_a.as_str())
        })
        .collect();
    let b_opens: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("didOpen")
                && e.get("uri").and_then(Value::as_str) == Some(uri_b.as_str())
        })
        .collect();
    let a_changes: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("didChange")
                && e.get("uri").and_then(Value::as_str) == Some(uri_a.as_str())
        })
        .collect();
    let b_changes: Vec<&Value> = events
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("didChange")
                && e.get("uri").and_then(Value::as_str) == Some(uri_b.as_str())
        })
        .collect();
    assert_eq!(a_opens.len(), 1, "a 仅首次 didOpen；events={events:?}");
    assert_eq!(b_opens.len(), 1, "b 仅首次 didOpen；events={events:?}");
    assert_eq!(a_changes.len(), 1, "a mtime 变化 1 次 → 1 次 didChange；events={events:?}");
    assert_eq!(b_changes.len(), 1, "b mtime 变化 1 次 → 1 次 didChange；events={events:?}");
    assert_eq!(a_opens[0].get("version").and_then(Value::as_i64), Some(1));
    assert_eq!(b_opens[0].get("version").and_then(Value::as_i64), Some(1));
    assert_eq!(a_changes[0].get("version").and_then(Value::as_i64), Some(2));
    assert_eq!(b_changes[0].get("version").and_then(Value::as_i64), Some(2));

    session.shutdown().await;
}

/// 用例 #4：外部改文件但 mtime 被拨回原值（mtime 粒度窗口内的改写）→
/// size 因子检出，仍触发 didChange。旧实现只对账 mtime 时此场景漏检。
#[tokio::test]
async fn ensure_open_same_mtime_different_size_emits_did_change() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("d.cpp");
    tokio::fs::write(&file, b"v1\n")
        .await
        .expect("write fixture v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _g1 = session.ensure_open(&file).await.expect("first ensure_open");

    // 改成不同长度的内容，再把 mtime 拨回记账值 —— 模拟 mtime
    // 粒度窗口内的外部改写（只有 size 能区分）。
    let old_mtime = tokio::fs::metadata(&file)
        .await
        .expect("stat v1")
        .modified()
        .expect("mtime");
    tokio::fs::write(&file, b"v2 with a much longer body\n")
        .await
        .expect("rewrite fixture v2");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .expect("open for set_times");
    f.set_times(std::fs::FileTimes::new().set_modified(old_mtime))
        .expect("set_times");

    let _g2 = session
        .ensure_open(&file)
        .await
        .expect("second ensure_open");

    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let changes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didChange"))
        .collect();
    assert_eq!(
        changes.len(),
        1,
        "同 mtime 不同 size 的外部改写必须被 size 因子检出并发 didChange；events={events:?}"
    );
    assert_eq!(changes[0].get("version").and_then(Value::as_i64), Some(2));

    session.shutdown().await;
}
