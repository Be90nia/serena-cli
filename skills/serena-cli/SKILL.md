---
name: serena-cli
description: 用 serena-cli 做符号级代码检索与编辑（LSP 后端，按名寻址，符号级读写实测省 token 71-99%；--compress 集合响应实测再省 ~8-13%）。当需要查符号/定义/引用/实现、读或改函数体、跨文件重命名、安全删除、找 bug 影响面、写代码前了解结构时使用；大文件（>500 行）读写前优先用它而非 read/grep。不适用：纯文本/配置/文档编辑（无 LS 的文件类型会拒写）、不需要理解代码语义的机械替换。
---

# serena-cli 使用纪律

二进制：`serena-cli`（已在 PATH 直接用；未装见 README「Install」——GitHub Release 下载或源码构建）。核心原则：**按名寻址，先结构后细节，禁整读文件**。实测对照（真实字符数）：读大文件里的一个函数 260 tok vs 传统 96,615 tok；改函数体 64 vs 223；跨文件 rename 21 vs 225。

## 黄金路径（覆盖 90% 场景）

```
1. serena-cli overview <file>                    # 顶层符号列表；禁 read 整文件
2. serena-cli edit-context <file> <symbol>       # 改前一次拿齐 body+callers+doc+tests（符号名也可 --symbol <NAME>）
3. serena-cli symbol-body <file> <symbol>        # 按名取函数体（免行号往返；符号名也可 --symbol <NAME>）
4. serena-cli replace-body <file> <symbol> --with <NEW>   # 换体（写门+hash 对账+原子写）
5. serena-cli find-referencing-symbols <file> <line> <col>  # 改前影响面
6. serena-cli diagnostics <file> --wait-gen 1    # 收尾验证（等新一轮诊断）
7. serena-cli undo                               # 改坏了回滚（IDE 级事务 undo）
```

## 关键纪律（违者返工）

- **行号一律 1-based 含端**——传 0 是用法错误。
- **多命令任务先预热**：`serena-cli warm`（免冷启动 ~5s）；语义类（def/hover/refs）就绪需 30-60s，用 `wait-ready --stage semantic` 阻塞等，就绪前语义查询会返空+warning（不是坏了）；紧接要跑 def/refs 的用 `--stage def`（hover 就绪后 def 仍可能空窗，bd y3c1），超时/空结果自带降级指引。
- **`--direct` = 纯冷进程，无 daemon**（bd serena-rust-9hy1）：跳过 lazy-spawn/缓存，只适合轻量读类（overview/read-file/status 等）；语义类工具（def/hover/refs/edit-context）`--direct` 首调必返空 + degraded warning（无预热索引）——语义查询一律走默认 daemon 模式，不要用 `--direct` 后误判"语义层坏了"。
- **编辑带 `--expected-hash <hash>`**（来自最近 read-file/编辑返回），防并发覆盖。
- **`--lang <lang>`** 显式指定语言当扩展名有歧义（如 .ts 项目里的 .js）。
- **`--symbol <NAME>`** 三命令通用符号名直查：find-referencing-code-snippets / symbol-body / edit-context（后两者的位置第二参保留兼容，二选一）。
- **ls-use 已知语言**（如 `ls-use python <bin>`）按二进制名智能匹配内置 server：唯一命中自动选（回显生效 id + 启动命令形态）；零/多命中拒改并列候选——想继承某内置条目请显式点名 server id（如 `ls-use jedi <bin>`）。注册条目重启 daemon 后**接管该语言会话启动**（T2 适配器让位，bd 9z0x）；doctor/ls-list 同步反映注册态。
- JSON 解析：stdout 首行可能是 `[warn] ...` 人读行——解析前先切出第一个 `{`。
- **写类命令对无 LS 的文件类型拒写**（如 .txt → BAD_ARGS "file not supported"）——纯文本用普通文件工具。
- 大改/不确定结果 → 改完跑 `undo` 验证能回滚再继续；`undo --list` 看栈。rename 改多文件 = 一个事务，undo 一次全回滚。
- 深度语义 call-hierarchy 依赖全量索引热身（分钟级），冷会话可能返空——改用 `refs` 拼接。`repo-map` 主源 documentSymbol + 文本兜底，冷会话可用（bd serena-rust-fj17）。
- **写前干跑**：写类命令加全局 `--dry-run` → 完整定位/校验但不落盘，返 `dry_run:true + applied:false + would_apply:true + would_write[{file,patch}]`（unified diff，无全文 token 炸弹）；`applied:true` 只在真写时出现。
- **search 噪音控制**：默认尊重 .gitignore（被忽略的测量/脚本不进结果）；仍嫌吵用 `--exclude <GLOB>`（可多次）；要看全量加 `--no-ignore`（.git/ 与构建产物仍排除）。
- **空结果先读 warning/hint 再下结论**：hover 裸 null 且无 warning = 该位置确无符号信息（LS 已就绪）；带 warning = 未就绪/降级，先 wait-ready。
- **--max-tokens ≥ 1**：0 在参数层拒（rc=2）——旧版会静默吐空集。
- **read-file 的 line_endings 字段**：`crlf/mixed` 时 content 已被归一为 LF 而 hash 按原字节——把读到的 content 拼接写回会转行尾，CRLF 文件慎用拼接写回（走行级三件套或带 hash 的整写）。

## 命令速查（66 个子命令，含 help；按类）

| 类 | 命令 |
|---|---|
| 读/导航(7) | overview · symbol-tree · read-file(1-based 含端，回传 hash+line_endings；`--start/--end` 为 `--start-line/--end-line` 短别名) · list-dir · find-file · search(--exclude <GLOB> 可多次；默认尊重 .gitignore，`--no-ignore` 逃生) · hover |
| 符号(8) | find-symbol · symbol-body · def · refs · find-implementations · find-referencing-symbols · find-referencing-code-snippets · containing-symbol |
| 上下文聚合 | edit-context(改前必备) · repo-map(全 project 按文件顶层符号清单，LS 免热身) · defining-symbol · signature-help |
| 诊断(2) | diagnostics(--wait-gen N) · workspace-diagnostic(LS 不支持时指路逐文件 diagnostics) |
| 编辑(11) | replace-body · replace-text-in-symbol · insert-text-{before,after}-symbol · delete-text-in-symbol · insert-at-line · replace-lines · delete-lines(start>end 报「顺序错」非「越界」) · rename-symbol(跨文件自动同步) · safe-delete-symbol(有引用拒删) · create-text-file |
| undo/redo(2) | undo(--steps N / --list；空栈返 nothing_to_undo:true，rc=0 非报错) · redo —— 事务级：rename 多文件一次回滚；新建文件 undo 即删；文件被外部改过则拒绝(WRITE_CONFLICT)；栈 20 步/200MB/30 天，重启升级不丢 |
| recipe 工作流(4) | `test <target> [name]`(cargo/npm 双后端跑测试+解析失败清单) · `diff [txn-id] [--patch]`(写事务写前写后对照，--patch 出 unified hunk) · `find-test <sym>`(也收 `--symbol <NAME>`，与 symbol-body 同形) · `recipe <name> [args]`(8 工作流编排，见下) |
| 补全/长尾 | completion · code-action · format · format-range · inlay-hint · folding-range · document-highlight · semantic-tokens · code-lens · call-hierarchy · type-hierarchy · moniker · document-link |
| 管理(14) | status · project-info · change-history(--symbol 走 git -L 符号级) · warm · wait-ready(--stage symbol\|semantic\|def) · stop-all · install <lang> · uninstall <lang> · ls-use <lang\|id> <path> · ls-list · ls-remove <id> · doctor · shell(JSONL 长连接) · lint-shell |

## recipe 工作流（8 个，单命令多步编排）

写步各自独立 undo 事务；单步失败即停并**逆序回滚已完成的写步**（错误 message 为单层人话：失败步（内部 `ct_` 前缀已剥）/原因/回滚账目——回滚不完整时显式标 ROLLBACK INCOMPLETE）；单步截断继续。AI 一次调用 = 多步，token 省过逐工具拼：

| recipe | 输入 | 步骤 |
|---|---|---|
| fix-bug | `<file> <sym> [--new-body T]` | ct_tldr → ct_goto_callers → ct_verify(前) → [replace-body] → ct_verify(后)；无 --new-body 只跑分析链 |
| add-feature | `<name> [--target F]` | ct_define_feature(报告/stub 文本) → [stub 落盘] → 可选测试(--tests-file+--tests) |
| rename | `<file> <sym> --to N` | ct_impact → [单文件 LSP rename，跨文件 edits 计入 skipped] → ct_verify |
| add-test | `<sym> [--run]` | find-test → 命中即报告；未命中 → 定义文件 append `mod tests` 模板（已有则显式跳过） |
| refactor-extract | `<file> <sym> --as N` | ct_smart_edit(extract 整符号抽取) → ct_verify |
| refactor-rename | `<sym> --to N` | ct_impact → [LSP rename workspace] → ct_verify(定义文件) |
| review-diff | `[txn-id]` | ct_review_diff（diff + 关联测试 + 报告聚合） |
| explore | `<path>` | ct_tldr → repo-map → ct_recent_activity |

## 错误契约

stdout = 紧凑 JSON（默认）+ 可能的 `[warn]` 前缀行；失败 `{"ok":false,"error":{code,message,retryable}}`（客户端侧校验错误同形打 stderr，rc=2）。高频码：`BAD_ARGS`(参数/文件类型错，不重试) · `WRITE_CONFLICT`(盘上内容与预期不符，先重读) · `LS_TIMEOUT`(retryable，重试) · `LS_NOT_INSTALLED`/`LS_SPAWN_FAILED`(环境问题，走下方处置流程)。退出码 **0**=成功 **1**=工具错(不可重试) **2**=参数错 **3**=INTERNAL(daemon/传输/序列化/IO 故障——重试同参大概率再败，先 `doctor`/`status` 查环境) **4**=就绪超时 **5**=可重试工具错(LS_TIMEOUT/LS_TERMINATED/LS_SPAWN_FAILED/LS_NOT_READY 瞬态，直接重试)。

### stderr 消费契约（PowerShell / 编排脚本）

CLI 故意把诊断信息（`[hint]`/`[warn]`/lazy-spawn 进度/工具错误全文）走 stderr——避免污染 stdout JSON 通道（agent 只看 stdout 第一行/单行 JSON 解析）。但 stderr 非空会触发 PowerShell `$LASTEXITCODE=1`（`NativeCommandError`），orchestrator 据此可能误判失败：

```powershell
# ❌ 错：PowerShell 默认 $ErrorActionPreference 遇 stderr 非 0 行就置 $LASTEXITCODE=1
$out = serena-cli find-symbol foo --lang python 2>$null  # 吞 stderr = 丢 hint
$LASTEXITCODE  # 偶发 1 即便 stdout JSON 成功

# ✅ 对：分离 stdout/stderr，按 $LASTEXITCODE 主判、stdout JSON 辅判
$out = serena-cli find-symbol foo --lang python 2>$null
if ($LASTEXITCODE -eq 0) { Parse-Json $out }  # rc=0 → 仅看 JSON
else { Parse-Json $out; switch -Wildcard ($err) { "[hint][DAEMON_STALE]*" { stop-all } "[hint][LS_MISSING]*" { install } } }
```

要点：`$LASTEXITCODE=0` 是唯一的成败信号，stderr 永远当诊断通道不参与判定。bd serena-rust-p2zp 决定**不加 `--quiet` 旗**——stdout 纯净度优先，stderr 噪声治理归消费者侧契约。

`diagnostics` / 写工具附带的 `post_write_diagnostics` 里 **`pending` 是新鲜度判定**（bd serena-rust-i52y）：`false` = LS 已确认本代，items 空=真无错；`true` = 等待窗口内 LS 未推新一代诊断，items 空**不代表无错**（快照可能陈旧）——用 `diagnostics <file> --wait-gen N` 显式复核后再当干净结论。

## 环境自检与 LS 故障处置

见到 `LS_NOT_INSTALLED` / `LS_SPAWN_FAILED` / 语义查询恒空 + "not ready" 时，按序执行：

```
1. serena-cli doctor                    # 六类体检：运行时/PATH/本机 LS/daemon/网络/workspace
2. serena-cli doctor --lang <lang>      # 聚焦单语言，看哪一项 MISS + hint
```

- **MISS 的是运行时**（node/uv/JDK…）→ 装运行时（hint 里有命令），重跑 doctor 确认 ok。
- **MISS 的是 LS 本体** → 先用生态原生装法（rust: `rustup component add rust-analyzer`；pip 系: `pip install ...`；npm 系: `npm i -g ...`）；**servers.toml 收录的语言**（`serena-cli install <lang>` 可装的 73 种）也可以直接 `serena-cli install <lang>` 或 `doctor --fix`（自动装 + sha256 校验）。
- **防重复下载**：LS 已由 rustup/npm/pipx/系统包管理器装过时**不要**再 `install <lang>` 重复拉一份——doctor 报 MISS 但你确定装过 = **当前进程 PATH 不含它**（GUI 启动的编辑器常见），修 PATH 后重试；仍不行用 `%APPDATA%/serena/external-servers.toml`（用户外部注册表，支持绝对路径 binary，优先级高于内置条目）直接指到已有可执行文件。
- **一键接入自装 LS**：`serena-cli ls-use <lang或id> <LS二进制绝对路径>`（写 external-servers.toml，已知语言自动继承内置 languages/extensions/exec；`--lang <LANG> --ext .<ext>` 注册全新语言；`--list` 列注册、`--remove <id>` 移除；`ls-list` 看全量实装状态、`ls-remove <id>` 卸载 serena 托管缓存）。注册在 daemon 重启后生效。
- rust/c++/go/java 等 T2 语言不在 `install <lang>` 名单内（rust 走 rustup 查找链：`rustup which` → PATH → `~/.cargo/bin`），报 NOT_INSTALLED 时按 hint 里给的生态命令装。

## 多语言

**52 语言 CI 真机验证 PASS**：rust · typescript/javascript · c/cpp · c# · go · java · python(pyright/basedpyright/ty/pyrefly 变体同门) · bash · powershell · json/yaml/toml · vue/svelte/astro · php · ruby · kotlin · swift · dart · scala · clojure · erlang · ocaml · lua/luau · julia · zig · gleam · lean4 · r · perl · fortran · rego · cue · docker · sql/pgsql · markdown/latex · html/css/sass · solidity · systemverilog · hlsl · haxe · elm · ada · ansible · deno。

**15 语言有门但证据 SKIP**（环境/许可证限制，明细在 `scripts/smoke_langs.toml` 账本）：al · haskell · pascal · qml · nix · crystal · nextflow · elixir · fsharp · angular · groovy · gdscript · msl · matlab · wolfram。

未装 LS → `install <lang>`（73 种可装，自动下载+sha256 校验；T2 语言如 rust 走生态原生命令，见环境处置节）。已知边界：bash hover 恒空（依赖 Unix man 页）；vue 诊断受限（走 tsserver 桥待接）；jdtls 需 JRE 25+。

**专有 LS 用户配置**：个别 server 需要你机器的专有路径才可用——matlab 门要求告知 MATLAB 安装路径（上游对 `workspace/configuration` 应答 `installPath`，launch env 注入 `MATLAB_INSTALL_PATH`；无它 LS 拉不起 MATLAB 进程）。`servers.toml` 的 `[servers.matlab]` 下附注释模板，取消注释并替换为实际安装目录即可（README「Vendor-specific LS configuration」同款说明）。

## 环境事实

- 首次命令 lazy-spawn 后台 daemon（~5s），之后毫秒级；闲置 10 分钟 LS 被收割、15 分钟 daemon 自退（`SERENA_IDLE_TIMEOUT_SECS=0` 可禁）。
- 测试/冒烟的 fixture **必须放 workspace 外**（独立目录），workspace 内的散文件语义层会静默返空。
- 项目记忆：`.serena/memories/*.md` 直接用文件工具读写，不走 CLI。
