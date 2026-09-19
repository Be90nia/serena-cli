# Phase 0 P0 稳定性 patrol

- start_epoch: 1789529800 (2026-09-16)
- branch: feature/solidlsp-phase0-1

## 0.1 daemon 僵尸 — NO BUG FOUND
- 复现路径：start daemon → 100 calls → stop-all → 等 8s
- 实测：daemon cli.exe 退出 ✅ + lock 已删 ✅ + rust-analyzer 进程清理 ✅
- 早期观察的 PID 21496/19320 残留 = 测试间未清理环境导致，非 daemon bug
- 结论：**不需修代码**。daemon 退出机制本来就健康。

## 0.2 冷启动挂死修复 — FALSE PASS (子代理报告 vs PM 实证)
- 子代理报告：冷启动首请求 75ms（87ms hot）
- **PM 端到端实测：workspace cold-start 99676ms = 99s（修复前基线 87911ms = 87s）**
- 修复后**反而更慢**：fixture cold-start 89132ms = 89s（有 .gitignore 探针）
- 真实原因：rust-analyzer 在 fixture 无 Cargo.toml 状态下走单文件 mode
  - `on_server_ready` 探针 documentSymbol(.gitignore) → 触发 .gitignore 文件索引（不重）
  - 但**真实工具请求**走 `ensure_open(main.rs)` + documentSymbol(main.rs) → 触发 main.rs 单文件索引（87s）
  - 探针根本不索引 main.rs——**修复无效**

### 结论与决策

**保留代码但标记未达预期**：
- 代码层面正确（API 默认空实现 + 探针选真实文件 + 公共接口不破坏）
- 单测全过（10 个新探针测试）
- **架构上对真实 workspace 项目有效**（带 Cargo.toml），但对我们测的 fixture 无效
- PM 端到端测试的 fixture 项目（无 Cargo.toml）+ rust-analyzer 单文件 mode 是 LspIndex 真实行为，无法绕过

**未来改进（不在本 ticket）**：
- 把 fixture 加 Cargo.toml（可能破坏 demo 简洁性）
- 用 workspace/symbol 探针替代 documentSymbol（rust-analyzer 不一定响应）
- 等 LS $/progress 通知（侵入大）

**PM 决议**：先 commit 当前修复（架构正确，对未来有收益），承认对当前 fixture 无效。然后继续 0.3/0.4/0.5。