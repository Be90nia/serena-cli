# 0001 · 七 crate 工作区布局

- 状态：Accepted
- 日期：2026-08-30（PLAN Task 1）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§9 A1
- 反向指针：ARCHITECTURE.md §9 表 A1

## 背景

DESIGN.md §4 草稿列 5 crate（`ls-runtime` / `lsp-core` / `ls-adapters` / `ls-registry` / `daemon`+cli）。复刻开工（M0 Task 1）需细化到可开工粒度，发现：supervisor 实例池+工具语义若埋在 daemon，未来任何 HTTP 端点被迫依赖 axum；cli 拆 lib/bin 更利于 `--direct` 模式（详见 ADR 0007）。

## 决策

DESIGN §4 的 5 crate 细化为 7：

- 拆出 `supervisor`：实例池（per-key 门 + dunce 规范化 + Failed 懒重启）+ 写互斥（ADR 0008）+ 工具语义
- 拆 `daemon` 为 lib + 唯一 bin `cli`（共享 lib 给 `--direct` 与 `install` / `doctor` / `file_detect` 进程内直连）

7 crate：

| crate | 类型 | 角色 |
|---|---|---|
| `ls-runtime` | lib | 进程管理 / 依赖下载 / stderr 泵 |
| `lsp-core` | lib | JSON-RPC 客户端 / 会话 / docsync |
| `ls-adapters` | lib | adapter trait + T2 手写 |
| `ls-registry` | lib | servers.toml + 语言路由 |
| `supervisor` | lib | 实例池 / LRU / 读写调度 / 工具语义 |
| `daemon` | lib | axum HTTP / lock file / idle reaper |
| `cli` | bin | 唯一可执行 serena-cli.exe |

## 后果

- **正向**：supervisor 与 HTTP 框架解耦，未来任何新接口不必带 axum 依赖；cli lib 与 bin 共享类型
- **代价**：多一个薄 crate（构建 graph 多一边）
- **回滚成本**：低（合并 crate 不影响外部 API，仅内部 Cargo.toml）

## 备选

- **5 crate（DESIGN 草稿）**：简单但 supervisor 耦在 daemon，未来扩展受限 — 否决
- **9 crate（拆 ls-runtime 内 transport）**：过度拆分，构建 graph 复杂 — 否决