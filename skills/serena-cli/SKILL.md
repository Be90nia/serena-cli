---
name: serena-cli
description: 用 serena-cli 做代码符号级检索与编辑（LSP 后端，position-free，省 token）。当需要查符号/引用/实现、按符号改代码、跨文件搜索、安全删除时使用。
---

# serena-cli 使用纪律（token 最小化）

核心原则：**先结构后细节，按名寻址，禁止整读文件**。

详细架构与性能基线见 [`README.md`](../../README.md)。

## 黄金路径（8 命令，覆盖 90% 场景）

```
1. serena-cli overview <file>                  # 先看顶层符号，不要 read 整个文件
2. serena-cli find-symbol <query>              # workspace 符号搜索
3. serena-cli symbol-body <file> <symbol>      # 按名取体（免行号定位往返）
4. serena-cli replace-body/replace-text-in-symbol <file> <symbol> ...   # 改
5. serena-cli refs/find-referencing-symbols <file> <line> <col>         # 影响面
6. serena-cli search <pattern> [--path-glob] [--max-results 50]         # 跨文件
7. serena-cli diagnostics <file>               # 收尾验证
```

## 命令速查（24 子命令）

| 类别 | 命令 |
|---|---|
| 读 | overview / symbol-tree / read-file(start,end，回传 hash) / list-dir / find-file / search / hover |
| 符号 | find-symbol / symbol-body / def / refs / find-referencing-symbols / find-referencing-code-snippets / find-implementations / containing-symbol / defining-symbol / signature-help |
| 诊断 | diagnostics(--wait-gen N，按代数等新一轮；pull diagnostics 透明 fallback) |
| 编辑（写门+hash 对账） | replace-body / replace-text-in-symbol / insert-text-before-symbol / insert-text-after-symbol / delete-text-in-symbol / insert-at-line / replace-lines / delete-lines / rename-symbol / safe-delete-symbol |
| 补全 | completion(--limit 默认 5；trigger 按 file 后缀自动推断) |
| 项目 | status(daemon+active project) / stop-all / shell(JSONL 长连接) / install <lang> |

- 行号一律 **1-based 含端**；编辑命令带 `--expected-hash`（来自最近 read-file/上次编辑返回）防并发覆盖
- `safe-delete-symbol`：有引用时**拒删**并返回引用列表（file+line）——先看列表再决定 rename 或逐处改
- `containing-symbol <file> <line> <col>`：按位置反查最深层的包含符号（grep 命中直接进符号工作流，免二次定位）
- `defining-symbol <file> <line> <col>`：`def` 拿到 Location 后精化取符号名/kind/范围/body 切片；多定义（重载）返多元素
- `signature-help <file> <line> <col>`：函数调用位置的参数签名提示
- `diagnostics --wait-gen N`：等 generation ≥ N（替代盲轮询 5s）；仍受 5s 上限
- `completion --trigger <char>`：省略时按后缀推断（C++/Rust 取 `.`/`::`，TS 取 `.`，Python 取 `.`）
- `symbol-tree <dir>`：跨文件符号树；带 `--max-files 200` 保险丝（超出截断并标 `truncated:true`）

## 错误契约

stdout 单行 JSON：`{"ok":true,...}` 或 `{"ok":false,"error":{"code","message","ls","retryable"}}`。
失败读 `error.message` 即可定位；`retryable=false` 时不要重试同一命令。退出码 = wire code 映射（见 `ARCHITECTURE.md` 9 错误码）。

| code | 含义 | retryable |
|---|---|---|
| `internal` | 程序 bug | false |
| `invalid_request` | 参数错 | false |
| `unauthorized` | token 不匹配 | false |
| `not_found` | 路径/符号不存在 | false |
| `rpc_error` | LSP server 返错 | 看 `ls` 字段 |
| `timeout` | 等 LS 超时 | true（cold 二次走 ready gate） |
| `ls_spawn_failed` | LS 启不来 | false（修 PATH/install） |
| `ls_not_installed` | LS 未装 | false（跑 `install <lang>`） |
| `unsupported` | 此项目不接此 LS | false |

## 性能与懒启动

- **首次命令自动 lazy-spawn daemon**，首次开销 ~5s（rust fixture workspace mode，无竞争）；之后 hot daemon 65-108 ms
- **多命令连发用 `serena-cli shell`**：JSONL 长连接，LS 热缓存，重复查询 <1 ms
- **二次 overview 命中符号缓存**：0.88-0.93 ms（shell 模式）/ 19-21 ms（CLI spawn 模式）
- **空 token = 自动跳过鉴权**（`gen_token` 确定性非加密，鉴权只是防同机误连，不防恶意）

## Memory 惯例（不用 CLI）

项目记忆 = `.serena/memories/*.md`，用你自己的文件工具直读直写：
- 首见项目：写 `.serena/memories/project_overview.md`（构建/测试命令、目录结构、约定）
- 改名/移动 memory 时 grep `mem:` 前缀引用并同步
- 跨项目查询：对另一目录跑 `serena-cli --project <dir> <cmd>`

## 安装 LS

```bash
serena-cli install rust       # rust-analyzer (rustup 优先 → PATH 兜底)
serena-cli install python    # pyright (npm uvx)
serena-cli install typescript # typescript-language-server (npm shim)
serena-cli install go         # gopls
serena-cli install java      # jdtls (auto-download + JVM args)
```

`install` 走 `servers.toml` 配置：A 类 GitHub release 二进制 / B 类 npm / C 类 uvx / D 类 dotnet tool / E 类 gem / F 类源码构建 / G 类 path-only（已装则跳过）。SHA-256 校验失败即 `ls_not_installed`，不静默放过。

## 省 token 要点

- `symbol-body`/`replace-body` 按符号名寻址，**永远不要**为了拿行号先 read-file
- `read-file` 默认返回 hash——直接 `--expected-hash <hash>` 给编辑命令，省一次对比读
- search 结果默认带上下文行，配合 `--max-results` 控制
- 多命令连发用 `serena-cli shell`（JSONL 长连接，LS 热缓存，毫秒级）
- daemon 常驻：首次命令自动 lazy-spawn，之后免冷启动

## 已知边界

- **5/7 适配器本机未装**：smoke 仅 rust + typescript 可本地跑；其他走单测 + 真实 fixture 数据
- **typescript-language-server 7.x 不兼容**：`typescript@7` native 线无 `tsserver.js` → LS 报 `-32603`；fixture pin `typescript@5.9.3`
- **fixture 冷启动受同机 VS Code rust-analyzer 影响**：重索引主 workspace 时 CPU 竞争可达 300s+（环境噪声，非代码 bug）
- **CLI e2e 测量伪影**：bash 完整重定向测量；禁 powershell `Select-Object -First N`（提前断管道致 EPIPE 双等死锁，误判 daemon 挂死）

[cbm-bridge reminder] This repo has a codebase-memory-mcp knowledge graph. For future code reading, prefer:
  codebase-memory-mcp cli search_code --project=D-Project-serena-rust --query="<your search>"
  codebase-memory-mcp cli get_code_snippet --project=D-Project-serena-rust --qualified_name="<symbol>"
Use the codebase-memory-mcp skill for invocation forms.