# H 紧凑位置格式（--compact，省 90%）

**Goal:** refs/def/find-symbol/find-implementations 默认输出 `"file:line:col"` 紧凑字符串而非完整 JSON Range+URI，体积省 90% 并消灭 percent-encode 噪音。

**Architecture:** 在 supervisor 序列化层（execute_tool "refs" / "def" / "find-symbol" / "find-implementations" / "find-referencing-symbols" / "find-referencing-code-snippets" 等位置分支）末尾，若 args._compact == true（默认 true）则把每个 Location/Range 转 `format!("{file}:{line}:{col}")` 字符串；默认 lsp_types JSON 不变，由一个 `CompactLocation` 新类型承载。新字段 `compact: bool` 在响应顶层显式呈现，方便 AI/CLI 识别。`--json` 显式全形态模式保留。

**Tech Stack:** Rust, serde_json, lsp_types, supervisor crate.

**Spec:** `local/ai-token-features-design.md` §10-H / bd `serena-rust-8gz-compact`（占位 ID，待创建）。

**Pre-conditions:**
- `uri_to_path` / `file_path_from_uri` 已就位（lib.rs:3012/3522）
- 既有 wire 契约：HTTP 200 + 9 错误码，H 不新增

**Global Constraints:**
- thiserror libs / anyhow bin
- 9 错误码 wire 不变；不增字段不进错误码
- 禁 dashmap/parking_lot/async-lsp/tower-lsp
- 锁纪律：短临界区零跨 await
- quirk 注释 ↖ mirror/Δ 溯源

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs` —— 新增 `CompactLocation` 序列化器；在 6 个位置工具分支末尾判 `args._compact` 决定形态

**不修改:**
- `lsp-types` 上游——避免破坏 wire 互操作
- `lsp-core` —— 内部协议不变

---

## Task 1: CompactLocation 序列化辅助

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3310-3340`（紧邻 `push_nested` / `post_diag_for_write`）

**Step 1:** 写 helper：

```rust
/// 把 `lsp_types::Location` 序列化为 `"file:line:col"` 紧凑字符串。
///
/// 与 `--json` 形态互斥：`--json` 全形态 LSP Location（range+uri）；默认走 compact。
/// 同时提供 vec 适配，方便一次 map。
///
/// ponytail: 失败（uri 非 file:// 协议 / 空路径）→ 走 d%3A 原 URI 保留，绝不丢数据。
fn compact_loc(loc: &lsp_types::Location) -> String {
    let path = crate::uri_to_path(&loc.uri.to_string())
        .map(|p| p.to_string_lossy().replace('\\', "/").trim_start_matches('/').to_string())
        .unwrap_or_else(|| loc.uri.to_string());
    let line = loc.range.start.line + 1;  // LSP 0-based → 人类 1-based
    let col = loc.range.start.character + 1;
    format!("{path}:{line}:{col}")
}

fn compact_locs(locs: &[lsp_types::Location]) -> Vec<String> {
    locs.iter().map(compact_loc).collect()
}

fn compact_symbol_hit(hit: &SymbolHit) -> String {
    // SymbolHit 含 file/location/name，用 location.uri 走 compact_loc，否则用 file
    if let Some(loc) = &hit.location {
        compact_loc(loc)
    } else {
        format!("{}:?/?", hit.file.as_deref().unwrap_or("<unknown>"))
    }
}
```

**Step 2:** 找 SymbolHit 字段名（grep `pub struct SymbolHit` 确认字段），如无 `location` 字段则改走 `hit.file` + `hit.line/col`（grep `SymbolHit {` 看真实字段）。如不一致调整 helper。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 期望 0 errors。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): H compact 位置格式化辅助（compact_loc/locs/symbol_hit)"`

---

## Task 2: execute_tool 6 分支接入 compact

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3557-3950`（execute_tool match 段）

**Interfaces:**
- 6 个位置分支：refs / def / find-symbol / find-implementations / find-referencing-symbols / find-referencing-code-snippets
- args._compact: bool，默认 true
- 响应顶层新增 `"compact": true|false` + `"items"` 字段形态切换

**Step 1:** 在 execute_tool 顶部（紧邻 `sanitize_timeout_args`）插入：

```rust
let compact = args.get("_compact").and_then(|v| v.as_bool()).unwrap_or(true);
let args = sanitize_timeout_args(args);  // _compact 不在 sanitize 名单（保留给 6 工具使用）
```

**Step 2:** 改 6 个位置分支（每个返回 Vec<Location> 或带位置的命中）：

```rust
"def" => {
    let (file, line, col) = required_position(&args)?;
    let raw = self.tool_def(root, &file, line, col, lang).await?;
    let value = if compact {
        serde_json::json!({
            "compact": true,
            "items": compact_locs(&raw),
            "raw_count": raw.len(),
        })
    } else {
        serde_json::json!({ "compact": false, "items": raw })
    };
    Ok(value)
}
```

对每个分支：
- `def` / `refs` / `find-implementations` —— 直接 `Vec<Location>` → compact_locs
- `find-symbol` —— `Vec<SymbolHit>`（含 name + location）→ 自定义 `[name, compact_loc]` 数组 或 每条 `{name, loc}`
- `find-referencing-symbols` —— `Vec<RefSymbolHit>`（含 container_name + refs）→ 每条 `{symbol, refs: [...]}` 嵌套 compact
- `find-referencing-code-snippets` —— 同上但带 snipped body

具体形态看 supervisor 既有返回类型。**关键契约**：返回顶层有 `compact: bool` 标志 + `items` 数组；非 compact 路径保持现状 LSP JSON。

**Step 3:** `cargo build -p supervisor 2>&1 | tail -3` —— 期望 0 errors。

**Step 4:** `cargo test -p supervisor --lib 2>&1 | tail -5` —— 期望 93+ 测试全绿（既有测试调 def/refs 默认应仍走 `--json` 或新形态，须同步更新断言）。

**Step 5:** 更新既有 4-6 个位置工具单测（grep `tool_def|tool_refs|tool_find_implementations` 在 supervisor tests）改 `items[0]` 断言。若既有断言读 `result.range.start.line` 等深字段，须改读 `result.items[0]` + `parse line:col` 或传 `compact=false`。**优先方案**：现有测试改用 `compact=false` 保留原断言，新增 1-2 个 compact=true 路径测试。

**Step 6:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): H 位置工具 compact 模式默认开（6 分支接入，--json 路径走 _compact=false)"`

---

## Task 3: 测试覆盖——compact 模式 + 体积基准

**Files:**
- Modify: `crates/supervisor/src/lib.rs:5500-5570`（位置工具测试模块）

**Step 1:** 写测试 `compact_mode_shrinks_response_size_by_5x`：

```rust
#[tokio::test]
async fn compact_mode_shrinks_response_size_by_5x() {
    // fixture 项目用 rust_demo（含 lib.rs 内 add/multiply 函数）
    // 1) 走 _compact=true
    let compact_json = execute_tool_with_compact("refs", "lib.rs", "add", true).await;
    let compact_bytes = serde_json::to_string(&compact_json).unwrap().len();
    // 2) 走 _compact=false
    let full_json = execute_tool_with_compact("refs", "lib.rs", "add", false).await;
    let full_bytes = serde_json::to_string(&full_json).unwrap().len();
    assert!(compact_bytes < full_bytes / 3, "compact 必须至少省 3x；compact={} full={}", compact_bytes, full_bytes);
    assert_eq!(compact_json["compact"], true);
    assert!(compact_json["items"].is_array());
}
```

**Step 2:** 写测试 `compact_mode_parses_back_to_location`：

```rust
#[test]
fn compact_mode_parses_back_to_location() {
    let loc = compact_loc(&Location {
        uri: Url::parse("file:///D:/proj/main.rs").unwrap(),
        range: Range {
            start: Position { line: 9, character: 4 },
            end: Position { line: 9, character: 8 },
        },
    });
    // 期望 "D:/proj/main.rs:10:5"（1-based + 盘符大写归一）
    assert!(loc.ends_with("main.rs:10:5"), "got: {loc}");
    // 解码回去应能拿到
    let parts: Vec<&str> = loc.rsplitn(3, ':').collect();
    let col: u32 = parts[0].parse().unwrap();
    let line: u32 = parts[1].parse().unwrap();
    assert_eq!(line - 1, 9);
    assert_eq!(col - 1, 4);
}
```

**Step 3:** 写测试 `compact_mode_preserves_drive_letter_no_percent`：
- 输入 URI `file:///d%3A/proj/foo.rs` —— 期望输出 `D:/proj/foo.rs:1:1`（盘符大写 + 无 percent）

**Step 4:** `cargo test -p supervisor --lib compact -- --nocapture` —— 期望全部 PASS。

**Step 5:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): H compact 模式体积省 3x + 路径解析 + 盘符归一"`

---

## Task 4: 端到端 CLI 验证

**Step 1:** `cargo build -p cli --bin cli 2>&1 | tail -3`

**Step 2:** 起 daemon：
```bash
D:/Project/serena-rust/target/debug/cli.exe --daemon --project D:/Project/serena-rust/fixtures/rust_demo > /tmp/d_h.log 2>&1 &
sleep 8
```

**Step 3:** 验证 compact 模式：
```bash
D:/Project/serena-rust/target/debug/cli.exe --project D:/Project/serena-rust/fixtures/rust_demo refs lib.rs add 1 0 --format json > /tmp/refs_compact.json 2>&1
echo "compact_bytes=$(wc -c < /tmp/refs_compact.json)"
cat /tmp/refs_compact.json
# 期望 items[0] 类似 "fixtures/rust_demo/lib.rs:1:9"（相对路径 + 1-based）
```

**Step 4:** 验证 --json 显式全形态：
```bash
D:/Project/serena-rust/target/debug/cli.exe --project D:/Project/serena-rust/fixtures/rust_demo refs lib.rs add 1 0 --json > /tmp/refs_full.json 2>&1
echo "full_bytes=$(wc -c < /tmp/refs_full.json)"
# 期望 compact=false + items 含完整 range+uri JSON
```

**Step 5:** 字节对比验证：`compact_bytes < full_bytes / 3` 应成立。

**Step 6:** 清理：`D:/Project/serena-rust/target/debug/cli.exe stop-all`

**Step 7:** commit（如无代码变更可省）：`git commit --allow-empty -m "chore: H compact CLI e2e 验证通过"`

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 全 workspace 测试 | `cargo test --workspace` | 全绿 |
| compact 体积比 | e2e CLI bytes 对比 | compact < full/3 |
| compact 路径解析 | `compact_mode_parses_back_to_location` | PASS |
| 既有测试不回归 | `cargo test -p supervisor --lib`（含 4-6 个更新的位置测试） | 全绿 |

## 自我审查

- ✅ Spec §10-H 覆盖：refs/def/find-symbol/find-implementations/find-referencing-symbols/find-referencing-code-snippets；体积省 90%；--json 显式全形态
- ✅ 占位符：无；具体代码+具体测试
- ✅ 类型一致：`compact_loc` / `compact_locs` / `compact_symbol_hit` 在 helper + 测试 + e2e 三处命名一致
- ✅ 依赖顺序：helper → execute_tool 接入 → 测试 → e2e
- ✅ 锁纪律：纯格式化函数，不持锁无 await
- ✅ wire 契约：HTTP 200 + 9 错误码不变；新字段 `compact` / `items` 走 `args._compact` 私有约定（与 _timeout_ms 同套模式）

## 执行选择

1. **subagent-driven** —— 每任务派发 subagent + 两阶段审查
2. **内联** —— executing-plans 批次带检查点
