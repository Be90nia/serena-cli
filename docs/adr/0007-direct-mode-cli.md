# 0007 · `--direct` 开发模式（CLI 进程内直调 supervisor）

- 状态：Accepted
- 日期：2026-08-30（PLAN Task 10）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§1 + §9 A7
- 反向指针：ARCHITECTURE.md §1 + §9 A7

## 背景

M0 冒烟与单测若经 HTTP（daemon）路径：起 daemon + bind + lock + idle 自杀协调 = 验收慢且易受 daemon 故障污染。

cli bin 必须能不经 HTTP 直连 supervisor / ls-registry / lsp-core（`install` / `doctor` / 语言自动探测 / `--direct`）。

## 决策

新增 `--direct` 开发模式：CLI 进程内直调 supervisor：

- 不起 daemon
- 不走 HTTP wire
- CLI 进程内复用 supervisor lib + ls-registry + lsp-core（依赖面见 `crates/cli/Cargo.toml`）
- 常规 CLI 转发路径仍走 daemon HTTP（双路径并存）

## 后果

- **正向**：M0 冒烟与单测可绕过 daemon（daemon 故障不污染验收）；`install` / `doctor` / `file_detect` 不必启 HTTP
- **代价**：cli 多一条对 supervisor 的依赖路径（本来就有，cli → daemon → supervisor 是间接路径，direct 是直连）
- **回滚成本**：低（`--direct` 是 opt-in flag，不影响默认 CLI 路径）

## 备选

- **强制 HTTP**：冒烟慢且易污染 — 否决
- **建独立 bin（直接给 client）**：重复建设 — 否决