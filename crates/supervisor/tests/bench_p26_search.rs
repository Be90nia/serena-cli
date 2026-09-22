//! 临时 bench：P2-6 search spawn_blocking 前后对照。
//! 200 文件 × 200 调用 × 3 轮。
//! 用法：`cargo test -p supervisor --test bench_p26_search -- --nocapture --ignored`

use std::time::Instant;

use supervisor::Supervisor;

fn setup_ws(root: &std::path::Path, count: usize) {
    let src = root.join("src");
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(&src).unwrap();
    for i in 1..=count {
        let name = format!("file_{i:03}.rs");
        let needle = format!("needle_{i:03}");
        let body = format!(
            "// file {i}\nfn func_{i:03}() -> i32 {{\n    let x = {i};\n    let s = \"{needle}\";\n    println!(\"{{}} {{}}\", x, s);\n    x\n}}\n\n#[cfg(test)]\nmod test_{i:03} {{\n    #[test]\n    fn it_works() {{ assert_eq!(super::func_{i:03}(), {i}); }}\n}}\n"
        );
        std::fs::write(src.join(&name), body).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf benchmark, run on demand"]
async fn bench_search_200_files_x50_calls() {
    let count = 200;
    let calls = 50;
    let root = std::env::temp_dir().join("serena-perf-ws");
    setup_ws(&root, count);

    let sup = Supervisor::direct().await.expect("supervisor init");

    // warm up
    for i in 1..=3 {
        let _ = sup
            .tool_search_for_pattern(&root, &format!("needle_{i:03}"), None, 50, true)
            .await
            .unwrap();
    }

    let t0 = Instant::now();
    for i in 1..=calls {
        let _ = sup
            .tool_search_for_pattern(&root, &format!("needle_{i:03}"), None, 50, true)
            .await
            .unwrap();
    }
    let elapsed = t0.elapsed();
    println!(
        "\n[P2-6 bench] {count} files × {calls} calls = {:.2} ms total, {:.3} ms/call avg",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / calls as f64
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "perf benchmark, run on demand"]
async fn bench_search_worker_release_during_long_scan() {
    // 验证：search 跑期间同 supervisor 上 /status 类同步路径不受阻。
    // 简化版：在一次大 search 的同时，连续触发小 search 测并发响应。
    let count = 800;
    let root = std::env::temp_dir().join("serena-perf-ws-big");
    setup_ws(&root, count);

    let sup = std::sync::Arc::new(Supervisor::direct().await.expect("supervisor init"));

    // warm
    let _ = sup
        .tool_search_for_pattern(&root, "needle_001", None, 50, true)
        .await
        .unwrap();

    let big = format!("needle_{:03}", count); // last file = max walk cost

    // 先单独跑一次大 search 测基线耗时
    let t_big = Instant::now();
    let _ = sup
        .tool_search_for_pattern(&root, &big, None, 50, true)
        .await
        .unwrap();
    let big_alone_ms = t_big.elapsed().as_secs_f64() * 1000.0;
    println!("[P2-6 worker] big search alone: {big_alone_ms:.2} ms");

    // 跑大 search 的同时，用独立 task 跑 10 次小 search —— 总耗时 ≤ big_alone + 串行 10 小 call
    // 若 async worker 在 search 期间完全阻塞，小 search 串行排队，总耗时 >> big_alone。
    let sup2 = sup.clone();
    let t_concurrent = Instant::now();
    let big_handle = tokio::spawn(async move {
        sup2.tool_search_for_pattern(&root, &big, None, 50, true).await
    });
    let small_durations: Vec<f64> = {
        let mut durs = Vec::new();
        for i in 1..=10 {
            let t = Instant::now();
            let _ = sup
                .tool_search_for_pattern(
                    &std::env::temp_dir().join("serena-perf-ws"),
                    &format!("needle_{i:03}"),
                    None,
                    50,
                    true,
                )
                .await
                .unwrap();
            durs.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        durs
    };
    big_handle.await.unwrap().unwrap();
    let concurrent_total_ms = t_concurrent.elapsed().as_secs_f64() * 1000.0;
    let small_avg_ms = small_durations.iter().sum::<f64>() / small_durations.len() as f64;
    println!("[P2-6 worker] concurrent total: {concurrent_total_ms:.2} ms (big + 10 small in parallel)");
    println!(
        "[P2-6 worker] small-search avg during big: {small_avg_ms:.3} ms (should be << big_alone)"
    );
    println!(
        "[P2-6 worker] ratio concurrent/big_alone = {:.3}",
        concurrent_total_ms / big_alone_ms
    );
}
