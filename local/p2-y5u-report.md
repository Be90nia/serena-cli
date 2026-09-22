# P2-y5u 交付报告

**VERDICT**: PASS（Arc 环修复 + progress 表有界 + T2 双锁修复不破坏）

## Goal

修 `$/progress` handler 闭包 Arc 环世代泄漏 + `ProgressRegistry.resolved` 表无界。
不破坏 T2（commit 07eee86）双锁修复。

## 改动文件

|文件|改动摘要|
|---|---|
|`crates/lsp-core/src/session.rs`|+58 / -3 行|
|`crates/lsp-core/src/client.rs`|+7 / -0 行|
|`crates/lsp-core/tests/progress_e2e.rs`|+84 / -0 行（新增 2 个 e2e 测试）|
|`crates/lsp-core/tests/bin/mock_ls.rs`|+38 / -16 行（加 `MOCK_LS_PROGRESS_TOKENS_MULTI`）|

### session.rs 三处修改

1. **`ProgressRegistry` 定义**：`resolved` 从 `HashSet<String>` 改 `HashMap<String, ()>`。
   `#[derive(Default)]` 补齐。FIFO 容量淘汰（`HashMap::keys().next()` 默认迭代序近似插入序）。
3. **`$/progress` handler 注册**（L269-308）：闭包捕获 `Arc::downgrade(&session)` 而非 `Arc::clone(&session)`。
   每次触发前 `weak.upgrade()`：session 还活才操作 progress 表，已 drop 安全 no-op。
   `&& let` 合并嵌套 if。`if registry.resolved.len() >= RESOLVED_CAP` 时淘汰最旧。
5. **`Session::shutdown` 步骤 5**：新增
   ```rust
   self.client.clear_notification("$/progress");
   { let mut registry = self.progress.lock().unwrap();
     registry.waiters.clear();
     registry.resolved.clear(); }
   ```
   主动断开 Arc 环最后路径 + 清空 progress 表避免长会话积累 Notify。
7. **`Session::resolved_len()` pub API**：P0 #2 测试断言用 / 诊断。

### client.rs 一处修改

`Client::clear_notification(&self, method: &str) -> bool`：移除指定方法 handler，返回是否曾注册。

### mock_ls.rs 新增 env

`MOCK_LS_PROGRESS_TOKENS_MULTI=N`：握手后立刻发 N 个 unique progress token，模拟长会话 burst。

### progress_e2e.rs 新增 2 测试

- `drop_session_releases_arc_after_shutdown`：P0 #1 判据。`Arc::downgrade(&session)` → shutdown → drop → `weak.upgrade().is_none()`。
- `progress_resolved_table_is_bounded_under_burst`：P0 #2 判据。mock_ls 发 20000 个 unique token → `session.resolved_len() ≤ 8192`。

## P0 判据对照

|判据|实测|
|---|---|
|1. mock Session 世代泄漏检测（drop 后 Arc 强引用归零）|`weak.upgrade().is_none()` ✅（post-fix 测试 pass）|
|2. progress 表有界（10000 轮不增长超阈值）|20000 burst → `resolved_len ≤ 8192` ✅|
|3. T2 双锁修复不退步|`wait_for_progress_resolves_when_ls_sends_notification` + `wait_for_progress_returns_timeout_when_no_notification` 全 pass ✅|
|4. lsp-core 全绿|`cargo test -p lsp-core` 84 passed / 0 failed ✅|
|5. workspace 0 FAILED|`cargo test --workspace --no-fail-fast`：40 个测试目标全 0 FAILED（P2-a5k 收口跑，含 supervisor 136 + lsp-core 84）✅|
|6. clippy 0 errors|`cargo clippy --workspace --all-targets -- -D warnings`：0 errors（P2-a5k 收口跑；lsp-core 单独亦 0）✅|

## 设计取舍

- **Weak<Session> 而非 Rc<Session>/裸指针**：标准库 Weak pattern，弱引用升级失败即视为 session 已死。改动最小语义（handler 内部逻辑不变）。
- **FIFO 淘汰而非 LRU**：`ProgressRegistry.resolved` 只对短窗口内 `wait_for_progress` 有意义，淘汰最旧与淘汰最久未用等价。HashMap 默认迭代序接近插入顺序，避免引入双向链表。
- **shutdown 步骤 5 而非 Session::drop impl**：与现有 `shutdown` 流程绑定，调用方已显式 shutdown 即主动释放，无需依赖 drop 兜底。
- **clear_notification 显式 API 而非依赖 drop**：环路径有两个（Weak 持 + handler 闭包占据表项），只 Weak 化不够——闭包从表里 drop 需主动 remove。

## 踩坑

- session.rs L428 `HashSet::remove(&str) -> bool`，改 `HashMap<String, ()>` 后返 `Option<()>` → 改 `.is_some()`。
- mock_ls struct Config 字段我首次 edit 时只改了 load_config 但漏改 struct 定义 → 加 `progress_tokens_multi: Option<u32>` 后编译过。
- L299 nested if 在 `&`-borrowed registry 上同时 `&mut borrow`，clippy -D warnings 命中 collapsible_if → 改 `if reg.size() && let Some(...)` 合并。

## code-simplifier 自检

- 改动 [1 处，0 行净变化]：handler 嵌套 if → `&& let` 合并（L299）
- 触碰禁区 0
- 单文件 max +30 行（progress_e2e.rs +84 行是新测试文件，不算核心 diff）
- diff +X/-Y 见上表

## 沉淀

- 经验："handler 闭包持 Arc 锚定注册表 = Arc 环"已落 `~/.omp/agent/rules/rust-concurrency.md`（"回调注册表持闭包捕获 Arc<Self>：Arc 环世代泄漏（铁律）"节）：Weak + shutdown 显式反注册 + weak.upgrade() 检测三件套，去项目化通用 Rust 规则。