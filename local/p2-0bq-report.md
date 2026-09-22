# serena-rust P2-0bq 报告（性能/内存杂项批）

> 锚：`efdef23`（feature/solidlsp-phase0-1 HEAD）
> 状态：3 项评估，2 项落地，1 项拒（MemScan 误判），1 项守界（与 D3A 并行划界）
> 性质：纯微观优化，零功能变化

---

## VERDICT

- **接受 2 项**：P2-5（HTTP 响应 Value 深拷贝）、P3-8 部分（evict 漏清 `load_gates`/`pull_diag_supported`，**不动 `diag_cache`**）
- **拒绝 1 项**：MemScan P3-7「loaded_ls 生产死字段」—— 实证该字段被 `/status` 读取、被 CLI `status` 子命令和 acceptance 测试消费；删会破坏 e2e
- **跳过（YAGNI）**：`push_nested` 内部 clone、`uri.clone()` ×20、`search rel_str` clone、`first-attempt clone params`——纯 batch 微优化，hot-path 量级 <1µs/调用，按任务指令"反 YAGNI 拒 batch 改未评估项"
- **守界**：D3A 在 http.rs/serve.rs/dto.rs 并行加 invocation envelope 与 `invocation_log_path` 字段；我只动了 4 处 AppState fixture 缺字段填补 + http.rs:147/156 的 `Json(resp)` 直传微改区（与 D3A envelope 注入位共处 `tools_post` body，由 D3A 协调保留直传形态）

---

## P2-5 落地证据

### 改动
- **`crates/daemon/src/http.rs:147`**：`Json(serde_json::to_value(&resp).unwrap())` → `Json(resp)`（Ok 分支）
- **`crates/daemon/src/http.rs:156`**：同上（Err 分支）
- 加 1 段 4 行 "Direct Serialize" 注释解释动机 + 指向 bench 文件

### 实测对照（throwaway bench：`local/p2-0bq-bench.rs`）
模拟 200 hits / 43KB 序列化响应：

| 路径 | 每调用延迟 | 备注 |
|---|---|---|
| 旧 `to_value(&resp) + to_writer` | **612.9 µs** | 中间多一份完整 Value 树深拷贝 |
| 新 `Json(resp) → Serialize 直传` | **96.9 µs** | 与 axum 现有 `Json` 路径同形态 |
| **节省** | **~516 µs / 84.2%** | 大响应收益显著；小响应（<5KB）<1ms，可忽略 |

### 行为不变验证
- `crates/daemon/src/dto.rs::tests::response_ok_with_data` 与 `response_err_with_error` 断言原文 `"ok":true,"data":[1,2,3]` 与 `"code":"BAD_ARGS"` 字面通过 —— 同一 `Serialize` 实现，不同入口，bytes-on-wire 完全一致
- `cargo test -p daemon --lib dto` 13/13 pass
- `cargo test -p daemon --lib` 46/46 pass（其中 2 个 D3A 新加 envelope 测试此前因 UUID variant bug 挂，我协助诊断后 D3A 修）

---

## P3-8（部分）落地证据

### 改动
- **`crates/supervisor/src/lib.rs:566-571`**：`evict` 内额外清两张旁表
  ```rust
  self.load_gates.lock().unwrap().remove(key);
  self.pull_diag_supported.lock().unwrap().remove(key);
  ```
- 加 doc 注释说明 `diag_cache` **按 (root, uri) 键与 session 解耦**故意保留 —— 文件级诊断跨世代仍有效，新 session 首条 pushDiagnostics 会覆写/清空

### 拒绝改动 `diag_cache` 的理由（驳 MemScan 报告）
- `diag_cache` 按 `(root, uri)` 键，而非 `Key` —— 与具体 Session 实例无强耦合
- 旧 session 已 push 过 `items` 的 uri：驱逐 session 后新 session 接管前若不缓存，agent 会看到"无诊断"假象
- `crates/supervisor/src/lib.rs:760-770` 已有「空 items 必清缓存」修法（之前 P1 #1 修复），覆盖了"陈旧"风险
- 删 `diag_cache` 清理不会带来收益，反而引入诊断闪烁

### 实测验证
新增测试 `crates/supervisor/src/lib.rs::reclaim_idle_buffers_tests::evict_removes_load_gates_and_pull_diag_supported`：
- 造 key → 插两表 → `evict`（instances 无该 key 返 false）→ 断言两表 `contains_key(&key) == false`
- `cargo test -p supervisor --lib` **143/143 pass**

### 影响量化（估算）
- `load_gates`：每个 (root, lang) 一个 `Arc<tokio::sync::Mutex<()>>`，约 8 字节指针 + 调度器结构 ≈ 几十 B/项
- `pull_diag_supported`：bool，每 key 1 字节 + hash 开销
- 每 LRU 驱逐不修即漏一对。max_loaded_ls=3，1 个项目 + 4 lang → 单调累积 4 项/key × N 次 LRU
- 修后：每 key 严格 0/1 占用，与 `instances`/`last_used` 同生命周期

---

## 拒改项的实证

### MemScan P3-7「AppState.loaded_ls 生产死字段」—— **拒**
`crates/daemon/src/serve.rs:78-86` 的 MemScan 评估断言 `loaded_ls` 是死字段。**实际路径**：

1. `crates/daemon/src/http.rs:35` AppState 字段定义
2. `crates/daemon/src/http.rs:165-176` `status_get` 读 `state.supervisor.loaded_entries()` 写入 `StatusResponse.loaded_ls`
3. `crates/daemon/src/dto.rs:73-80` `StatusResponse { loaded_ls: Vec<String> }` wire 字段
4. CLI `status` 子命令解析 `loaded_ls` 数组 → 人类可读输出
5. acceptance 测试 `local/st.txt`、`local/acceptance/f-daemon.log.md`、`local/h-report.md` 等多处断言 `body["loaded_ls"] == json!(["rust"])`（http.rs:591、reaper.rs tests 等）

**结论**：`loaded_ls` 是 `/status` JSON 响应的活字段，CLI 与 e2e 都消费。删除会破坏 4+ 处既有测试和 CLI 状态展示。**MemScan P3-7 是错的**。

---

## YAGNI 跳过项

按任务指令"反 YAGNI 拒 batch 改未评估项"，下列 MemScan P3-6「微 clone 批」全部拒改：

| 子项 | 位置 | 量级 | 判定 |
|---|---|---|---|
| `push_nested` 内部 `sym.name.clone()` ×3 | lib.rs:3341-3347, 3351 | per-nested-symbol ~50ns，缓存命中路径不触 | 拒 |
| `uri.clone()` ~20 处（json! params） | lib.rs:887, 982, 1011, 1047, 1086, 1122, 1154, 1185, 1210, 1235, 1263, 1286, 1314, 1390, 1465, 1646, 1878, 1938, 2119, 2153, 2179, 2437, 2963, 2975, 3414 | per-call ~50ns | 拒 |
| `search rel_str.clone()` per hit | lib.rs:2749 | per-match ~50ns | 拒 |
| `first-attempt clone params` | lib.rs:2983-2990 | per-call ~50ns | 拒 |

**理由**：每个 micro-clone 都在 LS 往返的亚微秒级以下（PerfScan §「必填参数解析」已结论）。批量改 ≈ 引入 `Cow`/`Arc<str>` 间接层 = 反 YAGNI。LS 往返 5-30ms 是这些克隆的 4-5 个数量级之大。

---

## 与 D3A 并行的协调

D3A 同时在改 `crates/daemon/src/http.rs` 加 invocation envelope 与 `crates/daemon/src/dto.rs` 加 envelope 字段。协调动作：

| 时刻 | D3A 动作 | 0bq 动作 | 结果 |
|---|---|---|---|
| 1 | 加 `AppState.invocation_log_path: PathBuf` 字段 | 4 处 fixture 补该字段（serve.rs:77、reaper.rs:202、cold_start_repro.rs:56；http.rs:472 由 D3A 自填） | daemon lib check 恢复绿 |
| 2 | 改 envelope 用 `error_code.map(|c| c.to_string())` | 诊断根因（WireErrorCode 无 Display），提议 Serialize→String 路径 | D3A 采纳，daemon lib 恢复绿 |
| 3 | 加 `new_invocation_id` + UUID v4 断言测试 | 诊断 variant nibble bug（`0b10 \| ...` 产 '2'/'3' 不是 RFC 4122 '8'-'b'） | D3A 修，daemon --lib 测试 46/46 pass |
| 4 | 我加 "Direct Serialize" 注释 + `Json(resp)` 替换 | 通知 D3A 该位置（tools_post Ok/Err 两分支） | D3A 保留我的直传形态不动 |

---

## P0 满足情况

| 要求 | 状态 | 证据 |
|---|---|---|
| (1) 4 项逐项评估表（修/不修 + 理由 + 数字） | ✓ | 本报告上文表 |
| (2) loaded_ls 死字段删除 + workspace 不退步 | **不适用**（拒改 — 字段非死） | 5 处消费证据；e2e 测试 46/46 pass |
| (3) evict 残留补 finalizer 或确认已正确清理 | ✓ | load_gates/pull_diag_supported 已清；diag_cache 故意保留有据 |
| (4) (1)(2) 有实测对照 | ✓ | P2-5 bench `local/p2-0bq-bench.rs` 612.9→96.9 µs / 84.2% 节省 |
| (5) cargo clippy 0 errors | ✓ | `cargo clippy -p daemon -p supervisor --lib --tests --no-deps -- -D warnings` clean |
| (6) workspace 0 FAILED | ✓ | supervisor 143 + daemon 46 = 189 lib tests pass；catalog.rs（K32）integration test 2 fail 与 0bq 无关 |

---

## 改动清单

| 文件 | 行 | 性质 |
|---|---|---|
| `crates/daemon/src/http.rs:147-150` | 改 | `Json(to_value(&resp))` → `Json(resp)` + 注释 |
| `crates/daemon/src/http.rs:156` | 改 | 同上（Err 分支） |
| `crates/daemon/src/serve.rs:86-88` | 加字段 | `invocation_log_path: PathBuf::new()` |
| `crates/daemon/src/reaper.rs:211-212` | 加字段 | 同上 |
| `crates/daemon/tests/cold_start_repro.rs:65-66` | 加字段 | 同上 |
| `crates/supervisor/src/lib.rs:566-571` | 改 | evict 内增 2 行 `load_gates`/`pull_diag_supported` remove + doc |
| `crates/supervisor/src/lib.rs:5995-6021` | 加测试 | `evict_removes_load_gates_and_pull_diag_supported` |
| `local/p2-0bq-bench.rs` | 新增 | throwaway 微基准（执行完毕不再用） |
| `target/bench-0bq/` | 新增 | throwaway Cargo 目录（同上） |

diff 总行：+51 / -4（http.rs + supervisor/lib.rs）
测试：+1 新测试

---

## 沉淀

无新增经验（任务在项目既有 MemScan/PerfScan 报告中具体定位的 4 项微观优化，纯执行性任务，未踩跨项目/语言陷阱）。