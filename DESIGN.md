# serena-rust 设计文档（v0.2）

> 目标：用 Rust 复刻 solidlsp + 薄 CLI，产出单个 `serena-cli.exe`。
> skill 调用时自动拉起常驻 daemon，空闲自动退出。主形态 CLI（工具描述不占模型上下文）；daemon 可选暴露 MCP endpoint，供 MCP-only 客户端复用。
> 复刻来源：oraios/serena（MIT），只抄代码和 quirk，不复用其 Python 运行时。

> **v0.2 变更**（2026-09-01）：三路评审完成——① momus 审计 verdict「打回」：4 Critical + 8 Important，全部修复/处置（见 §10）；② oracle 架构细化产出 `ARCHITECTURE.md` v0.1（565 行：7 crate 布局、8 张经渲染验证的 mermaid 图、锁权威表、错误码表、上游逐行追溯锚 `43ae0211`）；③ librarian 竞品调研 10 家（无 Rust 全量复刻先例，立项前提安全），融入机制 Top 10（见 §9）。

## 1. 背景与约束

- solidlsp 是全世界唯一独立发布的多语言 LSP 编排层（fork 自 microsoft/multilspy），4-5 万行 Python：核心客户端 `ls.py` 156KB + **73 个**语言服务器适配器（`src/solidlsp/language_servers/`）。
- Rust 生态没有同体量库：`async-lsp` 是协议底盘（helix-lsp 建在它上面），`tower-lsp` 是 server 端框架（方向相反），无编排层。
- 性能事实：查询延迟瓶颈在语言服务器进程（clangd/jdtls 本身），壳的语言只影响 daemon 冷启动。Rust 的收益 = 单 exe 分发 + daemon 冷启动 <100ms（Python import serena 全家桶约 2-3s）+ 无 Python 环境依赖。
- 平台：Windows 优先（用户环境 win32 x64），兼顾 linux/macOS。

## 2. 目标 / 非目标

**目标**
1. 单文件 `serena-cli.exe`（含 daemon），零运行时依赖。

2. CLI + skill 使用形态：`serena-cli <tool> [args]`，文本输出。
3. 生命周期自治：调用时 lazy-spawn daemon → 常驻 → 空闲 N 分钟自杀。
4. 语言覆盖路线图覆盖上游全部 73 个适配器（分层推进，见 §5）。

**非目标**
- 不以 MCP stdio 为主形态（每客户端一套进程树，多代理场景内存/冷启动翻倍）；MCP 仅作为 daemon 的可选前端端点（见 §3.2）。
- 不复刻 serena 的 agent 层（memories、prompts、tool 编排）——只复刻 solidlsp 检索/编辑能力 + CLI 壳。
- 不承诺与上游 100% 行为一致：追 quirk 修复，不追功能演进（见 §7 风险）。

## 3. 总体架构

```
serena-cli.exe（Rust，单二进制，双模式）
│
├─ CLI 模式（默认）
│   ├─ clap 解析 <tool> [args]
│   ├─ 探测 daemon 端口（127.0.0.1:7860 + lock file 校验）
│   │    ├─ 活着 → HTTP POST 转发，打印结果
│   │    └─ 死了 → spawn 自身 `--daemon`，轮询就绪后转发
│   └─ 输出：纯文本（agent 友好），--json 可选
│
└─ daemon 模式（--daemon）
    ├─ HTTP 服务（axum），端点：
    │    POST /tools/{name}   执行工具
    │    GET  /status          就绪/已加载语言服务器列表
    │    POST /shutdown        主动停机
    ├─ LspSupervisor：按项目×语言缓存 LanguageServer 实例（仿 ProjectServer 的 per-root 加载锁）
    ├─ IdleReaper：无请求 N 分钟（默认 15）→ 优雅停机退出
    └─ LSP 层（见 §4）
```

- **daemon 单例仲裁（C1 修复）**：lock file 以 `create_new` 原子创建竞速——胜者成为 daemon 并写入 (端口, boot 时间戳, pid, token)；败者**不 spawn**，轮询胜者端口直至就绪（≤3s，超时报错可重试；LS 冷启动等待由工具请求超时 300s 覆盖）。判活 = TCP 探活（connect 超时 500ms）+ boot 时间戳比对（防 pid 复用）+ token 校验；残留无响应 → 清理重启。
- **LS 孤儿回收（C2 修复）**：Windows 全部 LS 子进程挂入 Job Object（win32job，KILL_ON_JOB_CLOSE）——daemon 崩溃/被强杀时内核保证子进程树全灭；Unix 用 prctl PDEATHSIG 等价物。
- **idle 停机协议（I8）**：停机进 ShutdownDraining——拒绝新请求（503 + Retry-After）→ 等 in-flight 完成（上限 10s）→ 逐 LS shutdown（单个 5s 超时转 kill）→ 删 lock file → 退出。
- **管理命令与开发辅助**（抄 cli-lsp-client / lsp-devtools）：`serena-cli status / list / stop-all / logs`；`--record` 录制 JSON-RPC 流量（对拍上游 quirk）；`--direct` 不经 HTTP 直连 LS（M0 冒烟）。**日志宿**：daemon stdout/stderr 已重定向 NULL（I5），tracing 输出改走文件层 `%LOCALAPPDATA%/serena/daemon.log`（`RUST_LOG` 门控不变，`logs` tail 该文件）；`--record` 落 `%LOCALAPPDATA%/serena/records/<时间戳>.jsonl`。

### 3.1 并发模型（多子代理共享一个 daemon）

场景：omp/其他 harness 的多个子代理同时调用同一 daemon、同一项目。

- **读操作**（find-symbol/refs/overview/hover/def）：并发放行；LSP 请求本身支持并发，锁粒度 = LSP session。
- **写操作**（replace-body/diagnostics 触发重同步）：全局互斥队列，串行执行——两个代理并发改同一文件是数据事故。
- **document sync 归 daemon 独占持有**：didOpen/didChange/didClose 由 daemon 统一发送；代理侧永远不直接碰 LSP 连接。写操作完成后 daemon 负责通知 + 失效对应文件的 symbol 缓存，其他代理下次查询自动看到新状态。
- **内存闸门（语言服务器实例调度）**：内存大头是语言服务器进程（clangd/pyright 200MB-1GB、jdtls 可达 2GB），壳本身 ~20MB 可忽略。策略：
  1. 按需启动——项目注册不启动 LS，该语言文件首次被查询才拉起；
  2. LRU 上限——同时最多 `max_loaded_ls`（默认 3）个 LS 实例，超限驱逐最久未用者；
  3. 空闲卸载——单个 LS 实例 10 分钟无请求即 shutdown 释放；
  4. 单进程多项目——禁止一项目一进程；上游 ProjectServer 的项目缓存只进不出（`_loaded_projects_by_root` 无卸载），本设计显式改进此点。
  5. 边界——LS 是外部第三方进程（clangd=C++、pyright=TS/Node、gopls=Go、jdtls=Java/JVM），内部不可修改；内存控制仅限三手段：选型（同语言可换轻量实现，如 pyright→jedi）、启动参数（jdtls `-Xmx`、clangd `--limit-results`，参数表从 solidlsp 抄）、生命周期调度（上述 1-3）。solidlsp 本身也只做这三件事，从不改 LS 内部。
- **LspSupervisor 锁**（仿上游 ProjectServer）：per-project 加载锁（防重复冷启动）+ 工具执行的项目上下文锁（防串台）。
- **LS 实例不跨项目共享**：LSP `initialize` 绑定 `rootUri`，索引/编译数据库/配置全部 per-root（clangd 认 `compile_commands.json`、gopls 认 `go.mod`、pyright 认 `pyrightconfig.json`）。Supervisor 实例键 = `(project_root, language)`：同键唯一实例（防重复冷启动），同项目多语言=多实例，跨项目各建各的。`workspaceFolders` 多根仅限同一构建体系的 monorepo 场景。VS Code 开两个文件夹 = 两个 clangd 进程，即行业默认。切项目后旧 LS 靠 LRU/空闲卸载回收。
- **锁体系（C4 修复）**：全项目锁清单与获取顺序的权威表在 ARCHITECTURE.md §3.4。铁律：写门（全局 tokio::Mutex，FIFO）→ session 锁 → LRU/加载锁，禁逆序；LRU 驱逐与空闲卸载前必须确认实例 in-flight 计数为 0（引用计数门）。
- **replace-body 一致性链路（C3 修复）**：① 锁内原子——读最新 body 与写入在同一写门临界区，杜绝 stale 覆盖；② mtime 对账——操作前比对磁盘 mtime 与 daemon 持有的 didOpen buffer（抄 ls.py LSPFileBuffer@43ae021），外部修改先重同步；③ 写后读回 diff，不符则从写前临时副本回滚并报错。
- **实例键规范化（I6 修复）**：project_root 入键前经 dunce 规范化 + Windows 大小写折叠 + 去尾分隔符，杜绝同项目多实例。

### 3.2 前端协议：CLI 与 MCP 共存

MCP/CLI 只是前端，底层 LSP 编排同一套。axum daemon 可同时暴露：

- HTTP `/tools/*` —— `serena-cli` 用（agent 上下文零工具描述开销，主形态）
- MCP endpoint（streamable-http）—— 给 MCP-only 客户端（如 Claude Code）复用同一 daemon，后期加，约 1 天

不采用 MCP stdio 作为主形态：每客户端一套进程树（serena + 各语言服务器），多子代理场景内存与冷启动开销翻倍。

## 4. crate 划分

| crate | 内容 | 对应上游 |
|---|---|---|
| `ls-runtime` | 进程 spawn（Job Object）/ 依赖下载器（zed 式 (Os,Arch)→URL 矩阵 + sha256 + 解压 + PATH 回退）/ stderr 日志分级 | `ls_process.py` + `dependency_provider.py` |
| `lsp-core` | JSON-RPC 编解码、Transport trait（stdio/TCP，Godot 走 TCP）、initialize 握手、OffsetEncoding 协商、pending 表/超时/ContentModified 重试、server→client 分发、FileBuffer docsync | `ls.py` + `lsp_protocol_handler/` |
| `ls-adapters` | adapter trait + T2 手写模块 | `language_servers/*.py` 大户 |
| `ls-registry` | `servers.toml`（schema 抄 nvim-lspconfig：cmd/filetypes/root_markers 嵌套/settings/init_options/capabilities/initialize_params/required_root_patterns）+ 语言解析 + 根发现算法（抄 helix find_lsp_workspace） | 简单适配器 + `ls_config.py` |
| `supervisor` | 实例池 (root,lang) / LRU / 写门 / **符号解析组合层**（I3：上游在未复刻的 agent 层 symbol_tools.py，需自建） | `project_server.py` 形态 + `symbol_tools.py` 语义 |
| `daemon` | axum HTTP / lock 仲裁 / IdleReaper / 管理命令 / 可选 MCP endpoint | `project_server.py` |
| `cli`（唯一 bin） | clap / lazy-spawn / 转发 / 文本输出 | — |

**适配器 trait（v0.2 定稿）**：v0.1 草稿经审计 I1（缺 server→client 应答钩子、notification 订阅、过程式依赖管理——jdtls 的 DependencyProvider 实证为版本 pin + 多平台矩阵 + JDK 探测，非静态清单）后修订，定稿见 **ARCHITECTURE.md §4.1-4.2**：`launch_info` async 化、`request_hooks()` 值对象、`ServerSpec(TOML) → ConfigAdapter` 实现 T0 零代码。M0 按修订版定型，防 T2 阶段返工。

- T0 服务器 = `servers.toml` 一条记录 → 运行时生成的默认 adapter 实现，**零 Rust 代码**。
- T1 = 配置 + 下载清单字段。
- T2 = 手写模块，quirk 从上游对应 `.py` **逐函数抄译**（MIT 允许，注明来源）。

## 5. 73 个适配器的分档策略（核心成本决策）

已抽查两个端点样本：

| 档 | 判据 | 估计数量 | 实现方式 | 单个成本 |
|---|---|---|---|---|
| T0 纯模板 | ~150 行：PATH 找可执行 + 标准 initialize + 空 handler（样本：crystal 150 行、erlang、fortran、zls、r、regal、texlab、json、yaml…） | ~35 | `servers.toml` 条目 | 分钟级 |
| T1 配置+下载 | 模板 + RuntimeDependency 下载清单（uniform 模式） | ~20 | 配置表 + 下载字段 | <1h/个 |
| T2 quirk 大户 | >20KB 专用逻辑（eclipse_jdtls 77KB、al 47KB、vue 42KB、pascal 40KB、rust_analyzer 38KB、scala 35KB、csharp 34KB、svelte 35KB、kotlin 31KB、typescript 28KB、matlab 24KB、omnisharp 20KB、clangd 20KB…） | ~15 | 手写 Rust 模块 | **1-4 天/个；>30KB 的 9 个按 3-5 天**（I2 修正：jdtls 1432 行含过程式依赖管理 + JDK 测试环境） |

**推进顺序：M0 先做用户实际语言（clangd），M1-M2 用 T0/T1 把覆盖率推到 ~75%，M3 按使用频率逐个攻坚 T2。** "全部"是路线图终点，不是开工前提。

## 6. 工具面（CLI 暴露）

仿 serena 符号工具，全部 position-free（按符号名直达，这是相对内置 lsp 的核心价值）：

| 命令 | 语义 | LSP 底层 |
|---|---|---|
| `overview <file>` | 文件大纲 | documentSymbol |
| `find-symbol <pattern>` | 全库符号检索（regex/精确） | workspaceSymbol + documentSymbol |
| `symbol-body <symbol>` | 按名取函数/类完整源码 | documentSymbol range + 文件切片 |
| `refs <symbol>` | 引用列表 | references |
| `def <symbol>` | 定义跳转 | definition |
| `impls <symbol>` | 实现列表 | implementation（能力可用时） |
| `hover <symbol>` | 类型/文档 | hover |
| `replace-body <symbol> [--stdin]` | 符号体整体替换 | range textEdit（读回验证后写） |
| `diagnostics [file]` | 诊断 | publishDiagnostics 缓存 |

- `symbol-body`/`replace-body` 是对内置行号工具的抽象升级，属本项目独有价值。
- 编辑类命令默认带回显 diff + 原子写（临时文件 + rename），防 LSP range 过期。
- **符号解析组合层（I3）**：`find-symbol` 的 regex/模糊编排不在上游 solidlsp（在其未复刻的 agent 层 symbol_tools.py，34.5KB），由 supervisor 自建：精确名走 workspace/symbol；regex/模糊走全库 documentSymbol 遍历（虚拟 didOpen + ref-count，抄 ls.py open_file_buffers@43ae021）。**遍历有界**：文件数上限 2000 或软超时 5s，超出即停并在输出尾部标注 `…truncated`，防大仓库把 LS 灌爆。实现在 `supervisor` crate。
- **符号缓存失效（I4）**：统一为 fingerprint 每次校验（抄 ls.py@43ae021 `_raw_document_symbols_cache_fingerprint`），不做事件驱动失效——上游实证此路可靠。

## 7. 里程碑

| 里程碑 | 内容 | 预估（全职口径） |
|---|---|---|
| M0 打通 | `lsp-core` + clangd（PATH 直连，`--direct` 冒烟）跑通 def/refs/overview；trait 按 ARCHITECTURE §4.1 定型；clangd 基础 quirk 随 M0 做：OffsetEncoding 协商、stderr 日志分级、启动参数、UTF 偏移换算；fixture 配 `compile_flags.txt` 保证 clangd 稳定解析——compile_commands 生成/转换等重 quirk 随 M3 clangd 完整化（I7 修正：quirk 不整体推 M3，按依赖分摊） | 3-4 天 |
| M1 产品壳 | daemon+CLI 双模式（lock 原子竞速 + Job Object + ShutdownDraining）、idle 自杀、管理命令面、`symbol-body`/`replace-body`（含 §3.1 一致性链路）、exe CI | +1 周 |
| M2 广覆盖 | 下载器框架（zed 式矩阵）+ `servers.toml` 收录 T0/T1（~55 个）+ 冒烟基建（multilspy 式每语言一测试文件） | +1-2 周 |
| M3 攻坚 | T2 剩余大户按使用频率逐个（jdtls/rust_analyzer/vue/al…）；每个 1-4 天，>30KB 按 3-5 天 | 持续 |
| M4 深化 | symbols 磁盘缓存（fingerprint 校验）、`--record` 回放对拍、--json 输出 | +1 周 |

## 8. 风险与对策

| 风险 | 对策 |
|---|---|
| 上游移动靶：serena 活跃开发，73 适配器持续变动 | 定位为"快照复刻 + 安全修复跟随"；配置表提供 `sync` 子命令 diff 上游 `ls_config.py` 提示漂移 |
| quirk 翻译失真（抄得来守不住） | 每个 T2 移植必须对照上游对应 pytest 用例，测试先行；quirk 代码注释标 `// mirrored from clangd_language_server.py@<commit>` |
| Windows 差异：上游依赖 oslex/pythonnet 处理路径/引号 | M0 就在 Windows 上开发（用户环境即目标环境），shell 引用用 `to_win32_args` 类等价物 |
| T2 总量失控 | M3 严格按需：没被用到的 T2 语言停留在"配置表 + 降级警告"，不为覆盖率为覆盖率 |
| daemon 被防火墙拦 | 默认绑 127.0.0.1 + Windows 防火墙例外提示；备选 named pipe |
| Windows 文件竞争（I5） | 原子写 rename 遇共享冲突 → 重试退避（5 次 × 50ms）；spawn daemon 关闭句柄继承（DETACHED + CREATE_NEW_PROCESS_GROUP + stdin/stdout 重定向 NULL），防 CLI 退出连带 daemon 死亡 |

## 9. 调研融入机制（Top 10，来源见 §9.1）

| # | 机制 | 落点 | 来源 |
|---|---|---|---|
| 1 | `servers.toml` schema 定稿（cmd/filetypes/root_markers 嵌套/settings/init_options/capabilities/initialize_params/desc） | ls-registry | nvim-lspconfig + multilspy |
| 2 | initialize_params 声明式（solidlsp 丢掉的 multilspy 遗产，捡回） | ls-registry | multilspy initialize_params.json |
| 3 | required_root_patterns：根缺关键文件拒绝启动 LS（防随机目录拉起 2GB jdtls） | ls-registry/daemon | helix StartupError |
| 4 | OffsetEncoding 握手协商（utf-8/16/32） | lsp-core | helix + clangd |
| 5 | capabilities 懒初始化 + initialized 就绪门 | lsp-core | helix OnceCell+Notify |
| 6 | 同语言多服务器优先级回退 + features 开关 | ls-registry/daemon | helix features + lsp-mode :priority |
| 7 | 下载器宿主化：(Os,Arch)→URL 矩阵 + github release 解析 + 安装进度事件 | ls-runtime | zed_extension_api |
| 8 | workDoneProgress 聚合，/status 暴露索引进度 | daemon | helix LspProgressMap |
| 9 | 管理命令面 status/list/stop-all/logs；请求前自动 didOpen | cli/daemon | cli-lsp-client |
| 10 | `--record` JSON-RPC 录制回放（quirk 对拍神器） | daemon/开发 | lsp-devtools |

### 9.1 调研来源（tier）

T1：zed_extension_api docs.rs、eglot 官方手册、LSP 规范；T2：helix（源码实证）、nvim-lspconfig、multilspy（605★）、efm-langserver、lsp-devtools、rust-analyzer lsp-server；T3：cli-lsp-client（57★，同形态最接近竞品，已验证我们差异化 = 73 适配器 + 按符号名工具面 + 单 exe）、lspmux（概念借鉴，copyleft 代码不可抄）。

## 10. 审计修复记录（momus verdict: 打回 → v0.2 全部处置）

| ID | 缺陷 | 处置 |
|---|---|---|
| C1 | lazy-spawn 惊群多 daemon | §3 lock `create_new` 原子竞速 + 败者轮询 |
| C2 | daemon 崩溃 LS 孤儿（各 200MB-2GB） | §3 Job Object / PDEATHSIG |
| C3 | replace-body 一致性链路缺失 | §3.1 三步链路（锁内原子/mtime 对账/读回回滚） |
| C4 | 五类锁无获取顺序 | §3.1 铁律 + ARCHITECTURE.md §3.4 权威表 |
| I1 | trait 钩子面不够 | §4 定稿指引 ARCHITECTURE §4.1（M0 前完成） |
| I2 | T2 工作量低估 2-3 倍 | §5/§7 修正（1-4 天，>30KB 按 3-5 天） |
| I3 | 符号解析层无家可归 | §6 + supervisor crate 承接 |
| I4 | 缓存失效双机制割裂 | §6 fingerprint 每次校验 |
| I5 | Windows rename 冲突/句柄继承 | §8 风险表新行 |
| I6 | 实例键路径未规范化 | §3.1 dunce + 大小写折叠 |
| I7 | 里程碑矛盾（quirk 全推 M3 等） | §7 表修订 |
| I8 | idle 停机协议未定义 | §3 ShutdownDraining 协议 |

Minor 6 条不阻塞，随实现期自查（明细见 `history://DesignAuditor`）。

## 11. 开放问题（需拍板）

1. 项目名/命令名：`serena-rust`/`serena-cli` 为占位。
2. `replace-body`：C3 修复后一致性链路完整，设计默认**进 M1**；若要首版轻装，M1 验收前可撤（撤则延至 M4）。
3. T2 攻坚顺序是否按用户项目频率定：clangd → pyright → gopls → typescript？
4. 缓存（M4）是否复用上游 `.serena/cache` 格式以便互通？建议：不复用，独立 `.lsp-cache/`，避免格式耦合。
