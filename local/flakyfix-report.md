# flakyfix-report
- 改动：crates/supervisor/src/edit_context.rs busy-retry deadline 5s→20s（L276），同步 ponytail 注释；循环本就是 deadline-based，无需改结构。
- 原因：满载机上 rust-analyzer 冷启动索引 10-30s，5s 上限导致 flake。
- 验证：`cargo test -p supervisor --lib edit_context` 连跑 3 次 = 3× "5 passed; 0 failed"；`cargo clippy --workspace --all-targets -- -D warnings` Finished, 0 errors。
