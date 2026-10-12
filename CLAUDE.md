# serena-rust · 项目 Claude 协作规则

> 本文件是项目根级协作规则，**项目规则覆盖** `~/.omp/agent/AGENTS.md` 通用规则（铁律 6）。
> 进入项目目录时由自动读接力按本文件 → ARCHITECTURE.md → DESIGN.md → PLAN.md → CHANGELOG.md → docs/constitution.md → 适用 adr 顺序展开。

## 1. 项目身份

- **项目**：serena-rust — Rust 复刻 oraios/serena@`7a296833`（CLI + skill，无 MCP，用户明确拒绝 MCP）
- **形态**：7-crate Cargo workspace + 唯一 bin（`serena-cli.exe` 内含 daemon）
- **上游基线**：oraios/serena@`7a296833`（2026-09-24 main；mirror 锚多锚并存见 `ARCHITECTURE.md` 卷首）
- **复刻范围**：只读 `src/solidlsp/`（MIT）取行为语义；`src/serena/`（GPL）禁读禁参考
- **工作平台**：Windows 优先（用户环境 win32 x64）；Unix 等价路径标注但不阻塞
- **rust-toolchain**：`channel = "1.89"`（`rust-toolchain.toml` 钉版）

## 2. 项目级文件读优先序

进入项目目录时自动读接力：

1. `CLAUDE.md`（本文件）— 项目规则
2. `ARCHITECTURE.md` — 架构事实源（§3.4 锁表 / §6 错误 / §8 依赖 是权威）
3. `DESIGN.md` — 设计目标与非目标 / 调研融入
4. `PLAN.md` — 里程碑 + 验收（M0/M1/M2/M4）
6. `CHANGELOG.md` — 历史决策与变更
7. `docs/constitution.md` — 规范九件（constitution 形态）
8. `docs/api/contract.md` — 公开 API 契约入口
9. `docs/adr/` — 架构决策记录（NNN-kebab 序号不复用）
10. `docs/plans/` — 项目级计划落盘点

通用规则接 `~/.omp/agent/AGENTS.md`，**本文件项目规则覆盖通用规则**。

## 3. 项目规则（项目级）

### 3.1 平台与字符编码

- Windows 优先 + `SetConsoleOutputCP(65001)`（`ARCHITECTURE.md §8` 选型理由）
- 路径规范化走 `dunce::canonicalize`（去 `\\?\` UNC 前缀污染 LSP 参数与 file URI）
- 文件 URI 一律 lowercase 后做 key 匹配（rust-analyzer VFS 路径大小写双重身份教训）

### 3.2 上游锁（追溯约定）

- **抄译**：必须注释 `↖ mirror: <上游文件>@<sha> <符号>`（如 `↖ mirror: ls.py@43ae021 send_request`）
- **改进**：必须标 `Δ`
- **多锚并存**：`@43ae021` 初始复刻 / `@c4dc91a7` 09-20 同步 / `@7a296833` 09-28 同步（详见 `ARCHITECTURE.md` 卷首）
- **同步轮纪律**（详见 `local/upstream-sync-*.md`）：新 sha 推进基线 + 全仓 mirror 注释锚对账 + 上游已修复的旧 quirk 复查

### 3.3 错误分层：`thiserror` 边界 / `anyhow` 应用层

- 库 crate（`ls-runtime` / `lsp-core` / `supervisor`）一律 `thiserror` 具名类型
- `anyhow` 只存于 `ls-adapters` 内部 + `daemon` / `cli` 顶层
- 边界规则详见 `docs/constitution.md §3.1` → `ARCHITECTURE.md §6.1`
- 强制：禁止 `anyhow` 越界进入 `lsp-core`

### 3.4 Lint / 格式 / 测试门禁（CI 红线）

```bash
cargo fmt --check                                       # rustfmt.toml edition 2024
cargo clippy --workspace --all-targets -- -D warnings   # workspace.lints.clippy.all = deny
cargo test --workspace                                  # 全绿
cargo tree -p lsp-core                                 # 不含 ls-adapters / ls-registry
cargo tree -p supervisor                               # 不含 axum
```

### 3.5 依赖政策

- 依赖清单见 `ARCHITECTURE.md §8`（每项一行理由）
- **不引入清单**：`dashmap` / `parking_lot` / `async-lsp` / `tower-lsp`（理由见 §8）
- 升依赖 = 改 `Cargo.toml` + 同步 `rust-toolchain.toml` channel
- 跨平台 = 项目内统一处理（Windows-only 代码 unix 化的 CI 洋葱清单见 §3.9）

### 3.6 工具面与契约

- CLI 子命令全集 = `docs/rpc-catalog.json`（catalog 双源同步见 `crates/supervisor/src/catalog.rs` 内嵌 json!）
- HTTP wire 错误码（9 个）见 `ARCHITECTURE.md §6.3`
- 工具语义契约见 `docs/api/contract.md`

### 3.7 文档与变更

- **架构决策** → `docs/adr/NNNN-kebab.md`（序号不复用；superseded 沿用 + 指针文件）
- **项目工件**（CLAUDE.md / constitution / contract / adr / plan / CHANGELOG）→ 进版本控制
- **PM 工作材料**（报告 / 草稿 / 日志 / 探针 / 验收脚本）→ `local/`（已 gitignore）
- **CHANGELOG.md Unreleased 段**：每批对外行为变更（API 增 / 语义改 / CLI 子命令增 / 契约改）必追
- **派单第 8 项**「项目规范引用」必含 `docs/constitution.md` + 适用 ADR 编号

### 3.8 测试策略

- 单元 + 集成：`cargo test --workspace`（CI 门禁）
- 真 LS 冒烟：见 `scripts/smoke_*` + `docs/constitution.md §6`
- 真 LS e2e **不进 CI 门禁**（runner 预装 LS 版本漂移）；本地 + nightly 跑
- **测试绿 ≠ 功能通**：交付门禁完成 = 端到端功能跑通（curl 真实输入 / 验收脚本 / 主流程可观察）

### 3.9 平台 / Release / 二进制身份三查

- Release **前后各一次** `serena-cli stop-all`（daemon lazy-spawn 持旧 binary 表象酷似新功能缺失）
- 二进制身份三查（实锤教训 `fq7f`）：
  1. `target/` 旧产物（cargo build 不删）
  2. 运行中旧 daemon 进程（改二进制后 lazy-spawn 链打到旧实例）
  3. PATH 上安装版（`D:/Program Files/serena-cli/`——用户裸调用即中招）
- Release 二进制 ≠ debug：PM 推送前 `cargo build --release` 亲验 `--help`
- 跨平台 CI：Windows-only 代码 unix 化（fake binary `+x` / sha256sum 格式 / PATH 并发 / 盘符专有断言 `#[cfg(windows)]`）

## 4. 规范九件索引（constitution）

详见 [`docs/constitution.md`](docs/constitution.md)。九件如下：

| # | 件 | 权威来源 |
|---|---|---|
| 1 | 目录结构与命名约定 | `README.md` · `ARCHITECTURE.md §1` · 本文件 §3 |
| 2 | API 契约 | `docs/api/contract.md` · `ARCHITECTURE.md §6.3` · `docs/rpc-catalog.json` |
| 3 | 错误处理规范 | `ARCHITECTURE.md §6.1` §6.2 §6.3 · `docs/constitution.md §3` |
| 4 | 日志规范 | `docs/constitution.md §4` · `ARCHITECTURE.md §8` |
| 5 | 配置管理 | `docs/constitution.md §5` · `crates/ls-registry/src/spec.rs` · `external-servers.toml` |
| 6 | 测试策略 | `docs/constitution.md §6` · `PLAN.md §验收标准` · `scripts/smoke_*` |
| 7 | 并发与内存管理 | `ARCHITECTURE.md §3.4` 锁表（唯一权威） · `docs/constitution.md §7` |
| 8 | 依赖政策 | `ARCHITECTURE.md §8` · `Cargo.toml` workspace.lints |
| 9 | 文档树 | 本文件 · `docs/constitution.md §9` · `docs/adr/` · `docs/plans/` |

## 5. 架构决策记录（ADR）

`docs/adr/` 目录，NNN-kebab 序号不复用，superseded 用指针文件。

- 首批 7 条对应 `ARCHITECTURE.md §9` A1–A7（详见 `docs/adr/0001~0007`）
- 后续架构变更 = 新增 ADR 编号 + 在 `ARCHITECTURE.md §9` 表追加指针

## 6. 命令收口

```bash
# 验证（CI 红线）
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo tree -p lsp-core && cargo tree -p supervisor   # 分层铁律机器检查

# Release
cargo build --release
./target/release/serena-cli --help

# Daemon 治理（Release 前后各一次）
serena-cli stop-all
```

详见 [`docs/constitution.md`](docs/constitution.md)。