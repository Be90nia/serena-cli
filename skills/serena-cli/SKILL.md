---
name: serena-cli
description: 用 serena-cli 做代码符号级检索与编辑（LSP 后端，position-free，省 token）。当需要查符号/引用/实现、按符号改代码、跨文件搜索、安全删除时使用。
---

# serena-cli 使用纪律（token 最小化）

核心原则：**先结构后细节，按名寻址，禁止整读文件**。

## 黄金路径（覆盖 90% 场景）

```
1. serena-cli overview <file>                  # 先看顶层符号，不要 read 整个文件
2. serena-cli find-symbol <query>              # workspace 符号搜索
3. serena-cli symbol-body <file> <symbol>      # 按名取体（免行号定位往返）
4. serena-cli replace-body/replace-text-in-symbol <file> <symbol> ...   # 改
5. serena-cli refs/find-referencing-symbols <file> <line> <col>         # 影响面
6. serena-cli search <pattern> [--path-glob] [--max-results 50]         # 跨文件
7. serena-cli diagnostics <file>               # 收尾验证
```

## 命令速查

| 类别 | 命令 |
|---|---|
| 读 | overview / read-file(start,end，回传 hash) / list-dir / find-file / search / hover / diagnostics |
| 符号 | find-symbol / symbol-body / def / refs / find-referencing-symbols / find-referencing-code-snippets / find-implementations |
| 编辑（写门+hash 对账） | replace-body / replace-text-in-symbol / insert-text-before-symbol / insert-text-after-symbol / delete-text-in-symbol / insert-at-line / replace-lines / delete-lines / rename-symbol / safe-delete-symbol |
| 项目 | status（daemon+active project） / stop-all / shell(JSONL 长连接) |

- 行号一律 **1-based 含端**；编辑命令带 `--expected-hash`（来自最近 read-file/上次编辑返回）防并发覆盖
- `safe-delete-symbol`：有引用时**拒删**并返回引用列表（file+line）——先看列表再决定 rename 或逐处改

## 错误契约

stdout 单行 JSON：`{"ok":true,...}` 或 `{"ok":false,"error":{"code","message","ls","retryable"}}`。
失败读 `error.message` 即可定位；`retryable=false` 时不要重试同一命令。退出码 = wire code 映射。

## Memory 惯例（不用 CLI）

项目记忆 = `.serena/memories/*.md`，用你自己的文件工具直读直写：
- 首见项目：写 `.serena/memories/project_overview.md`（构建/测试命令、目录结构）
- 改名/移动 memory 时 grep `mem:` 前缀引用并同步
- 跨项目查询：对另一目录跑 `serena-cli --project <dir> <cmd>`

## 省 token 要点

- `symbol-body`/`replace-body` 按符号名寻址，**永远不要**为了拿行号先 read-file
- search 结果默认带上下文行，配合 `--max-results` 控制
- 多命令连发用 `serena-cli shell`（JSONL 长连接，LS 热缓存，毫秒级）
- daemon 常驻：首次命令自动 lazy-spawn，之后免冷启动
