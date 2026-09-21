# Serena 上游工具 vs serena-rust 覆盖对照（v5，2026-09-21：47 子命令 / 11 适配器全面核实，SolidLSP 层缺口闭合）

> 锚：oraios/serena@43ae0211（2026-09-20 上游同步后基线 c4dc91a7，见 `local/upstream-sync-2026-09-20.md`），`src/serena/tools/` 7 个工具文件逐类核实（jetbrains_tools 为 IDE 桥，不适用 CLI，不计）。
> 旧版"16%"/"56%"均过时：旧版把 `find_referencing_code_snippets` / `replace_text_in_symbol` 误记为上游工具（它们不在 43ae0211 工具表内，属我们的 Δ 增强），且上游实为 **39 工具**非 32。
> 本项目 CLI 面核实（v5）：`crates/cli/src/main.rs` Cmd 枚举 **47 子命令**（42 工具 + status/stop-all/install/shell/doctor）+ `--project/--direct/--daemon/--lang/--request-timeout/--index-timeout` flags；supervisor `execute_tool` 路由 42 个工具分支。

## 状态图例

✅ 直接等价 ｜ ◐ 部分等价（组合可达） ｜ 🔁 用法替代（0 CLI，更省 token） ｜ ✂ 刻意不做 ｜ ❌ 真缺口（待实现）

## symbol_tools（13）

| 上游工具 | 状态 | serena-rust |
|---|---|---|
| GetSymbolsOverview | ✅ | `overview <file>` |
| FindSymbol | ✅ | `find-symbol <query>`；`include_body` 拆为独立 `symbol-body`（position-free，Δ） |
| FindReferencingSymbols | ✅ | `refs` / `find-referencing-symbols` |
| FindImplementations | ✅ | `find-implementations` |
| FindDeclaration | ✅ | `def` |
| GetDiagnosticsForFile | ✅ | `diagnostics <file>` |
| GetDiagnosticsForSymbol（optional） | ◐ | symbol-body 定位 + `diagnostics` 行范围组合 |
| ReplaceSymbolBody | ✅ | `replace-body`（写门+hash 对账） |
| InsertAfterSymbol / InsertBeforeSymbol | ✅ | `insert-text-after/before-symbol` |
| RenameSymbol | ✅ | `rename-symbol`（prepareRename 前置+倒序 apply+写门） |
| SafeDeleteSymbol | ✅ | `safe-delete-symbol`（写门+引用计数检查，M3 已落地） |
| RestartLanguageServer | ◐ | `stop-all` + 懒重生等价；不单做子命令 |

## file_tools（10）

| 上游工具 | 状态 | serena-rust |
|---|---|---|
| ReadFile | ✅ | `read-file [start] [end]` |
| ListDir | ✅ | `list-dir` |
| FindFile | ✅ | `find-file <glob>`（限深 5） |
| SearchForPattern | ✅ | `search [--path-glob] [--max-results]` |
| CreateTextFile | 🔁 | agent 原生 write 覆盖（新文件无写门冲突面，不 CLI 化） |
| ReplaceContent（文内 regex 替换） | ◐ | `replace-text-in-symbol` 只覆盖符号体内；**全文行级 regex 替换缺** |
| DeleteLines / ReplaceLines / InsertAtLine | ✅ | `delete-lines` / `replace-lines` / `insert-at-line`（全文行级，走写门；M3 已落地） |
| ReplaceInFiles（跨文件批量替换） | ◐ | `search --json` + agent 原生批量编辑组合；不单做 CLI |

## memory_tools（6）→ 🔁 全部原生文件替代

| 上游 | 状态 | 替代 |
|---|---|---|
| Write/Read/List/Delete/Rename/Edit Memory | 🔁 | `.serena/memories/*.md` 纯文件；agent 用**自己的文件工具**直读直写，0 个 CLI 子命令。Rename 的 `mem:` 引用更新 = skill 一条纪律（改名时 grep 引用） |

## workflow_tools（3）→ 🔁 全部 skill 文本替代

| 上游 | 状态 | 替代 |
|---|---|---|
| InitialInstructions | 🔁 | **skill 文件本体**（CLI+skill 方案的核心替代物） |
| Onboarding | 🔁 | skill 惯例：首见项目 → 写 `.serena/memories/project_overview.md` |
| SerenaInfo | 🔁 | skill/README 按需读（同为 context-efficiency 设计） |

## cmd/config/query/config 其余（7）

| 上游 | 状态 | serena-rust |
|---|---|---|
| ExecuteShellCommand | ✅（形态不同） | `shell` JSONL 长连接（多命令共享 LS 热缓存；agent 亦可原生 shell） |
| ActivateProject | ◐ | `--project` flag（等价单项目激活） |
| GetCurrentConfig | ✅ | `status`（uptime / pid / loaded LS / active_project / draining） |
| OpenDashboard | ✂ | 无 web dashboard |
| RemoveProject | ✂ | 无项目注册表 |
| ListQueryableProjects / QueryProject | ✂→🔁 | 跨项目 = 对另一目录直接跑 `serena-cli --project <dir>`，skill 一句话 |

## Δ 我们多出（上游没有）

- **position-free 符号编辑全家桶**：`hover`、`symbol-body`（按名取体）、`find-referencing-code-snippets`（引用+上下文行）、`replace-text-in-symbol` / `delete-text-in-symbol`（符号体内行级切片）
- **LSP wrapper 批**（上游未包成 agent 工具）：`symbol-tree`（跨文件符号树）、`completion`（AI 裁剪版）、`containing-symbol` / `defining-symbol`（位置反查符号）、`signature-help`、`code-action`、`format` / `format-range`、`inlay-hint`、`document-highlight`、`folding-range`、`semantic-tokens`、`code-lens`、`document-link`、`call-hierarchy` / `type-hierarchy`、`moniker`、`workspace-diagnostic`、`diagnostics --wait-gen`
- **形态与管理**：daemon `--direct/--daemon/shell` 多形态、`install`（配置驱动安装：sha 门 + `--allow-unsigned-sha` 人工通道）、`doctor`（环境体检 5 类，--json/--fix）、`--request-timeout` / `--index-timeout` 双轨超时

## 总账（39 工具）

| 状态 | 数量 | 明细 |
|---|---|---|
| ✅ 直接等价 | **20** | overview/find-symbol/refs/find-impl/def/diagnostics/replace-body/insert×2/rename/**safe-delete**/read-file/list-dir/find-file/search/shell/**行级三件套×3**/**status(GetCurrentConfig)** |
| ◐ 部分等价 | **4** | diag-for-symbol、ReplaceContent、ReplaceInFiles、ActivateProject |
| 🔁 用法替代 | **9** | memory×6 + workflow×3 |
| ✂ 刻意不做 | **4** | dashboard/remove-project/query×2 |
| ❌ 真缺口 | **1** | RestartLS 独立子命令（stop-all+懒重生已等价，刻意不单做） |
| 合计 | 39 | 复刻覆盖 = (20+4+9)/39 ≈ **85%**；剔除刻意不做 = 33/35 ≈ **94%** |

**工具层编辑闭环已闭合**（safe-delete + 行级三件套 M3 落地）；SolidLSP 层 v4 缺口亦已闭合（见下节）。

> **验收战役结论**（2026-09-20 双验收：单测全绿 + 端到端实跑，含 rust 语义层死根因定位与修复）：**`local/acceptance/ACCEPTANCE-REPORT.md`**；文档-代码符合度审计（本文档 v4 过时之出处）：**`local/acceptance/design-conformance.md`**。

---

## SolidLSP 层（ls-runtime / lsp-core / supervisor 深度，2026-09-21 复核：v4 缺口全部闭合）

上游 `src/solidlsp/`（ls.py 3256 行 / 55 公开方法 / 73 适配器）对照本项目的**引擎层**账本。v4 所列缺口逐项核销：

- ~~悬空接线~~ ✅ `completion` 已接线：CLI 子命令（AI-friendly 字段裁剪 + trigger 自动推断）+ supervisor `tool_completion` 分支
- ~~wrapper 缺口~~ ✅ 六项全落地：signature-help、containing-symbol、defining-symbol、跨文件 `symbol-tree`（子命令）、诊断 generation（`diagnostics --wait-gen`）、pull diagnostics（`workspace-diagnostic` + per-LS `pull_diag_supported` 能力探测）
- ~~基建缺口~~ ✅ 文档符号缓存（`doc_symbol_cache_key`，mtime 键控，Phase 3.1）、per-LS 索引等待（adapter `wait_for_index`，写类工具入口探针，Phase 3.2）、ignore spec、sha256 门（`UnsignedRefused` 拒装；真值锚 GitHub API assets[].digest）
- **适配器**：73 落地 **11** 个（clangd / pyright / basedpyright / jedi / ty / pyre / typescript / gopls / csharp-ls / jdtls / rust-analyzer）；深度仍浅（对上游 11-77KB 的逐 quirk 抄译是持续工程，见 Phase 4）

开发计划（Phase 0-3 已完成 14/14；剩 Phase 4 适配器深度）：**`local/solidlsp-development-plan.md`**（权威）；缺口矩阵：**`local/solidlsp-gap-matrix.md`**；上游 API 面：**`local/solidlsp-upstream-api.md`**。

---

## Token 高效适配设计（CLI+skill vs MCP——为何换用法）

### 1. 常驻成本对比（大头）

| | MCP（上游） | CLI+skill（我们） |
|---|---|---|
| 常驻 system prompt | 39 个 tool schema + docstring ≈ **6-15K token 常驻** | **0**（CLI 不进 prompt） |
| 一次性加载 | — | skill 文件 ≤2K token |
| 每次调用包装 | JSON-RPC tool call 往返 | 裸 stdout JSON |
| 长会话 N 任务 | 常驻 × 全程 | skill 只付一次 |

### 2. Skill 文件 = InitialInstructions 替代（写什么）

黄金路径 8 命令覆盖 90% 场景，附 token 纪律：
1. `overview <file>` 先看结构——**禁止整读文件**
2. `find-symbol <query>` 定位
3. `symbol-body <file> <symbol>` 按名取体（免 read 定位往返）
4. `replace-body` / `replace-text-in-symbol` 改
5. `refs` / `find-referencing-code-snippets` 验影响面
6. `search` 跨文件
7. `diagnostics` 收尾
8. 失败读 `error.message` 即可，**不探索 --help**

### 3. 用法替代原则：文件能力不 CLI 化

- **Memory**：`.serena/memories/*.md` + agent 原生文件工具（写/读/列全免）——upstream 6 工具 = 我们 0 子命令，且无 JSON 包装 token
- **CreateTextFile**：原生 write
- **QueryProject**：换个 `--project` 目录再跑
- **CLI 只留需要 LSP/写门语义的操作**（符号级读 + 编辑写门 + rename/safe-delete）

### 4. 输出纪律（已就绪/低成本补）

- 结构化 JSON 单行输出、`--max-results` 上限（search 已有）
- 符号寻址（by name）代替行号寻址：省"先 read 找行号"的整个往返
- `shell` JSONL 长连接：多次操作免重复进程 banner/LS 冷启动
- daemon 常驻：LS 热缓存让 find/refs 毫秒级——"持续使用"的延迟底座

### 5. 待做排期（v5 修订，2026-09-21）

1. ~~SafeDelete~~ ✅ M3 落地（`safe-delete-symbol`）
2. ~~行级三件套~~ ✅ M3 落地（`insert-at-line` / `replace-lines` / `delete-lines`）
3. ~~skill 文件~~ ✅ `skills/serena-cli/` 已建
4. ~~status 补 active project 名~~ ✅ `/status` 返回 `active_project`（最近请求的 project_root）
5. ~~SolidLSP 层 Phase 0-3~~ ✅ 14/14（completion 接线 / wrapper 批 / 文档符号缓存 / 索引等待 / ignore spec / sha256 门）
6. **适配器深度（Phase 4）**：11 落地但仍浅壳，逐语言 quirk 抄译 → `local/solidlsp-development-plan.md`
7. 外部 LS 变体补全：fatou(Julia)/solidity 的 EXT_TABLE+LanguageId 变体（`local/external-ls-registration-design.md` §6 草案）
8. RestartLS 独立子命令：不做（stop-all+懒重生已等价）
