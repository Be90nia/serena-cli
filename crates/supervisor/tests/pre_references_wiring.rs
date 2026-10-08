//! P1 audit #2 接线测试：references 路径必须在发请求前调用 adapter.pre_references
//! （nextflow flush = 2×completion + 1×documentSymbol，↖ mirror
//! `_flush_deferred_workspace_scan`）。
//!
//! 桩 LS = 本测试进程自 spawn：libtest 以用例名过滤跑单个用例，该用例读到
//! `SERENA_MOCK_NF_CHILD` 环境变量后化身同步 LSP 桩（stdin/stdout 帧循环），并把
//! 收到的每个请求 method 追加写 track 文件。零新依赖、不需要真 nextflow JAR ——
//! 断言对象是「请求到达 LS 的顺序」，接线被删时 references 会先于一切 flush 到达。

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_core::session::Session;

const CHILD_ENV: &str = "SERENA_MOCK_NF_CHILD";
const CHILD_TEST: &str = "mock_nextflow_ls_child";

fn in_child() -> bool {
    std::env::var(CHILD_ENV).is_ok()
}

/// 桩进程入口：常规套件运行时 env 缺失 → 空操作；父测试以本用例名过滤 spawn
/// 本 exe，env 携带 track 路径 → 化身 LSP 桩。
#[test]
fn mock_nextflow_ls_child() {
    let Ok(track) = std::env::var(CHILD_ENV) else {
        return;
    };
    serve_stub(Path::new(&track));
}

fn read_frame(reader: &mut impl BufRead) -> Option<serde_json::Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            return None; // EOF：父进程关 stdin / Job 灭树 → 桩退出
        }
        let Some(len) = line
            .strip_prefix("Content-Length:")
            .and_then(|v| v.trim().parse::<usize>().ok())
        else {
            continue; // 头区间杂散行（libtest 输出等）跳过，帧体走 read_exact
        };
        let mut blank = String::new();
        reader.read_line(&mut blank).ok()?;
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).ok()?;
        return serde_json::from_slice(&body).ok();
    }
}

fn write_msg(out: &mut impl Write, v: &serde_json::Value) {
    let body = serde_json::to_string(v).expect("serialize");
    write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("write frame");
    out.flush().expect("flush");
}

fn track_request(track: &Path, method: &str) {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(track)
        .expect("open track");
    writeln!(f, "{method}").expect("append track");
}

fn serve_stub(track: &Path) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut out = stdout.lock();
    while let Some(msg) = read_frame(&mut reader) {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let Some(id) = msg.get("id").cloned() else {
            if method == "exit" {
                return;
            }
            continue; // 通知（initialized / didOpen）：忽略
        };
        if method == "initialize" {
            write_msg(
                &mut out,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "capabilities": {} },
                }),
            );
            continue;
        }
        track_request(track, method);
        write_msg(
            &mut out,
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": null }),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_references_flush_precedes_references_request() {
    if in_child() {
        return; // 桩进程内不跑父逻辑
    }
    let dir = std::env::temp_dir().join(format!("serena_prefl_wiring_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let track = dir.join("track.log");
    let _ = std::fs::remove_file(&track);
    std::fs::write(dir.join("a.nf"), "process GREET {}\n").unwrap();

    let exe = std::env::current_exe().expect("current_exe");
    let child = ls_runtime::process::Child::spawn(LaunchInfo {
        cmd: vec![exe.into_os_string(), std::ffi::OsString::from(CHILD_TEST)],
        cwd: dir.clone(),
        env: vec![(CHILD_ENV.into(), track.to_string_lossy().into_owned())],
        transport: TransportKind::Stdio,
    })
    .expect("spawn stub LS");

    let session = Arc::new(
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Session::start(Some(child), lsp_types::InitializeParams::default()),
        )
        .await
        .expect("handshake within 30s")
        .expect("session ready"),
    );
    session.set_language_id("nextflow");

    let refs = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        supervisor::ref_tools::find_referencing_symbols(&session, &dir, "a.nf", 0, 0),
    )
    .await
    .expect("references flow within 60s")
    .expect("references flow must succeed");

    // 响应可为空，但请求到达 LS 的顺序必须证明 flush 先行：references 之前
    // ≥2 次 completion + ≥1 次 documentSymbol。
    let log = std::fs::read_to_string(&track).unwrap_or_default();
    let methods: Vec<&str> = log.lines().collect();
    let first_refs = methods
        .iter()
        .position(|m| *m == "textDocument/references")
        .expect("references request must reach the LS");
    let completions = methods[..first_refs]
        .iter()
        .filter(|m| **m == "textDocument/completion")
        .count();
    assert!(
        methods[..first_refs].contains(&"textDocument/documentSymbol"),
        "flush documentSymbol must precede references; got {methods:?}"
    );
    assert!(
        completions >= 2,
        "flush 2×completion must precede references; got {methods:?}"
    );
    drop(refs);
    drop(session); // Arc 归零 → Job 关句柄 → 桩进程灭树
    let _ = std::fs::remove_dir_all(&dir);
}
