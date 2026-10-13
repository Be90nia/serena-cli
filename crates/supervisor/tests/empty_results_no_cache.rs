//! B-P2-4：空结果不入缓存专项回归锁（审计 §6.4 / 铁律「宁重查不可错缓存」）。
//!
//! 锁的行为：LS 语义就绪窗口内 overview 返回的空集**不**入 symbol_cache——
//! 若空集被缓存，命中路径先于 LS（overview_cache_hit_precedes_lang_resolution），
//! 就绪后的真符号会被同 key 空条目**永久遮蔽**。
//!
//! 判红设计：冷窗首查（毒化触发点）→ `execute_tool("warm")` 等就绪（warm 的
//! 就绪探针走裸 session documentSymbol，不经 symbol cache——独立 oracle）→
//! 再查同 key overview 必须含真符号。空集若曾入库，此处恒空 ⇒ 确定性 FAIL。
//!
//! 形态：真机 rust-analyzer e2e（独立 TEMP cargo 项目——workspace 外是语义层
//! 可达的硬前提）。SERENA_SKIP_LS_E2E=1 或无 rust-analyzer 时显式跳过（CI 无
//! 第三方 LS 门禁；真机/nightly 覆盖，同 warm/ct 惯例）。

use std::path::PathBuf;

use supervisor::{Supervisor, SupervisorTrait};

fn rust_analyzer_available() -> bool {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        return false; // CI: skip real-LS e2e (3rd-party LS version drift; covered locally/nightly)
    }
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|dir| {
                let exe = if cfg!(windows) {
                    "rust-analyzer.exe"
                } else {
                    "rust-analyzer"
                };
                dir.join(exe).is_file()
            })
        })
        .unwrap_or(false)
}

/// 独立 TEMP cargo 项目（进程 id 隔离；每次新目录 ⇒ RA 对它必冷启动，稳定
/// 复现「就绪窗口首查为空」的前置条件）。
fn scratch_project() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-bp24-empty-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir scratch src");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"bp24_empty_cache\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(dir.join("src/lib.rs"), "pub fn bp24_marker_fn() -> u32 { 7 }\n")
        .expect("write lib.rs");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_ready_window_result_does_not_shadow_post_ready_symbols() {
    if !rust_analyzer_available() {
        println!("skipped: SERENA_SKIP_LS_E2E=1 or rust-analyzer not on PATH");
        return;
    }
    let root = scratch_project();
    let root_str = root.to_string_lossy().to_string();
    let sup = Supervisor::direct().await.expect("supervisor");

    // 1) 冷窗首查：空集是合法语义结果——这正是「空不缓存」契约的作用点。
    //    （每轮新 scratch 目录 ⇒ RA 对它冷启动，此查大概率落在就绪窗口内。）
    let _first = sup
        .tool_overview(&root, "src/lib.rs", Some("rust"))
        .await
        .expect("cold overview");

    // 2) 就绪信号走 warm（裸 documentSymbol 探针，不经 symbol cache——不会被
    //    潜在的空缓存自遮蔽）。死线尽未就绪 = 环境窗口不可控，显式跳过不冒充
    //    失败（ct/tests.rs 同哲学）。
    let report = sup
        .execute_tool(
            "warm",
            &root_str,
            serde_json::json!({ "lang": "rust", "timeout_secs": 150 }),
            None,
        )
        .await
        .expect("warm");
    if report["ready"] != serde_json::json!(true) {
        println!("skipped: rust-analyzer not semantically ready in warm window: {report}");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    let probe = report["probe_file"].as_str().expect("probe_file").to_string();

    // 3) 同 key 复查：必须拿到真符号。空集若曾入缓存，命中路径先于 LS ⇒
    //    此处恒空 ⇒ FAIL（回归的确定性红）。
    let hits = sup
        .tool_overview(&root, &probe, Some("rust"))
        .await
        .expect("post-warm overview");
    assert!(
        hits.iter().any(|s| s.name == "bp24_marker_fn"),
        "warm ready 后同 key overview 必须含 marker 符号（空集若曾入缓存会被永久遮蔽）: {} hits",
        hits.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}
