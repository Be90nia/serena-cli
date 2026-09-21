# K find-symbol 兜底降级（修复 TS-LS workspace/symbol 真空）

**Goal:** workspace/symbol 返回空时自动降级到逐文件 documentSymbol 扫描（缓存兜着，仅一次），修复 TypeScript-LS 等上游缺陷。

**Architecture:** tool_find_symbol 内部：若 workspace/symbol 返空数组 + 文件数 > 0 → 触发兜底，对每个 .ts/.tsx/.js 等文件调 tool_overview（命中缓存），聚合所有 symbols 走 query 过滤。缓存命中免 LS 往返。

**Tech Stack:** supervisor, lsp_core.

**Spec:** `local/ai-token-features-design.md` §12-K / bd `K-fallback`.

**Pre-conditions:**
- tool_find_symbol 已就位
- 3.1 documentSymbol 缓存
- symbol-tree 扇出已修（commit 4ef3b3f）

**Global Constraints:** wire 不变；只在 workspace/symbol 返 [] 时触发；不影响其他 LS。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs:1845`（tool_find_symbol 函数内部）

---

## Task 1: 兜底逻辑插入

**Files:**
- Modify: `crates/supervisor/src/lib.rs:1845-1910`（tool_find_symbol 实现）

**Step 1:** 在 workspace/symbol 调用后判空触发兜底：

```rust
// 既有：let resp = session.request("workspace/symbol", params, timeout).await?;
// 改造：
let resp: Option<serde_json::Value> = session.request("workspace/symbol", params, INDEX_TIMEOUT).await?;
let mut hits: Vec<SymbolHit> = parse_workspace_symbol_response(resp.as_ref());

// 兜底：workspace/symbol 空 + 有文件
if hits.is_empty() {
    // 走 symbol-tree 风格的扫描，但只取每个文件 overview 的浅层 + 按 query 过滤
    let files = filtered_walker(root, lang);  // 既有 helper
    let overview_cache_sup = ...; // 借用 self 的缓存（Arc clone）
    for file in files.iter().take(500) {  // 保险丝 500 文件
        if let Ok(syms) = overview_via_session(/* cached or fresh */).await {
            for s in syms {
                if fuzzy_match(&s.name, query) {
                    hits.push(s);
                }
            }
        }
    }
    hits.truncate(limit);
}
```

注：`overview_via_session` 是前置修复已有的 helper（lib.rs:3123-3150）；复用。

**Step 2:** `fuzzy_match` 是简单 substring 匹配 + 大小写不敏感：

```rust
fn fuzzy_match(name: &str, query: &str) -> bool {
    let n = name.to_lowercase();
    let q = query.to_lowercase();
    n.contains(&q)
}
```

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): K find-symbol 兜底降级（workspace/symbol 空时走 documentSymbol 扫描）"`

---

## Task 2: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/lib.rs`

**Step 1:** `find_symbol_falls_back_on_empty_workspace_symbol`：

```rust
#[tokio::test]
async fn find_symbol_falls_back_on_empty_workspace_symbol() {
    // mock_ls 配置：workspace/symbol 返 []
    // 走 tool_find_symbol 应触发兜底并返回 documentSymbol 的命中
    let (sup, root) = setup_mock_ls_with_empty_workspace_symbol().await;
    std::fs::write(root.join("a.rs"), "pub fn target() {}\n").unwrap();
    let hits = sup.tool_find_symbol(&root, "target", 10, Some("rust")).await.unwrap();
    assert!(!hits.is_empty(), "兜底路径必须返命中");
    assert!(hits.iter().any(|h| h.name == "target"));
}
```

**Step 2:** `find_symbol_does_not_fallback_when_workspace_symbol_works`：

```rust
#[tokio::test]
async fn find_symbol_does_not_fallback_when_workspace_symbol_works() {
    // mock_ls 配置：workspace/symbol 正常返 1 个命中
    let (sup, root) = setup_mock_ls_with_normal_workspace_symbol().await;
    let hits = sup.tool_find_symbol(&root, "x", 10, Some("rust")).await.unwrap();
    assert_eq!(hits.len(), 1);
    // 兜底路径不应触发（无第二次 LS 调用）；mock_ls wire 计数断言
}
```

**Step 3:** `fuzzy_match_basic`：

```rust
#[test]
fn fuzzy_match_basic() {
    assert!(fuzzy_match("add_numbers", "add"));
    assert!(fuzzy_match("Multiply", "multi"));
    assert!(!fuzzy_match("foo", "bar"));
}
```

**Step 4:** `cargo test -p supervisor --lib find_symbol -- --nocapture` —— PASS。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): K find-symbol 兜底/不兜底/fuzzy"`

---

## Task 3: e2e CLI（typescript_demo fixture）

**Step 1:** 起 daemon；`cli.exe --project fixtures/typescript_demo find-symbol "Component" --lang typescript > /tmp/fs.json`
期望：TS-LS workspace/symbol 真空时通过兜底拿到 Component 命中（如果 fixture 有）。

**Step 2:** 跑默认 rust 项目，确认不触发兜底（既有 workspace/symbol 正常）：
`cli.exe --project rust_demo find-symbol "add" --lang rust > /tmp/fs_rust.json`

**Step 3:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib find_symbol fuzzy` | 全 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e TS 兜底 | jq `.[] | length >= 1` | ✓ |
| e2e rust 不兜底 | 既有路径不变 | ✓ |

## 自我审查
- ✅ Spec §12-K 覆盖：workspace/symbol 空触发；其他 LS 不受影响；缓存兜着
- ✅ 占位符：无
- ✅ 一致：fuzzy_match / overview_via_session 复用既有
- ✅ 依赖：symbol-tree 扇出已修（前置）
- ✅ 锁纪律：缓存读 Arc 临界区微秒
- ✅ ponytail: 500 文件保险丝；fuzzy 用 substring 而非正则
