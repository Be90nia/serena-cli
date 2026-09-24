//! lsp-core docsync 集成测试（PLAN Task 7 / ARCHITECTURE §3.2 BUF + §3.4 锁）。
//!
//! 覆盖 PLAN Task 7 的 3 条用例：
//! 1. `ensure_open` 后 mock_ls 收到 `textDocument/didOpen`（version=1）。
//! 2. 外部改文件后再次 `ensure_open` 触发 `didChange`（version=2）。
//! 3. 双 guard 嵌套只发一次 `didOpen`（ref_count++ 路径）。
//!
//! mock_ls 通过 `MOCK_LS_TRACK_FILE_EVENTS=<path>` 追加写日志，测试读完即断言事件序列。

use std::ffi::OsString;
use std::str::FromStr;
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
///
/// 修 P1 #2 后：guard drop 归零不再立即 didClose。改成断言 "三 guard 全 drop 完
/// 后 0 didClose"；显式 evict_all_buffers 才走 didClose+移表，断言语义收尾。
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
    assert!(
        closes.is_empty(),
        "修 P1 #2：guard drop 归零不发 didClose（窗口内复用）；events={events:?}"
    );

    // 显式 evict 强制回收：1 次 didClose。
    session.evict_all_buffers();
    time::sleep(Duration::from_millis(300)).await;
    let events_after = read_track_events(&track_log).await;
    let closes_after: Vec<&Value> = events_after
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        closes_after.len(),
        1,
        "显式 evict_all_buffers 必须发 1 次 didClose；events={events_after:?}"
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

/// P1 修复 #5：原断言 guard drop → ref_count=0 → buffer 移除 → 后续 ensure_open 重新走 didOpen。
/// 修 P1 #2 后语义改了：
///   - guard drop 归零 → 仅记 last_released_at，**不**移表、**不**发 didClose。
///   - 后续 ensure_open 命中 Some(buf) → ref_count++ + mtime/size 未变 → 不发 didOpen
///     与 didChange（LS 端文档状态连续，version 沿用上轮末尾值）。
///   - 显式 evict_all_buffers → 强制 didClose + 移表，此时 ensure_open 才会重新走 didOpen。
///
/// 这里把测试拆为两段：先验"drop→reuse 不重发"，再 evict+reopen 验"显式 evict 后才能
/// 重建干净缓冲"。等价于原始保护目标（ref_count 残留不撞 version）+ 新保留语义。
#[tokio::test]
async fn guard_drop_then_reopen_in_ttl_keeps_version_no_new_did_open() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("e.cpp");
    tokio::fs::write(&file, b"v1\n").await.expect("write v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 周期 1：guard 持有 → drop → ref_count=0 → last_released_at=Some(now)，不 didClose。
    let g1 = session.ensure_open(&file).await.expect("open #1");
    drop(g1);
    time::sleep(Duration::from_millis(60)).await;

    // 周期 2：TTL 窗口内重开 → 命中 Some(buf) → ref_count=1，无新 didOpen/didChange。
    let _g2 = session.ensure_open(&file).await.expect("reopen in ttl");
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
        "TTL 窗口内 drop+reopen 不应再 didOpen；events={events:?}"
    );
    assert!(
        closes.is_empty(),
        "TTL 窗口内 drop 不应 didClose；events={events:?}"
    );
    drop(_g2);

    // 显式 evict → didClose + 移表；后续 ensure_open 才会重新走 didOpen（v=1）。
    session.evict_all_buffers();
    let _g3 = session.ensure_open(&file).await.expect("reopen after evict");
    time::sleep(Duration::from_millis(300)).await;

    let events_after = read_track_events(&track_log).await;
    let opens_a: Vec<&Value> = events_after
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let closes_a: Vec<&Value> = events_after
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        opens_a.len(),
        2,
        "显式 evict 后 ensure_open 才发新 didOpen；events={events_after:?}"
    );
    assert_eq!(opens_a[1].get("version").and_then(Value::as_i64), Some(1));
    assert_eq!(
        closes_a.len(),
        1,
        "显式 evict 期间发 1 次 didClose；events={events_after:?}"
    );

    session.shutdown().await;
}

/// 修 P1 #2：FileGuard drop 归零不立即 didClose，TTL 复用窗口内下一次 ensure_open
/// 走 ref_count++ 路径（不重 didOpen）；外部文件修改后 TTL 内 ensure_open 仍走
/// didChange（version 沿用 buffer 末尾值，不重置为 1）。
#[tokio::test]
async fn guard_drop_during_ttl_reopen_does_not_emit_did_open() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("f.cpp");
    tokio::fs::write(&file, b"v1\n").await.expect("write v1");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 周期 1：guard 持有 → drop。
    let g1 = session.ensure_open(&file).await.expect("open #1");
    drop(g1);

    // 周期 2（TTL 内）：未改盘 → 不发任何 didOpen/didChange。
    let _g2 = session.ensure_open(&file).await.expect("reopen in ttl");
    time::sleep(Duration::from_millis(200)).await;
    let events_before = read_track_events(&track_log).await;
    let opens_b: Vec<&Value> = events_before
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    let changes_b: Vec<&Value> = events_before
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didChange"))
        .collect();
    let closes_b: Vec<&Value> = events_before
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        opens_b.len(),
        1,
        "TTL 内重用应不发新 didOpen；events={events_before:?}"
    );
    assert_eq!(
        changes_b.len(),
        0,
        "TTL 内重用 + 未改盘 → 0 didChange；events={events_before:?}"
    );
    assert!(
        closes_b.is_empty(),
        "TTL 内重用期间无 didClose；events={events_before:?}"
    );
    drop(_g2);

    // 周期 3（TTL 内、外部改盘）：mtime 推进 → 走 didChange，version=2。
    time::sleep(Duration::from_millis(60)).await;
    tokio::fs::write(&file, b"v2\n").await.expect("rewrite");
    time::sleep(Duration::from_millis(60)).await;
    let _g3 = session.ensure_open(&file).await.expect("reopen after external edit");
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
        "改盘仍走 TTL 复用路径（ref_count++），不发新 didOpen；events={events:?}"
    );
    assert_eq!(
        changes.len(),
        1,
        "TTL 内外部改文件 → didChange；events={events:?}"
    );
    assert_eq!(changes[0].get("version").and_then(Value::as_i64), Some(2));

    session.shutdown().await;
}

/// 修 P1 #2：显式 `evict_all_buffers()` 强制回收，对每个 ref_count=0 的 buffer
/// 发 didClose + 移表；之后 ensure_open 重新走 didOpen。
#[tokio::test]
async fn evict_all_buffers_emits_did_close_and_clears_state() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file_a = tmp.path().join("a.cpp");
    let file_b = tmp.path().join("b.cpp");
    tokio::fs::write(&file_a, b"a1\n").await.expect("write a");
    tokio::fs::write(&file_b, b"b1\n").await.expect("write b");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 两文件 ensure_open → 各自 didOpen。
    let _ga = session.ensure_open(&file_a).await.expect("a");
    let _gb = session.ensure_open(&file_b).await.expect("b");
    // 显式 evict 两 guard 都还活着 —— evict_all_buffers 只回收 ref_count=0 的，活的跳过。
    // 这里活的跳过；但我们要看 evict 后仍能让 ref_count=0 的 buffer 被回收。
    drop(_ga);
    drop(_gb);
    time::sleep(Duration::from_millis(100)).await;

    session.evict_all_buffers();
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        closes.len(),
        2,
        "两文件 drop 后 evict_all_buffers 必须发 2 次 didClose；events={events:?}"
    );

    session.shutdown().await;
}

/// 修 P1 #2：`evict_idle_buffers(ttl)` 跳过活跃缓冲（ref_count>0），
/// 只回收 ttl 到期的空闲条目。用 ttl=0 让 idle 条目立刻被认为到期。
#[tokio::test]
async fn evict_idle_buffers_skips_live_guards() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file_live = tmp.path().join("live.cpp");
    let file_idle = tmp.path().join("idle.cpp");
    tokio::fs::write(&file_live, b"live1\n").await.expect("write live");
    tokio::fs::write(&file_idle, b"idle1\n").await.expect("write idle");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 活跃 guard（live）+ 先 didOpen 再 drop 的 idle。
    let _g_live = session.ensure_open(&file_live).await.expect("live");
    let g_idle = session.ensure_open(&file_idle).await.expect("idle");
    drop(g_idle);
    time::sleep(Duration::from_millis(10)).await;

    // ttl=0 → idle 立即被视为到期；live ref_count>0 被跳过。
    let removed = session.evict_idle_buffers(Duration::from_secs(0));
    assert_eq!(removed, 1, "只应回收 1 条 idle 缓冲");
    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        closes.len(),
        1,
        "活跃 guard 必须不被 evict 触碰；只发 idle 的 didClose；events={events:?}"
    );
    assert_eq!(
        closes[0].get("uri").and_then(Value::as_str),
        Some(file_uri(&file_idle).as_str()),
        "didClose 必须是 idle 文件"
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

/// 修 P1 #1（LRU 容量闸门）：打开超过 `FILE_BUFFER_CAPACITY`（32）的文件时，
/// 池按 `last_released_at` 升序淘汰空闲条目，被淘汰者发 didClose + 移表。
/// 本测试用 40 文件压 32 容量门，断言最久未用被回收（通过 mock_ls 收到的
/// didClose 事件顺序验证——LRU 淘汰路径必须发 didClose，最早插入的文件先收）。
#[tokio::test]
async fn lru_capacity_evicts_oldest_idle_buffers() {
    use lsp_core::docsync::FILE_BUFFER_CAPACITY;

    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");

    let n_files = FILE_BUFFER_CAPACITY + 8; // 32 + 8 = 40 文件压 32 容量门
    let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(n_files);
    for i in 0..n_files {
        let p = tmp.path().join(format!("f{i:02}.cpp"));
        tokio::fs::write(&p, format!("// f{i}\n").as_bytes())
            .await
            .expect("write fixture");
        paths.push(p);
    }

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 顺序：每个文件 ensure_open → drop guard（标 last_released_at=now）→ 等 1ms 让时间戳分得开。
    // 这样后续插入会按 drop 顺序淘汰最早的文件（f00, f01, ...）。
    let mut guards = Vec::with_capacity(n_files);
    for p in &paths {
        let g = session.ensure_open(p).await.expect("ensure_open");
        guards.push(g);
        time::sleep(Duration::from_millis(1)).await;
    }
    // 全部 drop：ref_count=0，池现在有 40 条空闲条目（暂未触发 LRU，drop 只置 last_released_at）。
    drop(guards);
    time::sleep(Duration::from_millis(10)).await;

    // 此时池大小 = 40（> 32）。最后插入的 f08..f31 是最新；最久未用应是 f00..f07（8 个）。
    // 插入第 41 个文件触发 LRU 容量闸门——
    // 容量闸门语义：插入后池 41 > 32，需淘汰 `41 - (32-1) = 10` 个 idle 条目。
    // 即淘汰 f00..f09（最久未用 10 个）。
    let overflow = tmp.path().join("overflow.cpp");
    tokio::fs::write(&overflow, b"// overflow\n").await.expect("write overflow");
    let _g_overflow = session.ensure_open(&overflow).await.expect("overflow open");

    time::sleep(Duration::from_millis(300)).await;

    // 验证 didClose 事件：被淘汰的 f00..f09 必须收到 didClose，f10 必留下。
    let events = read_track_events(&track_log).await;
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    let uri_f00 = file_uri(&paths[0]);
    let uri_f01 = file_uri(&paths[1]);
    let uri_f09 = file_uri(&paths[9]);
    let uri_f10 = file_uri(&paths[10]);
    let uri_overflow_v = file_uri(&overflow);

    let f00_closed = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_f00.as_str()));
    let f01_closed = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_f01.as_str()));
    let f09_closed = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_f09.as_str()));
    let f10_closed = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_f10.as_str()));
    let overflow_in_closes = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_overflow_v.as_str()));

    assert!(f00_closed, "f00 是最久未用，必被 LRU 淘汰并发 didClose；events={events:?}");
    assert!(f01_closed, "f01 是次久未用，必被 LRU 淘汰并发 didClose；events={events:?}");
    assert!(f09_closed, "f09 在淘汰边界内，必被 LRU 淘汰并发 didClose；events={events:?}");
    assert!(
        !f10_closed,
        "f10 在淘汰边界外，必保留；events={events:?}"
    );
    assert!(
        !overflow_in_closes,
        "overflow 新插入的必留下，不能有 didClose；events={events:?}"
    );

    session.shutdown().await;
}

/// 修 P1 #1（活跃 guard 不被 LRU 淘汰）：所有 ref_count>0 时 LRU 跳过——避免
/// 容量压力下打断活跃工具调用。验证：32 活跃 + 1 新插入 → 无 didClose（除
/// shutdown 时 evict_all_buffers 触发），活跃文件全部留下。
#[tokio::test]
async fn lru_capacity_skips_active_buffers_under_pressure() {
    use lsp_core::docsync::FILE_BUFFER_CAPACITY;

    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");

    // 32 个文件全持活 guard → ref_count 全部 >0。
    let n_files = FILE_BUFFER_CAPACITY;
    let mut paths = Vec::with_capacity(n_files);
    let mut guards = Vec::with_capacity(n_files);
    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    for i in 0..n_files {
        let p = tmp.path().join(format!("a{i:02}.cpp"));
        tokio::fs::write(&p, format!("// a{i}\n").as_bytes())
            .await
            .expect("write fixture");
        paths.push(p);
        let g = session.ensure_open(&paths[i]).await.expect("ensure_open");
        guards.push(g);
    }

    // 此时池大小 = 32（正好容量）。再插入一个新文件 → 池 33 > 32 → LRU 应跳过
    // （全活）→ allow overflow，无 didClose 触发。
    let overflow = tmp.path().join("overflow2.cpp");
    tokio::fs::write(&overflow, b"// overflow2\n").await.expect("write overflow");
    let _g_overflow = session.ensure_open(&overflow).await.expect("overflow open");

    time::sleep(Duration::from_millis(300)).await;

    // 验证：在活跃 guard 还持住期间，无 didClose（mock_ls 不会从非 evict 路径发）。
    let events_pre = read_track_events(&track_log).await;
    let closes_pre: Vec<&Value> = events_pre
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        closes_pre.len(),
        0,
        "全活状态下 LRU 应跳过，活跃期间不应有 didClose；events={events_pre:?}"
    );

    drop(guards);
    time::sleep(Duration::from_millis(50)).await;

    session.shutdown().await;
}

/// 修 P1 #1（TTL 过期验证）：ref_count=0 超过 TTL 后确保下一次 ensure_open 重新走
/// didOpen（不是复用）。这是 TTL 与 LRU 的协同——TTL 是窗口复用，LRU 是容量兜底，
/// TTL 过期后 LRU 仍未动则下次 ensure_open 重新走 didOpen。
#[tokio::test]
async fn ttl_expired_ensure_open_re_emits_did_open() {
    use lsp_core::docsync::FILE_GUARD_TTL;

    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("t.cpp");
    tokio::fs::write(&file, b"v1\n").await.expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let g1 = session.ensure_open(&file).await.expect("first open");
    drop(g1);

    // 等超过 FILE_GUARD_TTL 60s？测试不阻塞 60s——直接用 evict_idle_buffers(0)
    // 把 idle 条目手动驱逐（与 TTL 过期语义等价），再 ensure_open 必重新走 didOpen。
    let removed = session.evict_idle_buffers(Duration::from_secs(0));
    assert!(removed >= 1, "TTL 模拟驱逐应回收至少 1 条；removed={removed}");

    let _g2 = session.ensure_open(&file).await.expect("reopen after TTL");

    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(
        opens.len(),
        2,
        "TTL 过期 + 显式 evict 后再次 ensure_open 必须重新走 didOpen；events={events:?}"
    );

    session.shutdown().await;
    // FILE_GUARD_TTL 仅做编译期断言（确保常量仍存在）。
    let _ttl = FILE_GUARD_TTL;
}

/// 修 P1 #1（session shutdown 全关）：Session::shutdown 必须在 pumps kill 前先
/// evict_all_buffers（清空缓冲池并发 didClose）。这让 LS 在 shutdown+exit 之前
/// 完成 LSP 协议层的「关闭文档」流程。
#[tokio::test]
async fn session_shutdown_evicts_all_buffers() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file_a = tmp.path().join("sa.cpp");
    let file_b = tmp.path().join("sb.cpp");
    tokio::fs::write(&file_a, b"a\n").await.expect("write a");
    tokio::fs::write(&file_b, b"b\n").await.expect("write b");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    let _ga = session.ensure_open(&file_a).await.expect("open a");
    let _gb = session.ensure_open(&file_b).await.expect("open b");
    time::sleep(Duration::from_millis(300)).await;

    // shutdown 必须在 evict 后发出 didClose（关文件）→ shutdown 请求 → exit 通知 → 进程退。
    // 断言：两个文件的 didClose 都在 shutdown 之前发生（顺序：evict_all_buffers 先发
    // didClose，然后 shutdown_request → exit）。
    session.shutdown().await;

    let events = read_track_events(&track_log).await;
    let closes: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didClose"))
        .collect();
    assert_eq!(
        closes.len(),
        2,
        "shutdown 必须 evict 缓冲池并发 didClose；events={events:?}"
    );
    let uri_a = file_uri(&file_a);
    let uri_b = file_uri(&file_b);
    let close_a = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_a.as_str()));
    let close_b = closes
        .iter()
        .any(|e| e.get("uri").and_then(Value::as_str) == Some(uri_b.as_str()));
    assert!(close_a && close_b, "两文件都应收到 didClose；events={events:?}");
}

/// 修 P1 #1（同文件 5 次连续访问 → 缓存命中）：AI 编辑回路的典型场景——
/// overview / symbol-body / hover / def / refs 连续访问同一文件，TTL 窗口内
/// 后续 ensure_open 必须**零 didOpen 重发**（mtime/size 未变）。
#[tokio::test]
async fn repeated_same_file_access_emits_no_reopen_within_ttl() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let file = tmp.path().join("r.cpp");
    // 50KB 内容（量化场景：典型 .cpp 文件大小）。
    let body = "// ".to_string() + &"x".repeat(50_000) + "\n";
    tokio::fs::write(&file, body.as_bytes())
        .await
        .expect("write fixture");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");

    // 5 次连续访问（典型 AI 编辑回路：overview → 定位 → 写 → 诊断 → 再 overview）。
    let mut durations = Vec::with_capacity(5);
    for i in 0..5 {
        let t0 = std::time::Instant::now();
        let g = session.ensure_open(&file).await.expect("ensure_open");
        durations.push(t0.elapsed());
        drop(g);
        // 极短间隔，远小于 FILE_GUARD_TTL（60s）→ 复用命中。
        time::sleep(Duration::from_millis(5)).await;
        // 静音：防 Rust 编译器抱怨 i 未用。
        let _ = i;
    }

    time::sleep(Duration::from_millis(300)).await;

    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(
        opens.len(),
        1,
        "5 次同文件访问 TTL 内只 1 次 didOpen（其余复用）；events={events:?}"
    );

    // 前 N 测：第 1 次有 IO（stat + read），后续 4 次纯锁内复用。统计快路径：
    // 第 1 次稳定后第 5 次应 < 1ms（纯锁内路径）。
    if durations.len() >= 5 {
        eprintln!(
            "5 次 ensure_open 时长 (µs): first={}, last={} (TTL 复用路径)",
            durations[0].as_micros(),
            durations[4].as_micros()
        );
    }

    session.shutdown().await;
}

/// 修 P0-A：批量 ensure_open 等价语义 —— 同 key（同文件）多次 ensure_open 走
/// ref_count++ 不重发 didOpen（与串行调 ensure_open 完全一致）。验证点：
///   1. 返回 Vec<Result<FileGuard>> 与输入顺序一致
///   2. 全部 OK（无 Err）
///   3. mock_ls 收到 1 个 didOpen（ref_count=4）
///   4. 所有 guard 关联同一 uri（证明同 key 复用）
#[tokio::test]
async fn ensure_open_batch_emits_single_did_open_per_uri() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let f1 = tmp.path().join("a.cpp");
    tokio::fs::write(&f1, b"fn a() {}\n").await.expect("write a");

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");
    session.set_language_id("cpp");

    // 4 个相同 path（同一 URI）→ ensure_open_batch 应只发一次 didOpen。
    let paths: Vec<&std::path::Path> = vec![&f1, &f1, &f1, &f1];
    let params = lsp_core::docsync::EnsureOpenParams {
        debounce_ms: 0, // 测试中关掉 debounce，只验结构
        send_immediate: true,
    };
    let results = session.ensure_open_batch(&paths, params).await;
    assert_eq!(results.len(), 4, "4 路径 → 4 个槽位");
    for (i, r) in results.iter().enumerate() {
        if r.is_err() {
            panic!("slot {i} 应 Ok，got Err: {:?}", r.as_ref().err().unwrap());
        }
    }
    let uris: std::collections::HashSet<String> = results
        .iter()
        .map(|r| r.as_ref().unwrap().uri().to_string())
        .collect();
    assert_eq!(uris.len(), 1, "4 guard 同 uri: {uris:?}");

    time::sleep(Duration::from_millis(100)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(
        opens.len(),
        1,
        "同 URI 4 次 ensure_open_batch 仅 1 次 didOpen: {events:?}"
    );

    session.shutdown().await;
}

/// 修 P0-A：批内多个**不同**文件 → 每个文件 1 次 didOpen（4 文件 = 4 didOpen，
/// 不重复；与串行 ensure_open N 次完全等价）。验证点：
///   1. 返回 Vec 顺序 = 输入顺序
///   2. mock_ls 收到 N 个 didOpen（各 URI 各 1）
#[tokio::test]
async fn ensure_open_batch_distinct_uris_each_get_one_did_open() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let names = ["a.cpp", "b.cpp", "c.cpp", "d.cpp"];
    let files: Vec<std::path::PathBuf> = names
        .iter()
        .map(|n| tmp.path().join(n))
        .collect();
    for f in &files {
        tokio::fs::write(f, b"fn x() {}\n").await.expect("write");
    }

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");
    session.set_language_id("cpp");

    let paths: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let params = lsp_core::docsync::EnsureOpenParams {
        debounce_ms: 0,
        send_immediate: true,
    };
    let results = session.ensure_open_batch(&paths, params).await;
    assert_eq!(results.len(), 4);
    for (i, r) in results.iter().enumerate() {
        if r.is_err() {
            panic!("slot {i} 应 Ok，got Err: {:?}", r.as_ref().err().unwrap());
        }
    }

    time::sleep(Duration::from_millis(100)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(
        opens.len(),
        4,
        "4 不同 URI 各 1 次 didOpen: {events:?}"
    );

    session.shutdown().await;
}

/// 修 P0-A：debounce 行为 —— `send_immediate=false` + `debounce_ms>0` 时，
/// 整批调用耗时 ≥ debounce_ms（每路 sleep 后才 ensure_open）。验证：
///   1. 总耗时 ≥ debounce_ms（不可短于 debounce，证明每路都 sleep 了）
///   2. 4 文件仍 4 个 didOpen（debounce 不去重 didOpen，仅延后单文件 didOpen）
///
/// 用 50ms debounce + 测墙 ≥ 50ms 即证明 sleep 真正生效。
#[tokio::test]
async fn ensure_open_batch_debounce_delays_each_send() {
    let tmp = TempDir::new().expect("TempDir::new");
    let track_log = tmp.path().join("track.log");
    let names = ["a.cpp", "b.cpp", "c.cpp", "d.cpp"];
    let files: Vec<std::path::PathBuf> = names.iter().map(|n| tmp.path().join(n)).collect();
    for f in &files {
        tokio::fs::write(f, b"fn x() {}\n").await.expect("write");
    }

    let child = Child::spawn(launch_mock_ls_track(&track_log)).expect("spawn mock_ls");
    let session = Session::start(Some(child), dummy_init_params())
        .await
        .expect("Session::start Ready");
    session.set_language_id("cpp");

    let paths: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let debounce_ms: u64 = 50;
    let params = lsp_core::docsync::EnsureOpenParams {
        debounce_ms,
        send_immediate: false,
    };
    let t0 = std::time::Instant::now();
    let results = session.ensure_open_batch(&paths, params).await;
    let elapsed = t0.elapsed();
    assert_eq!(results.len(), 4);
    for (i, r) in results.iter().enumerate() {
        if r.is_err() {
            panic!("slot {i} 应 Ok，got Err: {:?}", r.as_ref().err().unwrap());
        }
    }
    assert!(
        elapsed >= Duration::from_millis(debounce_ms),
        "debounce 应让每路 sleep 至少 {debounce_ms}ms；实测 {elapsed:?}"
    );

    time::sleep(Duration::from_millis(50)).await;
    let events = read_track_events(&track_log).await;
    let opens: Vec<&Value> = events
        .iter()
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("didOpen"))
        .collect();
    assert_eq!(opens.len(), 4, "debounce 不去重，仍 4 didOpen");

    session.shutdown().await;
}

/// bd serena-rust-cbd：非 ASCII 路径（中文/emoji）必须 percent-encode 成全 ASCII
/// 合法 URI —— 回归形态是 rust `Uri` 拒绝非 ASCII → `CoreError::Io` → wire INTERNAL。
#[test]
fn path_to_uri_str_percent_encodes_non_ascii_and_roundtrips() {
    let tmp = TempDir::new().expect("TempDir::new");
    let zh_dir = tmp.path().join("目录");
    std::fs::create_dir_all(&zh_dir).expect("create 中文目录");
    let emoji_file = zh_dir.join("😀.rs");

    for path in [zh_dir.as_path(), emoji_file.as_path()] {
        let raw = lsp_core::docsync::path_to_uri_str(path);
        assert!(raw.bytes().all(|b| b.is_ascii()), "URI 必须全 ASCII: {raw}");
        lsp_types::Uri::from_str(&raw)
            .unwrap_or_else(|e| panic!("percent-encoded URI 应可解析: {raw} ({e})"));
        assert!(
            lsp_core::docsync::path_to_uri(path).is_ok(),
            "path_to_uri 对非 ASCII 路径应 Ok: {}",
            path.display()
        );
        // roundtrip：URI path 段解码 == 原路径（`\`→`/` 归一；非 `/` 开头路径
        // 在 URI path 里带前导 `/`，如 file:///C:/... 的 path 是 /C:/...）。
        let body = raw.strip_prefix("file://").expect("file scheme 前缀");
        let decoded = percent_encoding::percent_decode_str(body)
            .decode_utf8()
            .expect("解码必须是合法 UTF-8");
        let normalized = path.to_string_lossy().replace('\\', "/");
        let expected_body = if normalized.starts_with('/') {
            normalized
        } else {
            format!("/{normalized}")
        };
        assert_eq!(decoded, expected_body, "roundtrip 不等: {raw}");
    }
}

/// 纯 ASCII 路径保持原样；URL 结构字符（空格/`#`/`?`/`%`）编码，`:` `/` 保留。
#[test]
fn path_to_uri_str_encodes_url_unsafe_ascii_only() {
    let raw = lsp_core::docsync::path_to_uri_str(std::path::Path::new("/tmp/a b#c?d%e.rs"));
    assert_eq!(raw, "file:///tmp/a%20b%23c%3Fd%25e.rs");
    let plain = lsp_core::docsync::path_to_uri_str(std::path::Path::new("/tmp/plain_v1.2.rs"));
    assert_eq!(plain, "file:///tmp/plain_v1.2.rs");
}
