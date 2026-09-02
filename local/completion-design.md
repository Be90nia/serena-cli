# 代码补全（Completion）设计 —— M2 第一个 Task

> 用户诉求：让 AI agent 写部分代码后能调补全，省 token。
> 目标：把 LSP `textDocument/completion` 包成"AI 友好 CLI"。

---

## 1. LSP 协议回顾

`textDocument/completion` 请求参数：
```json
{
  "textDocument": { "uri": "file:///..." },
  "position": { "line": N, "character": M },
  "context": {
    "triggerKind": 1,                // 1=Invoked, 2=TriggerCharacter, 3=TriggerForIncompleteCompletions
    "triggerCharacter": "."          // 可选：. :: -> < 等触发字符
  }
}
```

响应（clangd 22 实际形态）：
```json
{
  "isIncomplete": false,
  "items": [
    {
      "label": "printf",
      "kind": 3,                     // CompletionItemKind: Function=3, Variable=6, Method=2 ...
      "detail": "int printf(const char *fmt, ...)",
      "documentation": { "kind": "markdown", "value": "..." },
      "sortText": "00001",
      "insertText": "printf",
      "insertTextFormat": 1,         // 1=PlainText, 2=Snippet
      "additionalTextEdits": [...]   // 可选：补全时自动加 import 等
    }
  ]
}
```

---

## 2. AI 友好核心原则

| 原则 | 设计 |
|---|---|
| **省 token** | 默认 `--limit 5`；返回 AI 真正用得上的字段 |
| **可机读** | 一行 JSON（无 pretty-print）；agent 用 jq/grep 解析 |
| **可理解** | `documentation` 默认截断 200 字符；`detail` 保留 |
| **一致语义** | CLI 位置参数与现有工具一致：`completion <file> <line> <col>` |
| **触发字符自动推断** | 不要求 agent 手动传 trigger char —— CLI 看 file 后缀自动推断（C++ = `.`，Rust = `::`，TS = `.`） |
| **过滤噪音** | `kind` map 成人类词（"function"/"variable"/"method"）便于 agent 决策 |

---

## 3. 字段策略（响应裁剪）

**保留**（agent 必用）：
- `label` —— 显示文本（agent 看到的提示）
- `kind` —— 类型（决定 agent 行为：补 method vs function）
- `detail` —— 签名（agent 看 `int printf(const char *, ...)` 决定是否调用）
- `insertText` —— 实际插入（agent 直接用）
- `documentation` —— 截断到 200 char（agent 需要时再深入）

**丢弃**（agent 用不到，省 token）：
- `sortText` / `filterText` —— LSP 内部排序用，agent 拿到结果我们已排好序
- `commitCharacters` —— LSP 编辑器集成用
- `additionalTextEdits` —— **保留**（import 自动加，agent 需要）
- `command` —— editor-side command
- `data` —— resolve field，agent 不需要
- `tags` —— 装饰用
- `deprecated` —— 保留为布尔（agent 决策参考）

**数量裁剪**：
- 默认 `--limit 5`（AI 通常只挑 1 个）
- `--limit 0` 表示不限
- 截断时输出 `truncated: 5 of 23` 标注

---

## 4. 接口设计

### CLI 命令

```
serena-cli completion [--limit N] [--trigger CHAR] <file> <line> <col>
```

示例：
```bash
# 默认 5 条
serena-cli completion impl.cpp 10 5

# 全部
serena-cli completion --limit 0 impl.cpp 10 5

# 指定 trigger char
serena-cli completion --trigger "::" impl.cpp 10 5
```

### 输出格式（一行 JSON 数组）

```json
[
  {
    "label": "printf",
    "kind": "function",
    "detail": "int printf(const char *fmt, ...)",
    "insert": "printf",
    "doc": "Writes formatted output to stdout.\\n\\nReturns the number of ...",
    "deprecated": false,
    "additionalTextEdits": [...]
  },
  ...
]
```

顶部 metadata：
```
serena-cli completion --json-meta impl.cpp 10 5
```
输出：
```json
{"truncated": "5 of 23", "triggerKind": "TriggerCharacter", "items": [...]}
```

### daemon HTTP 端点

复用现有 `POST /tools/{name}`，新增 `"completion"` 分支。请求体：
```json
{
  "project_root": "...",
  "args": {
    "file": "impl.cpp",
    "line": 10,
    "col": 5,
    "limit": 5,
    "trigger": "."   // 可选
  }
}
```

响应 `data`：`{"truncated": "...", "items": [...]}`（同 CLI --json-meta 形态）

---

## 5. 架构分层

| 层 | 改动 |
|---|---|
| `crates/supervisor/src/lib.rs` | 加 `tool_completion(root, file, line, col, limit, trigger) -> CompletionResponse` |
| `crates/supervisor/src/lib.rs` | `SupervisorTrait::execute_tool` 加 `"completion"` 分支 |
| `crates/daemon/src/dto.rs` | 无改动（用泛型 Value 透传）|
| `crates/cli/src/main.rs` | 加 `Completion { file, line, col }` 子命令 + `--limit / --trigger` + 字段裁剪逻辑 |
| `crates/supervisor/tests/e2e_completion.rs` | 真 clangd e2e（fixture 加 hello.cpp 触发位置）|

### 字段裁剪位置

**在 supervisor 层裁剪**而非 CLI 层：
- 优势：daemon HTTP 转发也享受裁剪；wire 上数据量小
- 实现：`tool_completion` 返回 `serde_json::Value`（已裁剪好的形态），daemon 透传

### CompletionResponse 结构

```rust
pub struct CompletionItemLite {
    pub label: String,
    pub kind: String,           // "function" / "variable" / "method" / ...
    pub detail: Option<String>,
    pub insert: Option<String>, // insertText or label
    pub doc: Option<String>,    // truncated to 200 chars
    pub deprecated: bool,
    pub additional_text_edits: Vec<TextEdit>,
}

pub struct CompletionResponse {
    pub truncated: Option<String>,  // "5 of 23" if truncated
    pub items: Vec<CompletionItemLite>,
}
```

---

## 6. 实施步骤（commit 顺序）

### Step 1: supervisor tool_completion（TDD）

```rust
pub async fn tool_completion(
    &self,
    root: &Path,
    file: &str,
    line: u32,
    col: u32,
    limit: usize,
    trigger: Option<&str>,
) -> ToolResult<CompletionResponse>
```

测试（先 RED）：
1. 字段裁剪：返回 N 项含 label/kind/insert；不含 sortText/filterText
2. limit 默认 5：`fixture x.h x` 触发 → items.len() ≤ 5 + truncated 字段
3. kind map："Function" → "function"
4. doc 截断：mock 一个长 doc 的 CompletionItem → 200 char cut
5. trigger=Some(".")：params 包含 triggerCharacter

### Step 2: SupervisorTrait "completion" 分支

```rust
"completion" => {
    let file = args.get("file")...;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
    let trigger = args.get("trigger").and_then(|v| v.as_str());
    let resp = self.tool_completion(root, file, line, col, limit, trigger).await?;
    serde_json::to_value(resp).map_err(...)
}
```

### Step 3: CLI 子命令 + 触发字符推断

```rust
fn infer_trigger_char(file: &str) -> Option<&'static str> {
    let ext = file.rsplit('.').next()?;
    match ext {
        "cpp" | "c" | "h" | "hpp" | "cc" | "cxx" => Some("."),
        "rs" => Some("::"),
        "ts" | "tsx" | "js" | "jsx" => Some("."),
        "py" => Some("."),
        _ => None,
    }
}
```

### Step 4: fixture + e2e

`fixtures/cpp_demo/hello.cpp`：
```cpp
#include <stdio.h>

int main() {
    print  // <- 触发位置，agent 想要补成 printf
}
```

测试：
```rust
#[tokio::test]
async fn completion_returns_related_items() {
    if !has_clangd() { return; }
    let root = fixtures_root();
    let sup = Supervisor::direct().await.unwrap();
    let resp = sup.tool_completion(&root, "hello.cpp", 4, 9, 5, Some(".")).await.unwrap();
    assert!(resp.items.iter().any(|i| i.label == "printf"));
}
```

---

## 7. 文件改动预估

| 文件 | 行数 | 说明 |
|---|---|---|
| `crates/supervisor/src/lib.rs` | +120 | `tool_completion` + trait 分支 |
| `crates/supervisor/src/types.rs` | +30 | `CompletionItemLite` + `CompletionResponse`（或放 lib.rs 内） |
| `crates/cli/src/main.rs` | +80 | 子命令 + 字段裁剪 + trigger 推断 |
| `fixtures/cpp_demo/hello.cpp` | +8 | fixture |
| `crates/supervisor/tests/e2e_completion.rs` | +60 | e2e |
| **合计** | **~300** | 1 个 commit |

Commit message：`feat(supervisor): AI-friendly code completion (M2 #1)`

---

## 8. 风险与边界

1. **doc 截断的"字符"定义** —— UTF-8 字节 vs Unicode codepoint？选 `chars().take(200).collect()`。
2. **trigger char 不在文件后缀表时** —— `trigger = None`，让 LSP 默认行为（user typed text 后请求）。
3. **`additionalTextEdits` 大对象** —— clangd 会包含完整 import 路径。保留不裁剪（agent 需要）。
4. **snippet 模式**（kind=Snippet 的 insertText 含 `$0/$1` 占位符） —— 不解析，agent 自己处理。
5. **MSYS bash 测试环境** —— 同 Task 16：e2e 真 clangd 在 MSYS 下 spawn daemon 不稳，但 `--direct` 路径测试稳定。e2e 用 `--direct`。

---

## 9. 验收

- `cargo test --workspace`：+5 测试（tool_completion unit + e2e）
- `cargo build --release`：单 exe 仍 < 7 MB
- 手动验证：
  ```bash
  # fixture hello.cpp 第 4 行 `print` 触发位置
  ./target/release/cli.exe completion fixtures/cpp_demo/hello.cpp 4 5
  ```
  输出 `printf` / `puts` 等项

---

## 10. 不做的事

- 不支持 resolve（`completionItem/resolve`）—— agent 拿到详细 doc 已够
- 不实现 fuzzy filter —— clangd 内部已做
- 不实现 triggerKind=3 (TriggerForIncompleteCompletions) —— M2 不需要
- 不实现 TextEdit 智能合并 —— 透传 LSP 原值

---

## 总结（一句话）

**把 LSP completion 包成"5 条最相关 + 一行 JSON + trigger 自动推断"** 的 CLI，给 AI agent 用，省 token，省 round-trip。
