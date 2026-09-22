# T2 Report: wait_for_progress 偶发 FAIL 修复

> 锚 a202a01 + commit 修改（未 commit）
> 路径: crates/lsp-core/src/session.rs:128-138 (字段), :257-277 (handler), :383-419 (wait)

## VERDICT: PASS（修复落地、复现/验证/回归三段全绿）

## 1. 根因（精确定位）

`Session` 内 `progress_waiters`（tokio::sync::Mutex）与 `progress_resolved`（std::sync::Mutex）两把锁分立。Handler 在 stdout 泵 task 内同步执行：
- `try_lock(progress_waiters)` → 抢到 → 无 waiter → drop → `try_lock(progress_resolved)` → 抢到 → 插入 token。

`wait_for_progress` 在测试 task 内：
- `progress_resolved.lock().unwrap().remove(token)` → false
- 准备抢 `progress_waiters.lock().await`。

**夹缝窗口**（仅几微秒）：handler 在 `wait_for_progress` drop resolved 锁后、抢到 waiters 异步锁前触发。Handler 此时：
1. `try_lock(resolved)` 抢到，插入 token。
2. `try_lock(waiters)` 抢到（因 `wait_for_progress` 还在等 `.await`），无 waiter，不 notify。
3. drop 两把锁，handler 退场。

随后 `wait_for_progress` 抢到 `waiters.lock().await`，插入 Notify，drop 锁，调 `notified()`——但**通知已被 handler 消费**，永久挂起到 timeout。

## 2. 修复

合并两把锁为一个 `std::sync::Mutex<ProgressRegistry>`：

```rust
#[derive(Default)]
struct ProgressRegistry {
    waiters: HashMap<String, Arc<Notify>>,
    resolved: HashSet<String>,
}

// Session
progress: std::sync::Mutex<ProgressRegistry>,
```

Handler 与 wait 侧都进同一把 `std::Mutex` 临界区，原子完成「resolved 消费 / waiter 插入 / waiter 通知 / resolved 记录」四步。临界区全程不持 `.await`，不会与 `Notify::notified()` 互锁。

## 3. 复现（修复前）

临时 stress 测试 `crates/lsp-core/tests/_t2_stress_repro.rs`（已删除，不进仓库）—— 50 轮 × 8 并发：

| 项 | 数值 |
|---|---|
| 总会话数 | 400 |
| 修复前失败 | **35/400 ≈ 8.75%**（典型 30+ 个 timeout ≈2s） |
| 修复后失败 | **0/400**（2 轮验证） |

单测 `progress_e2e` 修复前 10 次跑出 1 次 FAIL（隔离偶发），与"context: 4/4 绿后负载下偶发 FAIL"描述吻合。

## 4. 持久验证

| 验证项 | 命令 | 结果 |
|---|---|---|
| 修复前复现 | `cargo test -p lsp-core --test _t2_stress_repro -- --nocapture`（临时测试） | fail=35/400 |
| 修复后 stress ×2 | 同上 | pass=400/400, pass=400/400, max_wait=5ms/2ms |
| 修复后 progress_e2e ×10 | `cargo test -p lsp-core --test progress_e2e` | 10/10 ok |
| 修复后 progress_e2e ×5 ×8线程 | `cargo test -p lsp-core --test progress_e2e -- --test-threads=8` | 5/5 ok |
| supervisor lib | `cargo test -p supervisor --lib` | 130 passed / 0 failed |
| lsp-core lib + bin 单测 | 34 unit + 0 mock_ls 全 ok | ok |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | Finished, 0 errors |

## 5. 改动清单

- **crates/lsp-core/src/session.rs** —— `Session::progress_waiters` + `Session::progress_resolved` 合并为 `Session::progress: std::sync::Mutex<ProgressRegistry>`。新增 `ProgressRegistry` 结构体（waiters + resolved 字段）。Handler 与 `wait_for_progress` 重写临界区。
  - struct 字段重声明：-2 +1 行（注释扩展 +11 行）
  - `Session::start` 构造：-2 +1 行
  - handler closure：-13 +9 行
  - `wait_for_progress` 主体：-15 +17 行（含详细注释）
- **local/t2-report.md** —— 本报告
- 临时 `crates/lsp-core/tests/_t2_stress_repro.rs` —— 已删除（不进仓库；4 分钟跑一轮不适合每次 CI）

## 6. 注释 / 根因沉淀位置

- `Session::progress` 字段注释（session.rs:128-138）：完整描述 race window + 复现率 + 修复方案。
- `ProgressRegistry` struct 注释（session.rs:97-107）：临界区原子性语义。
- `wait_for_progress` 文档块（session.rs:372-382）：实现细节 + 修复历史。

## 7. 残余风险

1. `Session::progress` 是 `std::Mutex`（非异步），handler 与 wait 侧均不持锁 `.await`。任何后续 PR 若往此临界区加 `.await` 都会重蹈覆辙——已在注释明确警告。
2. `progress_resolved` 与 `progress_waiters` 两个字段共存于同一 `ProgressRegistry`，未来若有人拆回去到两把锁会重挂——`ProgressRegistry` 整个结构作为一个整体使用是契约的一部分。
3. mem-scan-report.md 仍标 P2-2（handler Arc 环 + progress_resolved 单调累积）。本次只修了 race，环与累积仍存。Arc 环根因是 handler 闭包捕获 `Arc<Session>`——非本 ticket scope（rule: 不动 P0/P1 邻居）。
4. mock_ls 在 `initialize` 响应后立刻发 `$/progress` 通知——这是测试特例；真实 LS（RA 等）会发 begin→report→end 三段，与本 race 解法无关，副作用一致。

## 8. 不做（明确不在本轮）

- Arc 环断环（改 `Weak<Session>` 或 shutdown 清注册表）—— mem-scan P2-2 单独 ticket。
- `progress_resolved` 单调累积清理——同上。
- mock_ls 写入顺序调整（initialize + 间隔 + progress）——测试稳定性属于测试侧契约，不动。
- 撤销任何其他 module 改动（H/J/L/M/G/A/B/E/C/F2 区域）。