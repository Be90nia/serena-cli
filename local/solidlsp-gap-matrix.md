# SolidLSP 缺口矩阵（本项目 vs 上游 @43ae0211）

> 锚: `oraios/serena@43ae0211`。上游 API 面见 `local/solidlsp-upstream-api.md`；适配器安装机制见 `local/upstream-ls-catalog.md`。
> 本项目事实基线：lsp-core 通用 `Session::request(method, params, timeout)` 字符串驱动（无类型化 facade）；supervisor 23 个 tool 分支；CLI 透传 25 名单。
> 核对日期：2026-09-15。

## 1. 上游 wrapped method × 本项目状态（语言无关——lsp-core 透传，单列）

图例：✅ 已实现 ｜ ◐ 部分等价 ｜ ❌ 缺 ｜ ✂ 上游有 wrapper 但 serena 工具层不用（暂不做）｜ 🔁 Δ 替代

| 上游 SolidLanguageServer 方法 | 底层 LSP method | 本项目状态 | 备注 |
|---|---|---|---|
| `request_document_symbols` | documentSymbol | ✅ | overview / find-symbol / symbol-body 全走它 |
| `request_full_symbol_tree`（跨文件全树） | documentSymbol ×N | ❌ | M2 有 find-symbol（workspace/symbol）；全项目符号树缺 |
| `request_dir_overview` / `request_document_overview` | documentSymbol 过滤 | ◐ | overview 只做单文件全量平铺；dir 级聚合缺 |
| `request_hover` | hover | ✅ | `hover` |
| `request_definition` | definition | ✅ | `def` |
| `request_implementation` | implementation | ✅ | `find-implementations` |
| `request_references` | references | ✅ | `refs` / `find-referencing-code-snippets` |
| `request_referencing_symbols`（精化为符号） | references + documentSymbol | ◐ | `find-referencing-symbols` 已有符号精化 |
| `request_containing_symbol`（位置→包含符号） | documentSymbol 定位 | ❌ | symbol-body 按名查；按行号反查缺 |
| `request_defining_symbol`（位置→定义符号） | definition + 符号精化 | ❌ | `def` 只回 Location |
| `request_implementing_symbols`（精化） | implementation + 精化 | ◐ | find-implementations 只回 Location |
| `request_symbol_at_location` | documentSymbol 定位 | ◐ | `symbol-body` 按名（Δ position-free）；按行号缺 |
| `request_workspace_symbol` | workspace/symbol | ✅ | find-symbol 用 |
| `request_completions` | completion | ❌ **悬空** | 设计文档 `local/completion-design.md` 已写（~300 行，验收齐）；CLI 透传名单已含 `completion`，**supervisor 无 `tool_completion` 分支**——接线断裂，调用必 404 |
| `request_signature_help` | signatureHelp | ❌ | 上游有 wrapper；agent 调 API 时有用 |
| `request_rename_symbol_edit` | prepareRename + rename | ✅ | prepareRename 前置已做 |
| `request_text_document_diagnostics`（pull 3.17） | textDocument/diagnostic | ❌ | 我们只有 push 缓存+轮询；clangd 对 pull 返 -32601（已注释确认） |
| `request_published_text_document_diagnostics` + generation | publishDiagnostics push | ◐ | diag_cache 有；generation API（等待新一轮）缺——轮询 5s 上限靠盲等 |
| `apply_text_edits_to_file` | — | ✅ | 写门（hash 对账 + 原子写）|
| `insert_text_at_position` / `delete_text_between_positions` | — | ✅ | 行级三件套 + 符号级 insert/delete |
| `open_file` / LSPFileBuffer mtime 门 | didOpen/didChange | ✅ | lsp-core docsync `ensure_open` |
| 文档符号两级缓存（raw+parsed，fingerprint/version） | — | ❌ | **每次 tool 调用重跑 documentSymbol**；daemon 热路径无缓存 |
| `content_hash`（md5 缓存） | — | ❌ | docsync 只有 mtime |
| `set_request_timeout`（全局） | — | ◐ | 每调用传参；无全局/每 LS 配置 |
| `get_ignore_spec` / `is_ignored_dirname`（venv/node_modules） | — | ❌ | find-file 限深 5 缓解；LS 自身过滤 |
| `_get_wait_time_for_cross_file_referencing`（per-LS 索引等待） | — | ❌ | **与 rename 30s 超时 / replace-body 就绪 bug 同根** |
| additional workspace folders（monorepo） | — | ❌ | 单 workspace_folder |
| ContentModified 重试白名单 | — | ✅ | lsp-core opt-in；documentSymbol + workspace/symbol 已注册 |

## 2. 语言适配器深度（T0 浅壳 vs 上游 T2）

上游每适配器含：依赖自动下载（27 类单二进制 / 14 npm / 5 uvx…全目录见 catalog）、启动 quirk、就绪信号特判、初始化参数特调。我们 T0：PATH 查找 + documentSymbol 就绪探针 + 少量 initialize_patches。

| 语言 | 上游体量 | 本项目 | 就绪探针 | 关键 quirk 缺口 |
|---|---|---|---|---|
| C/C++ (clangd) | 20KB | 173 行 | documentSymbol ≤30s | compile_commands.json 探测/注入缺 |
| Python (pyright) | 11KB | 101 行 | documentSymbol ≤30s | venv/interpreter 探测缺；basedpyright/ty 等 4 变体缺 |
| Go (gopls) | 15KB | 80 行 | documentSymbol ≤30s | go.mod 多模块、GOFLAGS 缺 |
| TS (typescript-ls) | 28KB | 111 行 | documentSymbol ≤30s | tsconfig/jsconfig 探测、npm shim（已有 3 层 fix 模板）、monorepo workspace 缺 |
| C# (csharp-ls) | 34KB | 84 行 | documentSymbol ≤60s | 上游已换 roslyn LS（NuGet 下载）；我们仍是 csharp-ls 旧线 |
| Java (jdtls) | 77KB | 104 行 | language/status ✅（特判已有） | jdtls 下载/解包/JVM 参数/heap 缺——用户须自装并配 PATH |
| Rust (rust-analyzer) | 38KB | 87 行 | documentSymbol ≤30s | rustup 探测、cargo target 目录排除、flycheck 等待缺 |

依赖自动下载基建：`ls-runtime/deps.rs` 存在但 **verify_sha256 只验格式 + 占位假值**（审计已录）——扩适配器前必须先修。

## 3. 已知 P0/P1 bug（本层相关）

| 级别 | bug | 根因位置 | 状态 |
|---|---|---|---|
| P0 | daemon shutdown 后僵尸（永不退出） | supervisor/daemon finish_shutdown 无退出机制、serve.rs 无 graceful shutdown | 待修 |
| P0 | 冷启动后首个工具请求挂死 120s | 根因未定（lazy-spawn 路径） | 待修 |
| P1 | replace-body LS 未就绪时位置错 | 就绪等待缺失（同 per-LS 索引等待缺口） | 待修 |
| P1 | rename-symbol 30s 超时 | 大项目首次索引 > TOOL_TIMEOUT | 待修 |
| P1 | note_activity 无调用点（空闲时钟冻结 → 误杀热 daemon） | daemon 空闲计时 | 待修 |
| P1 | ToolError::Launch 兜底 ~20 处误映射 retryable | 错误分类 | 待修 |

## 4. 总账

- 上游 wrapper 面价值高的缺口：**completion（接线悬空）、signatureHelp、containing/defining symbol、跨文件符号树、诊断 generation、文档符号缓存、per-LS 就绪/索引等待**
- 适配器缺口：**66 个未落地**；既有 7 个全为 T0 浅壳
- infra 缺口：**sha256 校验假值、ignore spec、additional workspace、全局 timeout**
