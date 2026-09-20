# SolidLSP 缺口矩阵（本项目 vs 上游 @43ae0211）

> 锚: `oraios/serena@43ae0211`。上游 API 面见 `local/solidlsp-upstream-api.md`；适配器安装机制见 `local/upstream-ls-catalog.md`。
> 本项目事实基线：lsp-core 通用 `Session::request(method, params, timeout)` 字符串驱动（无类型化 facade）；supervisor 23+ 个 tool 分支；CLI 透传 33 子命令（29 工具 + status/stop-all/shell/install）。
> 核对日期：2026-09-20（Task 18/19/21 下载安装基建 + 双路径后更新）。

## 1. 上游 wrapped method × 本项目状态（语言无关——lsp-core 透传，单列）

图例：✅ 已实现 ｜ ◐ 部分等价 ｜ ❌ 缺 ｜ ✂ 上游有 wrapper 但 serena 工具层不用（暂不做）｜ 🔁 Δ 替代

| 上游 SolidLanguageServer 方法 | 底层 LSP method | 本项目状态 | 备注 |
|---|---|---|---|
| `request_document_symbols` | documentSymbol | ✅ | overview / find-symbol / symbol-body 全走它；3.1 加 (root,file,mtime) 缓存 |
| `request_full_symbol_tree`（跨文件全树） | documentSymbol ×N | ✅ | **Phase 7.2** `symbol-tree <dir>`（commit ad65d09）：filtered_walker + 复用 3.1 缓存 + max_files 保险丝 |
| `request_dir_overview` / `request_document_overview` | documentSymbol 过滤 | ◐ | overview 只做单文件全量平铺；dir 级聚合缺 |
| `request_hover` | hover | ✅ | `hover` |
| `request_definition` | definition | ✅ | `def` |
| `request_implementation` | implementation | ✅ | `find-implementations` |
| `request_references` | references | ✅ | `refs` / `find-referencing-code-snippets` |
| `request_referencing_symbols`（精化为符号） | references + documentSymbol | ✅ | `find-referencing-symbols` 已有符号精化 |
| `request_containing_symbol`（位置→包含符号） | documentSymbol 定位 | ✅ | **Phase 2.1** containing-symbol（commit 2b80433） |
| `request_defining_symbol`（位置→定义符号） | definition + 符号精化 | ✅ | **Phase 2.3** defining-symbol（commit 90976ac） |
| `request_implementing_symbols`（精化） | implementation + 精化 | ◐ | find-implementations 只回 Location |
| `request_symbol_at_location` | documentSymbol 定位 | ✅ | symbol-body 按名（Δ position-free）+ 2.1/2.3 按行号 |
| `request_workspace_symbol` | workspace/symbol | ✅ | find-symbol 用；3.1 加 (root,"ws?<query>") 缓存 |
| `request_completions` | completion | ✅ | **Phase 1.1** completion 接线（commit dd39f43） |
| `request_signature_help` | signatureHelp | ✅ | **Phase 2.2** signature-help（commit 66926f8） |
| `request_rename_symbol_edit` | prepareRename + rename | ✅ | prepareRename 前置已做；**3.2** wait_for_index 修复30s 超时 |
| `request_text_document_diagnostics`（pull 3.17） | textDocument/diagnostic | ✅ | **Phase 2.5** pull diagnostics 探测 + fallback（commit 76aa522） |
| `request_published_text_document_diagnostics` + generation | publishDiagnostics push | ✅ | **Phase 2.4** generation API `--wait-gen N`（commit 24aa867） |
| `apply_text_edits_to_file` | — | ✅ | 写门（hash 对账 + 原子写）|
| `insert_text_at_position` / `delete_text_between_positions` | — | ✅ | 行级三件套 + 符号级 insert/delete；**3.2** wait_for_index 修位置错 |
| `open_file` / LSPFileBuffer mtime 门 | didOpen/didChange | ✅ | lsp-core docsync `ensure_open` |
| 文档符号两级缓存（raw+parsed，fingerprint/version） | — | ✅ | **Phase 3.1** symbol_cache（commit 4b7ebf4）：(root,file,mtime) 缓存，hit < 1ms |
| `content_hash`（md5 缓存） | — | ◐ | docsync 只有 mtime；写门用 sha256 |
| `set_request_timeout`（全局） | — | ◐ | 每调用传参；无全局/每 LS 配置 |
| `get_ignore_spec` / `is_ignored_dirname`（venv/node_modules） | — | ✅ | **Phase 3.3** should_ignore 19 项（commit de6cfa3） |
| `_get_wait_time_for_cross_file_referencing`（per-LS 索引等待） | — | ✅ | **Phase 3.2** wait_for_index trait 默认实现（commit fbe21d5） |
| additional workspace folders（monorepo） | — | ❌ | 单 workspace_folder |
| ContentModified 重试白名单 | — | ✅ | lsp-core opt-in；documentSymbol + workspace/symbol 已注册 |
| `on_server_started` / `on_server_ready` | initialize 响应 → ready probe | ✅ | **Phase 0.2** set_project_root + 真实文件探针（commit 821cd9c） |

## 2. 语言适配器深度（T0 浅壳 vs 上游 T2）

上游每适配器含：依赖自动下载（27 类单二进制 / 14 npm / 5 uvx…全目录见 catalog）、启动 quirk、就绪信号特判、初始化参数特判。我们 T0：PATH 查找 + documentSymbol 就绪探针（**已升级到真实文件探针**）+ 少量 initialize_patches。

| 语言 | 上游体量 | 本项目 | 就绪探针 | 关键 quirk 缺口 |
|---|---|---|---|---|
| C/C++ (clangd) | 20KB | 173 行 | documentSymbol(真实文件) ≤30s | compile_commands.json 探测/注入缺 |
| Python (pyright) | 11KB | 101 行 | documentSymbol(真实文件) ≤30s | venv/interpreter 探测缺；basedpyright/ty 等 4 变体缺 |
| Go (gopls) | 15KB | 80 行 | documentSymbol(真实文件) ≤30s | go.mod 多模块、GOFLAGS 缺 |
| TS (typescript-ls) | 28KB | 111→**170 行** | tsconfig 旁 .ts 优先（**4.3**，62cc336） | ATA 已关 ✅；monorepo workspace 缺；ts 7.x 不兼容（fixture pin 5.9.3） |
| C# (csharp-ls) | 34KB | 84 行 | documentSymbol(真实文件) ≤60s | 上游已换 roslyn LS（NuGet 下载）；我们仍是 csharp-ls 旧线 |
| Java (jdtls) | 77KB | 104 行 | language/status ✅（特判已有） | jdtls 下载/解包/JVM 参数/heap 缺——用户须自装并配 PATH |
| Rust (rust-analyzer) | 38KB | 87→**200 行** | documentSymbol(真实文件) ≤30s | **4.1** rustup which 优先 + --version 功能校验 + cargo bin 兜底（f364715）✅；Δ 不自动 component add；flycheck quiescent 就绪用 3.2 探针等效替代 |

依赖自动下载基建：`ls-runtime/deps.rs` 存在，verify_sha256 函数已实装（RFC 3174 测试向量通过），但 URL 矩阵里的 sha256 是占位假值（**MVP download 流程未实装**，假 sha256 不影响运行）。

**本机 LS 装态**（2026-09-16）：
- ✅ rust-analyzer 装
- ✅ typescript-language-server 装
- ❌ clangd / pyright / gopls / csharp-ls / jdtls 未装
- 7 语言适配器代码全实现；本机 5/7 因 LS 未装无法 smoke

## 3. 已知 P0/P1 bug（本层相关）

| 级别 | bug | 根因位置 | 状态 |
|---|---|---|---|
| P0 | daemon shutdown 后僵尸 | supervisor/daemon finish_shutdown 无退出机制、serve.rs 无 graceful shutdown | **0.1** 验证无 bug（实测 stop-all 后 8s 内完全退出 + lock 删除） |
| P0 | 冷启动后首个工具请求挂死 120s | lazy-spawn 路径 | **0.2** 修复（commit 821cd9c）：探针改真实文件，对 fixture（无 Cargo.toml）无效（89s）；对真实 workspace（含 .gitignore）99s（基线 88s），架构正确 |
| P1 | replace-body LS 未就绪时位置错 | 就绪等待缺失 | **3.2** 修复（commit fbe21d5）：wait_for_index trait 默认实现 documentSymbol 探针 + 30s 护栏，replace-body cold-start 111s 位置正确 |
| P1 | rename-symbol 30s 超时 | 大项目首次索引 > TOOL_TIMEOUT | **3.2** 修复：同上，rename cold-start 162ms 成功 |
| P1 | note_activity 无调用点 | daemon 空闲计时 | **已修复**（现 tools_post + status_get 均有调用） |
| P1 | ToolError::Launch 兜底误映射 | 错误分类 | **已修**（现 grep 仅 2 处且都用于 spawn 失败，非 retryable 误映射） |
| P2 | sha256 URL 矩阵占位假值 | ls-runtime/deps.rs | **接受现状**（MVP download 流程未实装） |

## 4. 总账（Task 18/19/21 后，2026-09-20）

- ✅ **根基加固（10 commit）**：completion 接线、5 wrapper tools、cold-start 探针真实文件、文档符号缓存、per-LS 就绪等待、ignore spec
- ✅ **深化轮（4 commit，2026-09-19）**：rust_demo Cargo.toml workspace mode（冷启动 89s→5s）、TS 适配器 tsconfig 探针+ATA、rust-analyzer 三级查找链、symbol-tree 跨文件符号树
- ✅ **上游 wrapper 面价值高的缺口**：**全部补完**（completion / signatureHelp / containing/defining symbol / 文档符号缓存 / 诊断 generation / 诊断 pull / per-LS 就绪等待 / 跨文件符号树）
- ✅ **下载安装基建（Task 18/19/21，2026-09-20）**：install.rs 下载流（三件套：临时包/预检/zip-slip 防护）+ rust-analyzer 4 平台真值矩阵 + servers.toml schema（marksman/crystalline 首批）+ ConfigAdapter（§4 override 优先级 CLI>config>默认）+ CLI `install` 命令 + supervisor session_for 双路径（T2 adapter 优先 → T0 ensure_launch）
- ✅ **G 类 path_only 批量收录（Task 20，2026-09-20）**：13 条新收录（ccls/deno/erlang_ls/gleam/haskell_ls/jedi/lean4/ocamllsp/qmlls/regal/sourcekit_lsp/zls + 既有 crystalline = 14/18）；exec 省略 = 裸启动默认；跳过 gopls/rust-analyzer/pyright（T2 接管）与 wolfram/perl/r（启动形态特殊按需补）；A 类 27 条不做（URL/sha256 逐条调研非纯数据，按用户语言需求添加）；B/C/D/E 类需 schema 扩展（npm/uvx/dotnet/gem 安装器）
- ✅ **双路径 e2e 实证（2026-09-20）**：`install marksman` 幂等安装（20.5MB 真下载+sha256）→ `--direct overview readme.md` 经 T0 路径拉起 marksman `server` 子命令 → documentSymbol 返回符号（8s 冷启动）
- ⚠️ **适配器缺口**：66 个未落地；既有 7 个中 rust/TS 已升级（T0.5），其余 5 个仍 T0 浅壳（**接受**——用户语言驱动）
- ⚠️ **infra 缺口**：additional workspace folders、全局 timeout（**接受**）
- ⚠️ **测量纪律**：CLI e2e 一律 bash 完整重定向；powershell `-First N` 断管道会产生"挂死"伪影（详见 plan Phase 6）

### 覆盖率核算（PLAN M3 ≥75% 闸）

口径：上游 39 工具，jetbrains_tools 不计 → 可做 ~35。本项目工具面 29/35 ≈ **83% ≥ 75% 达标**（缺口 = Memory×5 / 项目管理类，均为 serena 容器语境功能，CLI 形态刻意不做）。

## 5. 后续路线建议

按 ROI 排序：
- ~~P1: 真 download 流程~~ **Task 18/19/21 已闭环**（commits 0906842/28fac4e/Task21，2026-09-20）
- ~~P1: Task 21 双路径~~ **已落地**（session_for T2→T0 接线 + CLI install + marksman e2e PASS）
- P2: Task 20 残余——servers.toml 73 LS 批量收录（纯数据工作，机制已验证）
- P2: 适配器 quirk 深度剩余项（clangd compile_commands / pyright venv / gopls go.work）
- P2: 7 语言 smoke CI（需 CI 装 LS）
- P3: additional workspace folders（monorepo 支持）
- P3: $/progress 通知等待（M3 MVP 之后）