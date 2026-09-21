# F2 验收报告

> 任务：写类工具返回值挂诊断快照（serena-rust D:/Project/serena-rust，锚 commit 4ef3b3f）。
> plan：`local/plan-f2-diagnostics-on-write.md`，设计：`local/ai-token-features-design.md §10-F2`。

## VERDICT: PASS

四个 Task 全部完成，全部验证命令按预期通过；未 commit（按约束）。

---

## Task 1：失败时降级的诊断拉取 helper

**改动：**

- `crates/supervisor/src/lib.rs:762-794` —— 在 `impl Supervisor` 内 `tool_diagnostics` 之前插入 `async fn post_diag_for_write`（2s 兜底超时；Ok(Ok(items)) 取 `items` 数组；Ok(Err)/Err 都降级为 `Vec::new()` 并 `tracing::debug!`）。
- `crates/supervisor/src/lib.rs:3949-3960` —— `replace-body` 写分支末尾添加 `post_write_diagnostics` 字段。
- `crates/supervisor/src/lib.rs:4056-4088` —— `replace-text-in-symbol` 同上。
- `crates/supervisor/src/lib.rs:4089-4100` —— `insert-text-after-symbol` 同上。
- `crates/supervisor/src/lib.rs:4101-4112` —— `insert-text-before-symbol` 同上。
- `crates/supervisor/src/lib.rs:4113-4143` —— `delete-text-in-symbol` 同上。
- `crates/supervisor/src/lib.rs:4144-4158` —— `safe-delete-symbol` 特殊处理（`SafeDeleteReport` 经 `to_value` 后 mutate `as_object_mut` 插入字段；file 直接从 args 读）。
- `crates/supervisor/src/lib.rs:4159-4190` —— `insert-at-line` 同上。
- `crates/supervisor/src/lib.rs:4191-4216` —— `replace-lines` 同上。
- `crates/supervisor/src/lib.rs:4217-4234` —— `delete-lines` 同上。

**验证：**
- `cargo build -p supervisor` —— Finished, 0 errors。
- `cargo test -p supervisor --lib` —— 94 passed / 0 failed（无回归；新增 1 个 Task 2 测试）。

---

## Task 2：单元测试——3 种失败模式降级

**改动：**

- `crates/supervisor/src/lib.rs:5222-5262` —— 在 `pull_diagnostics_tests` 模块内、`empty_push_clears_cache_entry` 之后插入 `post_diag_for_write_degrades_on_each_failure_mode`：
  - 场景 A：root 不存在 → 降级为 `[]`。
  - 场景 B：lang 无法解析 → 降级为 `[]`。
  - 场景 C：文件不存在 + 临时目录无 LS → 降级为 `[]`。

**验证：**
- `cargo test -p supervisor --lib post_diag_for_write_degrades` —— `1 passed; 0 failed`。
- `cargo test -p supervisor --lib` —— `94 passed; 0 failed`（无回归）。

---

## Task 3：集成测试——happy path 真 LS 触发

**改动：**

- `crates/supervisor/tests/diagnostics_post_write.rs` —— 新增 2348 字节文件。
  - `has_clangd()` 跳过守卫（沿用 `diagnostics.rs` 模式，clangd 不在即 println!("skipped") return）。
  - `post_write_diagnostics_appears_in_tool_result` —— 故意写错 `const char* s = 12345;`，调 `replace-lines 1 1 "garbage..."`，断言返回值含非空 `post_write_diagnostics` 数组。

**验证：**
- `cargo test -p supervisor --test diagnostics_post_write` —— `1 passed; 0 failed`（clangd 真触发错误，post_write_diagnostics 非空）。

---

## Task 4：端到端 CLI 验证

**步骤：**

1. `cargo build -p cli --bin cli` —— Finished, 0 errors。
2. 启动 daemon：`cli.exe --daemon --project D:/Project/serena-rust/fixtures/rust_demo`（background，sleep 8）。
3. 故意改错：`cli.exe --project fixtures/rust_replace-lines lib.rs 1 1 "garbage unmatched brace {{{"`。
4. 首次响应 `{"applied":true,"file":"lib.rs","post_write_diagnostics":[]}`（rust-analyzer 冷启动未就绪，2s 兜底超时降级为空数组 —— 这是 plan 设计的预期行为）。
5. 等 rust-analyzer 索引完成后（≈15s 后）再发同样命令：响应 `{"applied":true,"file":"lib.rs","post_write_diagnostics":[{...5+ 条 syntax-error...}]}` —— **非空**，rust-analyzer 真的报 `Syntax Error: expected an item` ×5+。
6. 还原文件：`git checkout -- fixtures/rust_demo/lib.rs`。
7. 清理：`cli.exe stop-all` —— `daemon draining (pid 9632); lock will be removed by reaper`，tasklist 确认 cli.exe 已退出。

**验证：**
- `cargo build -p cli --bin cli` —— Finished, 0 errors。
- 首次调用（索引未就绪）：`post_write_diagnostics: []`（**降级路径生效**）。
- 二次调用（索引就绪后）：`post_write_diagnostics` 长度 ≥ 5，包含 rust-analyzer 真实推送的 syntax-error —— **happy path 生效**。

---

## 改动文件清单

|文件|范围|说明|
|---|---|---|
|`crates/supervisor/src/lib.rs`|+109 / -10（行 762-794 helper，3949-4234 写分支 9 处，5222-5262 单元测试）|helper + 9 写分支接诊断 + 单测降级|
|`crates/supervisor/tests/diagnostics_post_write.rs`|新增文件 2348 字节|happy path 集成测试（clangd 真触发）|

无其他文件改动。无新 wire 字段 / 错误码 / 依赖。

---

## 自检纪律遵循

- 已查 spec：`local/plan-f2-diagnostics-on-write.md` + `local/ai-token-features-design.md §10-F2` + `crates/supervisor/src/lib.rs` execute_tool 写分支表。
- 未跑 workspace 全量（只 `cargo test -p supervisor`）。
- 未 commit。
- 未碰公共 API（helper 私有方法；写分支返回值结构沿用既有形态 + 末尾加字段）。

---

## 残余风险 / 已知边界

- **首次冷启动 → 2s 兜底超时降级为空**：与 plan 设计一致；rust-analyzer 索引通常需要 10-30s 才能触发首次 diagnostics。如需"等索引就绪后再拉"，可加 `wait_gen` 参数透传，但 plan 当前未要求。
- **`safe-delete-symbol` 走 `to_value` 后 mutate**：若 `SafeDeleteReport` 未来加 `post_write_diagnostics` 字段（理论冲突），会覆盖，但目前该结构不含此字段，0 风险。
- **`post_write_diagnostics` 字段不影响现有解析方**：daemon/CLI 透传原值，调用方（旧版本解析）拿到字段但不识别会忽略 —— wire 兼容。

---

## 沉淀

无新增经验（与既有 `tool_diagnostics` + LS wait_gen 模式对齐，0 平台 / 语言 / 工具链新踩坑）。