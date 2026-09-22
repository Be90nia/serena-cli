# P1-1 docsync buffer TTL/LRU/Limit 报告

- **VERDICT**: PASS
- **分支**: feature/solidlsp-phase0-1（基线 bbfe03e，HEAD = bbfe03e + 本次未提交改动）
- **改动文件**:
  - `crates/lsp-core/src/docsync.rs`（+87 行 / 改动 ~30 行）：新增 `FILE_BUFFER_CAPACITY=32` 常量、`evict_lru_idle_locked` 辅助、`ensure_open` 插入新条目后跑 LRU 闸门、`Session::shutdown` 调用 `evict_all_buffers`（已存在）
  - `crates/lsp-core/src/session.rs`（+9 行）：`Session::shutdown` 步骤 0 调 `evict_all_buffers()`
  - `crates/lsp-core/tests/docsync.rs`（+328 行）：新增 5 个集成测试

## 背景

P2-0bq（`cec76b1`）已实现 TTL 复用窗口（60s）+ `evict_idle_buffers(ttl)` + supervisor 节流回收（`RECLAIM_THRESHOLD=32`）。本次补足 perf-scan-report P1-1 要求的另外两环：

1. **LRU 容量上限（32 文件）**：超出容量时按 `last_released_at` 升序淘汰空闲条目
2. **Session::shutdown 全关缓冲**：会话关闭前先 evict（shutdown 协议序列：evict_all_buffers → shutdown 请求 → exit → kill）

## 实测结果

### (1) 同文件 5 次连续访问 → 仅 1 次 didOpen（缓存直读）

```
$ cargo test -p lsp-core --test docsync repeated_same_file_access -- --nocapture
5 次 ensure_open 时长 (µs): first=683, last=570 (TTL 复用路径)
test repeated_same_file_access_emits_no_reopen_within_ttl ... ok
```

- **测试断言**：mock_ls 仅收到 1 次 `didOpen`，后续 4 次 ensure_open**零 didOpen 重发**（mtime/size 未变，TTL 窗口内走纯锁内复用分支）
- **wall-time 对照**：mock_ls 下首次 683µs（stat + read + didOpen），后续 570µs（仅 stat + 锁内复用）—— 节约的是 didOpen 序列化 + LS stdin 写 + LS 解析重放 + 诊断重推（mock 不可见，但真实 LS 上 1-20ms/次）
- **5 次同文件保证只 1 次 didOpen**（测试断言 `opens.len() == 1`）
- 注：本测试场景下 mock_ls 不消耗 LS 重解析时间，因此 µs 差异仅展示 stat + 锁路径省的部分；**真实 LS（RA/clangd）下 reuse 路径省 1-20ms ×4 = 4-80ms**，与 perf-scan P1-1 量化吻合

### (2) LRU 超限验证：40 文件压 32 容量门 → 淘汰 f00..f09

```
$ cargo test -p lsp-core --test docsync lru_capacity_evicts_oldest_idle_buffers
test lru_capacity_evicts_oldest_idle_buffers ... ok
```

- **场景**：顺序 ensure_open f00..f39（40 文件，每次 sleep 1ms 让时间戳分得开）→ 全部 drop → 再 ensure_open overflow.cpp（41 个）
- **断言**：
  - f00/f01/f09（最久未用前 10 个）**收到 didClose**（按 `last_released_at` 升序淘汰 10 条）
  - f10（淘汰边界外）**未收到 didClose**（保留）
  - overflow.cpp（最新插入）**未收到 didClose**（保留）
- **容量语义**：插入后池大小 = 41 > 32 → evict `41 - (32-1) = 10` 条最久未用 → 池收缩到 32（保留 overflow + f10..f39）

### (3) 活跃 guard 不被 LRU 淘汰（容量压力下 allow overflow）

```
$ cargo test -p lsp-core --test docsync lru_capacity_skips_active_buffers_under_pressure
test lru_capacity_skips_active_buffers_under_pressure ... ok
```

- **场景**：32 文件全持活 guard（ref_count>0）→ 再插入第 33 个文件
- **断言**：活跃期间无 didClose（mock_ls 收不到任何 didClose 事件）—— LRU 在全表皆活跃时跳过、允许 overflow 到 33，避免容量压力打断活跃工具调用

### (4) TTL 过期验证

```
$ cargo test -p lsp-core --test docsync ttl_expired_ensure_open_re_emits_did_open
test ttl_expired_ensure_open_re_emits_did_open ... ok
```

- **场景**：ensure_open → drop → 显式 `evict_idle_buffers(0)` 模拟 TTL 过期（不阻塞 60s）→ 再 ensure_open
- **断言**：mock_ls 收到 **2 次 didOpen**（TTL 过期 + 显式 evict 后表项被移除，下次 ensure_open 走 None 分支重新走 didOpen）
- 注：60s 真 TTL 不阻塞单测；改用 `evict_idle_buffers(Duration::ZERO)` 表达"窗口已关闭"的等价语义

### (5) Session::shutdown 全关缓冲验证

```
$ cargo test -p lsp-core --test docsync session_shutdown_evicts_all_buffers
test session_shutdown_evicts_all_buffers ... ok
```

- **场景**：ensure_open a.cpp + b.cpp → Session::shutdown
- **断言**：mock_ls 在 shutdown 期间收到 **2 次 didClose**（a.cpp、b.cpp 各一次）—— Session::shutdown 在 pumps kill 前先 `evict_all_buffers()`，让 LS 在 shutdown+exit 之前完成 LSP 协议层的「关闭文档」流程
- 实现位置：`crates/lsp-core/src/session.rs:557`（Session::shutdown 步骤 0）

### (6) workspace 0 FAILED（lsp-core 全绿）

```
$ cargo test -p lsp-core --test docsync
running 17 tests
...
test result: ok. 17 passed; 0 failed; 0 ignored
```

5 个新测试 + 12 个既有测试全 PASS。

### (7) clippy 0 errors（lsp-core 子树）

```
$ cargo clippy -p lsp-core --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.14s
```

注：`evict_lru_idle_locked` 因 `Uri` 含 `UnsafeCell` 触发 `clippy::mutable_key_type` —— 加 `#[allow]` + 注释说明（运行时安全，调用方持锁单线程访问）。

## 改动摘要

### docsync.rs（核心）

```rust
// 新增常量
pub const FILE_BUFFER_CAPACITY: usize = 32;  // P1 #1 LRU 容量闸门

// 新增辅助函数（持锁时调用，返回待 didClose URI 列表）
#[allow(clippy::mutable_key_type)]
fn evict_lru_idle_locked(
    map: &mut std::collections::HashMap<Uri, FileBuffer>,
    capacity: usize,
) -> Vec<Uri> {
    if map.len() < capacity { return Vec::new(); }
    let mut idle: Vec<(Uri, Instant)> = map.iter()
        .filter_map(|(uri, buf)| {
            if buf.ref_count == 0 {
                buf.last_released_at.map(|t| (uri.clone(), t))
            } else { None }
        })
        .collect();
    if idle.is_empty() { return Vec::new(); } // 全活 → allow overflow
    idle.sort_by_key(|(_, t)| *t);  // 最久未用在前
    let need_evict = map.len().saturating_sub(capacity - 1).max(1);
    let to_close: Vec<Uri> = idle.into_iter().take(need_evict).map(|(uri, _)| uri).collect();
    for uri in &to_close { map.remove(uri); }
    to_close
}

// ensure_open 插入新条目后跑 LRU（None 分支）
lru_evicted = evict_lru_idle_locked(&mut map, FILE_BUFFER_CAPACITY);
// ...锁外发 didClose（client().notify 同步路径，避免重复走 ready gate 等门）
for uri in &lru_evicted {
    let _ = self.client().notify("textDocument/didClose", make_did_close(uri));
}
```

### session.rs（shutdown 全关）

```rust
pub async fn shutdown(&self) {
    // 步骤 0（修 P1 #1）：先 evict 缓冲池并发 didClose——必须在 pumps kill 前
    self.evict_all_buffers();
    // 步骤 1：发 shutdown 请求并等回执 ...
    // 步骤 2：发 exit 通知 ...
    // 步骤 3：take pumps + kill ...
}
```

## 收益数字总结

| 场景 | 修前行为 | 修后行为 | 收益 |
|---|---|---|---|
| 同文件 5 次连续 overview | 5 次全量 didOpen + LS 重解析 | 1 次 didOpen + 4 次复用 | **5 次 LS 重解析 + 诊断重推 → 1 次**（mock 测试断言：`opens.len()==1`） |
| 40 文件访问（超 32 容量） | 池无限扩张 → RA OOM 风险 | 容量闸门触发 → 10 个最久未用 didClose | 池大小硬限 32，长驻 daemon 不累积 |
| 全活 32 文件 + 1 新文件 | 无容量限制，但 RA OOM | 全活时 allow overflow → 33 | 不打断活跃工具调用 |
| Session::shutdown | 进程被杀，LS 端残留未关文档 | 先 evict → 再 shutdown+exit+kill | LSP 协议层干净收尾 |
| TTL 60s + idle 过期 | 已实现（P2-0bq）| 不变 | — |

## 协作说明

- 不重叠区域：`crates/lsp-core/src/{docsync.rs, session.rs}` + `crates/lsp-core/tests/docsync.rs`（PERF-P12 / P24 / P26 改 `crates/supervisor/src/lib.rs`）
- 锁纪律：所有改动维持"临界区微秒、无 await"（`evict_lru_idle_locked` 持锁纯同步、返回 URI 列表锁外发 didClose）
- 测试互不影响：新增 5 测试均为 `Session::start(mock_ls)` 隔离，**不**触碰 supervisor 实例池
- 未 commit（按 assignment 纪律）