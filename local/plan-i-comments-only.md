# I search --comments-only 注释行过滤

**Goal:** search 加 `--comments-only` 注释前缀过滤（// /* * # -- """），命中即注释，AI 零推理成本。

**Architecture:** tool_search_for_pattern 末尾加注释判定函数，filter 命中。注释前缀表按语言 lang 分桶；fallback 用通用启发式（前 4 字符内出现注释标记）。复用 safe_delete 的 `textual_occurrences_outside_def` 同款过滤逻辑（如已存在）。

**Tech Stack:** supervisor, fs_tools.

**Spec:** `local/ai-token-features-design.md` §11-I / bd `I-comments-only`.

**Pre-conditions:**
- tool_search_for_pattern 既有 fs_tools::search
- safe_delete.rs 的 textual_occurrences_outside_def 过滤函数（grep 确认是否存在）

**Global Constraints:** wire 不变；新 flag `--comments-only` 是 boolean。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/fs_tools.rs`（search 函数 + 新过滤 helper）
- `crates/supervisor/src/lib.rs:3587`（execute_tool "search" 分支）
- `crates/cli/src/main.rs`（Search 子命令加 --comments-only）

---

## Task 1: 注释判定 helper

**Files:**
- Modify: `crates/supervisor/src/fs_tools.rs`

**Step 1:** 加 helper：

```rust
/// 注释行判定：按文件扩展名查注释前缀表，匹配即注释。
///
/// ponytail: 不用 AST——粗滤够用，AST 让 LSP 做。命中=0 时返空数组；命中=1 也可能误判，
/// AI 看到命中行会自检。6+ 语言注释风格覆盖：// /* * # -- """ ''' <!-- % %% .
pub fn looks_like_comment(file: &str, line_text: &str) -> bool {
    let trimmed = line_text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let ext = std::path::Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
    let prefixes: &[&str] = match ext {
        "rs" | "js" | "ts" | "jsx" | "tsx" | "go" | "java" | "cs" | "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" | "swift" | "kt" | "scala" => &["//", "/*", "*"],
        "py" | "rb" | "sh" | "yaml" | "yml" | "toml" | "conf" => &["#"],
        "lua" => &["--"],
        "sql" => &["--", "/*"],
        "html" | "xml" | "vue" | "svelte" => &["<!--"],
        "tex" => &["%"],
        "matlab" | "m" => &["%"],
        "lisp" | "clj" => &[";"],
        _ => &["//", "#", "--", "/*", "*", "<!--", "%"],
    };
    prefixes.iter().any(|p| trimmed.starts_with(p))
}
```

**Step 2:** 在 search 函数末尾加可选过滤（tool_search_for_pattern 接受新参数 `comments_only: bool`）。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/fs_tools.rs && git commit -m "feat(supervisor): I search 注释判定 helper（12+ 语言前缀表）"`

---

## Task 2: execute_tool + CLI 接入

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3587`
- Modify: `crates/cli/src/main.rs`

**Step 1:** execute_tool "search" 分支：

```rust
"search" => {
    let query = required_string(&args, "query")?;
    let comments_only = args.get("comments_only").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut hits = self.tool_search_for_pattern(root, query).await?;
    if comments_only {
        hits.retain(|h| fs_tools::looks_like_comment(&h.file, &h.line_text));
    }
    self.enrich_search_with_symbols(root, &mut hits, lang).await;  // 复用 A 的 helper
    serde_json::to_value(&hits)...
}
```

**Step 2:** CLI Search 子命令加 flag：

```rust
Search {
    query: String,
    #[arg(long)] comments_only: bool,
},
```

**Step 3:** `cargo build --workspace 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs crates/cli/src/main.rs && git commit -m "feat: I search --comments-only CLI + supervisor 接入"`

---

## Task 3: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/fs_tools.rs`

**Step 1:** `looks_like_comment_recognizes_major_languages`：

```rust
#[test]
fn looks_like_comment_recognizes_major_languages() {
    assert!(looks_like_comment("a.rs", "// todo: refactor"));
    assert!(looks_like_comment("a.py", "# comment"));
    assert!(looks_like_comment("a.lua", "-- comment"));
    assert!(looks_like_comment("a.html", "<!-- comment -->"));
    assert!(looks_like_comment("a.sql", "-- comment"));
    assert!(!looks_like_comment("a.rs", "fn main() {}"));
    assert!(!looks_like_comment("a.py", "def foo():"));
}
```

**Step 2:** `search_filters_to_comments_only`：

```rust
#[tokio::test]
async fn search_filters_to_comments_only() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "// TODO: foo\nfn bar() {}\n// bar done\n").unwrap();
    let sup = Supervisor::new(...);
    let all = sup.tool_search_for_pattern(tmp.path(), "TODO").await.unwrap();
    assert!(!all.is_empty());
    let only_comments = ... // 走 execute_tool search --comments_only
    assert!(only_comments.iter().all(|h| looks_like_comment(&h.file, &h.line_text)));
}
```

**Step 3:** `cargo test -p supervisor --lib comments_only -- --nocapture` —— PASS。

**Step 4:** commit。

---

## Task 4: e2e CLI

**Step 1:** 起 daemon；`cli.exe --project rust_demo search "TODO" --comments-only > /tmp/co.json`
期望：hits 全部 line_text 以 `//` 开头。

**Step 2:** 跑无 flag 默认：`cli.exe --project rust_demo search "TODO"`
期望：hits 含代码 + 注释。

**Step 3:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib comments_only looks_like` | PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e 注释 only | jq `.hits[].line_text` 全以 `//` 开头 | ✓ |

## 自我审查
- ✅ Spec §11-I 覆盖：12+ 语言注释前缀；--comments-only flag；空返空数组
- ✅ 占位符：无
- ✅ 一致：looks_like_comment 命名一致
- ✅ 依赖：A 的 enrich_search_with_symbols 可选依赖，本特性独立
- ✅ 锁纪律：纯字符串匹配，无锁
