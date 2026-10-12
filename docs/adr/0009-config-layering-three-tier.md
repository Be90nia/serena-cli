# 0009 · 配置分层收敛为 3 层（删用户目录 servers.toml 层）

- 状态：Accepted
- 日期：2026-10-12
- 对应 bead：`serena-rust-li4x`（宪法合规审计修复波 2，决策拍板 = A 路）
- 修订对象：`docs/constitution.md §5.1`（及 §5 权威行）
- 反向指针：constitution §5.1、§9.4

## 背景

宪法首批落地（bd `serena-rust-j0hf`）时 §5.1 写了 4 层配置分层：

- L1 项目内 `servers.toml`（编译期内嵌）
- L2 `%APPDATA%/serena/servers.toml`（用户覆盖内置表）
- L3 `%APPDATA%/serena/external-servers.toml`（外部 LS）
- L4 环境变量 `SERENA_*`（平行）

Task B 审计（`local/audit-spec-2026-10-12-task-B.md` B-P0-1）对拍 `crates/ls-registry/src/config.rs` 实现证伪：代码里**不存在**「用户目录第二份 `servers.toml`」的解析路径。实际实现为 3 层文件分层——L2 的位置上是一个**不同 schema、不同粒度**的文件 `config.toml`（`[ls.<id>]` 字段级覆盖 → `LsOverride`，`config.rs user_override`）；条目级替换走 `external-servers.toml`（`load_external` + `merge_pick`）。宪法 L2 行在文件名与粒度上双重失真：文件名写 `servers.toml`、粒度写条目级，代码实际是 `config.toml` + 字段级。

## 决策

**宪法 §5.1 向代码收敛（A 路）：删「用户目录 servers.toml」层，分层改写为 3 层实况**：

| 层 | 路径 | 粒度 |
|---|---|---|
| 1 | 仓库内 `servers.toml`（`include_str!` 内嵌） | 内置默认表 |
| 2 | 用户目录 `config.toml` | 字段级覆盖（`[ls.<id>]` → `LsOverride`） |
| 3 | 用户目录 `external-servers.toml` | 条目级整块替换（priority 并列 external 胜出 + 加载时 warn） |

配套口径（写入 §5.1）：运行期合并优先级链 = CLI flag > `config.toml` > `external-servers.toml` > 内置 `servers.toml`（`merged_spec_for`）；`servers.toml` 唯一存在形态 = 仓库内编译期内嵌，用户目录没有第二份；环境变量是运行时行为开关（平行维度），不设「第 4 层」。

## 备选

- **B 路：在 `config.rs` 补 `%APPDATA%/serena/servers.toml` 条目级解析**，向宪法 4 层收敛 — 否决。`external-servers.toml` 已承担「用户目录条目级注册/覆盖」职责（`ls-use` 工具链、priority 语义、冲突 warn 全套就绪），再加一份同 schema 用户表 = 三个用户文件两套合并语义，无需求支撑，纯增表面。
- **折中：宪法 L2 行改为 `config.toml` 但保留「条目级」描述** — 否决。粒度仍是错的：`config.toml` 是字段级覆盖（`user_override` 逐字段并入），写成条目级会误导实现者把 `external-servers.toml` 的整块替换语义搬到 `config.toml` 上。

## 后果

- **正向**：宪法 §5.1 从「文档承诺不存在的机制」变为逐行可 grep 验证的实况；用户覆盖心智模型收敛为「字段微调用 config.toml、整条注册/换 server 用 external-servers.toml（`ls-use` 管理）」。
- **代价**：曾按旧宪法 4 层表述写过文档/讨论的地方需随之修正（本轮已同步 §5 权威行）。
- **回滚成本**：低（纯文档层；若未来真需要用户目录 `servers.toml`，走新 ADR + `config.rs` 实现，不复活本条）。
