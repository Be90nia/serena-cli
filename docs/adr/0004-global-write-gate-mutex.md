# 0004 · 写事务互斥 = supervisor 全局 Mutex（无独立队列）

- 状态：Accepted
- 日期：2026-08-30（PLAN Task 15）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§3.3 + §9 A4
- 反向指针：ARCHITECTURE.md §3.3 + §9 A4

## 背景

DESIGN §3.1 提出"互斥队列"语义：写类工具（rename / replace-body / insert / delete）必须串行，读类天然并发。复刻阶段需选数据结构。

选工具：
- **全局 `tokio::sync::Mutex<()>`**：FIFO 等待即"互斥队列"，实现量最小
- **独立队列结构 + 调度器**：更灵活（分项目分键），但实现量大且当前明确不做

## 决策

写互斥实现为 supervisor 一把全局 `tokio::sync::Mutex<()>`：

- 跨 await（写事务全程持锁）
- FIFO 等待（tokio Mutex 内部队列）
- 读类不持全局锁（直接并发 Session 调用 LSP）

**未来可选升级**：分项目分键（per-key Mutex）—— 已标 ponytail（教训 `bd serena-rust-m0p`：`/batch` Semaphore(8) 抢 FIFO 门制造顺序性假象，8 写 7 分钟排队）。

## 后果

- **正向**：实现量最小；语义清晰（写串行 + 读并发）；与上游语义对齐
- **代价**：写密集场景串行退化（同进程全局单写）
- **回滚成本**：低（全局 ↔ per-key 是结构改，调用方不变）

## 备选

- **独立 queue + 分项目分键**：更灵活，当前没需求 — 否决（标 ponytail 升级路径）
- **Semaphore**：锁粒度错（教训 `bd serena-rust-m0p`，8 写排队制造伪串行化假象）

## 关联

- 写事务中间态命名 `LocalDirty`（Δ 自 lsp-core docsync.rs），详见 §9.1
- `WRITE_CONFLICT` 错误码（wire 错误码 9 个之一）依赖本决策的串行语义做判定