---
name: serena-cli
description: 用 serena-cli 做符号级代码检索与编辑（LSP 后端，按名寻址，实测省 token 71-99%）。当需要查符号/定义/引用/实现、读或改函数体、跨文件重命名、安全删除、找 bug 影响面、写代码前了解结构时使用；大文件（>500 行）读写前优先用它而非 read/grep。不适用：纯文本/配置/文档编辑（无 LS 的文件类型会拒写）、不需要理解代码语义的机械替换。
---

# serena-cli 使用纪律

二进制：`serena-cli`（已在 PATH 直接用；未装见 README「Install」——GitHub Release 下载或源码构建）。核心原则：**按名寻址，先结构后细节，禁整读文件**。实测对照（真实字符数）：读大文件里的一个函数 260 tok vs 传统 96,615 tok；改函数体 64 vs 223；跨文件 rename 21 vs 225。

## 黄金路径（覆盖 90% 场景）

```
1. serena-cli overview <file>                    # 顶层符号列表；禁 read 整文件
2. serena-cli edit-context <file> <symbol>       # 改前一次拿齐 body+callers+doc+tests
3. serena-cli symbol-body <file> <symbol>        # 按名取函数体（免行号往返）
4. serena-cli replace-body <file> <symbol> --with <NEW>   # 换体（写门+hash 对账+原子写）
5. serena-cli find-referencing-symbols <file> <line> <col>  # 改前影响面
6. serena-cli diagnostics <file> --wait-gen 1    # 收尾验证（等新一轮诊断）
7. serena-cli undo                               # 改坏了回滚（IDE 级事务 undo）
```

## 关键纪律（违者返工）

- **行号一律 1-based 含端**——传 0 是用法错误。
- **多命令任务先预热**：`serena-cli warm`（免冷启动 ~5s）；语义类（def/hover/refs）就绪需 30-60s，用 `wait-ready --stage semantic` 阻塞等，就绪前语义查询会返空+warning（不是坏了）。
- **编辑带 `--expected-hash <hash>`**（来自最近 read-file/编辑返回），防并发覆盖。
- **`--lang <lang>`** 显式指定语言当扩展名有歧义（如 .ts 项目里的 .js）。
- JSON 解析：stdout 首行可能是 `[warn] ...` 人读行——解析前先切出第一个 `{`。
- **写类命令对无 LS 的文件类型拒写**（如 .txt → BAD_ARGS "file not supported"）——纯文本用普通文件工具。
- 大改/不确定结果 → 改完跑 `undo` 验证能回滚再继续；`undo --list` 看栈。rename 改多文件 = 一个事务，undo 一次全回滚。
- 深度语义（call-hierarchy、repo-map）依赖全量索引热身（分钟级），冷会话可能返空——改用 `refs` 拼接。

## 命令速查（55 个，按类）

| 类 | 命令 |
|---|---|
| 读/导航(7) | overview · symbol-tree · read-file(1-based 含端，回传 hash) · list-dir · find-file · search · hover |
| 符号(8) | find-symbol · symbol-body · def · refs · find-implementations · find-referencing-symbols · find-referencing-code-snippets · containing-symbol |
| 上下文聚合 | edit-context(改前必备) · repo-map(全 project 符号热度图) · defining-symbol · signature-help |
| 诊断(3) | diagnostics(--wait-gen N) · workspace-diagnostic · signature-help |
| 编辑(11) | replace-body · replace-text-in-symbol · insert-text-{before,after}-symbol · delete-text-in-symbol · insert-at-line · replace-lines · delete-lines · rename-symbol(跨文件自动同步) · safe-delete-symbol(有引用拒删) · create-text-file |
| undo/redo(2) | undo(--steps N / --list) · redo —— 事务级：rename 多文件一次回滚；新建文件 undo 即删；文件被外部改过则拒绝(WRITE_CONFLICT)；栈 20 步/200MB/30 天，重启升级不丢 |
| 补全/长尾 | completion · code-action · format · format-range · inlay-hint · folding-range · document-highlight · semantic-tokens · code-lens · call-hierarchy · type-hierarchy · moniker · document-link · folding-range |
| 管理 | status · warm · wait-ready(--stage symbol\|semantic) · stop-all · install <lang> · doctor · shell(JSONL 长连接) · lint-shell |

## 错误契约

stdout = 紧凑 JSON（默认）+ 可能的 `[warn]` 前缀行；失败 `{"ok":false,"error":{code,message,retryable}}`。高频码：`BAD_ARGS`(参数/文件类型错，不重试) · `WRITE_CONFLICT`(盘上内容与预期不符，先重读) · `LS_TIMEOUT`(retryable，重试) · `LS_NOT_INSTALLED`/`LS_SPAWN_FAILED`(环境问题，走下方处置流程)。退出码 0=成功 1=工具错 2=参数错 4=就绪超时。

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

## 环境事实

- 首次命令 lazy-spawn 后台 daemon（~5s），之后毫秒级；闲置 10 分钟 LS 被收割、15 分钟 daemon 自退（`SERENA_IDLE_TIMEOUT_SECS=0` 可禁）。
- 测试/冒烟的 fixture **必须放 workspace 外**（独立目录），workspace 内的散文件语义层会静默返空。
- 项目记忆：`.serena/memories/*.md` 直接用文件工具读写，不走 CLI。
