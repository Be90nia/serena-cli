//! Cold-start hang repro for the daemon HTTP path (diagnostic).
//!
//! Spawns an in-process daemon (axum + supervisor + rust-analyzer) and measures
//! the FIRST vs SECOND `POST /tools/overview` round-trip on a cold supervisor.
//!
//! See `local/cold-start-hang-diagnosis.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use supervisor::Supervisor;

fn rust_demo_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fixtures")
        .join("rust_demo")
}

fn rust_analyzer_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = if cfg!(windows) {
            dir.join("rust-analyzer.exe")
        } else {
            dir.join("rust-analyzer")
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_start_overview_via_daemon_http() {
    eprintln!("=== cold-start repro (daemon HTTP in-process) ===");
    let root = rust_demo_path();
    if rust_analyzer_on_path().is_none() {
        eprintln!("SKIP: rust-analyzer not on PATH");
        return;
    }
    eprintln!("fixture root: {}", root.display());
    let root_str = root.to_string_lossy().to_string();

    let t0 = Instant::now();
    let sup = Arc::new(Supervisor::direct().await.expect("direct supervisor"))
        as Arc<dyn supervisor::SupervisorTrait>;
    eprintln!("supervisor built: {:?}", t0.elapsed());

    let state = daemon::http::AppState {
        supervisor: sup,
        token: Arc::new("test-token".into()),
        start_ts: Instant::now(),
        loaded_ls: Arc::new(std::sync::Mutex::new(vec![])),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_project: Arc::new(std::sync::Mutex::new(None)),
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        // 空路径 = 不写 envelope 日志（压测探活不需要 d3a 重放索引）。
        invocation_log_path: std::path::PathBuf::new(),
        drain_window: std::time::Duration::from_secs(2),
    };
    let app = daemon::http::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    eprintln!("HTTP server listening on {}", addr);

    let client = reqwest::Client::new();
    let url = format!("http://{}/tools/overview", addr);
    let body = serde_json::json!({
        "project_root": root_str,
        "args": {"file": "main.rs"},
        "lang": "rust"
    });

    // FIRST call — cold start (no LS cached).
    let t1 = Instant::now();
    let res1 = client
        .post(&url)
        .header("X-Serena-Token", "test-token")
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await;
    let t2 = Instant::now();
    eprintln!("FIRST overview POST took: {:?}", t2 - t1);
    match res1 {
        Ok(r) => {
            let st = r.status();
            let body = r.text().await.unwrap_or_default();
            eprintln!("  status={} body_len={}", st, body.len());
            if !st.is_success() {
                eprintln!("  body: {}", body);
            } else {
                let v: serde_json::Value =
                    serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
                if let Some(err) = v.get("error") {
                    eprintln!("  error: {}", err);
                }
                if let Some(data) = v.get("data") {
                    eprintln!(
                        "  data array len: {}",
                        data.as_array().map(|a| a.len()).unwrap_or(0)
                    );
                }
            }
        }
        Err(e) => eprintln!("  ERROR: {}", e),
    }

    // SECOND call — cached path (LS already alive).
    let t3 = Instant::now();
    let res2 = client
        .post(&url)
        .header("X-Serena-Token", "test-token")
        .json(&body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await;
    let t4 = Instant::now();
    eprintln!("SECOND overview POST took: {:?}", t4 - t3);
    match res2 {
        Ok(r) => eprintln!("  status={}", r.status()),
        Err(e) => eprintln!("  ERROR: {}", e),
    }

    server.abort();
    eprintln!("=== done ===");
}
