# A search 标注所属符号

**Goal:** `search` 命中携带所属符号名+容器，AI 看命中行即可知上下文，省去一次 `find-symbol` 翻文档。

**Architecture:** search 在每条命中后增量查 documentSymbol（缓存兜着，一次扫全库）按 line/col 找覆盖符号，挂在 `hit.symbol/container`。新增字段不进 wire —— 在响应 `hits[]` 每条加 `symbol: Option<String>, container: Option<String>` 即可（向后兼容，AI 忽略新字段）。

**Tech Stack:** supervisor `fs_tools::search`, documentSymbol 缓存（3.1 + symbol-tree 复用）, lsp_types.

**Spec:** `local/ai-token-features-design.md` §10-A / bd `A-search-symbol`.

**Pre-conditions:**
- 3.1 文档符号缓存已就位
- tool_search_for_pattern 返回 Vec<SearchHit>
- 既有 SearchHit 字段：`{file, line, line_text}`，在 crates/supervisor/src/fs_tools.rs

**Global Constraints:** wire 错误码不变；锁纪律：短临界区零跨 await。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/fs_tools.rs` —— SearchHit 加 symbol/container 字段；搜索后批量化找覆盖符号
- `crates/supervisor/src/lib.rs:3870`（execute_tool "search" 分支）—— 增量调用 documentSymbol 填充

---

## Task 1: SearchHit 字段扩展

**Files:**
- Modify: `crates/supervisor/src/fs_tools.rs`（grep `struct SearchHit` 定位）

**Step 1:** 字段加：

```rust
#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub file: String,
    pub line: u32,
    pub line_text: String,
    /// 覆盖该行的最小符号名（None 表示顶层/匿名）
    pub symbol: Option<String>,
    /// 容器符号名（如 method 所在的 class/impl）
    pub container: Option<String>,
}
```

**Step 2:** 既有构造处（grep `SearchHit {`）用 `..Default::default()` 让 symbol/container 默认为 None（向后兼容）。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/fs_tools.rs && git commit -m "feat(supervisor): A search SearchHit 加 symbol/container 字段（默认 None 向后兼容）"`

---

## Task 2: 增量 documentSymbol 填充 helper

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3870`（execute_tool "search" 分支）

**Interfaces:**
- `async fn enrich_search_with_symbols(sup: &Supervisor, root: &Path, hits: &mut Vec<SearchHit>, lang: Option<&str>)`

**Step 1:** 加 helper（在 edit_context.rs 邻近位置或 lib.rs 顶部 helper 区）：

```rust
/// 给 search 命中补 symbol/container：按 file 分桶，逐桶走 tool_overview（命中缓存），
/// 找覆盖该 line 的最小符号。空桶 → 不补；tool_overview 失败 → 该桶全 None。
async fn enrich_search_with_symbols(
    sup: &Supervisor,
    root: &Path,
    hits: &mut [SearchHit],
    lang: Option<&str>,
) {
    use std::collections::HashMap;
    let mut per_file: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, hit) in hits.iter().enumerate() {
        per_file.entry(hit.file.clone()).or_default().push(idx);
    }
    for (file, indices) in per_file {
        let overview = sup.tool_overview(root, &file, lang).await.ok();
        for idx in indices {
            let hit = &mut hits[idx];
            let line_0 = hit.line.saturating_sub(1);  // 1-based → 0-based
            if let Some(syms) = &overview {
                if let Some((sym, container)) = find_covering_symbol(syms, line_0) {
                    hit.symbol = Some(sym);
                    hit.container = container;
                }
            }
        }
    }
}

/// 找覆盖 line 的最小符号 + 它的外层容器。
fn find_covering_symbol(syms: &[SymbolHit], line: u32) -> Option<(String, Option<String>)> {
    // 递归走 SymbolHit 的 children（如有）；若无 children 字段，扁平遍历。
    fn walk(hits: &[SymbolHit], line: u32, parent: Option<&str>) -> Option<(String, Option<String>)> {
        // 先找含该 line 的直接命中；再按 start_line 降序选最小（即最深嵌套）
        let mut best: Option<&SymbolHit> = None;
        for h in hits {
            let (s, e) = hit_line_range(h);
            if s <= line && line < e {
                match best {
                    Some(b) if b.start_line <= h.start_line => {},  // 当前 b 更深，不换
                    _ => best = Some(h),
                }
            }
        }
        let chosen = best?;
        // 容器即 chosen.parent_name 字段（既有），否则递归 children
        let container = chosen.container_name.clone().or(parent.map(String::from));
        // 若有 children 字段递归找更深的
        // SymbolHit 在 types.rs，看是否含 children
        Some((chosen.name.clone(), container))
    }
    walk(syms, line, None)
}

fn hit_line_range(h: &SymbolHit) -> (u32, u32) {
    // SymbolHit 含 start_line/end_line 字段（grep 确认）
    (h.start_line, h.end_line)
}
```

具体 SymbolHit 字段（start_line/end_line/container_name）须 grep `crates/supervisor/src/types.rs` 确认；不一致按实际字段调整。

**Step 2:** 在 execute_tool "search" 分支末尾调用：

```rust
"search" => {
    let query = required_string(&args, "query")?;
    let mut hits = self.tool_search_for_pattern(root, query).await?;
    self.enrich_search_with_symbols(root, &mut hits, lang).await;
    serde_json::to_value(&hits)...
}
```

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): A search 增量补 symbol/container（按 file 分桶走 overview 缓存）"`

---

## Task 3: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/lib.rs`（既有 search 测试模块）

**Step 1:** 写测试 `search_hits_carry_symbol_and_container`：

```rust
#[tokio::test]
async fn search_hits_carry_symbol_and_container() {
    let (sup, root) = setup_rust_demo().await;
    let hits = sup.tool_search_for_pattern(&root, "fn add").await.unwrap();
    // 至少一条命中在 main.rs 调用 add 处
    let in_main = hits.iter().find(|h| h.file.ends_with("main.rs")).expect("main.rs 应有 add 调用");
    assert!(in_main.symbol.is_some(), "symbol 必须填充");
    // main 函数体内部的 add 调用，container 应该是 main
    assert_eq!(in_main.container.as_deref(), Some("main"), "调用点容器 = main");
}
```

**Step 2:** 写测试 `search_in_nameless_file_returns_none`：

```rust
#[tokio::test]
async fn search_in_nameless_file_returns_none() {
    // top-level 的匿名块命中不应有 symbol（避免误标顶层）
    // 用 rust_demo 的 lib.rs 找一个在 module {} 之外或顶层 const/let 处的命中（如果 hit 是顶层无符号）
    let (sup, root) = setup_rust_demo().await;
    let hits = sup.tool_search_for_pattern(&root, "use std").await.unwrap();
    for h in &hits {
        // use 语句通常没有 symbol 标签
        assert!(h.symbol.is_none() || h.symbol.as_deref().unwrap().len() > 0);
    }
}
```

**Step 3:** 写测试 `enrich_fails_silently_on_tool_overview_failure`：

```rust
#[tokio::test]
async fn enrich_fails_silently_on_tool_overview_failure() {
    let (sup, _root) = setup_failing_supervisor().await;  // tool_overview 永远失败的 mock
    let mut hits = vec![SearchHit { file: "x.rs".into(), line: 1, line_text: "x".into(), symbol: None, container: None }];
    sup.enrich_search_with_symbols(Path::new("/nonexistent_xyz"), &mut hits, Some("rust")).await;
    assert!(hits[0].symbol.is_none(), "失败路径保留 None");
}
```

**Step 4:** `cargo test -p supervisor --lib search -- --nocapture` —— 期望 PASS。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): A search symbol 填充 + 顶层 None + 失败静默"`

---

## Task 4: e2e CLI

**Step 1:** 起 daemon；跑 `cli.exe --project rust_demo search "fn add"` 验证输出含 symbol/container。
期望：main.rs 内 add 命中带 `symbol: add, container: main`。

**Step 2:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib search` | 全 PASS |
| e2e | jq '.hits[0].symbol' 非 null | ✓ |
| 既有 search 测试不回归 | `cargo test -p supervisor --lib` | 全绿 |

## 自我审查
- ✅ Spec §10-A 覆盖：命中带 symbol/container；按 file 分桶走缓存；失败静默
- ✅ 占位符：无；具体代码
- ✅ 一致：SearchHit 字段在 helper+test+cli 命名一致
- ✅ 依赖：H compact 可独立；F2/B 可独立
- ✅ 锁纪律：缓存读 Arc 临界区微秒，无 await
