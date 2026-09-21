# L 批次端点（POST /batch）交付报告

VERDICT: PASS

## 改动文件

- `crates/daemon/src/http.rs`：+275 / −9（唯一改动文件；`.beads/interactions.jsonl` 为 bd 系统自动写入，非本任务）
- `Cargo.toml` / `Cargo.lock`：**零改动**（禁新依赖达成）

## Task 1: BatchRequest/Response 类型 — 完成

http.rs 新增（mod tests 之前）：`MAX_BATCH_SIZE=32`、`BatchRequest`、`BatchCall`（tool/project_root/args/lang，同 `/tools` ToolRequest 语义）、`BatchResult`（tool/ok/value/error 互斥，error 用既有 `WireError` 类型——9 码形状与单点完全一致）、`BatchResponse`。
`cargo build -p daemon` → `Finished dev profile in 5.20s`，0 errors。

## Task 2: /batch 处理器 + 路由 — 完成

- `batch_handler`：draining 503（与 tools_post 同套）→ 空批/超限拒收 → in_flight +1 / note_activity / active_project 戳 → run_batch → in_flight −1 → 200。
- `run_batch`：**tokio JoinSet + Semaphore(8)** 并发调度（≤8 同时在飞），任务收 `(idx, BatchResult)`，`join_next` 循环收集后 `sort_by_key(idx)` 保序；join Err 防御分支（不可达：任务体无 panic 源）以 `usize::MAX` 沉底不冒充请求位。
- `execute_batch_call`：单条失败隔离为 `{ok:false, error: WireError}`，绝不短路整批。
- 路由：`router()` 加 `.route("/batch", post(batch_handler))`——鉴权走既有全局 `require_token` layer，自动覆盖。
- 附带最小结构复用：tools_post 内联 503 块抽为 `draining_response()` 共用（响应字节级等价，`shutdown_sets_draining` 测试不改照样过）。
- `cargo build --workspace` → Finished，0 errors。

## Task 3: 测试 — 完成

MockSupervisor 增 `echo` 模式（`slow_x` 延迟 50ms 回显 / `bad_*` → BadArgs / 其余回显 tool 名；既有 call-once 路径不动）。4 个新测试全走 `router().oneshot()`（比 plan 的直调 handler 多覆盖路由+鉴权层）：

- `batch_returns_results_in_request_order`：首 call 最慢（最后完成），断言 results 顺序与 value 一一对应——若丢 idx 排序必失败
- `batch_isolates_failures`：1 好 1 坏 1 好，`results[1].ok=false` + `error.code=BAD_ARGS`，其余 ok
- `batch_rejects_too_large`：33 calls → 200 + `{ok:false}` + BAD_ARGS，无 results 字段
- `batch_rejects_empty`：空批同契约拒收

```
cargo test -p daemon --lib batch → 4 passed; 0 failed (35 filtered out)
cargo test -p daemon --lib       → 39 passed; 0 failed
```

## Task 4: e2e curl — 完成

`cargo build -p cli` 0 errors → 起 `cli.exe --daemon`（ready 229ms）→ lock 读得 port=7860/token → curl：

```
POST /batch {"calls":[overview(lib.rs), refs(lib.rs:1:0)]}   (rust_demo, lang=rust)
→ {"results":[{"tool":"overview","ok":true,"value":[add,multiply…]},
              {"tool":"refs","ok":true,"value":{"compact":true,"items":[],"raw_count":0}}]}
   0.39s；顺序与请求一一对应
POST /tools/overview（老路径回归）→ {"data":[add,multiply…]} 形状不变，无回归
清理：cli.exe stop-all → "daemon draining (pid 9100)" → daemon exit code 0 → lock removed
```

## 每 Task 后纪律

每个代码落地点后均跑 `cargo clippy --workspace --all-targets -- -D warnings`：**0 errors**（最终态复验同绿）。

## 对 plan 的偏离（以实际/约束为准）

1. **禁 futures 新依赖**：plan Step 2 要求 Cargo.toml 加 futures——Constraints 禁新依赖，改用 tokio JoinSet+Semaphore（workspace tokio features 已含 sync/rt，daemon 依赖零改动）。
2. **错误码不新发明**：plan 的 `BATCH_TOO_LARGE`/`EMPTY_BATCH` 是新码——Constraints 9 码 wire 不变，统一映射既有 `BAD_ARGS`（9 码中最贴近 VALIDATION 的码；plan 自审段自己也承认需走 9 码之一）。
3. **拒收 HTTP 状态**：plan 写 400——Constraints 明示 "HTTP 200 + {ok:false} 契约"，改 200 + `{ok:false, error:{code:BAD_ARGS}}`，与 A5 工具级失败契约自洽（CLI 复用同一解析/exit-code 路径）。
4. 测试经 router+oneshot 而非直调 handler（贴既有测试基建）。
5. plan 提及 `commands.rs` 改动——实际不需要（tool dispatch 就在 supervisor trait 后面，http.rs 直达）。

## code-simplifier 自检

改动 N 处 / 触碰禁区 0 / diff +275 / −9 行。清单：重复 503 块抽 helper（−5+1）；guard 早返回无深嵌套；命名达意；保留全部"为什么"注释（校验顺序原因、完成序≠请求序、9 码不变）；无新增抽象层；未动公共既有签名 / Cargo / 测试断言原文 / 错误信息字符串。

## 自我评估

- 准确性 5/5 — 每条声明可溯源：验收命令+输出原文在上文；错误码枚举核对自 dto.rs:39-49
- 完整性 5/5 — 4 Task 全落地；约束全数满足（32 上限/8 并发/失败隔离/顺序保留/9 码/200 契约/禁新依赖/每步 clippy）；空批+超限均覆盖
- 清晰度 5/5 — 偏离逐条标注理由；测试命名即行为
- 可执行性 5/5 — 报告含全部复现命令（curl 带 token 形状、cargo 命令、期望输出）
- 简洁性 4/5 — 扣分：本报告自身偏长，但属任务要求的完整交付文档

## 残余风险

- `Semaphore(8)` 并发上限未做 33-call 真实并发计时断言（mock 无法证明"同时 8 个"）——正确性由 4 测试锁定，吞吐语义属 supervisor 侧行为
- e2e 未单独 curl 无 token 的 /batch 验 403——鉴权为 router 全局 layer，既有 `missing_token_returns_403` 已锁该层

## 沉淀

无新增经验（E0716 临时值生命周期属编译器直给的基础知识；plan-冲突处理是任务级事实，报告已载）。
