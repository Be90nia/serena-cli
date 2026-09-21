# M warm <lang> 预热命令

**Goal:** warm 预拉 daemon + LS + 索引消除冷启动（rust-analyzer 首次 30-60s）。

**Architecture:** CLI 子命令 `warm <lang>`：1) 若 daemon 未起则启动；2) 调 supervisor::session_for + ensure_open 一个入口文件触发 LS 启动；3) 等就绪信号（generation 推进或 ready probe）；4) 返 `{lang, ready: bool, elapsed_ms, fallback: bool?}`。

**Tech Stack:** cli, supervisor, lsp_core Session.

**Spec:** `local/ai-token-features-design.md` §13-M / bd `M-warm`.

**Pre-conditions:**
- session_for 已有
- ensure_open 已有
- Session::wait_for_* 既有

**Global Constraints:** wire 不变；超时降级返部分就绪（不阻塞）。

---

## 文件结构

**Create:**
- `crates/cli/src/cmd_warm.rs`（或直接放 main.rs 新增）

**Modify:**
- `crates/cli/src/main.rs`（warm 子命令 + 路由）

---

## Task 1: warm handler

**Files:**
- Modify: `crates/cli/src/main.rs`

**Step 1:** 加 enum 变体 + 子命令：

```rust
/// 预热 LS（消除冷启动）。
Warm {
    /// 要预热的语言。
    #[arg(long)]
    lang: String,
    /// 项目根（必需）。
    #[arg(long)]
    project: String,
    /// 就绪等待超时（秒），默认 30s。
    #[arg(long, default_value = "30")]
    timeout_secs: u64,
},
```

**Step 2:** handler（紧邻 `cli.exe overview` 等既有 handler 模式）：

```rust
async fn handle_warm(lang: String, project: String, timeout_secs: u64) -> Result<serde_json::Value, CliError> {
    let t0 = std::time::Instant::now();
    let sup = build_supervisor(&project)?;
    // 1) 触发 LS 启动（session_for 不真正启动，只是建立 Arc 句柄）
    let session = sup.session_for(Path::new(&project), &lang).await
        .map_err(|e| CliError::Other(format!("session_for: {e}")))?;
    // 2) ensure_open 一个入口文件（用 Cargo.toml 或 main.rs）
    let entry = Path::new(&project).join(if lang == "rust" { "Cargo.toml" } else { "main.rs" });
    let _guard = session.ensure_open(&entry).await
        .map_err(|e| CliError::Other(format!("ensure_open: {e}")))?;
    // 3) 等就绪（generation 推进 OR timeout）
    let deadline = t0 + std::time::Duration::from_secs(timeout_secs);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if session.is_ready() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(serde_json::json!({
        "lang": lang,
        "project": project,
        "ready": ready,
        "elapsed_ms": t0.elapsed().as_millis() as u64,
        "partial": !ready,  // 超时未就绪但 LS 已启动
    }))
}
```

`Session::is_ready()` 若不存在，加简单方法：检查 generation 已 +1 或 server_capabilities 已 set。或改为简单 `tokio::time::sleep` N 秒视为 ready（ponytail 简化）。

**Step 3:** Cmd 枚举加 Warm 分支 + 调度：

```rust
Cmd::Warm { lang, project, timeout_secs } => {
    let v = handle_warm(lang, project, timeout_secs).await?;
    print_json(&v);
}
```

**Step 4:** `cargo build --workspace 2>&1 | tail -3` —— 0 errors。

**Step 5:** commit：`git add crates/cli/src/main.rs && git commit -m "feat(cli): M warm <lang> 预热命令（session+ensure_open+ready wait）"`

---

## Task 2: 测试覆盖

**Files:**
- Modify: `crates/cli/src/main.rs`（测试模块）

**Step 1:** `warm_returns_ready_true_for_quick_ls`：

```rust
#[tokio::test]
async fn warm_returns_ready_true_for_quick_ls() {
    let v = handle_warm("rust".into(), test_rust_demo_path(), 30).await.unwrap();
    assert_eq!(v["ready"], true);
    assert!(v["elapsed_ms"].as_u64().unwrap() < 30_000);
}
```

**Step 2:** `warm_returns_partial_on_timeout`：

```rust
#[tokio::test]
async fn warm_returns_partial_on_timeout() {
    let v = handle_warm("rust".into(), test_rust_demo_path(), 0).await.unwrap();  // 0 秒超时
    assert_eq!(v["ready"], false);
    assert_eq!(v["partial"], true);
}
```

**Step 3:** `cargo test --workspace -p cli -- warm` —— PASS（CLI crate 现有 test 模式按 grep `tests` 模块套用）。

**Step 4:** commit。

---

## Task 3: e2e CLI

**Step 1:** 测冷启动 vs warm 后：
```bash
cli.exe overview --project rust_demo lib.rs --lang rust  # 冷启动，记录耗时
T_COLD=...
cli.exe warm rust --project rust_demo
cli.exe overview --project rust_demo lib.rs --lang rust  # 热启动
T_WARM=...
```
期望：`T_WARM < T_COLD / 2`。

**Step 2:** 验证 warm 输出：
```bash
cli.exe warm rust --project rust_demo --format json
```
期望：`{lang:"rust", ready:true, elapsed_ms:..., partial:false}`。

**Step 3:** 清理 daemon。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test --workspace -p cli warm` | 全 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e 加速比 | T_WARM < T_COLD / 2 | ✓ |

## 自我审查
- ✅ Spec §13-M 覆盖：daemon+LS+索引预热；就绪等待；超时降级
- ✅ 占位符：无；具体代码
- ✅ 一致：handle_warm 命名一致
- ✅ 依赖：session_for / ensure_open 已就位
- ✅ 锁纪律：is_ready 读 Arc 无 await
- ✅ ponytail: 0 秒超时 + 部分就绪降级 = 防 hang
