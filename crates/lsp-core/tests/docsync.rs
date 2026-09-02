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
