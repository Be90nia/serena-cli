# 0010 · 写响应立返，诊断确认异步化（wait_gen 通道）

- 状态：Accepted
- 日期：2026-10-12（bd `serena-rust-qq3a` perf-P1-1）
- 对应 ARCHITECTURE：§3.3 写事务收尾、§5 写后诊断反馈（F2）
- 反向指针：supervisor `post_diag_for_write` / `spawn_post_write_diag_check`

## 背景

F2（70bee45）刻意设计：写类工具响应内联 `post_write_diagnostics` 快照，把「写后 AI 下一步通常查 diagnostics」折叠进写返回值。实现在响应路径上**同步**等 tool_diagnostics 的 version 确认（上限 3s）。

真实负载（bd qq3a 实测，n=174）：replace-body p50=3029ms / p95=3097ms，delete-lines p50=3021ms——预算几乎打满。AI 每次写 ≥3s；`/batch` 32 写最坏 +96s。小 fixture p50=130ms 证明耗时与「诊断确认等待」强相关，与写本身无关。

## 决策

写响应**立返** pending 快照，诊断确认移出响应路径：

- `post_diag_for_write` 保留写衍生缓存失效（root 信号 + 符号缓存 + recent_write 标记），随后直接返回 `{"items": [], "pending": true}`——与既有「3s 超时降级」**同一 JSON 形态**，slim 后压缩为 `"pending"` 字符串。**向后兼容取舍：置 pending 而非移除字段**——"pending" 是 tfa3 瘦身已定义的三态之一，调用方契约（pending = 未确认、items 空 ≠ 无错、回头复核）零变化；移除字段则是破坏性变更，否决。
- 诊断确认走**既有 wait_gen 通道**：`spawn_post_write_diag_check` fire-and-forget 后台调 `tool_diagnostics(wait_gen = 基线+1)`，复用 push 等待窗 + pull 兜底全链。结果不进响应（已发出），仅 tracing 留痕——完成 `debug!`、失败 `warn!`（**禁静默**，设计约束②）。LS 推送照常进 diag_cache，AI 显式 `diagnostics --wait-gen N` 查询不受影响。
- Supervisor 新增 `me: OnceLock<Weak<Supervisor>>` 自引用，daemon 在 `Arc` 包装后 `attach_self_weak()` 回填（serve.rs 一处）；未回填（CLI direct / 测试）→ 回查 debug 跳过。
- `/batch` 无需改动：batch_handler 每调复用 tools_post 同一套 execute_tool 分支，自动同享立返收益（约束③）。

## 后果

- **正向**：写响应 p50 从 ~3s 降到 <500ms（验收实测 n≥5）；/batch 写批量收益同享；F2 的诊断反馈价值保留（通道 + 缓存 + pending 语义全部不变）。
- **代价**：写响应不再内联诊断确认——依赖该字段拿确认结果的调用方需显式补一发 `diagnostics --wait-gen`；后台回查多占 ~5-20s 的后台任务槽（fire-and-forget，无响应路径影响）。
- **回滚成本**：低——恢复同步等待即把 `spawn_post_write_diag_check` 改回 `tokio::time::timeout(3s, tool_diagnostics(...))` 内联。

## 备选

- **预算自适应**（按文件大小/历史诊断时长动态预算）：状态多、判据脆，否决。
- **移除 post_write_diagnostics 字段**：调用方解析破坏性变更，否决（见决策）。

## 关联

- ADR-0011（LS 工具级预算）：后台回查里的 pull/push 等待同样受 LS 预算保护，不会成为新的无界点。
- 写后诊断 pending 双态语义定义见 `post_diag_for_write` doc 注释（tfa3 / blindtest v5 P3-I 锁定）。
