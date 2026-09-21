# C refs --grouped 分组翻页

**Goal:** 大结果按外层符号（容器/函数）分组计数 + sample，AI 看 1KB 内掌握引用热度，免翻数百条 refs。

**Architecture:** find_referencing_symbols（既有）按 `container_name` 分桶，每桶返回 `{container, file, count, samples[]}`（samples 最多 3 条），总桶数可控。CLI 加 `--grouped` flag；不加时退化为既有 `Vec<RefSymbolHit>` 形态。`--page N` 配合 `--page-size`（默认 20）翻页。

**Tech Stack:** supervisor ref_tools, lsp_types.

**Spec:** `local/ai-token-features-design.md` §10-C / bd `C-refs-grouped`.

**Pre-conditions:**
- RefSymbolHit 已有 `container_name` 字段（grep 确认）
- find_referencing_symbols 返回 `Vec<RefSymbolHit>`

**Global Constraints:** wire 错误码不变；锁纪律短临界区；不新增协议位。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs:3587`（execute_tool "find-referencing-symbols" 分支）
- `crates/cli/src/main.rs`（FindReferencingSymbols 子命令加 `--grouped` / `--page` / `--page-size`）

---

## Task 1: GroupedRef 分组数据结构 + 分桶逻辑

**Files:**
- Modify: `crates/supervisor/src/ref_tools.rs`

**Step 1:** 加数据结构：

```rust
#[derive(Debug, Serialize)]
pub struct RefGroup {
    pub container: Option<String>,  // None 表示顶层
    pub file: String,
    pub count: usize,
    pub samples: Vec<RefSymbolHit>,
}

#[derive(Debug, Serialize)]
pub struct GroupedRefReport {
    pub total: usize,
    pub group_count: usize,
    pub page: usize,
    pub page_size: usize,
    pub groups: Vec<RefGroup>,
}

pub fn group_refs(
    hits: Vec<RefSymbolHit>,
    page: usize,
    page_size: usize,
) -> GroupedRefReport {
    use std::collections::BTreeMap;
    let total = hits.len();
    // key = (container_name, file) 保序：BTreeMap 排序；按容器名优先，文件次之
    let mut buckets: BTreeMap<(Option<String>, String), Vec<RefSymbolHit>> = BTreeMap::new();
    for h in hits {
        let key = (h.container_name.clone(), h.file.clone());
        buckets.entry(key).or_default().push(h);
    }
    let all_groups: Vec<RefGroup> = buckets.into_iter().map(|((container, file), mut hits)| {
        let count = hits.len();
        // sample 取前 3 条
        hits.truncate(3);
        RefGroup { container, file, count, samples: hits }
    }).collect();
    let group_count = all_groups.len();
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let groups = all_groups.into_iter().skip(start).take(page_size).collect();
    GroupedRefReport { total, group_count, page, page_size, groups }
}
```

**Step 2:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 3:** commit：`git add crates/supervisor/src/ref_tools.rs && git commit -m "feat(supervisor): C refs --grouped 分桶结构 + 翻页"`

---

## Task 2: execute_tool 分支接入 + CLI flag

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3587`
- Modify: `crates/cli/src/main.rs`

**Step 1:** execute_tool "find-referencing-symbols" 末尾：

```rust
"find-referencing-symbols" => {
    let (file, line, col) = required_position(&args)?;
    let hits = self.tool_referencing_symbols(root, &file, line, col, lang).await?;
    let grouped = args.get("grouped").and_then(|v| v.as_bool()).unwrap_or(false);
    if grouped {
        let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let page_size = args.get("page_size").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let report = ref_tools::group_refs(hits, page, page_size);
        serde_json::to_value(report).map_err(...)
    } else {
        serde_json::to_value(hits).map_err(...)
    }
}
```

**Step 2:** CLI FindReferencingSymbols 子命令加 3 个 flag：

```rust
FindReferencingSymbols {
    file: String,
    line: u32,
    col: u32,
    #[arg(long)] grouped: bool,
    #[arg(long, default_value = "1")] page: usize,
    #[arg(long, default_value = "20")] page_size: usize,
},
```

匹配处把 grouped/page/page_size 塞 args。

**Step 3:** `cargo build --workspace 2>&1 | tail -3` —— 0 errors。

**Step 4:** `cargo test -p supervisor --lib` —— 既有测试不回归（默认 grouped=false 走老路径）。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs crates/supervisor/src/ref_tools.rs crates/cli/src/main.rs && git commit -m "feat: C refs --grouped execute_tool + CLI 接入（默认 off，老路径不变）"`

---

## Task 3: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/ref_tools.rs`

**Step 1:** 写测试 `group_refs_aggregates_by_container_and_file`：

```rust
#[test]
fn group_refs_aggregates_by_container_and_file() {
    let hits = vec![
        RefSymbolHit { file: "a.rs".into(), container_name: Some("Foo".into()), ...},
        RefSymbolHit { file: "a.rs".into(), container_name: Some("Foo".into()), ...},
        RefSymbolHit { file: "b.rs".into(), container_name: Some("Foo".into()), ...},
        RefSymbolHit { file: "a.rs".into(), container_name: Some("Bar".into()), ...},
        RefSymbolHit { file: "c.rs".into(), container_name: None, ...},
    ];
    let r = group_refs(hits, 1, 20);
    assert_eq!(r.total, 5);
    assert_eq!(r.group_count, 4);  // (Foo, a) + (Foo, b) + (Bar, a) + (None, c)
    // 第一组应是 (None, c) 因 BTreeMap key 排序 None 在前
    assert!(r.groups[0].container.is_none());
}
```

**Step 2:** 写测试 `group_refs_pagination_works`：

```rust
#[test]
fn group_refs_pagination_works() {
    let hits: Vec<_> = (0..50).map(|i| RefSymbolHit { file: format!("f{i}.rs"), container_name: Some(format!("C{i}")), ...}).collect();
    let p1 = group_refs(hits.clone(), 1, 20);
    let p2 = group_refs(hits.clone(), 2, 20);
    let p3 = group_refs(hits, 3, 20);
    assert_eq!(p1.groups.len(), 20);
    assert_eq!(p2.groups.len(), 20);
    assert_eq!(p3.groups.len(), 10);
    assert_ne!(p1.groups[0].file, p2.groups[0].file);
}
```

**Step 3:** 写测试 `samples_capped_at_three_per_group`：

```rust
#[test]
fn samples_capped_at_three_per_group() {
    let hits: Vec<_> = (0..10).map(|i| RefSymbolHit { file: "x.rs".into(), container_name: Some("C".into()), ...}).collect();
    let r = group_refs(hits, 1, 20);
    assert_eq!(r.groups[0].count, 10);
    assert_eq!(r.groups[0].samples.len(), 3);
}
```

**Step 4:** `cargo test -p supervisor --lib group_refs` —— 全 PASS。

**Step 5:** commit：`git add crates/supervisor/src/ref_tools.rs && git commit -m "test(supervisor): C group_refs 分桶/翻页/sample 上限"`

---

## Task 4: e2e CLI

**Step 1:** 起 daemon；跑 `cli.exe --project rust_demo find-referencing-symbols lib.rs add 1 0 --grouped --page 1 --page-size 20 > /tmp/grp.json`
期望：`jq '.group_count >= 1, .groups[0].container, .groups[0].count'` 验证。

**Step 2:** 跑 `--grouped=false`（默认）确认老路径不变：`cli.exe --project rust_demo find-referencing-symbols lib.rs add 1 0 > /tmp/grp_off.json`
期望：返回 `Vec<RefSymbolHit>` 数组形态。

**Step 3:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib group_refs` | 3 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e grouped | jq `.groups` 数组非空 | ✓ |
| 既有不回归 | 默认 off 形态同老 | ✓ |

## 自我审查
- ✅ Spec §10-C 覆盖：分桶按 (container, file)；sample 上限 3；翻页 page+page_size；默认 off 兼容
- ✅ 占位符：无
- ✅ 一致：RefGroup / GroupedRefReport 命名一致
- ✅ 依赖：H/F2/B/A 可独立；J delta 后续
- ✅ 锁纪律：纯数据变换，无锁无 await
