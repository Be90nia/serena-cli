# serena-rust 全面验收第 1 路：全命令 e2e + IDE 能力对标

- 分支 feature/solidlsp-phase0-1 @ 4705e7f；二进制 `target/release/cli.exe`（主入口，release 下无 stack overflow）+ `target/release/examples/cli_run_bin.exe`（smoke 五命令）
- 原计划用 cli_run_bin 绕开主入口，实测其手写分发只支持 doctor/install-all/install/read-file/ext-detect 五命令，全命令面必须走 cli.exe。release 构建下 cli.exe 未复现 stack overflow（debug 路径未测）。
- LS 版本：rust-analyzer (rustup stable shim)、clangd（D:\Program Files\llvm\bin）、typescript-language-server、pyright 1.1.414（uv tool install）、gopls v0.23.0（go install）。java/c# 本机缺运行时，未测（按约束标注跳过）。
- 原始证据：`local/acceptance/a-rust-daemon.log.md`、`a3-rust-tail.log.md`、`b-ts.log.md`、`c-c.log.md`、`d-py.log.md`、`e-go.log.md`、`f-daemon.log.md`、`a-rust.log.md`（--direct 对照）、`smoke.log.md`

VERDICT: FAIL —— 36 命令面在 ≥1 语言上存在不可用或危险行为；Python 全瘫（P0）、rust-analyzer 会话语义层死（P0）、daemon 竞态孤儿 403（P0）、safe-delete 假阴性删码（P1）。

## 一、七段结果表

### F. daemon 生命周期 —— 8/8 PASS（但发现竞态缺陷，见缺口 #3）
| 步骤 | 结果 | 证据 |
|---|---|---|
| 基线 status | PASS | rc=1 "daemon: not running"，lock absent |
| 冷启动 lazy-spawn | PASS | overview 首调 752ms（含 daemon spawn + RA 启动），lock 写入 pid/port/token(32hex) |
| 热态复用 | PASS | overview/def/hover 二调 40/42/42ms（≈18 倍提速） |
| stop-all | PASS | 67ms，"daemon draining (pid …); lock will be removed by reaper"；status 转 rc=1 |
| 竞态观察 | PASS | stop-all 后立即调用 → 懒重生成功 1288ms，无 403 |
| lazy 重生新 token | PASS | 旧 token 48c5239c… → 新 token 58caf22f…，pid 22444→33920 |
| 收尾清场 | PASS | status rc=1，lock 由 reaper 回收 |
| 缺陷活体 | **缺陷** | master 长跑中 daemon 中途死亡（约 A2 第 15 条命令后）→ 后续全部 rc=3；出现持 7860 但无锁的孤儿（403 FORBIDDEN missing or invalid X-Serena-Token）与无监听无锁僵尸 cli.exe；独立复现 AIBench 报告的同类竞态 |

### A. rust（fixtures/rust_demo，36 命令）
| 命令 | 结果 | 证据（ms / 错误） |
|---|---|---|
| overview | PASS | 1286ms 冷/50ms 热，add+multiply 符号正确 |
| symbol-tree | PASS | 50ms，files_scanned:2 |
| find-symbol | FAIL | rc=0 但 `[]`（RA workspace/symbol 空；clangd/gopls 对照均有数据） |
| def | FAIL | rc=0 但 `null`（main.rs 3:12 调用点 → 应指 lib/main 的 add；RA） |
| refs | FAIL | rc=0 但 `[]`（RA） |
| find-implementations | PASS | 89ms，[]（fixture 无 trait impl，空合理） |
| hover | FAIL | rc=0 但 `null`（RA） |
| diagnostics（干净文件） | PASS | 44ms items:[] |
| diagnostics（注入 E0425 错误） | FAIL | 注入 `undefined_xyz_fn()` 后 --wait-gen 2 仍 items:[] |
| symbol-body | PASS | 43ms，multiply 完整函数体 |
| containing-symbol | FAIL | rc=1 `RPC_ERROR: data did not match any variant of untagged enum DocumentSymbolResponse`（RA 响应解码失败） |
| defining-symbol | FAIL | rc=0 但 `null`（RA） |
| find-referencing-symbols | FAIL | rc=0 但 `[]`（RA） |
| find-referencing-code-snippets | FAIL | rc=0 但 `[]`（RA） |
| search | PASS | 69ms，2 hits（非 LSP 文本搜索） |
| read-file | PASS | 650ms，--start/end-line 生效，total_lines:7 |
| list-dir | PASS | 37ms |
| find-file | PASS | 45ms |
| signature-help | ◐ | 146ms rc=0 返回 null；RA 语义死 + 无法与位置问题解耦 |
| document-highlight | ◐ | rc=0 42ms（rust 会话返回体未逐条核对；clangd/ts 对照有真数据） |
| folding-range | PASS | 50ms |
| semantic-tokens | PASS | 38ms，真实 token 表（tokenType/modifiers 齐全） |
| inlay-hint | ◐ | 42ms []（RA 语义死，无数据点可核对） |
| code-action | PASS | 77ms []（干净位置空合理；ts 段对照返回真实 action） |
| format | PASS | 112ms 返回 edits；但全文替换 edit 把 LF 改成 CRLF（缺口 #10） |
| completion | FAIL | rc=2 `position 7:20 out of range`：0-based 合法位置被拒 → completion 是 1-based，与 def/refs 0-based 不一致（缺口 #7） |
| call-hierarchy prepare | ◐ | rc=0 但 `[]`（RA 语义死；gopls 未测三件套） |
| replace-body | FAIL | rc=1 同 DocumentSymbolResponse 解码失败（RA） |
| replace-text-in-symbol | ◐ | 落盘生效（a*b→a*9 实测）但返回 `null` 无回执 |
| insert-text-before-symbol | PASS | 54ms 返回编辑位置回执 |
| insert-text-after-symbol | PASS | 54ms 回执 ✓ |
| delete-text-in-symbol | FAIL | rc=2 参数语义混乱（1..1 被拒，body 5..7 是 1-based 绝对行）；传 6..6 落盘内容错位（见缺口 #7 证据） |
| safe-delete-symbol（无引用） | PASS | 46ms deleted:true |
| safe-delete-symbol（有引用 add） | **FAIL 危险** | deleted:true references:[] —— RA refs 失效导致假阴性，删掉了被 main.rs 调用的符号（缺口 #4） |
| insert-at-line | PASS | 40ms 回执 ✓ |
| replace-lines | ◐ | 落盘生效（实测第 5 行变 fn replaced(){}）但返回 `null` 无回执 |
| delete-lines | ◐ | 落盘生效（文件缩至 4 行）但返回 `null` 无回执 |
| rename-symbol | FAIL | rc=1 `rpc -32602: No references found at position`（RA） |
| smoke（cli_run_bin） | PASS | doctor --json / read-file(autodetect) / ext-detect(py) / 未知命令 rc=2 |

### B. typescript（fixtures/typescript_demo，核心 22 项）
| 命令 | 结果 | 证据 |
|---|---|---|
| overview | PASS | 2398ms 冷（tsserver 项目加载）/53ms 热 |
| symbol-tree | PASS | 53ms |
| find-symbol | ◐ | rc=0 `[]`（tsserver workspace/symbol 空） |
| def | PASS | 55ms 真实 Location |
| refs | PASS | 73ms |
| hover | PASS | 60ms |
| diagnostics | PASS | 5524ms 冷启动后 items:[]（干净文件） |
| symbol-body | PASS | 35ms，compute 完整体 |
| containing-symbol | PASS | 40ms（TS 的 documentSymbol 响应可解码） |
| defining-symbol | FAIL | rc=2 `definition at /d%3A/Project/... is outside workspace root` —— percent-encode URI 未解码即比对（缺口 #5 实锤） |
| find-referencing-symbols | PASS | 47ms |
| find-referencing-code-snippets | ◐ | rc=0 `[]` |
| completion | PASS | 183ms，"5 of 2526" 候选（Calculator/AbortController…，kind 齐全） |
| signature-help | ◐ | rc=0 null（测试位置在标识符内非实参位，未定性） |
| document-highlight | PASS | 45ms |
| semantic-tokens | PASS | 44ms |
| folding-range | PASS | 39ms |
| code-action | PASS | 558ms 返回真实 action 数组 |
| format | PASS | 76ms 非空 edits |
| search | PASS | 44ms files_scanned:4 |
| rename-symbol | **FAIL** | rc=0 但 `edits_applied: 0`，RENAME_CHECK=0 —— 静默零编辑（缺口 #5） |
| safe-delete-symbol | PASS | deleted:false（有引用正确拒删） |

### C. c/clangd（fixtures/c_demo，核心 10 项）—— 10/10 PASS
| 命令 | 结果 | 证据 |
|---|---|---|
| overview | PASS | 1310ms 冷，add/main 符号正确 |
| find-symbol | PASS | 38ms 命中 add |
| def | PASS | 99ms 调用点→add 定义 (1,4-7) |
| refs | PASS | 93ms 2 处（定义+调用点 6:12-15） |
| hover | PASS | 109ms |
| diagnostics | PASS | 40ms items:[] |
| completion | PASS | 40ms |
| format | PASS | 46ms 4 处真实空白编辑 |
| rename-symbol | PASS | 93ms `edits_applied: 2`，RENAME_CHECK=2 落盘实证 |
| document-highlight | PASS | 98ms 2 ranges |

### D. python/pyright（fixtures/py_demo，核心 10 项）—— 0/10，根因已定位
| 命令 | 结果 | 证据 |
|---|---|---|
| overview | FAIL | rc=1 `LS_TERMINATED: ls: stdout pump EOF` 2458ms |
| find-symbol | ◐ | rc=0 `[]`（LS 已死状态下返回空） |
| def/refs/hover/diagnostics/completion/inlay-hint/rename/format | FAIL×8 | 全部 rc=1 同 `LS_TERMINATED … stdout pump EOF`，约 1.2s 死亡 |

根因（复现 + 代码定位）：`crates/ls-adapters/src/pyright.rs:77-84` 对 `pyright-langserver` 裸启动 `vec![exe]`，注释称"自动进入 LSP 模式（无额外 flag）"——错误。pip/uv 版 pyright-langserver 必须显式 `--stdio`，否则立即退出：`Error: Connection input stream is not set. Use … '--node-ipc', '--stdio' or '--socket=…'`（本机独立复现，1.7s 退出，stderr 849B；带 --stdio 时存活并正确应答 initialize："Pyright language server 1.1.414 starting"）。同文件裸 `pyright` 分支已正确加 `--stdio`。

### E. go/gopls（fixtures/go_demo，核心 10 项）—— 9/10
| 命令 | 结果 | 证据 |
|---|---|---|
| overview | PASS | 2061ms 冷 |
| find-symbol | PASS | 63ms 命中本地 add + stdlib math/bits.Add 等（真 workspace/symbol） |
| def | PASS | 169ms (9,5)→func main 定义 |
| refs | PASS | 44ms 2 处（定义 5 行 + 调用 10 行） |
| hover | PASS | 32ms markdown 签名+doc 注释 |
| diagnostics | PASS | 28ms items:[] |
| completion | PASS | 34ms "5 of 54"（fmt/add/main/keywords） |
| document-highlight | PASS | 29ms |
| rename-symbol | FAIL | rc=1 `rename response has no 'changes' map (M2 only supports changes, not documentChanges)`——gopls 走 documentChanges，工具端不支持 |
| format | PASS | 41ms []（已 gofmt，空合理） |

### G. external-servers.toml —— 机制可用，覆盖面有边界
| 项 | 结果 | 证据 |
|---|---|---|
| 注册 | PASS | `%APPDATA%\serena\external-servers.toml` 写入 `[servers.ra-external]`（path_only → rust-analyzer.exe，priority=1，extensions=[".rs"]） |
| 加载告警 | PASS | 首次使用即 warn：`extension '.rs' shadows built-in routing (built-in wins)` |
| install 路由 | PASS | `cli_run_bin install ra-external` → `{"cmd":["…rust-analyzer.exe"],"ok":true,"source":"external"}` |
| ext-detect | PASS | `ext-detect main.rs → rust` |
| status 标注 | PASS | source 字段 external/builtin 区分 |
| 会话级路由 | **缺口** | rust 是 T2 手写适配器，session_for 不查 merged spec：external 条目无法接管 rust-analyzer/gopls/pyright/ts 等手写语言（缺口 #11）；测后已删除注册文件并验证恢复 |

## 二、IDE 能力对标表（对照 VSCode 同 LS 体验）

图例：✅ 等价 / ◐ 部分或缺参 / ❌ 不可用 / · 未测（本段命令集外）

| 能力 | rust (RA) | ts (tsserver) | c (clangd) | py (pyright) | go (gopls) |
|---|---|---|---|---|---|
| 补全（触发/样例） | ❌（rc=2 位置基线拒收；RA 语义死） | ✅ 2526 候选/183ms，trigger 自动推断 | ✅ 40ms | ❌ LS 死 | ✅ 54 候选/34ms |
| 诊断（发布延迟/精确度） | ❌ 注入 E0425 零发布 | ◐ 冷启动 5.5s 后才有；干净文件 0 条正确 | ✅ 40ms | ❌ | ✅ 28ms |
| code action（quickfix） | ❌（RA） | ✅ 真实 action 数组 558ms | · 未测 | ❌ | · 未测 |
| format | ◐ edits 有效但 LF→CRLF 全文替换 | ✅ 76ms | ✅ 4 处精准编辑 | ❌ | ✅ |
| rename | ❌ No references found | ◐ 静默 0 编辑（rc=0） | ✅ 2 处落盘 | ❌ | ❌ documentChanges 不支持 |
| hover | ❌ null | ✅ | ✅ | ❌ | ✅ |
| go-to-def | ❌ null | ✅ | ✅ | ❌ | ✅ |
| find-refs | ❌ [] | ◐ syms ✓/snippets 空 | ✅ 2 处 | ❌ | ✅ 2 处 |
| signature-help | ◐ | ◐ | · | ❌ | · |
| document-highlight | ◐ | ✅ | ✅ 2 ranges | ❌ | ✅ |
| semantic-tokens | ✅ | ✅ | · | ❌ | · |
| folding-range | ✅ | ✅ | · | ❌ | · |
| inlay-hint | ◐ 空 | · | · | ❌ | · |
| call-hierarchy | ◐ prepare 空 | · | · | ❌ | · |

结论：clangd/gopls/tsserver 三路 LSP 管道（didOpen/位置/编解码/编辑落盘）正确；RA 会话与 pyright 会话两条链路不可用，5 语言中 2 语言失去 IDE 核心能力。

## 三、缺口清单（按严重度）

1. **P0 · Python**：pyright-langserver 裸启动缺 `--stdio` → Python 全命令瘫痪。修复：`crates/ls-adapters/src/pyright.rs:77-84` 裸 langserver 分支改 `vec![exe, "--stdio".into()]`（同文件另一分支已是此形态；servers.toml 内 uvx 条目 args 也带 --stdio 可佐证）。basedpyright_server 同壳同疑，需一并核。
2. **P0 · rust**：rust-analyzer 会话语义层死——def/hover/refs/find-symbol/defining/find-ref-*/call-hierarchy/inlay-hint/diagnostics 全部 null/[]，documentSymbol 系（overview/symbol-tree/symbol-body/semantic-tokens）正常；clangd/gopls/tsserver 对照正常，排除 serena 管道问题。独立 LSP 探针显示 RA initialize 应答病态延迟（>30s，响应迟到大拖延）——疑似 serena 会话握手/就绪等待与 RA 不兼容。连带把 #4 放大成数据丢失。
3. **P0 · daemon**：长跑中 daemon 进程中途死亡（无日志，stderr→nul），随后懒重生竞态产生两类残留：持 7860 端口但 lock 被判 stale 删除的孤儿（客户端 403）与无监听无锁的僵尸 cli.exe；reaper 延迟删锁与仲裁存在竞窗（F 段 snap[after-stop] 仍见锁）。建议：daemon stderr 落盘 + 死亡原因可观测；lock 删除与端口/进程活性二次校验。
4. **P1 · safe-delete 假阴性（危险）**：refs 后端返回空（如 RA 失效）时静默判定"无引用"并删除被引用符号（A 段 add 被删实证）。建议：refs 空/异常时拒绝删除或返回 unverified 标记；rename 的 "No references found" 同源。
5. **P1 · ts percent-encode URI**：defining-symbol rc=2 `definition at /d%3A/... is outside workspace root`——URI 未解码即比对；rename edits_applied:0 静默零编辑疑似同族（AIBench 独立观察到 edits uri `d%3A/`）。建议：统一对 LS 返回 URI 做 percent-decode + root 归一。
6. **P1 · RA 响应解码**：containing-symbol / replace-body 报 `untagged enum DocumentSymbolResponse` 解码失败（rc=1）——RA 的 documentSymbol 返回形态未被枚举覆盖；tsserver/clangd 同命令正常。
7. **P2 · 位置/行号基线不一致**：completion 实测 1-based（0-based 合法位置被 out of range 拒收），def/refs 0-based；delete-text-in-symbol 声称符号体 5..7（1-based 绝对行）但按 6..6 删除时落盘内容错位（快照：`pub fn multiply(a: i32, b: i3a * b`）。建议统一 0-based 并写进 CLI 帮助。
8. **P2 · 编辑回执缺失**：replace-text-in-symbol / replace-lines / delete-lines 落盘成功但返回 `null`（insert 系有回执），agent 无法确认写结果。
9. **P2 · rename 仅支持 changes**：gopls 返回 documentChanges 即失败（E 段 rc=1，错误消息自认 M2 限制）。
10. **P2 · format EOL 噪音**：rust format 对 LF 文件返回全文 CRLF 替换 edit，非 IDE 等价行为（IDE 尊重文件现有 EOL）。
11. **P3 · external-servers.toml 不路由 T2 语言**：仅表驱动语言生效；install/status 可标注，但 rust/go/py/ts 会话不受 priority 影响——建议文档明示边界（本次实测 rust 会话不受 external 条目影响）。
12. **P3 · 可观测性**：daemon stderr 直弃 nul，死亡/崩溃零痕迹（本报告 #3 只能靠外围现象定性）；LS 侧 stderr 同样不可查，pyright 问题靠独立复现定位。
13. **P3 · 静默空返回**：多个命令 rc=0 但 null/[]（find-symbol、find-ref-snips、sig-help），无法区分"真没有"与"后端失效"；建议至少在 stderr 注明后端就绪状态。

## 四、覆盖与清理

- 命令面：40 个子命令探测（clap usage 全量核对）；执行 A 36+smoke5、B 22、C 10、D 10、E 10、F 8 步、G 6 项。
- 未测（明示）：format-range/code-lens/document-link/moniker/type-hierarchy/workspace-diagnostic 逐语言矩阵（仅冒烟路径）、java/c#（本机无运行时）、shell JSONL、mcp stdio。
- 清理：fixtures 全部 `git checkout` 还原（R5 后验证干净）；external-servers.toml 已删；daemon 已停（hub 托管 stop）、lock absent、无残留 cli.exe；测试脚本与日志保留于 local/acceptance/。
