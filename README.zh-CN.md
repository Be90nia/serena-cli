[English](README.md) | 简体中文

> 与英文版如有出入，以 [README.md](README.md) 为准。

# serena-rust

> 一个面向 AI agent 的 position-free（免行号）、symbol 级代码助手 —— [oraios/serena](https://github.com/oraios/serena) 的 Rust 复刻，以 CLI + skill 形态交付（无需 MCP server）。

`serena-rust` 让 LLM agent 能够按 **symbol 名**（函数、类型、字段）而非行号来导航、搜索和编辑代码库。它原生讲 LSP（Language Server Protocol），因此同一套工具可跨 Rust、TypeScript、Python、Go、C/C++、C# 和 Java 使用，无需按语言分别适配。

这是对 [Serena](https://github.com/oraios/serena) 的聚焦复刻，锚定在上游 commit `43ae0211`，持续跟进 CLI 型 agent 实际消费的功能子集（无 MCP、无 Python、除标准的每 root 单 session 模型外不做 LSP server 多路复用）。

## 为什么是 CLI + skill，而不是 MCP

上游 Serena 项目以 MCP server 形态发布：agent 通过 Model Context Protocol 连接，工具以 MCP method call 的形式到来。我们选择了不同的交付形态：

- **CLI 优先** —— 每项能力都是 `serena-cli` 的一个子命令。agent 通过 `bash`、`serena-cli shell`（JSONL 长连接会话）或管道 JSON 来驱动它。
- **Skill 优先** —— 单个 skill 文件（`skills/serena-cli/SKILL.md`）向 agent 提供黄金路径工作流与 token 纪律规则。没有协议协商，没有握手，没有 `mcp.json`。
- **单 daemon** —— 每个 workspace 一个进程，首条命令时懒启动。空闲超时 = 15 分钟。`stop-all` 干净退出（无僵尸进程）。

权衡：你失去了"MCP 自动发现"的故事；换来的是 `bash` 可调试性、JSONL 流式输出、确定性的退出码，以及 100% 可复现的 wire 格式（`ARCHITECTURE.md` 中的 9 错误码契约）。

## 覆盖范围

### CLI 命令（52 个）

读取 / 导航（7 个）：`overview` · `symbol-tree` · `read-file` · `list-dir` · `find-file` · `search` · `hover`

Symbol（8 个）：`def` · `refs` · `find-symbol` · `symbol-body` · `find-implementations` · `find-referencing-symbols` · `find-referencing-code-snippets` · `containing-symbol`

诊断（3 个）：`diagnostics`（支持 `--wait-gen N`）· pull diagnostics 兜底 · `signature-help`

编辑（10 个）：`rename-symbol` · `safe-delete-symbol` · `replace-body` · `replace-text-in-symbol` · `insert-text-before-symbol` · `insert-text-after-symbol` · `delete-text-in-symbol` · `insert-at-line` · `replace-lines` · `delete-lines`

补全（1 个）：`completion`（支持 `--limit` 与按文件后缀的 trigger 推断）

管理（4 个）：`status` · `stop-all` · `install <lang>` · `shell`（JSONL stdin/stdout 会话）

长尾（19 个）：`defining-symbol` · `edit-context` · `repo-map` · `warm` · `wait-ready` · `doctor` · `lint-shell` · `workspace-diagnostic` · `format` · `format-range` · `inlay-hint` · `document-highlight` · `folding-range` · `semantic-tokens` · `code-lens` · `document-link` · `call-hierarchy` · `type-hierarchy` · `moniker`

**位置基线约定**：接受 `line`/`col` 的命令（按位置寻址：`def`、`refs`、`hover`、`find-implementations`、`rename-symbol`、`find-referencing-*`、`containing-symbol`、`defining-symbol`、`signature-help`、`code-action`、`document-highlight`、`completion`、`format-range`、`inlay-hint`、`call-hierarchy prepare`、`type-hierarchy prepare`、`moniker`）在 CLI 表面为 **1-based** —— 内部转换为 LSP 的 0-based `Position`（`normalize_positions`，bd serena-rust-7xv）；传 `0` 属于用法错误。行范围与行编辑类命令（`read-file`、`insert-at-line`、`replace-lines`、`delete-lines`、`delete-text-in-symbol`）为 **1-based 含端点**。每条命令的 `--help` 中也各自注明了这一点。

对比上游 oraios/serena：19/19 高 ROI wrapper 全部覆盖（agent 实际会用的每个工具），外加长尾（`documentHighlight`、`codeLens`、`documentLink`、`foldingRange`、`call/type hierarchy`、`moniker`、`semanticTokens`、`inlayHint`）—— 全部落地并于 2026-09-23 验证；`document-link`/`moniker` 在不支持该能力的 LS 上返回空结果（如 rust-analyzer stable）。

### Language server（上游目录 73 个中的 11 个）

| 语言 | Server | 状态 | 备注 |
|---|---|---|---|
| Rust | tokio | ready | rustup 感知的查找链（`rustup which` → cargo bin → PATH），workspace 模式 |
| C / C++ | clangd | ready | 需要 `compile_commands.json`；传递 `--compile-commands-dir` |
| TypeScript / JS | typescript-language-server | ready | tsconfig 向上查找，ATA 关闭，npm shim 兜底 |
| Python | pyright | ready（adapter 壳） | venv 解释器检测在 `crates/ls-adapters/src/python.rs` 中为 stub |
| Go | gopls | ready（adapter 壳） | `go.work` / 多模块目录检测为 stub |
| C# | csharp-ls | ready（adapter 壳） | 决策记录：`local/csharp-ls-decision.md`（上游已迁移至 roslyn LS） |
| Java | jdtls | ready（自动下载） | 约 100MB 下载，JVM 参数模板见 `crates/ls-runtime/src/install.rs` |
| Bash | bash-language-server | ready（npm） | tree-sitter 语法诊断；ShellCheck 集成未捆绑 |
| JSON | vscode-json-languageserver | ready（npm） | schema 驱动的 hover/诊断 |
| PowerShell | PowerShellEditorServices | ready（下载） | 需要 `pwsh` 7+；内置 PSScriptAnalyzer 诊断 |
| Vue | @vue/language-server | ready（npm，hybrid） | 伴生 typescript-language-server 挂 `@vue/typescript-plugin`；语义 hover/路由已通，诊断走 tsserver 桥待接 |

11 门全部在真实 language server 上端到端冒烟验证（rust、typescript、c/cpp、python、go —— 2026-09-25；c#、java —— 2026-09-25；bash、json、powershell、vue —— 2026-09-25；见 `local/report-ls-smoke-5of7.md` / `local/report-ls-smoke-7of7.md` / 各 adapter 报告 `local/report-*-adapter.md`）。CI 7 语言冒烟脚本见 `scripts/ci_smoke.sh`。

## 安装

```bash
# 从源码安装（单二进制，除 rustup 本身外无运行时依赖）
git clone https://github.com/Be90nia/serena-cli.git
cd serena-cli
cargo install --path crates/cli --locked

# 验证
serena-cli --help
```

### 首次运行的 language server 安装

`serena-cli install <lang>` 依 `servers.toml` 逐项执行 —— 对每个受支持的语言按序尝试：

1. **PATH 探测** —— `which <bin>`，接受 `rustup which <bin>` 等。
2. **下载** —— GitHub release 资产 / npm tarball / uvx wheel，取决于该语言的 `InstallSpec`（A 类二进制、B 类 npm、C 类 uvx、D 类 dotnet tool、E 类 gem、F 类源码构建、G 类仅路径）。
3. **SHA-256 校验** —— 哈希锚定于 `local/ls-download-matrix.md` 中的上游 SolidLSP 矩阵。字节不匹配即失败关闭（fail closed），报 `LS_NOT_INSTALLED`。

手动安装的 T2 server（rustup toolchain、系统 Python 等）同样被接受 —— `install` 是便利设施，不是门槛。

## 黄金路径（8 条命令，约占 agent 流量的 90%）

```bash
# 1. 我在哪？（跳过通读整个文件）
serena-cli overview <file>

# 2. 这个 symbol 是什么？
serena-cli symbol-body <file> <symbol>
serena-cli hover <file> <line> <col>

# 3. 它在哪里被使用？定义在哪？
serena-cli refs <file> <line> <col>
serena-cli def <file> <line> <col>
serena-cli find-referencing-symbols <file> <line> <col>

# 4. 按 symbol 名编辑，而不是按行号
serena-cli replace-body <file> <symbol> --with '<new body>'
serena-cli replace-text-in-symbol <file> <symbol> '<old>' '<new>'

# 5. 验证
serena-cli diagnostics <file>
serena-cli safe-delete-symbol <file> <symbol>   # 有引用时拒绝删除
```

持续性工作请使用 `serena-cli shell`（stdin/stdout JSONL）—— 保持 daemon 温热，缓存查询亚毫秒级，无每命令的进程启动开销。

完整的 token 纪律指南见 [`skills/serena-cli/SKILL.md`](skills/serena-cli/SKILL.md)。

## 性能基线

测量于 `feature/solidlsp-phase0-1`，2026-09-16，Windows 11 / i9-10900F，无竞争的 rust-analyzer：

| 工作负载 | 延迟 |
|---|---|
| `find-symbol` daemon 温热、命中缓存 | < 1 ms |
| `overview` daemon 温热、首次命中 | 65 – 108 ms（CLI 进程启动）/ 0.88 – 0.93 ms（shell 模式） |
| `find-referencing-symbols` 温热 | ~70 ms |
| `symbol-tree <dir>` 2 文件聚合 | 0.34 s |
| `rename-symbol` 冷启动（rust fixture workspace） | 162 ms（修复前为 30s+） |
| 冷 daemon 首次 overview（rust fixture，workspace 模式） | ~5 s（无 workspace 时为 89 s、有 VS Code 竞争时为 300 s+） |

`cargo test --workspace`：49 个测试目标全绿，新增 30+ 单测。`cargo clippy --workspace --all-targets -- -D warnings`：0 错误。

## 30 秒看懂架构

7-crate Cargo workspace：

- `crates/lsp-core` —— JSON-RPC 帧封装，带 id 归一化的请求/响应 client，`ContentModified` 重试
- `crates/runtime` — 托管 LSP 进程启动，3-pump 拓扑（上游 `ls_process.py` 的镜像）
- `crates/transport` —— stdio / TCP 传输
- `crates/registry` —— `LanguageServerId` ↔ 文件扩展名映射表
- `crates/ls-adapters` —— 各 LS 的启动参数 + 就绪探测（rust-analyzer / clangd / pyright / gopls / typescript / csharp-ls / jdtls）
- `crates/supervisor` —— `Supervisor` trait + `DaemonSupervisor` 实现：per-key 负载门、session 缓存、工具分发、写门、symbol 缓存
- `crates/daemon` —— HTTP 前端（axum）、9 错误码 wire 契约、单例锁、空闲 reaper、优雅关闭
- `crates/cli` —— clap 子命令、`--json` 模式、`shell` JSONL 会话、`install`、管理命令
- `crates/ls-runtime` —— `deps.rs`（下载/SHA）、`install.rs`（自动安装流程）、`servers.toml` 适配器

架构唯一事实源是 [`ARCHITECTURE.md`](ARCHITECTURE.md)。9 个错误码（`internal`、`not_found`、`invalid_request`、`unauthorized`、`rpc_error`、`timeout`、`ls_spawn_failed`、`ls_not_installed`、`unsupported`）是 wire 契约 —— 禁改名、禁拆分、禁合并。

## 局限

| 局限 | 影响 | 何时回头解决 |
|---|---|---|
| 非 clangd LS 的 SHA-256 下载矩阵不完整 | `install` 对已校验条目可用；未校验条目回退到 PATH 探测 | 当 CI 需要封闭（hermetic）安装时 —— 从上游 SolidLSP 源码填充 `local/ls-download-matrix.md` |
| 无 monorepo 多 root 支持 | 每次 `serena-cli` 调用只作用于一个 workspace root | 增加 `additionalWorkspaceFolders`（Phase 4 延伸项） |
| 无 `$/progress` 通知缓冲 | 长时运行的工具阻塞直至完成 | 当 refactor 类工具跨过 30s 边界时 |
| 仅有 per-LS 全局超时 | 单个慢文件阻塞整个会话 | 增加 per-call `timeout_ms` 参数 |
| `typescript-language-server` 7.x 不兼容 | LSP 返回 `-32603`（无 `tsserver.js`） | 在 fixture / 用户项目中钉住 `typescript@<6` |
| typescript-language-server 在 Windows 上：npm shim 必须是 `.cmd` | 裸名 shim 是 `sh` 脚本，不可 spawn | 已强制 —— 见 Task 20 commit `c0e3cea` |

含理由的完整 Phase 6 局限表：[`local/solidlsp-development-plan.md`](local/solidlsp-development-plan.md) § Phase 6。

## Token 纪律（本项目为何存在）

读文件找行号，再读一遍确认编辑，再读一遍验证改动 —— 这是一次编辑付出了 3 次文件读取。改用按 symbol 寻址的工具后：

- `symbol-body <file> <symbol>` 直接返回函数体，无需行号腾挪
- `replace-body` 携带 `--expected-hash` 做乐观并发控制，无需往返读盘
- `search --max-results 50` 让 grep 式工作有界
- `shell`（JSONL）让 daemon 跨多条小命令保持温热复用

粗略的经验法则：同样一个 20 步任务，用 `read-file` 比用 `overview` → `symbol-body` → `replace-body` 多消耗约 10× token。skill 文件是权威参考；README 是电梯演讲。

## 测试

```bash
# 全部测试
cargo test --workspace

# 真实安装路径（需网络 + npm）
SERENA_TEST_DOWNLOAD=1 cargo test -p ls-registry --test npm_install_e2e

# Lint（CI 门槛）
cargo clippy --workspace --all-targets -- -D warnings
```

## 致谢

- [oraios/serena](https://github.com/oraios/serena) —— 本复刻所参照的原始 Python MCP server。锚定在上游 commit `43ae0211`。
- [helix-editor/helix](https://github.com/helix-editor/helix) —— `crates/supervisor/src/root.rs` 中的 `find_lsp_workspace` 算法为其直接翻译。
- Cargo 依赖树（jsonrpc-core、tokio、axum、lsp-types 等）—— 见 `Cargo.lock`。

## 许可证

[MIT](LICENSE)

## 详细数据

端到端 PR 描述、逐 commit 指标与 Round 2 backlog：[`local/PR-description.md`](local/PR-description.md)。
