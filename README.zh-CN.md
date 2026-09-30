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

### CLI 命令（58 个）

读取 / 导航（7 个）：`overview` · `symbol-tree` · `read-file` · `list-dir` · `find-file` · `search` · `hover`

Symbol（8 个）：`def` · `refs` · `find-symbol` · `symbol-body` · `find-implementations` · `find-referencing-symbols` · `find-referencing-code-snippets` · `containing-symbol`

诊断（3 个）：`diagnostics`（支持 `--wait-gen N`）· pull diagnostics 兜底 · `signature-help`

编辑（11 个）：`rename-symbol` · `safe-delete-symbol` · `replace-body` · `replace-text-in-symbol` · `insert-text-before-symbol` · `insert-text-after-symbol` · `delete-text-in-symbol` · `insert-at-line` · `replace-lines` · `delete-lines` · `create-text-file`

撤销 / 重做（2 个）：`undo`（`--steps N`、`--list`）· `redo` —— 事务级快照栈：每次写成功前记录旧状态；跨文件操作（如 rename 改多文件）是一个事务、整体回滚。事务中新建的文件 undo 时删除。冲突门：事务之后文件在磁盘上被改动过，undo 拒绝执行而不是覆盖。快照栈存于用户缓存目录（重启/升级不丢），上限 20 事务 / 200 MB / 30 天。

补全（1 个）：`completion`（支持 `--limit` 与按文件后缀的 trigger 推断）

管理（7 个）：`status` · `stop-all` · `install <lang>` · `ls-use <lang或id> <path>` · `ls-list` · `ls-remove <id>` · `shell`（JSONL stdin/stdout 会话）

长尾（19 个）：`defining-symbol` · `edit-context` · `repo-map` · `warm` · `wait-ready` · `doctor` · `lint-shell` · `workspace-diagnostic` · `format` · `format-range` · `inlay-hint` · `document-highlight` · `folding-range` · `semantic-tokens` · `code-lens` · `document-link` · `call-hierarchy` · `type-hierarchy` · `moniker`

**位置基线约定**：接受 `line`/`col` 的命令（按位置寻址：`def`、`refs`、`hover`、`find-implementations`、`rename-symbol`、`find-referencing-*`、`containing-symbol`、`defining-symbol`、`signature-help`、`code-action`、`document-highlight`、`completion`、`format-range`、`inlay-hint`、`call-hierarchy prepare`、`type-hierarchy prepare`、`moniker`）在 CLI 表面为 **1-based** —— 内部转换为 LSP 的 0-based `Position`（`normalize_positions`，bd serena-rust-7xv）；传 `0` 属于用法错误。行范围与行编辑类命令（`read-file`、`insert-at-line`、`replace-lines`、`delete-lines`、`delete-text-in-symbol`）为 **1-based 含端点**。每条命令的 `--help` 中也各自注明了这一点。

对比上游 oraios/serena：19/19 高 ROI wrapper 全部覆盖（agent 实际会用的每个工具），外加长尾（`documentHighlight`、`codeLens`、`documentLink`、`foldingRange`、`call/type hierarchy`、`moniker`、`semanticTokens`、`inlayHint`）—— 全部落地并于 2026-09-23 验证；`document-link`/`moniker` 在不支持该能力的 LS 上返回空结果（如 rust-analyzer stable）。

### Language server（上游目录 73 个中的 53 个，与 EN 表对齐）

| 语言 | Server | 状态 | 备注 |
|---|---|---|---|
| Rust | tokio | ready | rustup 感知的查找链（`rustup which` → cargo bin → PATH），workspace 模式 |
| C / C++ | clangd | ready | 需要 `compile_commands.json`；传递 `--compile-commands-dir` |
| TypeScript / JS | typescript-language-server | ready | tsconfig 向上查找，ATA 关闭，npm shim 兜底 |
| Python | pyright | ready（adapter 壳） | venv 解释器检测在 `crates/ls-adapters/src/python.rs` 中为 stub |
| Go | gopls | ready（adapter 壳） | `go.work` / 多模块目录检测为 stub |
| C# | csharp-ls | ready（adapter 壳） | 决策记录：`local/csharp-ls-decision.md`（上游已迁移至 roslyn LS） |
| Java | jdtls | ready（自动下载） | 约 100MB 下载，JVM 参数模板见 `crates/ls-runtime/src/install.rs` |
| Bash | bash-language-server | ready（npm） | tree-sitter 语法诊断；hover 依赖 Unix `man` 页（Windows 上为空）；ShellCheck 集成未捆绑 |
| JSON | vscode-json-languageserver | ready（npm） | schema 驱动的 hover/诊断 |
| PowerShell | PowerShellEditorServices | ready（下载） | 需要 `pwsh` 7+；内置 PSScriptAnalyzer 诊断 |
| Vue | @vue/language-server | ready（npm，hybrid） | 伴生 typescript-language-server 挂 `@vue/typescript-plugin`；语义 hover/路由已通，诊断走 tsserver 桥待接 |
| Astro | @astrojs/language-server | ready（npm，hybrid） | 伴生 typescript-language-server 挂 `@astrojs/ts-plugin`（上游 `7a296833`）；ts/js refs 路由到伴生 LS，`.astro` 语义 hover 已通 |
| Docker | docker-langserver | ready（npm） | `Dockerfile*`（含 `Dockerfile.dev` 变体）+ `*.dockerfile`；didOpen languageId 发 `dockerfile` |
| SQL | sqls | ready（下载） | `*.sql`；无 config 文件时语法层 hover/def 可用 |
| PostgreSQL | postgres-language-server（pgls） | ready（下载） | `--lang pgsql`（`.sql` 默认归 sql 条目）；无 DB 时语法诊断可用（libpg_query 本地 parser）；documentSymbol/schema hover 需 DB 连接 —— LS 能力边界 |
| MySQL | sqls | ready（下载） | `--lang mysql`；与 sql 条目同一 sqls 二进制（独立缓存目录）；hover/诊断需 DB 连接（LS 警告 "no database connection"） |
| HTML | vscode-html-language-server | ready（npm） | `*.html`/`*.htm`；文件内元素/id 符号 + mdn 驱动 hover/补全（`vscode-langservers-extracted`）；跨文件 refs/def 对 HTML 无意义（上游明示） |
| CSS | vscode-css-language-server | ready（npm） | `*.css`；属性/选择器 mdn 驱动 hover/补全；与 html 条目同一 npm 包（独立缓存目录） |
| Kotlin | Kotlin LSP（JetBrains managed intellij-server） | ready（下载） | `*.kt`/`*.kts`；同文件 hover/def + documentSymbol 可用（managed LSP 钉上游 `263.4702.0`，sha 校验，包内捆绑 JBR 免系统 JDK）；跨文件 def/refs 需 LS 工程导入（`build.gradle.kts`/`pom.xml` 标记，会自动下载 Gradle 发行包）—— LS 能力边界 |
| Dart | Dart SDK analysis server（`dart language-server`） | ready（下载） | `*.dart`；整 SDK 下载（206 MiB zip，sha 校验；钉上游 `3.7.1`）；裸目录即可 hover/def/refs/documentSymbol（pubspec.yaml 自动识别） |
| YAML | yaml-language-server | ready（npm） | `*.yaml`/`*.yml`；schema 驱动 hover/补全/诊断（schemastore）；无 schema 时语法诊断可用 |
| Markdown | marksman | ready（下载） | `*.md`/`*.markdown`；标题 documentSymbol/workspace symbol；链接 def/refs/hover 需项目根可识别（git 仓库或 `.marksman.toml`）—— marksman 侧项目识别，无标记散目录降级为单文件辅助 |
| Svelte | svelte-language-server（svelteserver） | wired（npm）；CI 冒烟待跑 | `*.svelte`；hybrid：主 svelteserver + 伴生 typescript-language-server 挂 `typescript-svelte-plugin`（上游 `7a296833`）；ts/js 语义路由到伴生；`.svelte` 预打开在伴生上使 plugin 见到完整 TS 图 |
| Deno | `deno lsp`（Deno CLI 内置） | wired（下载）；CI 冒烟待跑 | 仅 `--lang deno` 显式路由——TS 家族扩展名归 typescript 门（上游正因该重叠标注 deno experimental）；GitHub release zip（钉 `2.9.7`，`assets[].digest` sha 校验）；入口是 `deno lsp` 子命令而非 `--stdio` flag；注入 init options `{enable, lint}`（裸 deno lsp 默认不启用） |
| Sass | some-sass-language-server | wired（npm）；CI 冒烟待跑 | `--lang sass`（servers.toml 条目 id `scss` = 缓存目录键）；`*.sass`/`*.scss`（`.css` 归 css 门）；didOpen languageId `scss` + `.sass` per-file 覆盖；somesass init options + `workspace/configuration` 配置片照抄上游 `7a296833` |
| PHP | intelephense（冒烟门）/ phpactor | wired（npm）；CI 冒烟待跑 | `*.php`；语言路由 `php` 归存量 phpactor download 条目（既有避撞设计），冒烟门按 entry id 装/路由 `--lang intelephense`（phpantom 先例）；didOpen languageId 显式映射 `intelephense`→`php`（官方口径）；intelephense 纯 npm 零运行时，phpactor PHAR 需 PHP 8.1+ |
| Lua | lua-language-server（LuaLS） | wired（下载）；CI 冒烟待跑 | `*.lua`；GitHub release tar.gz 钉 `3.15.0`（sha 校验）；首轮 CI 已绿（run 36528495148）——本批补 Rust 侧路由闭环（LanguageId/EXT_TABLE） |
| Scala | metals | wired（path_only）；CI 冒烟待跑 | `*.scala`；上游 PATH 有 metals 则用之，否则 coursier bootstrap（钉 `metals_2.13:1.6.4` = 上游 `DEFAULT_METALS_VERSION`）；servers.toml 条目为 path_only（GitHub release 无预编译资产，v1.6.9 2026-09-29 实查）；CI 门用 coursier bootstrap 装到 `/usr/local/bin`（JDK 11+，runner 预装 17）；fixture 不带 build 文件——T0 不应答 import 构建提示（上游应答），无 build 走 metals standalone PC；已登记 `fallback_assert` hover 探针 |
| Swift | sourcekit-lsp | wired（path_only）；CI 冒烟 = PLATFORM SKIP | `*.swift`；随 Xcode/Swift 工具链分发（path_only 条目，无可装资产）——ubuntu runner 无 Swift 工具链，门记 PLATFORM SKIP（SKIP 账本第 8 条，PM 批准 7→8，硬顶 8）；若未来 runner 出 Swift 工具链或 sourcekit-ls 独立发行即转真门；Rust 侧接线完备（LanguageId/EXT_TABLE/doctor） |
| Fortran | fortls | wired（uvx）；CI 冒烟已过 | `*.f90`/`*.f95`/`*.f03`/`*.f08`/`*.f`/`*.for`/`*.fpp`；pip `fortls` 3.2.2 经 uvx——矩阵门上轮已 PASS，本批补 Rust 侧路由闭环（LanguageId/EXT_TABLE，每门交付模板） |
| Pascal | pasls | wired（下载）；CI 冒烟 = BUDGET SKIP | `*.pas`/`*.pp`；预编译 v0.2.0（条目 win/macOS 资产）；完整功能需 FPC 工具链（PP/FPCDIR）——apt fpc ≈400MB 超单门预算（存量 PM 裁决）；unix 接线 + FPC 前置归本批后续 |
| Haskell | haskell-language-server-wrapper | wired（path_only）；CI 冒烟待跑 | `*.hs`/`*.lhs`；entry exec 修复 `--lsp`（裸 wrapper 打印 usage 即退——terraform `serve` 同类）；HLS 需配对 GHC——选 bindist 2.9.0.1 因 2.15 弃 GHC 9.4（ubuntu-24.04 apt 上限）；bare 文件走 default cradle 调 apt ghc |
| Groovy | （无托管服务器——上游要求用户自备 JAR） | 不入矩阵（angular 先例——不存在可安装的 LS；接线保留随时秒接） | `*.groovy`/`*.gvy`；上游 `groovy_language_server.py` 硬性要求 `ls_jar_path`——npm 无 LS 包、GroovyLanguageServer GitHub releases 为 `[]`、apt 无 LS：四条安装路线穷尽；LanguageId/扩展名已接线，servers.toml 不建条目（angular/java 先例） |
| OCaml | ocamllsp | wired（path_only）；CI 冒烟待跑 | `*.ml`/`*.mli`；opam `ocaml-lsp-server`（switch 挂系统编译器免工具链源码构建）；上游经 `opam exec` 取 ocamllsp 路径后裸直启 = path_only 语义；OCaml 5.1.0 不兼容（上游明示） |
| Erlang | erlang_ls | wired（path_only）；CI 冒烟待跑 | `*.erl`/`*.hrl`；entry exec 修复 `--transport stdio`（默认 transport 是 TCP——stdio 客户端会挂死）；按 OTP 版本配对的预编译 escript（ubuntu-24.04 apt = OTP 25.3 → `-25` tarball）；需 erlang runtime 在 PATH |
| Perl | Perl::LanguageServer（经 perl） | wired（path_only）；CI 冒烟待跑 | `*.pl`/`*.pm`/`*.t`；launch argv 逐字镜像（`perl -MPerl::LanguageServer -e Perl::LanguageServer::run`）；cpanm 安装；上游应答 `workspace/configuration`——T0 走 lsp-core 默认 null 成功应答，文件过滤回落 LS 默认值 |
| R | languageserver（经 R） | wired（path_only）；CI 冒烟待跑 | `*.r`/`*.rmd`/`*.rnw`；launch argv 逐字镜像（`R --vanilla --quiet --slave -e ... languageserver::run()`）；CRAN 安装源码编译（runner 自带 gcc） |
| Crystal | crystalline | wired（path_only）；CI 冒烟待跑 | `*.cr`；musl 静态单二进制（免 crystal 工具链）；documentSymbol 上游注释 "works reliably"；条目保持 path_only（legacy 锚 canary）——CI 门 curl 钉版 v0.20.0 release URL（sha 校验） |
| Zig | zls | wired（下载）；CI 冒烟待跑 | `*.zig`/`*.zon`；条目 path_only → download 升级（六平台 pin，`assets[].digest` sha 校验，0.16.0）；zls 与同 minor zig 严格配对——门装 ziglang.org 0.16.0 工具链 tarball（官方 index.json sha 校验）并 symlink 入 PATH |
| TOML | taplo | ready（下载） | `*.toml`；单文件 gzip 二进制（钉 `0.10.0`，sha 对上游内嵌校验和验证）；表/键 documentSymbols；存在 schema 关联时 schema 驱动 hover/诊断（taplo 特性） |
| Terraform | terraform-ls | ready（下载） | `*.tf`/`*.tfvars`；块/资源 documentSymbols（钉 `0.36.5`，HashiCorp releases sha 校验；以 `terraform-ls serve` 启动）；上游要求 PATH 有 `terraform` CLI 才有模块特性—— |
| Cue | `cue lsp`（cue CLI 内置） | ready（下载） | `*.cue`；cue CLI 把 LSP 藏在隐藏 `lsp` 子命令后（v0.16.1 实证）；字段/包 documentSymbols |
| Nix | nixd | source build | `*.nix`；`git clone` + `nix build` 安装——需要 Nix 工具链（上游无预编译资产，与上游 adapter 同约束）；构建后 attribute documentSymbols |
| Ansible | ansible-language-server | wired（npm）；CI 冒烟待跑 | `--lang ansible`（`.yaml`/`.yml` 归 YAML 门）；hover/补全/诊断可用；**无 documentSymbol**——上游拒绝（vscode-ansible#601 NOT_PLANNED），smoke 走 fallback |
| Rego | regal | wired（下载）；CI 冒烟待跑 | `*.rego`；单文件二进制（钉 `0.42.0`，sha 对 GitHub release `assets[].digest` 校验）；`regal language-server` 启动；documentSymbol/hover/def/诊断 |
| Nextflow | Nextflow language server | wired（下载）；CI 冒烟待跑 | `*.nf`；fat JAR（钉 `26.04.3`，sha 校验；需 PATH 有 JDK ≥17）；outline/def/refs/hover/诊断；无 npm 包（registry 404）——上游即 JAR 发行 |
| Gleam | `gleam lsp`（Gleam CLI 内置） | wired（path_only）；CI 冒烟待跑 | `*.gleam`；LS 内置在自包含 gleam 编译器二进制里——门装钉版 v1.18.1 musl release（`assets[].digest` sha 校验）；条目 exec = `gleam lsp` 子命令（deno 先例）；上游等首批 `$/progress` 依赖解析——T0 无此等待门（工具层超时承担），bare fixture 无 `gle.toml` 走单文件分析，已登记 hover 降级探针 |
| QML | qmlls（Qt 6 官方） | wired（path_only）；CI 冒烟待跑 | `*.qml`；随 Qt 6 分发——apt `qt6-declarative-dev-tools` 装 `/usr/bin/qmlls6`（Debian install 清单实锚），门 symlink 成 `qmlls` 对齐条目单名（上游 which 顺序 qmlls6→qmlls）；ubuntu-24.04 钉 Qt 6.4.2（qmlls 初版 LSP，功能面窄）——documentSymbol 降级 hover 已登记；更新版 Qt 需交互式官方安装器（CI 不可脚本化） |
| Lean 4 | `lean --server`（Lean 工具链内置） | wired（path_only）；CI 冒烟待跑 | `*.lean`；语言名 `lean` 路由到条目 id `lean4`（zls/zig 双名先例）；门装钉版 v4.34.1 全工具链 tarball（580 MB tar.zst，`assets[].digest` sha 校验，免 elan）；standalone fixture 走基础符号（def/theorem documentSymbol）——上游 lake env LEAN_PATH 注入（跨文件语义）未镜像 |
| Julia | LanguageServer.jl（经 julia） | wired（path_only）；CI 冒烟待跑 | `*.jl`；launch argv 逐字镜像（`julia --startup-file=no --history-file=no -e 'using LanguageServer; runserver()'`）——尾参 repo_root 省略：T0 spawn cwd = 项目根，runserver 的 env 回落链含 pwd；需 julia runtime + `Pkg.add("LanguageServer")`（门内预装；budget 1200s 上限估待 CI 实测校准）；`workspace/configuration` 回落 lsp-core null 应答（perl 先例），lint 设置保持 LS 默认 |
| Wolfram | WolframKernel LSPServer paclet | LICENSE SKIP 候选；待 PM 裁决 | `*.wl`/`*.nb`；LS 仅随 Mathematica 13.0+ / Wolfram Engine 12.1+ 分发（许可安装，无可脚本化 CI 渠道——上游 `wolfram_language_server.py` 发现链全依赖本机 Wolfram 安装）；条目 `[servers.wolfram]` 保留为持有安装用户的 PATH 探测面（haskell_ls 运行时自备语义）；出现免许可可脚本化渠道即转真门 |
| GDScript（Godot） | （无独立服务器——上游经 TCP 连已运行编辑器） | 不入矩阵（angular 先例——LS 是连已运行编辑器的 TCP 客户端，无 stdio T0 形态） | `*.gd`；上游 `godot_language_server.py` 是连已运行 Godot 编辑器 ：6008 的 TCP 客户端，从不启动进程——我们 transport 仅 stdio；LanguageId/扩展名已接线，servers.toml 不建条目（angular/groovy 先例）；lsp-core 出 TCP transport + 编辑器编排即转真门 |
| mSL（mIRC） | （上游 LS = serena 仓库内嵌 pygls 脚本） | 不入矩阵（angular 先例——LS 是连已运行编辑器的 TCP 客户端，无 stdio T0 形态） | `*.mrc`；上游 launch `[python, msl_lsp_server.py]`——脚本随 serena 仓库分发，非独立发行、我们 Rust 二进制不随包；W5 契约的 "metal" 系本门误读（73 门对账无 metal 门——msl = mIRC 脚本语言）；LanguageId/扩展名已接线，servers.toml 不建条目；msl_lsp 独立发行（pip 包/独立仓库）即转真门 |

20 门全部在真实 language server 上端到端冒烟验证（rust、typescript、c/cpp、python、go —— 2026-09-25；c#、java —— 2026-09-25；bash、json、powershell、vue —— 2026-09-25；astro —— 2026-09-28；docker、sql —— 2026-09-28；postgresql、mysql —— 2026-09-28；yaml、markdown —— 2026-09-28；kotlin、dart、html、css —— 2026-09-28；见 `local/report-ls-smoke-5of7.md` / `local/report-ls-smoke-7of7.md` / 各 adapter 报告 `local/report-*-adapter.md`）。单语言 CI 冒烟脚本见 `scripts/smoke_one.sh`（矩阵清单 `scripts/smoke_langs.toml`，周任务 workflow `.github/workflows/ls-smoke.yml`）。

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

### 使用你已装好的 LS（自定义路径）

机器上已有的 LS 不必重复下载。`ls-use` 把 `servers.toml` 条目改指你自己的二进制，写入 `%APPDATA%/serena/external-servers.toml`（Unix `~/.config/serena/`）：

```bash
serena-cli ls-use python D:/tools/jedi-ls.exe    # 已知语言/id：继承内置 languages/extensions/exec，仅换二进制
serena-cli ls-use mydsl D:/tools/mydsl-ls.exe --lang mydsl --ext .mydsl   # 全新语言
serena-cli ls-use --list                         # 列注册条目 + 每语言生效来源
serena-cli ls-use --remove mydsl                 # 移除注册（其余内容与注释逐字节保留）
serena-cli ls-list                               # 全量清单：内置 × installed / external-override / not-installed + 可释放字节
serena-cli ls-remove marksman                    # 只卸载 serena 托管缓存（不碰 PATH/生态安装）
```

注册在 daemon 重启后生效（`serena-cli stop-all` 或等空闲超时）。条目是普通 TOML，可手改；`ls-use` 只重写自己的 `[servers.<id>]` 块。

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
