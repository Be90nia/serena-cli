# SolidLSP 加固 Round 1（feature/solidlsp-phase0-1, 29 commits）

> 锚: `oraios/serena@43ae0211`
> 范围: wrapper 缺口 + 稳定性地基 + 性能基建 + 适配器深度
> 不含: MCP（用户决定纯 CLI+skill 路线）

## 端到端指标

- `cargo test --workspace`：**49 个 test target 全绿**（含 30+ 新单测）
- `cargo clippy --workspace --all-targets -- -D warnings`：**0 错**
- 9 错误码 wire 契约不动；不破坏公共 API
- 0 新增第三方依赖（ARCHITECTURE §8 严守）
- 真实 CLI smoke 命中所有改动路径

## 改动 29 commits（按 Phase 排列）

### Phase 0 · 稳定性地基（5 commit）

| commit | 内容 | 关键指标 |
|---|---|---|
| b6525e0 | 0.impl-stable P0 三件套：cold-start 修复 + daemon 优雅退出 + note_activity + sha256 真校验 | 冷启动首个请求 120s → 15s 内；daemon 停后不僵尸；空 catch 全清 |
| 821cd9c | Phase 0.2 cold-start 探针改用真实项目文件 | 6 adapter 探针验证（虚拟 URI 兜底） |
| fbe21d5 | Phase 3.2 per-LS 就绪/索引等待 | rename 162ms 成功（修 30s 超时）+ replace-body 位置正确（修 LS-not-ready 位错） |
| 4b7ebf4 | Phase 3.1 文档符号缓存 | 二次 overview 0.88-0.93ms (shell) / 19-21ms (CLI spawn) |
| de6cfa3 | Phase 3.3 ignore spec | find-file/list-dir 过滤 21 类（venv/node_modules/target/...） |

### Phase 1 · 接线闭环（1 commit）

| commit | 内容 | 关键指标 |
|---|---|---|
| dd39f43 | Phase 1.1 completion 接线闭环（消费 `local/completion-design.md`） | 5 单测 + 真实 CLI smoke |

### Phase 2 · 上游 wrapper 缺口（7 commit）

| commit | 内容 | 关键指标 |
|---|---|---|
| 2b80433 | Phase 2.1 containing-symbol | 按位置反查符号（5 单测 + 真实 CLI smoke） |
| 66926f8 | Phase 2.2 signature-help | 按位置取函数签名（3 单测 + `add(int a, int b)` smoke） |
| 90976ac | Phase 2.3 defining-symbol | def + symbol 精化（4 e2e + `add in math.h` smoke） |
| 24aa867 | Phase 2.4 generation API | `diagnostics --wait-gen N` 按代数等（5 单测 + 100% 向后兼容） |
| 76aa522 | Phase 2.5 pull diagnostics | `textDocument/diagnostic` 探测 + 透明 fallback（10 单测） |
| ad65d09 | Phase 7.2 跨文件 symbol-tree | 项目级符号导航（2 单测 + 2 文件聚合 0.34s） |
| Phase 2.6 | （与 2.5 合并实现，见 76aa522） | — |

### Phase 4 · 适配器深度（3 commit）

| commit | 内容 | 关键指标 |
|---|---|---|
| 62cc336 | Phase 4.3 TS 适配器深度 | tsconfig 探针 + 关 ATA（5 单测；e2e 冷 ~5s 热 65-108ms） |
| f364715 | Phase 4.1 rust-analyzer 查找链升级 | rustup which 优先 + 功能校验 + cargo bin 兜底（+2 单测） |
| 3a8ae48 | rust_demo 加 Cargo.toml | workspace mode 生效；冷启动 89s→5.07s（17×）；热 274ms |

### Phase 5 · 安装机制基建（4 commit）

| commit | 内容 | 关键指标 |
|---|---|---|
| 0906842 | Task 18 下载安装基建（auto-install §3/§5） | download 流三件套 + 单测抓出 Windows rename 锁覆盖 + GitHub 域名迁移（302 Location 实测） |
| 28fac4e | Task 19 servers.toml schema + ConfigAdapter | ServerSpec→InstallSpec 映射 + override 优先级 |
| f5ca3b9 | Task 20 G 类 path_only 批量收录（14/18） | path-only 安装形态走通 |
| c0e3cea | npm/uvx 安装器机制（Task 20 B/C/D/E 类铺路） | NpmSpec/UvxSpec 子表 + NpmInstaller/UvxInstaller；npm 11.12.1 真装 bash-language-server 7.9s PASS |

### 工具 & 收尾（9 commit）

| commit | 内容 |
|---|---|
| b86b777 | M3 编辑闭环：safe-delete + 行级三件套 + 测试桩 |
| 4e43ad6 | 删除 MCP stdio 子命令（用户决定纯 CLI+skill 路线） |
| 62732c2 | rustfmt + `.codebase-memory` gitignore |
| 55a9563 | serena-cli skill：CLI 黄金路径 8 命令 + token 纪律 |
| acd508e / 9556d3f / 82cdeff / 422fa68 / ab6a656 | 文档：gap matrix + 计划 + patrol 同步实际完成 |

## wrapper 面（vs 上游 39 tool）

- **核心 13/13 全覆盖**（symbol_tools 全部 + diagnostics + replace + insert）
- **Phase 2 新增 5 个高 ROI wrapper**：containing-symbol / signature-help / defining-symbol / diagnostics generation / pull diagnostics
- **symbol-tree** 跨文件目录级
- **合计 19/19 高 ROI 上游 tool 全覆盖**（不做的 7 个 = documentHighlight / codeLens / documentLink / foldingRange / call hierarchy / type hierarchy / semanticTokens / inlayHint——等真实 agent 工作流需求再立项）

## 适配器（vs 上游 73 LS）

- **已落地 7/73**：rust-analyzer / clangd / pyright / gopls / typescript / csharp-ls / jdtls
- **本机可 smoke 2/7**：rust + typescript（其他 5 适配器代码就绪，本机无 LS）
- **主力适配器深度升级**：rust-analyzer（rustup 查找链）/ typescript（tsconfig 探针 + ATA off）
- **数据基线**（不动服务器代码，只落 local/）：
  - `local/ls-lang-extensions.md` — 71 个 LanguageServerId → 扩展名映射
  - `local/ls-download-matrix.md` — 25/26 A 类下载矩阵（含 sha256 + URL + 官方校验）

## 核心 LSP method 覆盖

✅ hover / definition / implementation / references / workspace/symbol /
   completion / signatureHelp / publishDiagnostics / textDocument/diagnostic (pull) /
   prepareRename / rename / documentSymbol

❌ 未接：textDocument/codeAction / textDocument/formatting /
   textDocument/rangeFormatting / textDocument/semanticTokens（按需立项）

## 性能基线（hot daemon）

- 重复查询：65-108ms（CLI spawn）/ <1ms（缓存直读）
- 冷启动（rust fixture workspace mode）：~5s（无 CPU 竞争时）
- symbol-tree 跨文件：2 文件聚合 0.34s

## 已知局限（详见 `local/solidlsp-development-plan.md` Phase 6）

- 同机 VS Code rust-analyzer 重索引主 workspace 时 CPU 竞争（fixture 冷启动可达 300s+，环境噪声）
- 7 个适配器 5 个本机未装，smoke 仅 2/7
- typescript-language-server 7.x 不兼容（fixture pin ts 5.9.3）
- CLI e2e 测量伪影：powershell `Select-Object -First N` 提前断管道致 EPIPE 双等死锁（教训已吸收，bash 完整重定向）

## 安装基线验证

`SERENA_TEST_DOWNLOAD=1 cargo test -p ls-registry npm_install_e2e`：
- npm 11.12.1 真装 `bash-language-server` 7.9s
- `node_modules/.bin` bin 落地断言 + 二调缓存短路断言 PASS
- 抓出真 bug：Windows npm bin 裸名是 sh 脚本不可 spawn——严格只认 `.cmd` shim（已修）

## Round 2 候选（按 ROI）

| 优先级 | 内容 | 预估 |
|---|---|---|
| P0 | Round 1 数据基线（71 语言扩展名 + 25 LS 下载矩阵） → servers.toml 逐条填实 | 每条 ~30 min |
| P1 | Phase 4.2 pyright venv 探测 / 4.4 clangd compile_commands / 4.5 gopls go.work | 各 ~80 行 |
| P1 | 新 LS：bash / lua / powershell / markdown / yaml / json（npm/path 直装） | 各 ~120 行 |
| P2 | textDocument/codeAction / formatting（统一走 lsp_types::CodeActionResponse） | ~200 行 |
| P3 | 7 语言全景 e2e（CI 装 LS） | infra |