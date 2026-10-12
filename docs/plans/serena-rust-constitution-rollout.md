# serena-rust · Constitution 落地子计划

> 项目级计划落盘点（pm-project skill §2 第 4 点）。
> bead：`serena-rust-j0hf` · 落地日：2026-10-12 · 状态：Done（落地中）

## 1. 背景

serena-rust 立项之初（M0~M2）**未做项目规范约束**：项目根无 CLAUDE.md、规范九件散落（错误分层在 ARCHITECTURE.md §6.1 / 锁表在 §3.4 / 依赖在 §8）、文档树无 `docs/adr/` `docs/api/` `docs/plans/`、ADR 风格合并表 §9 而非独立档案。

pm-project skill §1 第 3 点明确："工程化基线=规范九件（constitution 形态，一次性；存量项目开工前先盘点补缺）"。本次补建为后续派单第 8 项「项目规范引用」提供指向源。

## 2. 决策（用户拍板，2026-10-12）

| 维度 | 拍板 | 备注 |
|---|---|---|
| CLAUDE.md 范围 | **规则 + 索引** | 不复述九件，索引指向 `docs/constitution.md` 各节 |
| 已有规范处理 | **权威保留 + 引用** | ARCHITECTURE.md §6.3 / §3.4 / §8 保持权威，新 doc 用引用 |
| ADR 形态 | **NNN-kebab.md** | GitHub Spec-Kit 同款，序号不复用 |
| 执行节奏 | **一次性建齐** | CLAUDE.md + 九件 + contract + 7 条 ADR 同步完成 |

## 3. 实施清单（落地中）

### 3.1 项目根

- [x] `CLAUDE.md` — 项目规则 + 九件索引（< 200 行）

### 3.2 docs/constitution.md（九件一文件）

- [x] §1 目录结构与命名约定（权威指向 README/ARCHITECTURE/Cargo.toml）
- [x] §2 API 契约（指向 docs/api/contract.md 与 ARCHITECTURE.md §6.3）
- [x] §3 错误处理规范（指向 ARCHITECTURE.md §6.1/§6.2/§6.3）
- [x] §4 日志规范（tracing 初始化点 / 级别约定 / 凭据脱敏 / LS stderr 隔离）
- [x] §5 配置管理（servers.toml / external-servers.toml / SERENA_* env）
- [x] §6 测试策略（层级 / 真 LS e2e 不进 CI / 红线命令 / 测试绿≠功能通）
- [x] §7 并发与内存管理（指向 ARCHITECTURE.md §3.4 锁表）
- [x] §8 依赖政策（准入门槛 / 升级纪律 / 不引入清单）
- [x] §9 文档树（git / gitignore 双层 + ADR 命名 + 变更同步矩阵）

### 3.3 docs/api/contract.md（公开契约入口）

- [x] 契约分层（HTTP wire / rpc-catalog / catalog 双源）
- [x] 稳定性矩阵（破坏性变更必追清单）
- [x] 请求/响应包络（指针）
- [x] 错误码枚举（9 个 + HTTP 保留码）
- [x] 工具目录（catalog 双源纪律）
- [x] 协议版本（`WIRE_PROTOCOL_VERSION=1`，协商待 bd `serena-rust-1mnf`）

### 3.4 docs/adr/（首批 7 条）

| ADR | 主题 | 对应 ARCHITECTURE §9 |
|---|---|---|
| 0001 | 七 crate 工作区布局 | A1 |
| 0002 | LanguageServerAdapter trait 默认方法完整化 | A2 |
| 0003 | LS 子进程 stdin 单 writer task | A3 |
| 0004 | 写事务互斥 = supervisor 全局 Mutex | A4 |
| 0005 | HTTP 200 + `{ok:false}` 工具级失败形态 | A5 |
| 0006 | lock file TCP 探测 + boot_ms + token | A6 |
| 0007 | `--direct` 开发模式 | A7 |

### 3.5 CHANGELOG.md

- [x] Unreleased 段追加本次补建条目（待 PM 写）

## 4. 验收（每件一行可验收）

| 件 | 验收命令 | 期望 |
|---|---|---|
| CLAUDE.md | `wc -l CLAUDE.md` | < 200 行 |
| docs/constitution.md | `wc -l docs/constitution.md` | < 600 行；9 节齐全 |
| docs/api/contract.md | `wc -l docs/api/contract.md` | < 150 行；含 9 错误码表 |
| docs/adr/0001~0007 | `ls docs/adr/000[1-7]-*.md \| wc -l` | 7 个文件 |
| docs/plans/constitution-rollout.md | `wc -l docs/plans/serena-rust-constitution-rollout.md` | 本文件存在 |
| CHANGELOG.md | `head -10 CHANGELOG.md` | Unreleased 段有本次条目 |
| 权威引用 | grep `↖ mirror` 无新增 + `## 后果` 节齐全 | ADR 模板一致 |
| 暂存卫生 | `git status --short` | 4 个无关 modified 不动 + 新文件逐 add |

## 5. 风险

| 风险 | 对策 |
|---|---|
| 新 doc 与权威来源不一致 | 评审时以权威（ARCHITECTURE.md 等）为准；后续 ADR 修订 |
| CLAUDE.md 过长变成"第二份 AGENTS.md" | 限制 < 200 行；项目规则覆盖通用，**不重复** AGENTS.md 内容 |
| catalog 双源遗漏 | 后续增 execute_tool 必双同步（教训 undo/redo 战役） |
| 锁表与新 ADR 漂移 | 后续新增/修改锁必先 ADR 再表 |

## 6. 拍板清单（汇报列「我替你拍板了什么」）

按 AGENTS.md 铁律 2（零猜测=调研优先），本次拍板：

1. **CLAUDE.md 选「规则 + 索引」** 而非「constitution 全文」—— 避免 ARCHITECTURE/各 docs 不重复原则违反
2. **已有规范选「权威保留 + 引用」** 而非「拆出独立 doc」—— minimal change 原则，ARCHITECTURE.md §6.3/§3.4/§8 是历史沉淀，不重构
3. **ADR 形态选「NNN-kebab.md」** 而非「section 分割」—— 序号不复用 = 未来 supersede 留指针（GitHub Spec-Kit 同款）
4. **执行节奏选「一次性建齐」** 而非「分两批」—— CLAUDE.md 是索引，要等 constitution 真实存在落位；分批制造临时不一致

## 7. 后续约束（落地后须遵守）

- **每条派单第 8 项**「项目规范引用」必含 `docs/constitution.md` + 适用 ADR 编号
- **新增/修改锁** = 改 ARCHITECTURE.md §3.4 表 + 加 ADR
- **新增/修改 wire 错误码** = 改 ARCHITECTURE.md §6.3 + 加 ADR + CHANGELOG Unreleased
- **新增/修改依赖** = 改 ARCHITECTURE.md §8 + CHANGELOG Unreleased
- **catalog 双源纪律**：增 execute_tool 分支必双同步 `docs/rpc-catalog.json` + `crates/supervisor/src/catalog.rs` 内嵌

## 8. 修订记录

| 日期 | bead | 摘要 |
|---|---|---|
| 2026-10-12 | `serena-rust-j0hf` | 一次性补建：CLAUDE.md + 九件 constitution + 公开契约 + 首批 7 条 ADR + 本计划落盘 |