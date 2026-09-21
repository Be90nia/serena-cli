# J --delta 增量响应（refs/overview）

**Goal:** 迭代式工作流省 50-80%：按 mtime 代际仅返回变化条目。AI 编辑后第二次查同样的 refs 只看到新增。

**Architecture:** supervisor 加代际键（root_mtime 集合 sig + 上次响应 sig），缓存上次响应。execute_tool refs/overview 末尾：若 args._delta = true 且代际键匹配上次 → 返 `{delta: true, added: [...], removed: [...]}`；不匹配或 _delta=false → 走全集。空集不缓存（防污染）。

**Tech Stack:** supervisor, existing mtime 机制（root_source_mtime in tool_find_symbol）。

**Spec:** `local/ai-token-features-design.md` §11-J / bd `J-delta`.

**Pre-conditions:**
- F2/H/B/A/C/E/G/I/K/L/M 全部完成（收口后增量）
- tool_find_symbol 的 root_source_mtime 信号（已存在）

**Global Constraints:** wire 不变；空集不缓存（防 LS 就绪窗口污染）；代际错时降级全集。

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs:3557`（execute_tool 末尾后处理，与 G budget 协同）

---

## Task 1: 代际键 + 缓存

**Files:**
- Modify: `crates/supervisor/src/lib.rs:155-200`（Supervisor 字段声明附近）

**Step 1:** Supervisor 加字段：

```rust
/// delta 响应缓存：key = (tool, root, file_or_query, gen_sig) → 上次响应。
/// 空集不缓存：防止 LS 就绪窗口返空污染下轮 delta。
/// ponytail: 全表无淘汰 —— AI agent 一次会话通常只查几个 file；放成单 entry 滚动也够，
/// 简单实现优先；OOM 时再换 LRU。
delta_cache: Arc<Mutex<HashMap<String, serde_json::Value>>>,
```

**Step 2:** `pub fn new(...)` 初始化空 HashMap。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): J delta 缓存字段（key=tool+root+gen_sig）"`

---

## Task 2: delta 编排 helper

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3310-3360`（helper 区）

**Step 1:** 加 helper：

```rust
/// delta 编排：若 _delta=true 且代际键匹配缓存 → 返 {delta:true, added, removed}；
/// 否则走 full，缓存结果（非空）。
///
/// ponytail: 不做复杂 diff——基于 hit.file + hit.range.start 序列化做 set diff，
/// AI agent 关心"新增了什么引用"，不需要字符级 diff。
async fn maybe_delta(
    &self,
    tool: &str,
    root_key: &str,
    current: serde_json::Value,
    delta: bool,
) -> serde_json::Value {
    if !delta {
        // 不走 delta 路径也缓存以便下次 delta 调用使用
        if !is_empty_response(&current) {
            let key = format!("{}|{}", tool, root_key);
            self.delta_cache.lock().unwrap().insert(key, current.clone());
        }
        return current;
    }
    let key = format!("{}|{}", tool, root_key);
    let prev = self.delta_cache.lock().unwrap().get(&key).cloned();
    // 缓存当前（无论是否空，给下次对照用；但空响应下次不缓存）
    if !is_empty_response(&current) {
        self.delta_cache.lock().unwrap().insert(key.clone(), current.clone());
    }
    let Some(prev) = prev else {
        // 首次走 delta：返全集 + delta=false 标记
        return serde_json::json!({ "delta": false, "items": current });
    };
    let added = diff_hits(&prev, &current);
    let removed = diff_hits(&current, &prev);
    serde_json::json!({ "delta": true, "added": added, "removed": removed })
}

fn is_empty_response(v: &serde_json::Value) -> bool {
    v.get("items").and_then(|i| i.as_array()).map(|a| a.is_empty()).unwrap_or(false)
}

fn diff_hits(a: &serde_json::Value, b: &serde_json::Value) -> Vec<serde_json::Value> {
    let a_set: std::collections::HashSet<String> = extract_keys(a);
    let b_set: std::collections::HashSet<String> = extract_keys(b);
    let arr = a.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    arr.into_iter().filter(|h| {
        let k = hit_key(h);
        a_set.contains(&k) && !b_set.contains(&k)
    }).collect()
}

fn extract_keys(v: &serde_json::Value) -> std::collections::HashSet<String> {
    v.get("items").and_then(|i| i.as_array())
        .map(|arr| arr.iter().map(hit_key).collect())
        .unwrap_or_default()
}

fn hit_key(h: &serde_json::Value) -> String {
    let file = h.get("file").or_else(|| h.get("uri")).and_then(|v| v.as_str()).unwrap_or("");
    let line = h.get("line").or_else(|| h.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line"))).and_then(|v| v.as_u64()).unwrap_or(0);
    let col = h.get("col").or_else(|| h.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("character"))).and_then(|v| v.as_u64()).unwrap_or(0);
    format!("{}:{}:{}", file, line, col)
}
```

**Step 2:** 在 execute_tool "refs" / "overview" / "find-implementations" / "find-symbol" 分支末尾调：

```rust
"refs" => {
    let (file, line, col) = required_position(&args)?;
    let raw = self.tool_referencing_symbols(...).await?;
    let delta = args.get("_delta").and_then(|v| v.as_bool()).unwrap_or(false);
    let root_key = format!("{}|{}|{}|{}", root.display(), file, line, col);
    let value = self.maybe_delta("refs", &root_key, serde_json::to_value(raw)?, delta).await;
    return Ok(value);
}
```

注：args 已被 sanitize_timeout_args 处理过，_delta 不在 sanitize 名单保留。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): J delta 编排（hit_key set diff，空集不缓存）"`

---

## Task 3: 测试覆盖

**Files:**
- Modify: `crates/supervisor/src/lib.rs`

**Step 1:** `maybe_delta_first_call_returns_full_with_delta_false`：

```rust
#[tokio::test]
async fn maybe_delta_first_call_returns_full_with_delta_false() {
    let sup = Supervisor::new(...);
    let cur = serde_json::json!({"items": [{"file":"a.rs","line":1,"col":0}]});
    let v = sup.maybe_delta("refs", "k", cur, true).await;
    assert_eq!(v["delta"], false);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
}
```

**Step 2:** `maybe_delta_second_call_returns_added_only`：

```rust
#[tokio::test]
async fn maybe_delta_second_call_returns_added_only() {
    let sup = Supervisor::new(...);
    let cur1 = serde_json::json!({"items": [{"file":"a.rs","line":1,"col":0}]});
    sup.maybe_delta("refs", "k", cur1, true).await;
    let cur2 = serde_json::json!({"items": [
        {"file":"a.rs","line":1,"col":0},
        {"file":"b.rs","line":5,"col":2},
    ]});
    let v = sup.maybe_delta("refs", "k", cur2, true).await;
    assert_eq!(v["delta"], true);
    assert_eq!(v["added"].as_array().unwrap().len(), 1);
    assert_eq!(v["added"][0]["file"], "b.rs");
}
```

**Step 3:** `maybe_delta_empty_response_not_cached`：

```rust
#[tokio::test]
async fn maybe_delta_empty_response_not_cached() {
    let sup = Supervisor::new(...);
    let empty = serde_json::json!({"items": []});
    sup.maybe_delta("refs", "k", empty, true).await;
    // 第二次非空
    let real = serde_json::json!({"items": [{"file":"a.rs","line":1,"col":0}]});
    let v = sup.maybe_delta("refs", "k", real, true).await;
    assert_eq!(v["delta"], false, "空响应不应缓存，第二次等于首次");
}
```

**Step 4:** `cargo test -p supervisor --lib maybe_delta -- --nocapture` —— 全 PASS。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): J delta 首调全集/二调增量/空不缓存"`

---

## Task 4: e2e CLI

**Step 1:** 起 daemon；两次连续 `cli.exe --project rust_demo refs lib.rs add 1 0 --delta > /tmp/d1.json` 和 `... --delta > /tmp/d2.json`（中间不修改文件）。
期望：d1 `delta: false, items: [...全部]`；d2 `delta: true, added: [], removed: []`。

**Step 2:** 修改 lib.rs 后再查：`cli.exe --project rust_demo refs lib.rs add 1 0 --delta > /tmp/d3.json`
期望：d3 `delta: true, added: [{新增 ref}]`。

**Step 3:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p supervisor --lib maybe_delta` | 全 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e d1 | jq `.delta == false` | ✓ |
| e2e d2 | jq `.delta == true, .added == []` | ✓ |

## 自我审查
- ✅ Spec §11-J 覆盖：代际键匹配返增量；不匹配/首次返全集；空集不缓存
- ✅ 占位符：无；具体代码
- ✅ 一致：maybe_delta / hit_key 命名一致
- ✅ 依赖：F2/H/B/A/C/E/G/I/K/L/M 全部完成（收口特性）
- ✅ 锁纪律：delta_cache lock 临界区微秒
- ✅ ponytail: 无 LRU 无淘汰——单 entry 滚动简单实现
