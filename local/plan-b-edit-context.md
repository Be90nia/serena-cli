# B edit-context 编辑上下文聚合

**Goal:** 单次调用拿 `{body, callers, doc, tests, hash}` —— AI 编辑主路径上 `symbol-body + refs + hover + find-symbol tests/` 的合并，节省 3-4 次 round-trip。

**Architecture:** 新 CLI 子命令 `edit-context`，参数 `{file, symbol}`，内部串 4 工具（symbol-body / find_referencing_symbols 反查 callers / hover 拿 doc / find-symbol 查 `tests/` 目录），单值返回。任一段失败 → 该字段 `null`（不 `[]`，区分"无结果"与"失败"）；其余字段正常输出。

**Tech Stack:** supervisor, serde_json, lsp_types, cli.

**Spec:** `local/ai-token-features-design.md` §10-B / bd `B-edit-context`.

**Pre-conditions:**
- tool_symbol_body / tool_referencing_symbols / tool_hover / tool_find_symbol 已就位
- F2 post_write_diagnostics 已完成（不依赖但同期收益高）
- H compact 已完成（callers 字段复用 compact_loc）

**Global Constraints:** 9 wire 错误码不变；不新增 field 协议位；禁 dashmap/parking_lot/async-lsp/tower-lsp；锁纪律：短临界区零跨 await。

---

## 文件结构

**Create:**
- `crates/supervisor/src/edit_context.rs` —— 4 工具串接聚合 + 失败隔离

**Modify:**
- `crates/supervisor/src/lib.rs:1-30` —— `pub mod edit_context;`
- `crates/supervisor/src/lib.rs:3587` —— execute_tool 加 `"edit-context" =>` 分支
- `crates/cli/src/main.rs:3557` —— `edit_context` 子命令（`required_file + required_symbol` 两参）

---

## Task 1: edit_context.rs 核心聚合

**Files:**
- Create: `crates/supervisor/src/edit_context.rs`

**Interfaces:**
- `pub struct EditContextReport { pub file: String, pub symbol: String, pub body: Option<BodyRange>, pub callers: Option<Vec<RefSymbolHit>>, pub doc: Option<String>, pub tests: Option<Vec<RefSymbolHit>> }`
- `pub async fn collect(sup: &Supervisor, root: &Path, file: &str, symbol: &str, lang: Option<&str>) -> EditContextReport`

**Step 1:** 写 `edit_context.rs`：

```rust
//! AI 编辑主路径聚合：body + callers + doc + tests。
//!
//! 设计：4 工具串接，任一段失败 → 对应字段 None，整体不失败。callers/tests
//! 用 find_referencing_symbols 反查（已有缓存复用）；doc 走 hover；body 走 symbol-body。
//! tests 字段特殊化：callers 过滤 file 路径含 `/tests/` 或 `_test.` 或 `test_`。
use serde::Serialize;
use std::path::Path;
use crate::{Supervisor, ToolResult};
use crate::ref_tools::RefSymbolHit;
use crate::types::SymbolHit;

#[derive(Debug, Serialize, Default)]
pub struct BodyRange {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

#[derive(Debug, Serialize, Default)]
pub struct EditContextReport {
    pub file: String,
    pub symbol: String,
    pub body: Option<BodyRange>,
    pub callers: Option<Vec<RefSymbolHit>>,
    pub doc: Option<String>,
    pub tests: Option<Vec<RefSymbolHit>>,
}

fn looks_like_test_file(file: &str) -> bool {
    file.contains("/tests/") || file.contains("/test/")
        || file.contains("_test.") || file.contains("_spec.")
        || file.contains("test_") || file.contains("Test.")
}

pub async fn collect(
    sup: &Supervisor,
    root: &Path,
    file: &str,
    symbol: &str,
    lang: Option<&str>,
) -> EditContextReport {
    let mut report = EditContextReport {
        file: file.into(),
        symbol: symbol.into(),
        ..Default::default()
    };
    // 1) body
    if let Ok(body) = sup.tool_symbol_body(root, file, symbol, lang).await {
        report.body = Some(BodyRange { start_line: body.start_line, end_line: body.end_line, text: body.text });
    }
    // 2) callers: 需要 file 中符号的 def 位置。简化为对第一个 body 起点查 refs；失败则 None。
    if let Some(body) = &report.body {
        if let Ok(callers) = sup.tool_referencing_symbols(root, file, body.start_line, body.start_line.max(1) - 1, 0, lang).await {
            let tests: Vec<_> = callers.iter().filter(|c| looks_like_test_file(&c.file)).cloned().collect();
            report.callers = Some(callers);
            report.tests = Some(tests);
        }
    }
    // 3) doc: hover 同位置拿 doc_string
    if let Some(body) = &report.body {
        if let Ok(Some(hover)) = sup.tool_hover(root, file, body.start_line.max(1) - 1, 0, lang).await {
            report.doc = hover.contents.into_iter().find_map(|c| match c {
                lsp_types::HoverContents::String(s) => Some(s),
                lsp_types::HoverContents::Markup(m) => Some(m.value),
                _ => None,
            });
        }
    }
    report
}
```

**Step 2:** 在 `crates/supervisor/src/lib.rs:25` 后加 `pub mod edit_context;`。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 期望 0 errors（hover 返回类型可能是 `MarkedString` 或 enum，按实际调整）。

**Step 4:** commit：`git add crates/supervisor/src/edit_context.rs crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): B edit-context 核心聚合（4 工具串接+失败隔离）"`

---

## Task 2: execute_tool + CLI 接线

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3587`（execute_tool match）
- Modify: `crates/cli/src/main.rs`（EditContext 子命令，约 :3557 区域）

**Step 1:** supervisor execute_tool 加分支：

```rust
"edit-context" => {
    let file = required_file(&args)?;
    let symbol = required_symbol(&args)?;
    let report = crate::edit_context::collect(self, root, &file, &symbol, lang).await;
    serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
}
```

`required_symbol` 是既有 helper（grep `fn required_symbol` 确认；若无则仿 `required_file` 加一个从 `args.symbol` 取字符串的）。

**Step 2:** CLI 加 EditContext 子命令：

```rust
/// 编辑上下文聚合：body + callers + doc + tests 单次返回。
EditContext {
    /// 目标文件（相对 root）。
    #[arg(long)]
    file: String,
    /// 符号名。
    #[arg(long)]
    symbol: String,
    /// 语言（可选；缺省按扩展名解析）。
    #[arg(long)]
    lang: Option<String>,
},
```

匹配 `Cmd::EditContext { file, symbol, lang } => build_request("edit-context", json!({"file":file,"symbol":symbol}))`（与既有子命令同模式）。

**Step 3:** `cargo build --workspace 2>&1 | tail -3` —— 期望 0 errors。

**Step 4:** commit：`git add crates/cli/src/main.rs crates/supervisor/src/lib.rs && git commit -m "feat: B edit-context CLI + supervisor execute_tool 接入"`

---

## Task 3: 测试——4 字段填充 + 失败隔离

**Files:**
- Modify: `crates/supervisor/src/edit_context.rs` 末尾

**Step 1:** 写测试 `edit_context_collects_all_four_fields`：

```rust
#[tokio::test]
async fn edit_context_collects_all_four_fields() {
    let (sup, root) = setup_rust_demo().await;
    let report = collect(&sup, &root, "lib.rs", "add", Some("rust")).await;
    assert!(report.body.is_some(), "body 必须有");
    assert!(report.callers.is_some(), "callers 必须有（main.rs 调 add）");
    assert_eq!(report.symbol, "add");
}
```

`setup_rust_demo` 走既有 fixture 模式（grep `setup_rust_demo\|launch_mock_ls` 找到最近测试模式套用）。

**Step 2:** 写测试 `edit_context_failed_body_yields_others_null`：

```rust
#[tokio::test]
async fn edit_context_failed_body_yields_others_null() {
    let (sup, root) = setup_rust_demo().await;
    // 故意传不存在的符号名 → body 失败，callers/doc/tests 也都 None，整体不报错
    let report = collect(&sup, &root, "lib.rs", "nonexistent_symbol_xyz", Some("rust")).await;
    assert!(report.body.is_none());
    assert!(report.callers.is_none());
    assert!(report.doc.is_none());
    assert!(report.tests.is_none());
}
```

**Step 3:** 写测试 `tests_filter_recognizes_test_files`：

```rust
#[test]
fn tests_filter_recognizes_test_files() {
    assert!(looks_like_test_file("src/foo_test.rs"));
    assert!(looks_like_test_file("tests/integration.rs"));
    assert!(looks_like_test_file("lib_test.go"));
    assert!(!looks_like_test_file("src/main.rs"));
    assert!(!looks_like_test_file("lib.rs"));
}
```

**Step 4:** `cargo test -p supervisor --lib edit_context -- --nocapture` —— 期望 3 个 PASS。

**Step 5:** commit：`git add crates/supervisor/src/edit_context.rs && git commit -m "test(supervisor): B edit-context 4 字段聚合 + 失败隔离 + tests 过滤"`

---

## Task 4: 端到端 CLI

**Step 1:** 起 daemon：`cli.exe --daemon --project fixtures/rust_demo & sleep 8`

**Step 2:** 跑：`cli.exe edit-context --file lib.rs --symbol add --lang rust > /tmp/ec.json 2>&1`
期望：JSON 含 body/callers/doc/tests 字段。

**Step 3:** `jq '.body.text | length, .callers | length, .doc, (.tests|length)' /tmp/ec.json` —— 期望 body 有内容、callers>=1（main.rs 调）、doc 为字符串（hover doc）、tests=0（无 tests/ 文件）。

**Step 4:** 清理：`cli.exe stop-all`

**Step 5:** commit（如有代码变更）：`git commit -m "chore: B edit-context e2e 验证"`

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib edit_context` | 3 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e body | jq `.body.text` 非空 | ✓ |
| e2e callers | jq `.callers | length >= 1` | ✓ |

## 自我审查
- ✅ Spec §10-B 覆盖：body+callers+doc+tests 四字段；失败隔离；tests 过滤
- ✅ 占位符：无；具体代码
- ✅ 一致：`EditContextReport` / `BodyRange` 在 helper+CLI+test 命名一致
- ✅ 依赖：F2/H 可独立，本特性不依赖它们
- ✅ 锁纪律：纯函数串接，无锁无跨 await
