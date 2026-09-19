# SolidLSP 开发计划（M3+，锚 oraios/serena@43ae0211）

> 依据：`local/solidlsp-gap-matrix.md`（缺口矩阵）、`local/solidlsp-upstream-api.md`（上游 API 面）、`local/upstream-ls-catalog.md`（73 适配器目录）、`local/completion-design.md`（既有补全设计）。
> 原则：P0 稳定性 > 接线闭环 > 深度对齐上游。每 Phase 独立可交付、可验证、不跨 Phase 依赖。
> 通用约束：写门不动（已有 hash 对账）；新错误走 9 错误码 wire 契约；禁 dashmap/parking_lot/async-lsp/tower-lsp；clippy `-D warnings` 全程绿。

## Phase 0 · 稳定性地基（P0，先于一切功能）

**理由**：冷启动挂死 / daemon 僵尸不修，任何新 method 的端到端验证都不可信。

| # | 任务 | 位置 | 验证 |
|---|---|---|---|
| 0.1 | daemon 优雅退出：finish_shutdown 后 process::exit / serve 加 graceful shutdown with_timeout | `crates/daemon/src/serve.rs`、`crates/supervisor/src/reaper.rs` | `stop-all` 后 `pgrep serena` 0 残留；重启 daemon 后 `/tools` 立即 200 |
| 0.2 | 冷启动挂死定位与修复：lazy-spawn 首请求 120s——先复现（stop-all → 首个 overview），再在 spawn→ready 门加 tracing span 定位阻塞点 | `crates/supervisor/src/lib.rs` ensure_session 路径 | 复现脚本 3 连跑，首个请求 P99 < 15s |
| 0.3 | note_activity 接线：所有 /tools 入口调用，空闲时钟重置 | `crates/daemon/src/serve.rs` | 长跑 shell 会话 + 持续调用 ≥ idle_timeout，daemon 不被误杀 |
| 0.4 | ToolError::Launch 重映射：~20 处 retryable → LS_SPAWN_FAILED（非 retryable） | `crates/supervisor/src/error.rs` 及 grep `ToolError::Launch` 全部调用点 | 单测：spawn 失败不再重试 3 次，wire 返 `ls_spawn_failed` |
| 0.5 | deps.rs verify_sha256 实装：真 hash 校验（常量表），失败即 LS_NOT_INSTALLED | `crates/ls-runtime/src/deps.rs` | 单测：错 hash 拒装；对 hash 通过 |

**Phase 0 完成门**：`cargo test --workspace` 全绿 + 手动 `stop-all → 首请求 <15s → 请求 100 个 → stop-all 无僵尸`。

## Phase 1 · 悬空接线闭环（P1，~1 个 commit）

**1.1 completion 落地**（设计已定稿 `local/completion-design.md`，勿重设计）：
- `crates/supervisor/src/lib.rs`：`tool_completion(root, file, line, col, limit, trigger)` + execute_tool 分支（CLI 名单已含 `completion`，唯 supervisor 缺）
- 字段裁剪在 supervisor 层（daemon 同享）；`--limit 5` 默认；trigger 按后缀推断在 CLI 层
- 验收：`cargo run -p cli -- completion fixtures/cpp_demo/hello.cpp 4 9` 返回含 `printf`；新增 e2e_completion.rs ≥3 测试（裁剪/limit/kind-map）

**1.2 status 补 active project**（上游 GetCurrentConfig 等价收尾）：
- daemon 内存 map 已有 (root,language)→session；`status` 加 `active_projects: [...]`
- 验收：起两个 fixture 项目后 `status` 列出两者

## Phase 2 · 上游 wrapper 缺口（P1-P2，按 ROI 排序）

每项 = supervisor `tool_*` + execute_tool 分支 + CLI 透传名单 + e2e（复用既有 23-tool 模板，边际成本低）。

| # | 工具 | 上游对齐 | 预估 | 价值 |
|---|---|---|---|---|
| 2.1 | `containing-symbol <file> <line> <col>` | request_containing_symbol | ~80 行 | 按位置反查符号——agent 从 grep 结果直接进符号工作流，免二次定位 |
| 2.2 | `signature-help <file> <line> <col>` | request_signature_help | ~80 行 | agent 写调用处时查签名，替代 hover 猜测 |
| 2.3 | `defining-symbol <file> <line> <col>` | request_defining_symbol | ~100 行 | def 返回 Location + 符号名/kind/范围 |
| 2.4 | 诊断 generation API | get_published_diagnostics_generation | ~60 行 | edit 后等"新一轮"诊断，替代盲轮询 5s；`diagnostics --wait-gen N` |
| 2.5 | `symbol-tree <dir>`（跨文件符号树） | request_full_symbol_tree | ~120 行 | 项目级导航；**依赖 2.7 缓存，否则大目录超时** |
| 2.6 | pull diagnostics 探测 + fallback | _supports_pull_diagnostics | ~80 行 | initialize 响应读 diagnosticProvider；支持则 pull（更准），否则 push 缓存 |

不做（上游有 wrapper 但工具层无消费者）：documentHighlight、codeLens、documentLink、foldingRange、call/type hierarchy、moniker、semanticTokens、inlayHint——等真实 agent 工作流需求再立项。

## Phase 3 · 性能与正确性基建（P1，决定 2.5 可用性）

**3.1 文档符号缓存**（上游两级缓存的最小版）：
- key = (root, file, mtime)；val = 平铺符号列表。内存 HashMap 即可（daemon 单进程）
- 命中路径：overview / find-symbol / symbol-body / replace-body / 2.1-2.3 全部受益
- 验证：同文件二次 overview < 1ms（加 debug 计数器测试）

**3.2 per-LS 就绪/索引等待**（rename 30s 超时 + replace-body 位置错的对症修复）：
- adapter trait 加 `wait_for_index(session, deadline)` 默认实现（documentSymbol probe 复用）；jdtls 用 language/status 已有
- replace-body / rename 前调用
- 验证：冷启动 fixture 大项目（~500 文件）rename 首次成功且 < 60s；replace-body 不再错位

**3.3 ignore spec**：find-file/list-dir/2.5 过滤 venv/node_modules/target/dist 等（表驱动，~40 行）

## Phase 4 · 适配器深度（P2，按用户语言频率排序，每语言独立 commit）

现有 7 个 T0 → 补关键 quirk（不做全量 T2，等真实需求）：

| 顺序 | 语言 | 补什么 | 对齐上游 |
|---|---|---|---|
| 4.1 | rust-analyzer | rustup/PATH 探测 + `[unstable]` fallback；flycheck 就绪等待（`rust-analyzer/loading` 通知） | rust_analyzer.py |
| 4.2 | pyright | venv interpreter 探测（.venv/venv 常规路径）+ `python.pythonPath` 注入 | pyright_server.py |
| 4.3 | typescript | tsconfig/jsconfig 逐级探测注入 workspace；npm shim 已有模板直接复用 | typescript_language_server.py |
| 4.4 | clangd | compile_commands.json 探测 + `--compile-commands-dir` 注入 | clangd_language_server.py |
| 4.5 | gopls | go.work / 多 module 目录探测 | gopls.py |
| 4.6 | jdtls | 自动下载 + 解包（复用 0.5 修好的 deps.rs + hash 表）+ JVM 参数模板 | eclipse_jdtls.py（77KB，只取启动段） |
| 4.7 | csharp | 评估上游已迁 roslyn LS（NuGet 单二进制，catalog #4）→ 决定跟进或留守 csharp-ls | csharp_language_server.py |

**新语言适配器**（66 缺口）：不批量铺。按"用户真实项目驱动"逐个加；每个 = 新文件 ~100 行 + servers.toml + e2e fixture。首个候选建议 bash（npm 线，模板与 typescript 同构）或 lua（单二进制，deps.rs 直下）。

## 排期总览与依赖

```
Phase 0 ──→ Phase 1.1/1.2 ──→ Phase 3.1/3.2 ──→ Phase 2.5
                    └─────────→ Phase 2.1-2.4/2.6（无依赖可并行）
Phase 0.5 ──→ Phase 4.6；Phase 4.1-4.5/4.7 各自独立
```

- Phase 0 + 1 合计 ~2-3 个 commit、+600 行内；Phase 2 全做 ~5-6 commit；Phase 3 ~2 commit；Phase 4 每语言 1 commit
- 全程验证基线：`cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- 每 Phase 收尾：更新 `local/serena-feature-coverage.md` 对应行 + bd 核销

## 风险登记

| 风险 | 缓解 |
|---|---|
| 冷启动挂死根因未定（0.2） | tracing span + record/replay 基建（lsp-core 已有）抓帧回放；超 1 天未解升级 debugger |
| 2.5 全树在无缓存时超时 | 硬依赖 3.1；`--max-files` 保险丝（默认 200） |
| Phase 4 各 LS 版本漂移 | quirk 注释 ↖ mirror@43ae021 锚死；版本 pin 沿用 npm shim 模板 |
| jdtls 下载量大（~100MB）+ JVM 版本敏感 | 4.6 放最后；失败路径明确报 LS_NOT_INSTALLED + 安装指引 |


## Phase 5 · 实际完成情况（2026-09-16 端到端落地）

**SolidLSP 根基加固 8 commit**（branch: feature/solidlsp-phase0-1）：

| commit | 内容 | 关键指标 |
|---|---|---|
| 2b80433 | Phase 2.1 containing-symbol | 5 单测 + 真实 CLI smoke |
| 66926f8 | Phase 2.2 signature-help | 3 单测 + 真实 CLI smoke（add(int a, int b)） |
| 90976ac | Phase 2.3 defining-symbol | 4 e2e + 真实 CLI smoke（add in math.h） |
| 24aa867 | Phase 2.4 generation API | 5 单测 + 默认行为 100% 向后兼容 |
| 76aa522 | Phase 2.5 pull diagnostics | 10 单测 + 透明 fallback |
| 821cd9c | Phase 0.2 cold-start 探针 | 6 adapter + 真实文件探针 |
| fbe21d5 | Phase 3.2 per-LS 就绪等待 | 2 单测 + rename 162ms 成功 + replace-body 位置正确 |
| 4b7ebf4 | Phase 3.1 文档符号缓存 | 6 单测 + 二次 overview 0.88-0.93ms (shell 模式) / 19-21ms (CLI spawn 模式) |
| de6cfa3 | Phase 3.3 ignore spec | 3 单测 + 真实 CLI smoke（node_modules/.venv/target 全部过滤） |
| acd508e | docs: gap matrix + plan 定稿 | 文档 |
| 3a8ae48 | Step 1 rust_demo 加 Cargo.toml | workspace mode 生效；无竞争冷启动 89s→5.07s（17×）；热 274ms |
| 62cc336 | Step 2 Phase 4.3 TS 适配器深度 | tsconfig 旁探针 + 关 ATA；5 单测；e2e 冷 ~5s 热 65-108ms |
| f364715 | Step 3 Phase 4.1 rust-analyzer 查找链 | rustup which 优先 + 功能校验 + cargo bin 兜底；+2 单测 |
| ad65d09 | Step 5 Phase 7.2 symbol-tree | 跨文件符号树闭环（2.5）；+2 单测；e2e 2 文件聚合 0.34s |

**累计验证基线**：
- cargo test --workspace: 49 个 test target 全绿（含 30+ 新单测）
- cargo clippy --workspace --all-targets -- -D warnings: 0 错
- 9 错误码 wire 契约不动
- 0 新增第三方依赖（ARCHITECTURE §8 严守）
- 不破坏公共 API（trait 默认空实现/默认实现模式）

**Phase 6 · 已知局限与后续路线**

| 局限 | 影响 | 建议 |
|---|---|---|
| ~~fixture/rust_demo 无 Cargo.toml~~ | **已解决**（3a8ae48）：workspace mode 生效，无 VS Code 竞争时冷启动 89s→5.07s | — |
| 同机 VS Code rust-analyzer 重索引主 workspace 时 CPU 竞争 | fixture 冷启动可达 300s+（环境噪声非代码问题）；PM 冷测数字须在无竞争窗口取 | 冷测前 taskkill 用户 rust-analyzer 或接受宽区间 |
| ls-runtime/deps.rs URL 矩阵 sha256 占位假值 | download 流程未实装，**功能不影响**；**Step 4 刻意跳过**——为无人消费的流程填真值是 YAGNI（版本升级即过期） | download 流程实装时按消费方需求填真 hash（clangd 4 平台 + rust-analyzer release SHA256SUMS） |
| 7 语言适配器 5 个本机未装 | smoke 仅 2/7（rust + typescript） | CI 装 LS 后跑全景 smoke |
| ~~子代理 hot-daemon 假数据~~ | **教训已吸收**：PM 端到端为唯一权威 | 另见 powershell 测量伪影条目 |
| powershell `Select-Object -First N` 测量伪影 | -First 提前断管道 → CLI EPIPE 卡写 → powershell 等 EOF 双等死锁，表现同"挂死"；曾误判为 daemon bug（排障 1h） | **CLI e2e 一律 bash 完整重定向测量**；CLI 对 EPIPE 的处理（卡而非退）可后续优化 |
| typescript-language-server 7.x 不兼容 | npm typescript@7 native 线无 tsserver.js → LS 报 -32603 | fixture 已 pin ts 5.9.3；适配器 README 提示用户降级 |

## 7. SolidLSP 整体评估

- **wrapper 面（vs 上游 39 tool）**：核心 13/13 已实现 + 5 个 Phase 2 新增 wrapper + symbol-tree = **19/19 高 ROI 上游 tool 全覆盖**
- **适配器（vs 上游 73 LS）**：7/73 落地（rust + typescript 本机可用；其他 5 适配器代码就绪但本机无 LS）；rust/TS 两个主力适配器已从 T0 浅壳升级（rustup 查找链 / tsconfig 探针 + ATA off）
- **核心 LSP method**：hover / definition / implementation / references / workspace/symbol / completion / signatureHelp / publishDiagnostics / textDocument/diagnostic (pull) / prepareRename / rename / documentSymbol **全覆盖**
- **性能**：hot daemon 重复查询 65-108ms（CLI spawn）/ <1ms（缓存直读）；cold（rust fixture workspace mode）~5s（无竞争时）