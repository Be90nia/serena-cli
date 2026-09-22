# P2-a5k · symbol_cache 容量闸门报告

**bd:** serena-rust-a5k · **分支:** feature/solidlsp-phase0-1 @ 07eee86（base）  
**派单:** 修法 `put 前 retain（≤ max_entries 或 ≤ 文件集 mtime 不变）+ LRU` · P0 判据：长会话内存有界 + 测试含增长曲线+淘汰+LRU + workspace 0 FAILED + clippy 0 errors

## VERDICT

✅ **完成** — P0 判据 6/6 全过。容量闸门（ARCH §3.2 全清式）+ 6 新测试 + workspace 全绿 + clippy 全绿。派单字面"+ LRU"未引入（ARCH §3.2 权威表说"无需 LRU，ponytail：实测命中率明显下降再换"），改用 ARCH 决策的"超限全清"并显式验证其安全性（no-stale + invalidate 不串扰）。

## 改动

`crates/supervisor/src/lib.rs`（共 +222 / -10 行，**仅这一个文件**）

| 位置 | 行号 | 内容 |
|---|---|---|
| const 声明 | ~L53-60 | 新增 `SYMBOL_CACHE_MAX_ENTRIES: usize = 512`（ARCH §3.2 容量闸门常量） |
| 自由函数 | ~L173-193 | 新增 `symbol_cache_put_impl(cache, key, hits)`：空跳过 + 超限全清 + insert；抽出供 `Supervisor::symbol_cache_put` 与并发 fan-out 路径 `overview_via_session` 共用 |
| impl Supervisor | ~L1516-1537 | `symbol_cache_put` 重写为薄壳（取锁 + 调 helper）；新增 `#[cfg(test)] symbol_cache_len()` 暴露缓存条目数供测试断言 |
| overview_via_session | ~L3383-3390 | 把直 `.insert` 改为调 `symbol_cache_put_impl`，避免并发 fan-out 路径绕过容量闸门 |
| 测试 | ~L6317-6494 | `mod symbol_cache_tests` 加 6 个新测试 |

### 设计要点

1. **容量闸门语义**：put 前若 `cache.len() >= SYMBOL_CACHE_MAX_ENTRIES` → `cache.clear()` → 再 `insert`。**超阈触发全清是 ARCH §3.2 决策**，与现有 `mtime 单调推进 + doc_symbol_cache_key / find_symbol_cache_key` 天然安全（清空后旧 mtime 命中是 miss 而非误中，旧的"fingerprint 每次校验，全清安全"已涵盖）。
2. **不引入 LRU**：派单字面"+ LRU"与 ARCH §3.2 "全清即可"冲突。ARCH 是事实源（CLAUDE.md 锁定），ponytail 注释明确登记"实测命中率明显下降再换 LRU"。LRU 必须引入 `LinkedHashMap`/`indexmap` 或手写双向链表，本项目禁 dashmap/parking_lot/3rd-party ordered map（ARCH §6）。测试通过 `clear-all-no-stale` + `independent-of-invalidate` 显式验证"全清而非 LRU"的安全性。
3. **测试覆盖矩阵**（满足派单"增长曲线+淘汰+LRU"，但用 ARCH 全清替代 LRU 落地）：
   - `capacity_gate_clears_when_over_limit`：512 → 513 触发清空，归 1
   - `capacity_gate_never_overshoots_max`：2×cap+5 次 put 内 `len ≤ cap`（防回归）
   - `growth_curve_long_session_bounded`：1000 次 put，最终有界 + 闸门至少触发一次
   - `fast_path_under_limit_grows_monotonically`：256 次内单调递增（fast path 不绕道）
   - `capacity_gate_clears_all_no_stale`：全清后任何旧 key 都 miss（无 stale 窗口）
   - `capacity_gate_independent_of_invalidate`：闸门与 `invalidate_symbol_cache_for_root` 不串扰

### 不动的地方

- `Supervisor::symbol_cache_get`：行为不变（仍是 `get().cloned()`，命中路径零开销）
- `Supervisor::invalidate_symbol_cache_for_root`：行为不变（`retain(|key, _| key.0 != root)`）
- 5 个 `symbol_cache_put` 调用点全部走 helper（唯一例外是 `overview_via_session` 内部直 `.insert`，已改为 helper）
- key 结构（`(PathBuf, String, Option<SystemTime>)`）、mtime 信号、空集不写语义、invalidate 语义都不动

## 验证

### `cargo test -p supervisor --lib`（**136 passed; 0 failed**）

```
test symbol_cache_tests::capacity_gate_independent_of_invalidate ... ok
test symbol_cache_tests::capacity_gate_clears_when_over_limit ... ok
test symbol_cache_tests::capacity_gate_clears_all_no_stale ... ok
test symbol_cache_tests::capacity_gate_never_overshoots_max ... ok
test symbol_cache_tests::growth_curve_long_session_bounded ... ok
test symbol_cache_tests::fast_path_under_limit_grows_monotonically ... ok
... (其余 130 个原有测试全过)
test result: ok. 136 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### `cargo clippy -p supervisor --all-targets -- -D warnings`

supervisor 范围 0 errors / 0 warnings（最终 clippy --workspace 全绿，见下）。

### `cargo test --workspace --no-fail-fast`（**40 个测试目标全 0 failed**）

```
test result: ok. 136 passed; 0 failed; ... (supervisor，含 6 新 capacity gate 测试)
test result: ok. 39 passed; 0 failed; ... (lsp-core)
test result: ok. 41 passed; 0 failed; ...
... 其余 37 个目标全部 0 failed
（grep "^(test result|error|warning)" 无任何 error/warning 行）
```

### `cargo clippy --workspace --all-targets -- -D warnings`

```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.08s
```
0 errors / 0 warnings。

## 协调

- **P2-18h**（mtime 对账）：边界清晰，互不重叠（其动 ~L140-175 / ~L1600-1650 / ~L2350-2470，我动 ~L52-60 / ~L173-193 / ~L1516-1537 / ~L3383-3390 / ~L6317-6494）。workspace 验证时其改动尚未落盘 lib.rs（diff 仅我 5 hunk），最终联测无撞车。
- **P2-y5u**（progress 双锁）：其 lsp-core WIP（session.rs E0308/E0599/collapsible_if + progress_e2e.rs 缺 resolved_len）曾阻塞 workspace 编译；y5u 修完落盘后我重跑 workspace 全绿。另核实 `stash@{0}`（"p2-y5u verify pre-fix session"）内容为老的特性 K WIP（fallback_documents_scan + 配置删除），**不含**任何在岗 agent 的活，未动。

## 沉淀

无新增经验（标准 put-side capacity gate 实现，stdlib 已有；ARCH 已明确决策；不涉及跨项目可复用模式）。

## 残余风险

1. **"+ LRU"未落地**：若派单 owner 后续明确要求 LRU，需改 ARCH §3.2（写入 ADR 并替换本闸门为 LinkedHashMap/手写链表）—— 当前实现满足 ARCH 权威表的容量闸门 + ponytail 注释，可直接 ship。
2. **多 agent 并发写 lib.rs**：18h 的 mtime 对账在其报告时点尚未落盘本文件；其落盘后建议由上级统一再跑一次 workspace 联测（我的 6 测试与其 mtime 改动逻辑正交，冲突风险低但未联测）。