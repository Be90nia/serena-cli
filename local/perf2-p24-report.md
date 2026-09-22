# P2-4 Performance Report (PERF-P24)

**VERDICT**: 落地完成，三项性能目标全部满足 + clippy/test 干净。

## 改动文件清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `crates/supervisor/src/lib.rs` | +90 / -36（净增 54 行） | 见下方分段 |
| `crates/supervisor/tests/perf_p2_4.rs` | +200 / 0（新增） | P0 (1)(2)(3) 量化验收测试 |

### lib.rs 改动分段

1. **lib.rs:217-240** 新增 `walk_root_signal(root)` —— 单次 `ignore::WalkBuilder` depth-3 walk 同时收集 `max mtime` 与 `BTreeSet<lang>`。原实现两遍 walk（cache key 一次 + lang 探测一次）合一。
2. **lib.rs:242-247** `root_source_mtime` 改为 `walk_root_signal` 的 thin wrapper，仅为 `root_source_mtime_change_invalidates_find_symbol_cache` 测试保留同步无缓存入口（保证 mtime 变化立刻可见，绕开 TTL）。
3. **lib.rs:249-261** 新增模块级 `ROOT_SIGNAL_CACHE: LazyLock<Mutex<HashMap<PathBuf, RootSignalEntry>>>` + `ROOT_SIGNAL_TTL = 2s` + `RootSignalEntry = (采集时刻, mtime, langs)` 类型别名。
4. **lib.rs:266-294** 新增 `async fn root_signal_cached(root)` —— fast-path（缓存新鲜 → clone 走，亚微秒临界区）/ slow-path（`spawn_blocking(walk_root_signal)` + 二次检查防并发回填）。完全 async-safe（无跨 await 持锁）。
5. **lib.rs:2097** `tool_find_symbol` 改用 `let (root_mtime, walked_langs) = root_signal_cached(root).await;`，lang 集合直接复用 walked_langs，**miss 路径不再二次 walk**。

### lib.rs 不动的边界（按 task 要求）

- `find_symbol_cache_key` / `doc_symbol_cache_key` 签名 + 行为不变（信号位仍接 Option<SystemTime>）。
- `walk_root_signal` 仍同步；`spawn_blocking` 包在 `root_signal_cached` 异步层。
- `root_source_mtime` 函数保留（仅测试用），`#[cfg_attr(not(test), allow(dead_code))]` 抑制生产 dead-code 警告。

## P0 验收

| # | 指标 | 命令 | 结果 |
|---|---|---|---|
| 1 | find-symbol 100 次连续调用 BEFORE → AFTER | `cargo test -p supervisor --test perf_p2_4 p2_4_100_call_per_call_under_1ms_with_ttl -- --ignored --nocapture` | **BEFORE 38.3 ms/call → AFTER 0.213 ms/call（180× 提速）** |
| 2 | mtime 信号 TTL：2s 内信号值不变 | `cargo test -p supervisor --test perf_p2_4 p2_4_ttl_returns_same_value_within_window -- --ignored --nocapture` | **5 次连续调用 mtime/langs 完全相同** |
| 3 | miss 路径不再二次 walk | `cargo test -p supervisor --test perf_p2_4 p2_4_miss_path_no_second_walk -- --ignored --nocapture` | **100 calls within TTL = exactly 1 walk** |
| 4 | supervisor --lib 0 FAILED | `cargo test -p supervisor --lib` | **143 passed; 0 failed** |
| 5 | clippy 0 errors | `cargo clippy -p supervisor --no-deps --all-targets -- -D warnings` | **clean** |

## 完整 benchmark 输出

```
workspace: C:\Users\Begonia\AppData\Local\Temp\perf_root
TTL signal stable within window: mtime=Some(SystemTime { intervals: 134345267308325179 }) langs={"rust"}
test p2_4_ttl_returns_same_value_within_window ... ok
P0 (3): miss path no second walk — 100 calls = 1 walk(s)
test p2_4_miss_path_no_second_walk ... ok
BEFORE (2 walks/call): total 3.8304322s per-call 38.304322ms
AFTER  (TTL cache):    total 21.322ms per-call 213.22µs
speedup: 179.6x
test p2_4_100_call_per_call_under_1ms_with_ttl ... ok
```

## 关键设计决策

- **TTL=2s**：与诊断等待同量级容忍；外部修改感知延迟 ≤2s。
- **全局静态 `LazyLock<Mutex<HashMap>>`**：单 root 串行 find-symbol 场景零争用；多 root 并行场景受全局锁约束——ponytail 注释已登记升级路径（path-hash 分片 Mutex）。
- **`#[cfg_attr(not(test), allow(dead_code))]`**：`root_source_mtime` 生产路径无调用方（运行时一律走 `root_signal_cached`），仅测试同步直接调用以验证"绕开 TTL 后 mtime 变化立刻可见"。
- **二次检查（double-check）**：slow-path spawn_blocking 完成后回填前再 check 一次，避免并发请求同一 root 时回填覆盖（实测：100 并发 → 1 walk）。
- **二次 walk 彻底消除**：`walk_root_signal` 单次同时返回 mtime 和 langs，`tool_find_symbol` 直接复用 `walked_langs`，原 miss 路径的 `WalkBuilder` for-loop 整个删除。

## 已知限制（按 task "附注" 标注，不强改）

- `max_depth(3)` 截断对深嵌套文件盲（`crates/*/src/*.rs` 在深度 4，**不参与** max mtime）。深层文件外部修改不会推进信号。E/J 的缓存失效设计若依赖"任一源码文件修改即失效"需知此边界；当前实现"删除不推进"已存在的注释保留。
- TTL=2s 内同一 root 的 mtime 重复 walk 会被完全跳过——若 AI 编辑循环期望"我刚改了文件，下一次 find-symbol 必须看到"，最长感知延迟 = 2s。设计上可接受（诊断等待同量级）。

## 未做（明确不属本轮）

- 把 `max_depth(3)` 改为无限制（性能/IO 成本不可控，留作后续）。
- 加 path-hash 分片 Mutex 替代全局锁（万级 root 时再升级）。
- 把 root_source_mtime 完全删掉（保留测试同步入口验证"绕开 TTL 即时可见"的不变量）。

## 不 commit

按 Main 约束，本轮不 commit，由 Main 最终 `cargo test --workspace` 验证后提交。
