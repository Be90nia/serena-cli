// Throwaway microbench: compare to_value(deep Value) vs direct Serialize for ToolResponse.
// Run: rustc -O local/p2-0bq-bench.rs -o /tmp/p2-0bq-bench && /tmp/p2-0bq-bench
//
// Measures: per-call latency + transient memory for a realistic large search result.
// Payload: 200 hits × ~80 bytes ≈ 16 KB Value tree.

use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum ToolResponse {
    Ok {
        ok: bool,
        data: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    Err {
        ok: bool,
        error: serde_json::Value,
    },
}

fn build_large_response() -> serde_json::Value {
    let hits: Vec<serde_json::Value> = (0..200)
        .map(|i| {
            serde_json::json!({
                "file": format!("crates/foo/src/bar_{i}.rs"),
                "line": i,
                "col": 4,
                "text": "pub fn example_long_function_name_for_match() -> Result<Self, Error> {",
                "match_start": 7,
                "match_end": 38,
                "symbol": "example_long",
                "container": "impl_block"
            })
        })
        .collect();
    serde_json::Value::Array(hits)
}

fn main() {
    let data = build_large_response();
    let resp = ToolResponse::Ok { ok: true, data, format: None };

    // ---- Warmup ----
    for _ in 0..1000 {
        let _ = serde_json::to_value(&resp).unwrap();
        let mut buf = Vec::with_capacity(32 * 1024);
        let _ = serde_json::to_writer(&mut buf, &resp).unwrap();
    }

    // ---- Benchmark: to_value (old code path) ----
    let iters = 20_000;
    let t0 = Instant::now();
    for _ in 0..iters {
        let v = serde_json::to_value(&resp).unwrap();
        // Simulate axum's serialize_from_value: drops v after producing bytes
        let mut buf = Vec::with_capacity(32 * 1024);
        serde_json::to_writer(&mut buf, &v).unwrap();
        std::hint::black_box(&buf);
        std::hint::black_box(&v);
    }
    let dt_to_value = t0.elapsed();

    // ---- Benchmark: direct Serialize (new code path) ----
    let t1 = Instant::now();
    for _ in 0..iters {
        let mut buf = Vec::with_capacity(32 * 1024);
        serde_json::to_writer(&mut buf, &resp).unwrap();
        std::hint::black_box(&buf);
    }
    let dt_direct = t1.elapsed();

    println!("payload = {} bytes serialized", serde_json::to_vec(&resp).unwrap().len());
    println!("iters   = {iters}");
    println!("old (to_value + serialize): total={:?}, per-call={:?}", dt_to_value, dt_to_value / iters);
    println!("new (direct Serialize):     total={:?}, per-call={:?}", dt_direct, dt_direct / iters);
    let saved_ns = (dt_to_value.as_nanos() - dt_direct.as_nanos()) / iters as u128;
    println!("saved per call: ~{} ns ({:.1}%)",
        saved_ns,
        100.0 * (dt_to_value.as_nanos() as f64 - dt_direct.as_nanos() as f64) / dt_to_value.as_nanos() as f64,
    );
}