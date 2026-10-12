# 0003 · LS 子进程 stdin 单 writer task 取代 `_stdin_lock`

- 状态：Accepted
- 日期：2026-08-30（PLAN Task 4）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§3.2 拓扑 + §9 A3
- 反向指针：ARCHITECTURE.md §3.2 + §9 A3

## 背景

上游 `ls_process.py@43ae021` 早期用写线程 + 每请求 Queue；后续自身放弃（`_stdin_lock` 直写）。本项目映射：3 task 泵（stdout/read、stdin/write、pending/done）+ oneshot，**stdin 用单 writer task 独占（无锁写）**。

## 决策

- **3 task 泵**：stdout 读 task / stdin 写 task / pending oneshot 映射 task
- **stdin 单所有者**：writer task 独占所有 LSP 请求帧（init / request / notification），帧入 mpsc(cap 64)/Session
- **无锁写**：writer task 是 stdin 的唯一读者，多消费者（业务 task）通过 mpsc 排队进入，writer 单线程顺序写
- **oneshot/请求**：每个请求对应一个 `oneshot::Sender`，响应通过 stdout 读 task 按 id 路由到对应 sender

## 后果

- **正向**：多任务并写一把锁 ↔ 单任务顺序写，语义相同、无锁竞争；与上游 serena 现行实现对齐；省一层 std Mutex
- **代价**：writer task 故障 = 全站（直连 daemon 失败点明确）
- **回滚成本**：低（mpsc + Mutex 互换）

## 备选

- **Mutex 守 stdin 直写**：与上游早期 `_stdin_lock` 等价；锁竞争 + 死锁风险 — 否决
- **每请求起 thread**：多线程开销大，DAEMON 调度复杂 — 否决

## 参考

- helix `helix-lsp/src/transport.rs`（send task / pending map / inject 通道 / shutdown 期空响应 quirk）— Rust 先例参考
- 上游 serena `_stdin_lock` 直写（其后期已演进到本方案同构）