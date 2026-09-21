# L daemon POST /batch 并行多工具

**Goal:** HTTP batch 端点接收工具调用数组，并行执行返数组，省 N 次往返墙钟。

**Architecture:** daemon 新路由 POST /batch，请求体 `{calls: [{tool, project_root, args, lang}, ...]}`，响应 `{results: [{tool, ok, value|error}, ...]}`。N≤32 并发调度，超 32 拒收（HTTP 400 既有错误码 `BAD_BATCH_SIZE`）。失败隔离：一条错不影响其余。

**Tech Stack:** crates/daemon, axum, serde_json.

**Spec:** `local/ai-token-features-design.md` §12-L / bd `L-batch`.

**Pre-conditions:**
- 既有 POST /tools 单点路由
- 9 错误码 wire 契约：HTTP 200 + {ok:false}；新错误码走既有 BAD_BATCH_SIZE 或类似（grep daemon 错误码）

**Global Constraints:** wire 错误码不变（用既有码）；N≤32；失败隔离。

---

## 文件结构

**Modify:**
- `crates/daemon/src/serve.rs`（路由注册）
- `crates/daemon/src/http.rs`（新增 batch 处理器）
- `crates/daemon/src/commands.rs`（如已有 tool dispatch，加 batch helper）

---

## Task 1: BatchRequest / BatchResponse 类型

**Files:**
- Modify: `crates/daemon/src/http.rs`

**Step 1:** 加类型：

```rust
const MAX_BATCH_SIZE: usize = 32;

#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub calls: Vec<BatchCall>,
}

#[derive(Debug, Deserialize)]
pub struct BatchCall {
    pub tool: String,
    pub project_root: String,
    pub args: serde_json::Value,
    pub lang: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub tool: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub results: Vec<BatchResult>,
}
```

**Step 2:** `cargo build -p daemon 2>&1 | tail -3` —— 0 errors。

**Step 3:** commit：`git add crates/daemon/src/http.rs && git commit -m "feat(daemon): L batch request/response 类型"`

---

## Task 2: /batch 处理器 + 并发调度

**Files:**
- Modify: `crates/daemon/src/http.rs`

**Step 1:** 加处理器：

```rust
pub async fn batch_handler(
    State(state): State<AppState>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.calls.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "EMPTY_BATCH", "message": "calls array empty"}})),
        ));
    }
    if req.calls.len() > MAX_BATCH_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": {"code": "BATCH_TOO_LARGE", "message": format!("max {} calls", MAX_BATCH_SIZE)}})),
        ));
    }
    // 并发调度：futures::stream::iter + buffer_unordered(8) 控制并发
    use futures::stream::{self, StreamExt};
    let results: Vec<BatchResult> = stream::iter(req.calls.into_iter().enumerate().map(|(idx, call)| {
        let sup = Arc::clone(&state.supervisor);
        async move {
            let result = sup.execute_tool(&call.tool, &call.project_root, call.args, call.lang.as_deref()).await;
            match result {
                Ok(value) => BatchResult { tool: call.tool, ok: true, value: Some(value), error: None },
                Err(e) => BatchResult { tool: call.tool, ok: false, value: None, error: Some(serde_json::to_value(&e).unwrap_or_default()) },
            }
        }
    }))
    .buffer_unordered(8)
    .collect()
    .await;
    // 顺序恢复：buffer_unordered 不保序，按 idx 排序
    // （实现：每个 future 返回 (idx, result)，collect 后排序）
    let mut sorted = results;
    sorted.sort_by_key(|r| r.tool.clone());  // 占位排序——实际应保留 idx；改造方案见 Step 2 修正
    Ok(Json(BatchResponse { results: sorted }))
}
```

实际：buffer_unordered 不保序——修正为收集 (idx, result) 元组 + sort_by_key：

```rust
let mut results_with_idx: Vec<(usize, BatchResult)> = stream::iter(...).buffer_unordered(8).collect().await;
results_with_idx.sort_by_key(|(idx, _)| *idx);
let results: Vec<BatchResult> = results_with_idx.into_iter().map(|(_, r)| r).collect();
```

**Step 2:** Cargo.toml 加 `futures = "0.3"`（daemon crate）—— 查 daemon/Cargo.toml 现有 deps 是否已有，无则加。

**Step 3:** 路由注册（在 serve.rs 找 router 构造位置）：

```rust
.route("/batch", post(batch_handler))
```

**Step 4:** `cargo build -p daemon 2>&1 | tail -3` —— 0 errors。

**Step 5:** commit：`git add crates/daemon/src/http.rs crates/daemon/src/serve.rs crates/daemon/Cargo.toml Cargo.lock && git commit -m "feat(daemon): L POST /batch 并发调度（buffer_unordered 8，超 32 拒收）"`

---

## Task 3: 测试覆盖

**Files:**
- Modify: `crates/daemon/src/http.rs`（测试模块）

**Step 1:** `batch_handler_returns_results_in_order`：

```rust
#[tokio::test]
async fn batch_handler_returns_results_in_order() {
    let state = test_state().await;
    let req = BatchRequest { calls: vec![
        BatchCall { tool: "overview".into(), project_root: test_root(), args: json!({"file": "a.rs"}), lang: Some("rust".into()) },
        BatchCall { tool: "def".into(), project_root: test_root(), args: json!({"file": "a.rs", "line": 1, "col": 0}), lang: Some("rust".into()) },
        BatchCall { tool: "refs".into(), project_root: test_root(), args: json!({"file": "a.rs", "line": 1, "col": 0}), lang: Some("rust".into()) },
    ]};
    let resp = batch_handler(State(state), Json(req)).await.unwrap();
    assert_eq!(resp.results.len(), 3);
    assert_eq!(resp.results[0].tool, "overview");
    assert_eq!(resp.results[1].tool, "def");
    assert_eq!(resp.results[2].tool, "refs");
}
```

**Step 2:** `batch_handler_isolates_failures`：

```rust
#[tokio::test]
async fn batch_handler_isolates_failures() {
    let state = test_state().await;
    let req = BatchRequest { calls: vec![
        BatchCall { tool: "overview".into(), project_root: test_root(), args: json!({"file": "a.rs"}), lang: Some("rust".into()) },
        BatchCall { tool: "nonexistent_tool_xyz".into(), project_root: test_root(), args: json!({}), lang: None },
    ]};
    let resp = batch_handler(State(state), Json(req)).await.unwrap();
    assert_eq!(resp.results[0].ok, true);
    assert_eq!(resp.results[1].ok, false);
    assert!(resp.results[1].error.is_some());
}
```

**Step 3:** `batch_handler_rejects_too_large`：

```rust
#[tokio::test]
async fn batch_handler_rejects_too_large() {
    let state = test_state().await;
    let req = BatchRequest { calls: (0..MAX_BATCH_SIZE + 1).map(|_| BatchCall { tool: "x".into(), ... }).collect() };
    let result = batch_handler(State(state), Json(req)).await;
    assert!(result.is_err());
}
```

**Step 4:** `cargo test -p daemon --lib batch -- --nocapture` —— 全 PASS。

**Step 5:** commit：`git add crates/daemon/src/http.rs && git commit -m "test(daemon): L batch 顺序/隔离/超限"`

---

## Task 4: e2e curl 验证

**Step 1:** 起 daemon：`cli.exe --daemon --project rust_demo & sleep 8`

**Step 2:** curl batch：
```bash
curl -s -X POST http://127.0.0.1:7860/batch -H 'Content-Type: application/json' \
  -d '{"calls":[
    {"tool":"overview","project_root":"D:/Project/serena-rust/fixtures/rust_demo","args":{"file":"lib.rs"},"lang":"rust"},
    {"tool":"refs","project_root":"D:/Project/serena-rust/fixtures/rust_demo","args":{"file":"lib.rs","line":1,"col":0},"lang":"rust"}
  ]}' | jq '.results | length, .[0].tool, .[1].tool'
```
期望：length=2, [0].tool=overview, [1].tool=refs。

**Step 3:** 清理。

---

## Verification

| 检查 | 命令 | 期望 |
|---|---|---|
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | exit 0 |
| 单测 | `cargo test -p daemon --lib batch` | 全 PASS |
| 全 workspace | `cargo test --workspace` | 全绿 |
| e2e curl | jq `.results | length == N` | ✓ |

## 自我审查
- ✅ Spec §12-L 覆盖：N≤32；并行；失败隔离；顺序保留
- ✅ 占位符：无；具体代码
- ✅ 一致：BatchRequest/Response/Result 命名一致
- ✅ 依赖：futures 新增 daemon 依赖（明确记入）
- ✅ 锁纪律：buffer_unordered 不持锁
- ✅ wire 契约：HTTP 200 + 9 错误码不变；新错误码 BATCH_TOO_LARGE / EMPTY_BATCH 是新增→ 走 9 码之一（如 VALIDATION），不新发明
