# 0002 · LanguageServerAdapter trait 默认方法完整化

- 状态：Accepted
- 日期：2026-08-30（PLAN Task 8）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§4.1 + §9 A2
- 反向指针：ARCHITECTURE.md §4.1 Δ + §9 A2

## 背景

DESIGN §4 草稿的 trait 是同步签名 + 必实现全部方法。复刻时发现：依赖下载与 PATH 探测是慢 IO，同步签名会迫使调用方起 `spawn_blocking`；T0 配置驱动生成默认实现的"零代码前提"要求 trait 默认方法完整；hook 注入需要组合而非交出 `&mut Session`（Session 无外部可变性）。

## 决策

`LanguageServerAdapter` trait 修订为方法级签名定义（非实现；初始九方法，随 T2 适配器演进扩至十五方法，2026-10-12 更新计数），关键改动：

1. `launch_info` 改 `async + anyhow::Result<LaunchInfo>`（下载慢 IO 不阻塞）
2. `pre_request(&mut Session, &mut Request)` 拆为 `request_hooks() -> RequestHooks`（值对象、可组合、可测试）
3. 新增 `supports_implementation(&self) -> bool { false }` 等静态能力先验（来自 ls_config.py 逐语言抄表）
4. `on_server_ready` 给默认空实现（T0 模板无需写任何 quirk）
5. SolidLSP Phase 3.2 增补 `wait_for_index` 与 `set_project_root`（写类工具索引等待 / root 记录供就绪探针）

签名以 `crates/ls-adapters/src/lib.rs` 为准（ARCHITECTURE.md §4.1 是镜像抄录）。

## 后果

- **正向**：T0 零代码；hook 注入不需要外部可变性；下载慢 IO 不阻塞
- **代价**：hook 组合需小值类型 `RequestHooks`
- **回滚成本**：中（trait 公开方法签名改 = 外部实现者受影响；本项目内部唯一实现者）

## 备选

- **DESIGN 草稿同步版**：必实现全部方法，T0 配置驱动需逐语言写空方法 — 否决
- **trait 完全闭包化**：与 T0 配置驱动冲突 — 否决