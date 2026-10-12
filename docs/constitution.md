# serena-rust · 规范九件（Constitution）

> 一次性补建的工程化基线，2026-10-12 落盘（bd `serena-rust-j0hf`）。
> **权威保留 + 引用原则**：本文件指向 `ARCHITECTURE.md` / `Cargo.toml` / `docs/api/contract.md` 等既有权威来源，不复述代码可推导内容。如发现本文件与权威来源不一致，**以权威来源为准**并通过 ADR 修订本文件。

## 1. 目录结构与命名约定

**权威**：`README.md` §Install · `ARCHITECTURE.md §1` Workspace 布局 · `CLAUDE.md §3`

### 1.1 顶层布局

| 路径 | 角色 |
|---|---|
| `Cargo.toml` | workspace（resolver = "2"，edition = "2024"，lint clippy all = deny） |
| `Cargo.lock` | 锁定（提交进版本控制，遵循 Cargo 默认） |
| `rust-toolchain.toml` | 钉版 `channel = "1.89"`（升钉改本文件 + 各 workflow toolchain） |
| `rustfmt.toml` | edition = "2024"（CI 跑 `cargo fmt --check`） |
| `pyrefly.toml` | Python 工具校验（`scripts/*.py`） |
| `.gitignore` | 覆盖 `local/` / `smoke-logs/` / `stress-logs/` / `target-*/` / `.expert/` / `Temp/` / `fixtures/**/target/` |
| `.beads/` | bd 项目任务系统（不进 git 含 `.dolt/`） |
| `CLAUDE.md` | 项目根级规则 + 九件索引 |
| `README.md` / `README.zh-CN.md` | 对外文档 |
| `ARCHITECTURE.md` | 架构事实源（§3.4 锁表 / §6 错误 / §8 依赖 是权威） |
| `DESIGN.md` | 设计目标与非目标 / 调研融入 |
| `PLAN.md` | 里程碑 + 验收标准 |
| `CHANGELOG.md` | 历史决策与变更（Unreleased 段优先） |
| `crates/` | 7 个 lib/bin（见 §1.2） |
| `docs/` | 项目工件：`api/` `adr/` `plans/` `rpc-catalog.json` |
| `extensions/` | 编辑器扩展产物 |
| `fixtures/` | 集成测试夹具（gitignore 子目录构建产物） |
| `local/` | PM 工作材料（报告 / 草稿 / 日志 / 探针）—— 不进 git |
| `scripts/` | 冒烟 / 压测 / 验收脚本（`smoke_*` `stress_*`） |
| `skills/serena-cli/` | 项目内 skill（agents 集成面） |
| `Temp/` | 测试产物快照—— 不进 git |
| `.github/workflows/` | CI |

### 1.2 crates 命名（7 个）

| crate | 类型 | 角色 |
|---|---|---|
| `ls-runtime` | lib | 进程管理 / 依赖下载 / stderr 日志泵 |
| `lsp-core` | lib | JSON-RPC 客户端 / 会话 / docsync（核心类型底座） |
| `ls-adapters` | lib | `LanguageServerAdapter` trait + T2 手写适配器 |
| `ls-registry` | lib | servers.toml 解析 + 语言路由（EXT_TABLE） |
| `supervisor` | lib | 实例池 / LRU / 读写调度 / 工具语义 |
| `daemon` | lib | axum HTTP / lock file / idle reaper |
| `serena-cli` | bin | 唯一可执行 `serena-cli.exe`（crate 目录 `crates/cli/`；含 daemon；二进制身份以更名战役定案为准） |

新增 crate = 改本节 + `Cargo.toml` workspace.members + `ARCHITECTURE.md §0` 依赖图。

### 1.3 命名约定

- **crate 名**：kebab-case（`ls-runtime`）
- **模块名**：snake_case（`write_gate`）
- **类型/枚举**：PascalCase（`ToolError`）
- **常量**：SCREAMING_SNAKE（`WIRE_PROTOCOL_VERSION`）
- **文件名**：snake_case（`session.rs`）；多模块用目录（`transport/mod.rs`）
- **测试夹具**：`fixtures/<lang>_demo/`（gitignore 子目录 target/）
- **脚本**：`scripts/smoke_<topic>.sh` / `scripts/stress_<topic>.sh` / `scripts/test_<topic>.py`
- **报告 / 草稿**：`local/<topic>-report.md`（PM 工作材料，不进 git）

## 2. API 契约

**权威**：[`docs/api/contract.md`](api/contract.md) · `ARCHITECTURE.md §6.3` wire 错误码 · `docs/rpc-catalog.json` 工具目录

### 2.1 契约双源

| 源 | 角色 |
|---|---|
| `docs/api/contract.md` | 公开契约入口（**对外引用起点**） |
| `ARCHITECTURE.md §6.3` | wire 错误码权威（9 个 error.code 枚举 + retryable + CLI exit 表） |
| `docs/rpc-catalog.json` | 工具目录（input/output schema，与 `crates/supervisor/src/catalog.rs` 内嵌 json! 同步——catalog 双源） |

### 2.2 对外契约稳定性

- **错误码枚举** = 公开契约（`ws` enum 改 = 主版本号 bump）
- **CLI 子命令增/改/删** = 公开契约变更（追 CHANGELOG Unreleased + README）
- **wire 字段新增** = 非破坏性（向后兼容旧 client）
- **wire 字段删除/类型变** = 破坏性（追 CHANGELOG + ADR）

## 3. 错误处理规范

**权威**：`ARCHITECTURE.md §6.1` 类型分层 · `§6.2` 传播路径 · `§6.3` wire 错误码

### 3.1 类型分层铁律（`thiserror` 边界 / `anyhow` 应用层）

| crate | 错误类型 | 上游对应物 |
|---|---|---|
| `ls-runtime` | `RuntimeError::{Spawn, Download, MissingRuntime, Env}` | ls_exceptions.py + dependency_provider |
| `lsp-core` | `CoreError::{Framing, Io, Rpc, Timeout, Terminated, ServerCancelled, NotReady}` | LSPError / TimeoutError / Terminated |
| `ls-adapters` | `anyhow::Error`（末端编排） | 上游子类 raise |
| `ls-registry` | `String` 消息（spec 解析） | — |
| `supervisor` | `ToolError::{BadArgs, NotInstalled, Core, WriteConflict, Launch, Serialize, Protocol}` | — |
| `daemon` / `serena-cli` | `anyhow`（顶层装配与兜底） | — |

**边界规则**：
- 库 crate 一律 `thiserror` 具名类型（按变体分类重试/上报）
- `anyhow` 仅 `ls-adapters` 内部 + `daemon` / `cli` 顶层
- `supervisor` 仅以 `ToolError::Launch/Serialize` 内嵌 `anyhow` 收口，**不外泄 anyhow 类型**
- **禁止 `anyhow` 越界进入 `lsp-core`**（调用方需要按变体分类）

### 3.2 特殊码

- `ContentModified (-32801)` 不设独立变体 = `Rpc{code:-32801}` 判定值，重试逻辑在 `client.rs` 内部消化（opt-in 方法表 + 3 次 + 200ms；与 `init_params::RETRY_ON_CONTENT_MODIFIED` 同源——`opt-in 注册函数/配置开关` 必查生产调用点）
- `NotReady` 为 Δ 新增（bd `serena-rust-iyz`，未就绪竞态从 Io 兜底拆出，wire 映射 `LS_NOT_READY` retryable）

### 3.3 CLI exit code

| exit | 语义 |
|---|---|
| 0 | 成功 |
| 1 | 工具失败（非 retryable） |
| 2 | 用法错误 |
| 3 | daemon 或传输故障（含 daemon 拉起失败） |
| 4 | `wait-ready` 超时（bd `serena-rust-55m`） |
| 5 | retryable 工具失败（瞬态可直接重试，bd `719`） |

## 4. 日志规范

**权威**：`ARCHITECTURE.md §8` 依赖选型 · `crates/ls-runtime/src/process.rs` stderr 泵

### 4.1 库与初始化

- **库**：`tracing` + `tracing-subscriber`（env-filter feature）
- **初始化点**：daemon CLI 分支（曾全库无 init → 所有 tracing 静默丢弃 → 排障全盲，2026-09-22 接上）
- **门控**：`RUST_LOG` 环境变量（上游 `serde` semantic `debug` 行为对齐）
- **方向**：stderr（绝不进 stdout；stdout = wire 输出）

### 4.2 级别约定

| 级别 | 用途 |
|---|---|
| `error!` | 不可恢复错误（daemon 退出 / daemon 启不动 LS） |
| `warn!` | 可恢复但需关注（idle 拔锁、binary 旧版警告、`stop-all` 后 draining） |
| `info!` | 关键事件（daemon 启停 / LS 启停 / lazy-spawn / write gate 进/出） |
| `debug!` | 协议帧录制 / frame-level 状态（`SERENA_RECORD` gate） |
| `trace!` | per-byte 缓冲 / 协议帧边界（默认关） |

### 4.3 凭据脱敏

- **绝不**记录：token / SHA-256 / 安装路径 / 用户目录绝对路径
- 记录用 key 而非 value（`tracing = "log_token={key}"`）
- 内部 LS 进程路径输出前先正则脱敏（`D:\\Users\\X\\AppData` → `<user_home>`）

### 4.4 LS stderr 隔离

- per-LS 前缀（clangd `I[..]/E[..]`）随 T2 深度 quirk 落地
- stderr 泵按通用缺省分级汇入（不设独立 logmap.rs——上 T2 再补）
- LS 进程身份 = `ExecutablePath`（tasklist 误判：编辑器内置 `rust-analyzer`，认路径不认进程名）

## 5. 配置管理

**权威**：`crates/ls-registry/src/spec.rs` · `servers.toml`（项目内 `include_str!` 内嵌）· `config.rs`（分层合并 + 用户 `config.toml`）· `external-servers.toml`（用户目录覆盖）

### 5.1 配置分层（3 层，与 `config.rs` 实现一致）

| 层 | 路径 | 粒度 | 优先级 |
|---|---|---|---|
| 1 | 项目内 `crates/ls-registry/src/servers.toml`（编译期 `include_str!` 内嵌） | 内置默认表 | 最低（默认值） |
| 2 | 用户目录 `config.toml`（Windows `%APPDATA%\serena\`；Unix `~/.config/serena/`） | 字段级覆盖（`[ls.<id>]` 逐字段 → `LsOverride`） | 中 |
| 3 | 用户目录 `external-servers.toml`（同目录，`ls-use` 管理） | 条目级整块替换（`[servers.<id>]`） | 最高（同 id/language 并列 external 胜出 + 加载时逐条 warn） |

- **运行期合并优先级链**：CLI flag（`LsOverride`）> 用户 `config.toml` > `external-servers.toml` > 内置 `servers.toml`（`ls-registry/src/config.rs` `merged_spec_for`）。字段级与条目级覆盖并存不冲突。
- **servers.toml 归属**：唯一内置 LS 注册表，只存在于仓库内、编译期内嵌（`spec.rs` 解析 + `config.rs::SERVERS`）；用户目录**没有**第二份 `servers.toml`——条目级用户注册/覆盖一律走 `external-servers.toml`（决策见 ADR-0009）。
- **环境变量不在本分层内**：§5.2 的 `SERENA_*` 是运行时行为开关（超时/预算/诊断钩子），不参与 LS spec 解析，属平行维度，不设「第 4 层」。
- **项目级配置不参与 cmd 构造**（防恶意仓库注入二进制；环境变量 override 禁止——`config.rs` 头注）。

### 5.2 环境变量清单（生产路径全集）

| 变量 | 默认 | 语义 | 注册点（grep 锚） |
|---|---|---|---|
| `SERENA_IDLE_TIMEOUT_SECS` | `900` | daemon 全局 idle 自杀阈值；`0` = 永不自杀 | `daemon/src/reaper.rs` `intervals_from_env` |
| `SERENA_LS_IDLE_EVICTION_SECS` | `600` | 单 LS 空闲驱逐阈值；`0` = 永不驱逐 | `daemon/src/reaper.rs` `intervals_from_env` |
| `SERENA_MAX_LOADED_LS` | `6` | LRU 驱逐线；`0` 非法（持续驱逐）回默认 | `daemon/src/reaper.rs` `parse_count` |
| `SERENA_NO_TOKEN_ESTIMATE` | 未设 | `1` = 工具成功响应不附 `~tokens` 估算 | `daemon/src/serve.rs` |
| `SERENA_DAEMON_LOG` | 未设 | daemon stderr 落盘路径（append；排障/smoke per-door 归属） | `cli/src/main.rs` `daemon_log_file` |
| `SERENA_RECORD` | 未设 | LSP 帧录制到 `<path>` | `lsp-core/src/session.rs` `ENV_RECORD` |
| `SERENA_REPLAY` | 未设 | 帧回放驱动（不接真 LS；失败复现/回归） | `lsp-core/src/session.rs` `recorder_from_env` |
| `SERENA_HANDSHAKE_TIMEOUT_SECS` | `30` | LS 握手段超时；`0`/非法回默认（0 = 永不超时禁用） | `lsp-core/src/session.rs` `handshake_timeout` |
| `SERENA_WAIT_READY_TIMEOUT_SECS` | `120` | `wait-ready` 就绪等待上限；显式 `--timeout` 优先 | `cli/src/main.rs` `wait_ready_timeout_secs` |
| `SERENA_JDTLS_SNAPSHOT` | 未设 | `1` = jdtls 切 snapshot 发行版 + 系统 JDK 模式 | `ls-adapters/src/jdtls.rs` `snapshot_mode` |
| `SERENA_SKIP_LLVM_FALLBACK` | 未设 | `1` = 禁 clangd Windows LLVM 兜底探测（验证 not-installed 路径） | `ls-adapters/src/lib.rs` `var_os` |
| `SERENA_NETWORK_PROBE_HOST` | `github.com:443` | `doctor` 网络探活目标覆盖（镜像环境） | `supervisor/src/doctor.rs` `check_network` |
| `SERENA_DEBUG_RAW` | 未设 | `1` = 静默空结果附 `raw_lsp_response` 200B 快照 | `supervisor/src/lib.rs` `debug_raw_enabled` |
| `SERENA_WARMUP_TIMEOUT_MS` | `15000` | 语义渐进首答预热预算；CLI `--warmup-timeout` / `args._warmup_ms` 优先 | `supervisor/src/lib.rs` `parse_warmup_env` |
| `SERENA_DEFAULT_MAX_TOKENS` | 未设 | 未显式 `_max_tokens` 时的响应 token 预算 | `supervisor/src/lib.rs` `execute_tool` |
| `SERENA_DEFAULT_MAX_ITEMS` | 未设 | list 类工具未显式条数旗时的默认条数 | `supervisor/src/lib.rs` `arg_limit` |
| `SERENA_WRITE_GATE_TIMEOUT_SECS` | `60` | 写门等待超时（超时 → `LS_TIMEOUT` retryable）；`0` 视同非法 | `supervisor/src/write_gate.rs` `gate_timeout_secs` |
| `SERENA_SKIP_LS_E2E` | 未设 | `1` = 真 LS e2e 全跳过（CI 门禁，§6.2） | `supervisor/src/ct/tests.rs` `skip_ls_e2e` 等 |

非法值（负数 / 非数字）warn 后用默认，**配置错误不致命**。

**边界（不入本表）**：测试/脚本门控 env（`SERENA_TEST_DOWNLOAD`、`SERENA_CI_ONLY_INCL`、`SERENA_MOCK_NF_CHILD`、`SERENA_EXTRA_ENV`、`SERENA_FXDIR`）、示例二进制 env（`SERENA_DAEMON_LOCK` / `SERENA_DAEMON_PORT`，仅 `daemon/examples/`）、日志门控 `RUST_LOG`（§4.1）。新增生产 env = 改本表。

### 5.3 Secret 与凭据

- **绝不**入库：用户 token、绝对路径（home/~/Project/）、SHA-256 期望值之外的网络资产 URL
- 期望值入 `servers.toml` 是公开事实（脱敏后即失效）
- 用户目录覆盖文件（`config.toml` / `external-servers.toml`，位于 `%APPDATA%\serena\` 或 `~/.config/serena/`，仓库外）天然不入库；仓库内本地运行形态 `.config/serena/` 由 `.gitignore` 覆盖。不存在 `.local.toml` 后缀文件。

## 6. 测试策略

**权威**：`PLAN.md §验收标准` · `ARCHITECTURE.md §5` 状态机 · `scripts/smoke_*` `scripts/stress_*`

### 6.1 测试层级

| 层 | 工具 | 门禁 |
|---|---|---|
| 单元 | `cargo test --workspace`（lib + bin） | **CI 红线**（全绿） |
| 集成 | `cargo test --workspace` 同上 + `crates/*/tests/*.rs` | **CI 红线** |
| 真 LS 冒烟 | `scripts/smoke_one.sh` + `scripts/smoke_langs.toml` | 本地（CI gate: `SERENA_SKIP_LS_E2E=1` 跳过） |
| 真 LS 压测 | `scripts/stress_loop.sh` + `scripts/stress_churn.sh` | nightly + 本机 |
| 端到端 | `local/accept_*.py`（PM 亲验，gitignore PM 工作材料） | 收口（每里程碑） |

### 6.2 真 LS e2e 不进 CI 门禁

- runner 预装第三方 LS（LLVM / clangd）版本随 image 漂移
- CI 用 `SERENA_SKIP_LS_E2E=1` 显式跳过（has_clangd 门会漏，详见 memory summary）
- 真机覆盖留在本地 + nightly

### 6.3 验收红线（每阶段收尾必跑）

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo tree -p lsp-core && cargo tree -p supervisor   # 分层铁律
./scripts/smoke_one.sh --help                        # smoke 工具自检
```

### 6.4 测试绿 ≠ 功能通

- 交付门禁完成 = 端到端功能跑通（curl 真实输入 / 验收脚本 / 主流程可观察）
- 子代理报"测试通过"≠完成——PM 亲验 1 条真实命令/截图（铁律 7）
- **机制存在≠接线生效**（教训 `0vj1`）：审计含 opt-in 注册的机制（capability / feature gate / config）必查生产调用点

## 7. 并发与内存管理

**权威**：本节 §7.1 全量登记表 · `ARCHITECTURE.md §3.4`（锁序与临界区约定权威）· `rules/rust-concurrency` · `rules/rust-pitfalls`

### 7.1 锁与同步原语（生产代码全量登记表）

**权威**：本表（全项目生产代码全量登记）· `ARCHITECTURE.md §3.4`（语义性原语的临界区约定与锁序叙事；与本表重叠行内容一致）

**登记范围与豁免判据**（机械可判，三条**全部满足**才豁免登记）：

1. 不在测试范围：声明处非 `#[cfg(test)]`、不在 `tests/`、不在 `tests/bin/` 测试夹具；
2. 非 static、非 struct 字段：函数体内 `let` 创建且函数返回前 drop；
3. 叶锁：临界区内无 `.await` 且不获取第二把锁。

任一条不满足 → **必须入本表**。补充口径：`LazyLock`/`OnceLock` 一次性初始化容器本身不算原语，其内层 `Mutex`/`Atomic` 单独登记；函数内 `static`（如 id 计数器）因第 2 条**入表**。

**锁序总纲**：全局写门 `WRITE_GATE` = 全项目最外层，所有写类工具入口先取门再触其他锁（16 处 `write_gate::acquire` 调用点一致）；`load_gates` 外层 map 锁取 `Arc` 后立即释放，跨 await 等待只发生在内层 per-key tokio Mutex 上；其余 std `Mutex`/`Atomic` 一律叶锁（微秒临界区、无 await、锁内不获第二把锁）。

| 原语 | 位置 | 保护对象 | 纪律 |
|---|---|---|---|
| `next_id: AtomicI64` | `lsp-core client.rs` | 请求 id 分配 | 叶·无锁 |
| `pending: Mutex<HashMap<Id, oneshot::Sender>>` | `lsp-core client.rs` | pending 响应表 | 叶 |
| `notification_handlers: Mutex<HashMap<String, NotificationHandler>>` | `lsp-core client.rs` | 通知 handler 表（未注册即丢弃） | 叶 |
| `server_request_handlers: Mutex<HashMap<String, ServerRequestHandler>>` | `lsp-core client.rs` | server→client 请求 handler 表 | 叶 |
| `content_modified_retry_methods: Mutex<HashSet<String>>` | `lsp-core client.rs` | `-32801` 重试白名单 | 叶 |
| `TokenBucket.state: Mutex<TokenState>` | `lsp-core client.rs` | Background 帧令牌桶 | 叶 |
| `oneshot` ×1/请求 | `lsp-core client.rs` | 响应完成 | — |
| `state: Mutex<SessionState>` | `lsp-core session.rs` | 状态机 Uninitialized→Initializing→Ready/Failed | 叶 |
| `pumps: Mutex<Option<Pumps>>` | `lsp-core session.rs` | 泵句柄集合（shutdown 终态 drop → Job 句柄回收） | 叶 |
| `server_capabilities: Arc<Mutex<Option<Value>>>` | `lsp-core session.rs` | initialize 响应 capabilities 原值 | 叶 |
| `progress: Mutex<ProgressRegistry>` | `lsp-core session.rs` | `$/progress` waiter + 早到通知合并登记（两步原子防通知丢失） | 叶 |
| `language_id: Mutex<Box<str>>` | `lsp-core session.rs` | didOpen languageId | 叶 |
| `language_by_ext: Mutex<HashMap<Box<str>, Box<str>>>` | `lsp-core session.rs` | per-file languageId 覆盖表（astro/vue 双服务器） | 叶 |
| `buffers: Mutex<HashMap<Uri, FileBuffer>>` | `lsp-core session.rs`（docsync 语义归属） | 打开文件表；盘上 IO 全在锁外 | 叶 |
| `IndexProgressTracker`（`active: Mutex<HashSet>` + `count_tx: watch` + `first_query_done: AtomicBool`） | `lsp-core session.rs` | 跨文件索引 `$/progress` 在飞 token | 叶 |
| `mpsc` 出站 cap 64 + reply cap 8 ×1/Session | `lsp-core session.rs` | client→LS 出站三路路由 / server→client 请求回程 | — |
| `Recorder.inner: Arc<Mutex<RecorderInner>>` | `lsp-core recording.rs` | 录制文件写者 / 回放队列 | 叶 |
| `Queues: Arc<tokio::sync::Mutex<Queues>>` | `lsp-core transport/stdio.rs` | 出站优先级三队列 high/normal/bg | 跨 await（pump task） |
| `instances: Mutex<HashMap<Key, Arc<Session>>>` | `supervisor lib.rs` | 实例表 | 叶 |
| `load_gates: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>` | `supervisor lib.rs` | LS 冷启动去重（外层 map 锁即取即释；内层 per-key 跨 await） | 外层叶 / 内层跨 await |
| `last_used: Mutex<HashMap<Key, Instant>>` | `supervisor lib.rs` | reaper LRU 时间戳 | 叶 |
| `launch_exe: Mutex<HashMap<Key, PathBuf>>` | `supervisor lib.rs` | LS exe 登记（复用前复验在盘） | 叶 |
| `diag_cache`（`Arc<Mutex<HashMap>>`） | `supervisor lib.rs` | publishDiagnostics 缓存 | 叶 |
| `diag_generation: Arc<AtomicU64>` | `supervisor lib.rs` | 诊断代计数（wait_gen 探询） | 叶 |
| `pull_diag_supported: Arc<Mutex<HashMap<Key, bool>>>` | `supervisor lib.rs` | per-key pull 诊断支持标记 | 叶 |
| `version_seen: Arc<Mutex<HashMap<(PathBuf, String), bool>>>` | `supervisor lib.rs` | per-uri version 纪律标记 | 叶 |
| `diag_pending_streak: Mutex<HashMap<…>>` | `supervisor lib.rs` | 连续空 pending 降级记账 | 叶 |
| `recent_writes: Mutex<HashMap<(PathBuf, String), Instant>>` | `supervisor lib.rs` | 写后一致性窗口 | 叶 |
| `workspace_errors: Arc<Mutex<HashMap<String, String>>>` | `supervisor lib.rs` | workspace 加载失败记录 | 叶 |
| `symbol_cache`（`Arc<Mutex<HashMap>>`） | `supervisor lib.rs` | 文件指纹→符号缓存 | 叶 |
| `delta_cache: Arc<Mutex<HashMap<String, Value>>>` | `supervisor lib.rs` | 文档符号缓存 | 叶 |
| `idle_buffers_reclaim_counter: AtomicU64` | `supervisor lib.rs` | 缓冲回收节流计数 | 叶 |
| `cache_hit_counter: AtomicU64` | `supervisor lib.rs` | 符号缓存命中计数（invocation 差分） | 叶 |
| `ls_warmup: Mutex<HashMap<String, LsWarmup>>` | `supervisor lib.rs` | per-root 预热窗口 | 叶 |
| `warmup_budget_ms: AtomicU64` | `supervisor lib.rs` | 预热预算（跨请求覆盖竞态无害） | 叶 |
| `failure_memo: Mutex<HashMap<(PathBuf, String), FailureMemoEntry>>` | `supervisor lib.rs` | LS 失败 memo（TTL 短路） | 叶 |
| `timeout_streak: Mutex<HashMap<Key, u32>>` | `supervisor lib.rs` | 连续 Timeout 计数（Q2 evict+respawn 自愈链） | 叶 |
| `ROOT_SIGNAL_CACHE: LazyLock<Mutex<HashMap<PathBuf, RootSignalEntry>>>` | `supervisor lib.rs` | root 信号 TTL 缓存（static） | 叶 |
| `WRITE_GATE: tokio::sync::Mutex<()>` | `supervisor write_gate.rs` | 写事务全局串行化 | 最外层·跨 await |
| `WAITERS: AtomicUsize` | `supervisor write_gate.rs` | 排队取门请求数 | 叶 |
| `HOLDER: StdMutex<Option<&'static str>>` | `supervisor write_gate.rs` | 当前持门者标签 | 叶 |
| `OPEN_TXNS: StdMutex<Vec<(u64, PathBuf)>>` | `supervisor undo.rs` | 在飞 undo 事务登记（static） | 叶 |
| `NEXT_UID: AtomicU64` | `supervisor undo.rs` | 事务 uid 分配（static） | 叶 |
| `TOUCHED: StdMutex<Vec<(u64, TouchedFile)>>` | `supervisor undo.rs` | 恢复写文件登记（static，LS 态同步桥） | 叶 |
| `WAL_INIT: tokio::sync::Mutex<()>` | `supervisor undo.rs` | WAL 初始化串行化（static） | 跨 await |
| `TxnGuard.settled: AtomicBool` | `supervisor undo.rs` | 事务 settle 标记 | 叶 |
| `GLOBAL_LAST_ACTIVITY: LazyLock<Mutex<Instant>>` | `daemon reaper.rs` | 全局活跃时钟（idle 自杀判据） | 叶 |
| `COUNTER: AtomicU64`（fn-local static） | `daemon lockfile.rs gen_token` | lock token 计数 | 叶 |
| `loaded_ls / draining / active_project / switch_reported / in_flight`（`Arc<Mutex/_Atomic>` 字段组） | `daemon http.rs AppState` | 观测与 drain 状态（`in_flight` 由 `InFlightGuard` drop 维护，同一 Arc） | 叶 |
| `shutdown_notify: Arc<tokio::sync::Notify>` | `daemon http.rs AppState` | `/shutdown` 优雅停机门 | — |
| `invocation_count / recent_errors / recent_agents / seen / log_lines`（`Arc<Mutex/_Atomic>` 字段组） | `daemon http.rs ObsState` | invocation 观测面 | 叶 |
| `COUNTER: AtomicU64`（fn-local static） | `daemon http.rs new_invocation_id` | invocation id 计数 | 叶 |
| `RecipeProgressRelay.stop: Arc<AtomicBool>` | `serena-cli main.rs` | recipe 进度中继停止旗 | 叶 |
| `NG_COMPANION` / `HTML_COMPANION`（static `Mutex<Option<…>>`） | `ls-adapters angular.rs` | angular 伴生会话槽 | 叶 |
| `COMPANION`（static `Mutex<Option<…>>`） | `ls-adapters astro.rs` / `svelte.rs` / `vue.rs` | 伴生会话槽（各文件一枚） | 叶 |
| `PYTHON_PATH_CACHE: Mutex<Vec<(ProjectKey, String)>>` | `ls-adapters pyrefly_server.rs` | pythonPath 缓存（static） | 叶 |
| `ProjectRootSlot.inner: Mutex<SlotInner>` | `ls-adapters project_root.rs` | 会话级项目 root 槽 | 叶 |
| `SEQ: AtomicU64`（fn-local static） | `ls-adapters powershell.rs pses_session_files` | 会话文件序号 | 叶 |
| `COUNTER: AtomicU64`（fn-local static） | `ls-runtime deps.rs sha256_hex` | 临时文件序号 | 叶 |

**新增/修改锁 = 改本表**（涉语义行同步 `ARCHITECTURE.md §3.4`）**+ 加 ADR**（§9.4）。

### 7.2 内存管理

- LS 实例空闲驱逐：默认 10 min（`SERENA_LS_IDLE_EVICTION_SECS=0` 关闭）
- daemon 全局 idle 自杀：默认 15 min（`SERENA_IDLE_TIMEOUT_SECS=0` 关闭）
- **LRU + 空闲回收 = 上游 `_loaded_projects_by_root` 只进不出的对症点**（Δ 自 lsp-core 上移 supervisor）
- Arc 循环引用 Self 类 handler 闭包陷阱——`Drop` 兜底路径依赖显式回收（Job 句柄等）

## 8. 依赖政策

**权威**：`ARCHITECTURE.md §8` 依赖清单 · `Cargo.toml` workspace.lints · `rust-toolchain.toml`

### 8.1 准入门槛

新依赖前过三道门：
1. 项目内已有实现？（图谱 `codebase-memory-mcp` / `graphify`）
2. 已装依赖能解？（`extend Cargo.toml workspace.dependencies`）
3. 生态成熟方案？（`librarian` 对标）

三门都过不了才手搓。交付附对标结论（`参考 X 的 Y，差异 Z`）。

### 8.2 升级纪律

- 升 `tokio` / `axum` / `lsp-types` / `reqwest` = 同步 `rust-toolchain.toml` + CI workflow toolchain
- 升 `serde` / `clap` / `tracing` = 改 `Cargo.toml` + 跑 `cargo update` 全绿
- 全量后**未必**跑 CI 全绿（runner 预装版本漂移，记忆 `CI 首次上 GitHub Actions`）

### 8.3 不引入清单（reasoned）

| crate | 拒绝理由 |
|---|---|
| `dashmap` | std `Mutex<HashMap>` 临界区微秒级无 await，足够 |
| `parking_lot` | std `Mutex` 同上 |
| `async-lsp` | tower Service 面向 server 中间件；本项目 quirk 注入点在裸传输层，自研泵 ~400 行 |
| `tower-lsp` | 方向相反（server 端框架） |

新增到不可引清单 = 改本节 + 加 ADR。

## 9. 文档树

**权威**：`CLAUDE.md §3.7` · `docs/adr/` · `docs/plans/` · `local/`

### 9.1 项目工件（git）

| 路径 | 角色 |
|---|---|
| `CLAUDE.md` | 项目根级规则 + 九件索引 |
| `docs/constitution.md` | 本文件（九件） |
| `docs/api/contract.md` | 公开契约入口 |
| `docs/adr/NNNN-kebab.md` | ADR（NNN 序号不复用，superseded 用指针） |
| `docs/plans/<topic>-plan.md` | 项目级子计划 |
| `docs/rpc-catalog.json` | 工具目录（与 `crates/supervisor/src/catalog.rs` 内嵌 json! 双源同步） |
| `ARCHITECTURE.md` / `DESIGN.md` / `PLAN.md` / `CHANGELOG.md` / `README*.md` | 根级大文档 |

### 9.2 PM 工作材料（gitignore `local/`）

| 路径 | 角色 |
|---|---|
| `local/<topic>-report.md` | 调研 / 审计 / 验证报告 |
| `local/<topic>.md` | 草稿 / 设计文档（实施后迁 docs/） |
| `local/accept_*.sh` | PM 验收脚本 |
| `local/upstream-sync-*.md` | 上游同步轮报告 |

### 9.3 ADR 命名约定

- 格式：`NNNN-kebab-case-topic.md`（NNN = 4 位序号，0-pad）
- **序号不复用**；superseded = 沿用序号 + 写 `NNNN-superseded-by-MMMM.md` 指针
- 涉及章节修订 = 改对应权威文件（ARCHITECTURE.md 等）+ 在 ADR 加反向指针
- 首批 7 条 = `0001~0007`，对应 `ARCHITECTURE.md §9` A1–A7

### 9.4 文档变更同步

| 改动 | 必追 |
|---|---|
| 公开 API 增/改 | `docs/api/contract.md` + `docs/rpc-catalog.json` + `crates/supervisor/src/catalog.rs` 内嵌 + CHANGELOG Unreleased |
| wire 错误码增/改 | `ARCHITECTURE.md §6.3` + 新增 ADR + CHANGELOG Unreleased |
| 架构决策 | 新增 ADR + `ARCHITECTURE.md §9` 表追加指针 |
| 依赖增/改 | `Cargo.toml` + `ARCHITECTURE.md §8` + CHANGELOG Unreleased |
| 锁/原语增/改 | `constitution §7.1` 全量表 + `ARCHITECTURE.md §3.4`（涉语义行）+ 新增 ADR |

---

**修订记录**

| 日期 | bead | 摘要 |
|---|---|---|
| 2026-10-12 | `serena-rust-j0hf` | 一次性补建：CLAUDE.md + 九件 + contract + 首批 7 条 ADR + plans/ 落地子计划 + CHANGELOG Unreleased 段 |
| 2026-10-12 | `li4x`/`qt1u`/`o8ab`/`xvs7` | §5.1 配置分层收敛 3 层实况（ADR-0009）；§5.2 env 清单扩为生产全集（18 条 + 注册点列）；§7.1 锁表重写为生产代码全量登记（豁免判据 + 锁序总纲，计数矛盾归零）；§1.2/§3.1 crate 名对齐 Cargo.toml（`serena-cli`） |