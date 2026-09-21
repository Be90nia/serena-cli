# G --max-tokens 预算护栏 + --compress 签名压缩

**Goal:** 全局 token 预算护栏在工具返回前截断；签名压缩去容器+kind 冗余。AI 看 1KB 内掌握大结果结构。

**Architecture:** 在 execute_tool 末尾插入统一后处理层：(1) 若 args._max_tokens 设了，按 `serde_json::to_vec(value).len() / 4`（近似 token 数）截断到 ≤N，超截断返 `truncated: true, items: [...前N条]`；（2) 若 args._compress = true，去除 hit.symbol/container 等"二级"字段。退出码不变（截断是成功语义）。

**Tech Stack:** supervisor, serde_json.

**Spec:** `local/ai-token-features-design.md` §10-G / bd `G-budget`.

**Pre-conditions:**
- 各工具稳定：F2/H/B/A/C/E 全部完成
- serde_json 既有依赖

**Global Constraints:** wire 错误码不变；退出码不变；截断是 success 语义（非 error）。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs:3557`（execute_tool 末尾统一后处理）

---

## Task 1: budget + compress 后处理层

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3950-3960`（execute_tool 结束位置）

**Interfaces:**
- `fn apply_budget(value: &mut serde_json::Value, max_tokens: usize) -> bool /* was_truncated */`
- `fn apply_compress(value: &mut serde_json::Value)`

**Step 1:** 加 helper：

```rust
/// 工具响应 token 预算：超出则截断 items 数组 + 加 truncated=true。
/// 估算：4 字节 ≈ 1 token（BPE 粗略近似；与上游 opencode 约定一致）。
/// ponytail: 不做精确 BPE——预算护栏是 soft limit，精确度不是核心。
fn apply_budget(value: &mut serde_json::Value, max_tokens: usize) -> bool {
    let budget_bytes = max_tokens * 4;
    let current = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    if current <= budget_bytes {
        return false;
    }
    // 截断 items 数组
    if let Some(items) = value.get_mut("items").and_then(|v| v.as_array_mut()) {
        let original_len = items.len();
        // 二分查找最大保留数
        let mut lo = 0usize;
        let mut hi = items.len();
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let trial = serde_json::json!({ "items": &items[..mid] });
            let trial_bytes = serde_json::to_vec(&trial).map(|v| v.len()).unwrap_or(0);
            // 加上 truncated=true 标志 (约 18 bytes) 的开销
            if trial_bytes + 18 <= budget_bytes {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        items.truncate(lo);
        if let Some(obj) = value.as_object_mut() {
            obj.insert("truncated".into(), serde_json::json!(true));
            obj.insert("original_count".into(), serde_json::json!(original_len));
        }
        return true;
    }
    false
}

/// 签名压缩：去容器 + kind 字段（保留 name + location）；按需未来扩展。
fn apply_compress(value: &mut serde_json::Value) {
    fn strip(v: &mut serde_json::Value) {
        if let Some(arr) = v.as_array_mut() {
            for item in arr.iter_mut() { strip(item); }
        } else if let Some(obj) = v.as_object_mut() {
            obj.remove("container");
            obj.remove("container_name");
            obj.remove("kind");
            // 递归 items
            if let Some(items) = obj.get_mut("items") {
                strip(items);
            }
            for (_, sub) in obj.iter_mut() {
                strip(sub);
            }
        }
    }
    strip(value);
}
```

**Step 2:** 在 execute_tool 末尾统一调：

```rust
// 所有分支 return Ok(value) 之前不调（分支内 return 太散）；
// 改为改 match 整体结构：把 match 表达式赋给 value，再统一后处理。
// 简化：在每个分支末尾不再直接 return，统一一个变量 + 末尾处理。
let mut value: serde_json::Value = match tool {
    // ... 所有 match 分支都把 Ok 包成 Ok(value)，分支内不再调 serde_json::to_value
};
// 后处理
if let Some(max) = args.get("_max_tokens").and_then(|v| v.as_u64()) {
    let was_truncated = apply_budget(&mut value, max as usize);
    let _ = was_truncated;  // 已写入 value.truncated 标志
}
if args.get("_compress").and_then(|v| v.as_bool()).unwrap_or(false) {
    apply_compress(&mut value);
}
return Ok(value);
```

实际：当前 match 各分支直接返回 `Ok(value)`，重构需把所有分支改为 `let value = ...; value`（最后整体赋）。**简化**：把每个分支的 Ok 包成 `Ok(value)`，然后在 match 末尾统一处理。PM 接受适度重构。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): G token 预算护栏 + 签名压缩（_max_tokens/_compress 全局后处理）"`

---

## Task 2: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/lib.rs`

**Step 1:** `apply_budget_truncates_items_when_exceeded`：

```rust
#[test]
fn apply_budget_truncates_items_when_exceeded() {
    let mut v = serde_json::json!({
        "items": (0..100).map(|i| serde_json::json!({"name": format!("f{}", i), "container": "x"})).collect::<Vec<_>>(),
    });
    let truncated = apply_budget(&mut v, 50);  // 200 bytes 预算
    assert!(truncated);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["original_count"], 100);
    assert!(v["items"].as_array().unwrap().len() < 100);
}
```

**Step 2:** `apply_budget_passes_through_when_under`：

```rust
#[test]
fn apply_budget_passes_through_when_under() {
    let mut v = serde_json::json!({"items": [{"name": "x"}]});
    let truncated = apply_budget(&mut v, 10000);
    assert!(!truncated);
    assert!(v.get("truncated").is_none());
}
```

**Step 3:** `apply_compress_removes_container_and_kind`：

```rust
#[test]
fn apply_compress_removes_container_and_kind() {
    let mut v = serde_json::json!({
        "items": [
            {"name": "x", "container": "Foo", "kind": "Function", "file": "a.rs"},
            {"name": "y", "container_name": "Bar", "kind": "Method", "file": "b.rs"},
        ],
    });
    apply_compress(&mut v);
    let items = v["items"].as_array().unwrap();
    assert!(items[0].get("container").is_none());
    assert!(items[0].get("kind").is_none());
    assert!(items[1].get("container_name").is_none());
    assert_eq!(items[0]["name"], "x");
}
```

**Step 4:** `cargo test -p supervisor --lib apply_budget apply_compress -- --nocapture` —— 全 PASS。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): G budget 截断/透传 + compress 字段清理"`

---

## Task 3: e2e CLI

**Step 1:** 起 daemon；跑一个已知大结果的命令（如 find-referencing-symbols 在多 caller 的项目上）：
```bash
cli.exe --project rust_demo find-referencing-symbols lib.rs add 1 0 --max-tokens 200 --format json > /tmp/budget.json
jq '.truncated, .original_count, (.items | length)' /tmp/budget.json
```
期望：truncated=true 或 items 数 < 原始。

**Step 2:** 跑 `--compress`：
```bash
cli.exe --project rust_demo overview lib.rs --lang rust --compress --format json > /tmp/comp.json
jq '.items[0] | keys' /tmp/comp.json
```
期望：键集不含 container/kind。

**Step 3:** 跑默认（无 _max_tokens / 无 _compress）确认不回归：
```bash
cli.exe --project rust_demo overview lib.rs --lang rust > /tmp/default.json
jq '.items[0] | keys' /tmp/default.json  # 含 container + kind
```

**Step 4:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib apply_budget apply_compress` | 全 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e budget | jq `.truncated == true` | ✓ |
| e2e compress | jq 键集不含 container/kind | ✓ |
| 默认无 flag | 既有测试不回归 | ✓ |

## 自我审查
- ✅ Spec §10-G 覆盖：_max_tokens 截断 + _compress 字段压缩 + truncated 标志
- ✅ 占位符：无；具体代码
- ✅ 一致：apply_budget/apply_compress 在 helper+test+e2e 命名一致
- ✅ 依赖：F2/H/B/A/C/E 收口特性，本任务依赖它们稳定
- ✅ 锁纪律：纯数据变换，无锁
- ✅ wire 契约：HTTP 200 + 9 错误码不变；截断是 success 语义
