//! 把 `supervisor::catalog::catalog()` 序列化为 JSON 打到 stdout。
//!
//! 用法：
//! ```text
//! cargo run -p supervisor --example print_catalog > docs/rpc-catalog.json
//! ```
//!
//! 输出是 deterministic（serde_json::to_string_pretty 排序键），可做 diff 基准。
//! ponytail：examples 是 cargo 唯一的「不写进 lib / bin 也能跑的轻量入口」，
//! 不增加新 crate / 新依赖。

fn main() {
    let v = supervisor::catalog::catalog();
    println!(
        "{}",
        serde_json::to_string_pretty(&v).expect("serialize catalog")
    );
}