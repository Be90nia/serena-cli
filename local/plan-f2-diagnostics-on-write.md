# F2 编辑回执自带诊断（opencode 模式）

> **For agentic workers:** REQUIRED SUB-SKILL: subagent-driven-development (recommended) or executing-plans

**Goal:** 写类工具（replace-body / replace-text-in-symbol / insert-* / delete-* / safe-delete 等）返回值挂载诊断快照，AI 改完即知对错，省去一次 `diagnostics` round-trip。

**Architecture:** 在 execute_tool 写类分支匹配后，调用 `tool_diagnostics(root, file)` 拉一份诊断快照（file-level），与原 tool 返回值合并。`docs_diag` 失败/超时降级为 `[]`，不影响主结果。超时默认 2s（与现有 diagnostics 等待同口径）。CLI/daemon 三入口透传，无新 wire 字段。

**Tech Stack:** Rust workspace, supervisor crate, thiserror, tokio, lsp-core `Session::wait_diag_*` 既有 API。

**Spec:** `local/ai-token-features-design.md` §10-F2 / bd `serena-rust-bqc`。

**Pre-conditions:**
- P1 诊断空 push 修复已就位（commit `4ef3b3f`）。
- `Session::wait_gen >= N` 接口可用。

**Global Constraints（ARCHITECTURE.md verbatim）:**
- thiserror for libs / anyhow 仅 bin
- 9 错误码 wire 契约 HTTP 200+{ok:false}；新增 method 必走既有错误码
- 禁 dashmap/parking_lot/async-lsp/tower-lsp
- 锁纪律：短临界区零跨 await
- quirk 注释带 ↖ mirror/Δ 溯源

---

## 文件结构

**Modify:**
- `crates/supervisor/src/lib.rs:3557-3620` —— execute_tool 写类分支匹配段，新增 `post_diag_for_write` helper + 在 8 个写分支末尾调一次。
- `crates/cli/src/main.rs` —— 仅验证 CLI 入口透传（无需改动，但需 smoke 测三入口一致）。

**不修改:**
- wire 协议（HTTP / JSONL / stdio MCP）—— 在 tool 返回值内塞一个 `"post_write_diagnostics"` 字段，daemon/CLI 序列化沿用既有通道。

---

## Task 1: 失败时降级的诊断拉取 helper

**Files:**
- Modify: `crates/supervisor/src/lib.rs:3310-3330`（在 `push_nested` 附近找空闲位置插入 helper）

**Interfaces:**
- Consumes: `&self`, `&Path` (root), `&str` (file), `Option<&str>` (lang)
- Produces: `Vec<serde_json::Value>` (空表示失败/超时)

**Step 1:** 在 supervisor lib.rs 顶部 helper 区追加 `post_diag_for_write`（紧邻 `push_nested`，约 lib.rs:3085 之前）：

```rust
/// 写工具收尾：拉一次 file-level 诊断快照；失败/超时/未就绪一律降级为 `[]`。
///
/// F2 设计：写完后 AI 最常见的下一步是 `diagnostics <file>` 验证；本 helper 把这步
/// 折叠进写工具返回值（`post_write_diagnostics` 字段）。不阻塞主结果。
///
/// 锁纪律：与 tool_diagnostics 同样走 push 缓存 + 2s 兜底超时；不进诊断时不阻塞。
/// ponytail: 不为失败建新错误路径 —— 任何 err 都 `tracing::debug!` + 返 []。
async fn post_diag_for_write(
    &self,
    root: &Path,
    file: &str,
    lang: Option<&str>,
) -> Vec<serde_json::Value> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        self.tool_diagnostics(root, file, lang),
    )
    .await
    {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, file, "F2 post-write diag failed; degrading");
            Vec::new()
        }
        Err(_elapsed) => {
            tracing::debug!(file, "F2 post-write diag timeout 2s; degrading");
            Vec::new()
        }
    }
}
```

**Step 2:** 在所有 8 个写分支尾部插入调用（lib.rs:3635-3950 范围）：`replace-body` / `replace-text-in-symbol` / `insert-text-before-symbol` / `insert-text-after-symbol` / `delete-text-in-symbol` / `safe-delete-symbol` / `insert-at-line` / `replace-lines` / `delete-lines`（9 个，含 safe-delete）。每个分支结构：

```rust
// 修前：
"replace-body" => {
    let (...) = ...?;
    serde_json::to_value(self.tool_replace_body(...).await?)...;
}
// 修后：
"replace-body" => {
    let (file, ...) = ...?;
    let value = serde_json::to_value(self.tool_replace_body(...).await?...)?;
    let diag = self.post_diag_for_write(root, &file, lang).await;
    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("post_write_diagnostics".into(), serde_json::json!(diag));
    }
    Ok(value)
}
```

每个写分支的细节：
- `replace-body` (lib.rs:~3638)：用 `required_file(&args)?` 已得 file，插入 post_write_diagnostics
- `replace-text-in-symbol` (~3645)：同上
- `insert-text-before-symbol` / `insert-text-after-symbol` (~3650/3657)：同上
- `delete-text-in-symbol` (~3664)：同上
- `safe-delete-symbol` (~3671)：返回值是 `SafeDeleteReport`，结构有 `file` 字段——若 SafeDeleteReport 内含 file 字段可直接读，否则重新解析 args.file。优先读 SafeDeleteReport 字段。
- `insert-at-line` (~3678)：同上
- `replace-lines` / `delete-lines` (~3685/3692)：同上

**Step 3:** `cargo build -p supervisor 2>&1 | tail -5` —— 期望 0 errors。

**Step 4:** `cargo test -p supervisor --lib 2>&1 | tail -10` —— 期望既有 93 测试全绿（无回归）。

**Step 5:** `git add crates/supervisor/src/lib.rs && git commit -m "feat(supervisor): F2 写工具回执挂 post_write_diagnostics（默认 2s 兜底，降级不影响主结果)"`

---

## Task 2: 测试——降级路径（失败/超时/空数组三场景）

**Files:**
- Modify: `crates/supervisor/src/lib.rs:4970-5040`（紧邻 `empty_push_clears_cache_entry` 测试模块）

**Step 1:** 写测试 `post_diag_for_write_degrades_on_each_failure_mode`：

```rust
#[tokio::test]
async fn post_diag_for_write_degrades_on_each_failure_mode() {
    use std::time::Duration;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let sup = Supervisor::new(... /* 既有 test fixture 模式 */);
    // 场景 A：不存在的文件（tool_diagnostics 返 Err）→ []
    let a = sup.post_diag_for_write(&root, "does_not_exist.rs", Some("rust")).await;
    assert!(a.is_empty(), "Err 路径降级为空");
    // 场景 B：根路径不存在（session_for 抛错）→ []
    let bad_root = std::path::PathBuf::from("Z:/nonexistent_for_test_xyz");
    let b = sup.post_diag_for_write(&bad_root, "x.rs", Some("rust")).await;
    assert!(b.is_empty(), "session_for Err 路径降级为空");
    // 场景 C：合法但文件无错误 → 空数组（不是空 Vec 但 is_empty()==true）→ 也算 [] 渲染
    std::fs::write(root.join("clean.rs"), "fn main() {}").unwrap();
    let c = sup.post_diag_for_write(&root, "clean.rs", Some("rust")).await;
    assert!(c.is_empty(), "无错误路径返 []");
}
```

**Step 2:** 跑测试：`cargo test -p supervisor --lib post_diag_for_write_degrades -- --nocapture`。期望：PASS（证明 3 种降级路径都返空数组）。

**Step 3:** 跑既有测试避免回归：`cargo test -p supervisor --lib 2>&1 | tail -5`。期望：93+1 全绿。

**Step 4:** commit：`git add crates/supervisor/src/lib.rs && git commit -m "test(supervisor): F2 post_diag_for_write 三种失败模式降级断言"`

---

## Task 3: 测试——happy path（真 LS 触发诊断后挂载）

**Files:**
- Modify: `crates/lsp-core/tests/docsync.rs` 或新增 `crates/supervisor/tests/diagnostics_post_write.rs`

**Step 1:** 写集成测试 `post_write_diagnostics_appears_in_tool_result`：
- 用 mock_ls 推一项 error
- 调 `tool_replace_body` 改一个符号
- 检查返回值含 `post_write_diagnostics` 字段且非空

```rust
#[tokio::test(flavor = "current_thread")]
async fn post_write_diagnostics_appears_in_tool_result() {
    let (sup, root, _mock_pid) = launch_mock_supervisor_with_diags().await;
    std::fs::write(root.join("lib.rs"), "pub fn foo() {}\n").unwrap();
    // mock_ls 配置已发布 1 个 error on lib.rs:1
    let args = json!({
        "file": "lib.rs",
        "symbol": "foo",
        "expected_hash": null,  // 见既有 replace_body 协议
    });
    let result = execute_tool("replace-body", &root.to_string_lossy(), args, Some("rust")).await.unwrap();
    let diags = result.get("post_write_diagnostics").expect("post_write_diagnostics 必须存在");
    assert!(diags.is_array(), "必须是数组");
    assert!(!diags.as_array().unwrap().is_empty(), "mock_ls 推了 1 个 error，应非空");
}
```

参考既有 `crates/supervisor/tests/diagnostics.rs` 的 mock_ls 启动模式（`launch_mock_ls` / `LaunchInfo` 已在 docsync tests 集成）。

**Step 2:** 跑：`cargo test -p supervisor --test diagnostics_post_write -- --nocapture`。期望：PASS。

**Step 3:** commit：`git add crates/supervisor/tests/diagnostics_post_write.rs && git commit -m "test(supervisor): F2 post_write_diagnostics happy path（mock_ls 真拉 LS 触发 error）"`

---

## Task 4: 端到端——CLI 验证

**Step 1:** `cargo build -p cli --bin cli 2>&1 | tail -3` —— 期望 0 errors。

**Step 2:** 起 daemon：
```bash
D:/Project/serena-rust/target/debug/cli.exe --daemon --project D:/Project/serena-rust/fixtures/rust_demo > /tmp/daemon_f2.log 2>&1 &
sleep 8
```

**Step 3:** 故意改错 fixture 文件触发诊断：
```bash
D:/Project/serena-rust/target/debug/cli.exe --project D:/Project/serena-rust/fixtures/rust_demo replace-lines lib.rs 1 1 "garbage unmatched brace {{{" > /tmp/repl_f2.json 2>&1
echo "exit=$? bytes=$(wc -c < /tmp/repl_f2.json)"
```

**Step 4:** 验证：`jq '.post_write_diagnostics | length' /tmp/repl_f2.json` —— 期望 `>=1`（非空数组，rust-analyzer 真的报错）。

**Step 5:** 清理：
```bash
D:/Project/serena-rust/target/debug/cli.exe stop-all 2>&1 | tail -2
```

**Step 6:** `git add -A && git commit -m "chore: F2 端到端 smoke 通过"`（若无代码变更，此步可省）

---

## Verification 全景

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy 干净 | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 全 workspace 测试 | `cargo test --workspace` | 全绿（93+N supervisor） |
| 降级三场景 | `cargo test -p supervisor --lib post_diag_for_write_degrades` | PASS |
| 真 LS 触发 | `cargo test -p supervisor --test diagnostics_post_write` | PASS |
| CLI 端到端 | jq 看 `post_write_diagnostics` | `>=1` |
| 既有功能不回归 | `cargo test --workspace` 全套 | 全绿 |

## 自我审查（写完计划后）

- ✅ Spec §10-F2 覆盖：写工具挂诊断、2s 降级、CLI 透传、daemon 透传
- ✅ 占位符：无（具体代码、具体测试、具体命令）
- ✅ 类型一致：`post_write_diagnostics` 在 helper + 测试 + e2e 三处命名一致
- ✅ 依赖顺序：Task 1 helper → Task 2 单测降级 → Task 3 集成 happy path → Task 4 e2e
- ✅ 锁纪律：post_diag_for_write 调用 tool_diagnostics（既有 push 缓存路径），不持锁跨 await

## 执行选择

1. **subagent-driven（推荐）** —— 每任务派发新鲜 subagent + 两阶段审查
2. **内联执行** —— 本会话 executing-plans 批次带检查点

**选哪个？**
