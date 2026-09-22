//! Daemon HTTP 前端（PLAN Task 12 / ARCHITECTURE §6.3）。
//!
//! - `POST /tools/{name}`：执行工具，返回 wire DTO。
//! - `GET /status`：uptime + 进程统计。
//! - `POST /shutdown`：进入 ShutdownDraining。
//!
//! Middleware：`X-Serena-Token` 必须与 lock 文件 token 一致；不符 403。
//! 工具级失败走 200 + `{ok:false}`（A5），transport 错误才用 4xx/5xx。
//! 503 用于 ShutdownDraining 拒绝新请求（I8）。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::dto::{
    StatusResponse, ToolResponse, WireError, WireErrorCode, wire_error_code_to_exit,
    wire_error_from_tool_error,
};

/// Daemon 状态。`token` 来自 lock 文件（Task 11）；draining 标志由 Reaper（Task 14）置。
#[derive(Clone)]
pub struct AppState {
    pub supervisor: Arc<dyn supervisor::SupervisorTrait>,
    pub token: Arc<String>,
    pub start_ts: std::time::Instant,
    pub loaded_ls: Arc<std::sync::Mutex<Vec<String>>>,
    pub draining: Arc<std::sync::atomic::AtomicBool>,
    /// 最近一次工具请求的 project_root（status 观察 + 重启定位用）。
    pub active_project: Arc<std::sync::Mutex<Option<String>>>,
    /// /shutdown 触发：axum::serve.with_graceful_shutdown 等此 Notify。
    /// 一拍即过（notify_waiters 一次性广播）。
    pub shutdown_notify: Arc<tokio::sync::Notify>,
    /// 正在执行的工具请求数（tools_post 通过 draining 检查后 +1，返回前 -1）。
    /// drain 窗口的排空判据。
    pub in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// drain 窗口上限：/shutdown 后保持 listener 可接受、新请求拿
    /// 503 DAEMON_DRAINING 的时长（生产 2s；测试注入短值）。
    pub drain_window: std::time::Duration,
    /// 工具调用重放日志（d3a，JSONL 索引按 invocation_id）。空路径 = 不写
    /// （不关心日志的测试用；生产由 serve 注入 default_invocation_log_path()）。
    pub invocation_log_path: std::path::PathBuf,
}

impl AppState {
    pub fn uptime_secs(&self) -> u64 {
        self.start_ts.elapsed().as_secs()
    }

    /// drain 排空等待：in-flight 持续归零（quiet 期）或窗口尽先走。
    /// /shutdown 与 reaper 收尾共用。quiet 期防并发请求间隙的瞬时归零
    /// 被误判为排空（20 worker 请求间隙 ~ms 级，竞速归零会让 daemon
    /// 在请求洪水中提前自杀）。
    pub async fn wait_drain(&self) {
        use std::time::Duration;
        let deadline = std::time::Instant::now() + self.drain_window;
        let quiet = Duration::from_millis(500);
        let mut last_busy = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            if self.in_flight.load(std::sync::atomic::Ordering::Acquire) > 0 {
                last_busy = std::time::Instant::now();
            } else if last_busy.elapsed() >= quiet {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Token 校验中间件：缺失或不等 → 403。
async fn require_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let header_token = headers
        .get("X-Serena-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(header_token.as_bytes(), state.token.as_bytes()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": {"code": "FORBIDDEN", "message": "missing or invalid X-Serena-Token"}
            })),
        )
            .into_response();
    }
    next.run(request).await
}

/// 常时比较（防 timing attack；std 没有，写 ~10 行）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 装配 router。
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/tools/{name}", post(tools_post))
        .route("/batch", post(batch_handler))
        .route("/status", get(status_get))
        .route("/shutdown", post(shutdown_post))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn tools_post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(req): Json<crate::dto::ToolRequest>,
) -> Response {
    if state.draining.load(std::sync::atomic::Ordering::Acquire) {
        return draining_response();
    }
    state.in_flight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    crate::reaper::note_activity();

    // d3a：invocation_id 三级来源——body envelope > X-Invocation-Id header >
    // 自动生成（向后兼容：老客户端两处都不发）。body envelope 优先：编排器
    // 的幂等键语义完整（带版本与证据），header 只是轻量透传。
    let invocation_id = req
        .envelope
        .as_ref()
        .map(|e| e.invocation_id.clone())
        .or_else(|| {
            headers
                .get("X-Invocation-Id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .unwrap_or_else(new_invocation_id);

    // 记录最近请求的 project_root（供 /status 观察；不区分成败，只要请求到达）。
    *state.active_project.lock().unwrap() = Some(req.project_root.clone());

    let started = std::time::Instant::now();
    let resp = match state
        .supervisor
        .execute_tool(&name, &req.project_root, req.args, req.lang.as_deref())
        .await
    {
        Ok(data) => {
            log_invocation(
                &state.invocation_log_path,
                &invocation_id,
                &name,
                &req.project_root,
                true,
                None,
                started.elapsed(),
            );
            let resp = ToolResponse::Ok {
                ok: true,
                data,
                format: None,
            };
            // Direct Serialize (no intermediate Value clone). For large responses
            // (search 200+ hits, refs, repo-map) saves ~500µs / 84% vs the prior
            // to_value+serialize double walk — see local/p2-0bq-bench.rs.
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(err) => {
            let wire = wire_error_from_tool_error(&err);
            log_invocation(
                &state.invocation_log_path,
                &invocation_id,
                &name,
                &req.project_root,
                false,
                Some(wire.code),
                started.elapsed(),
            );
            let status = StatusCode::OK; // 工具级失败走 200（A5）
            let resp = ToolResponse::Err {
                ok: false,
                error: wire,
            };
            (status, Json(resp)).into_response()
        }
    };
    state.in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    resp
}

/// 生成 invocation_id（d3a）：UUID v4 形状。std 熵源（时间纳秒 + pid +
/// 进程内计数器，同 lockfile `gen_token` 惯例）；排障/重放键用途，非加密。
pub fn new_invocation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 三路熵混出 128 bit；高位 32 bit 取 now 保证跨进程不撞。
    let a = now;
    let b = (std::process::id() as u64) ^ cnt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let c = now.rotate_left(17) ^ cnt;
    let d = now ^ cnt.rotate_left(29).wrapping_mul(0xD1B5_4A32_D192_ED03);
    // 变体段（第四段）：bit15=1 + bit14=0 即 `10xx`，其余 14 bit 取 c。
    let variant = 0x8000u16 | ((c >> 48) as u16 & 0x3fff);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        a as u16,
        (b >> 48) as u16 & 0x0fff,
        variant,
        (d ^ (b << 13)) & 0xffff_ffff_ffff
    )
}

/// 追加一条工具调用记录到重放日志（d3a）。JSONL，行首键即 invocation_id
/// （`grep <id> invocations.jsonl` 即索引）。写失败只 warn 不影响工具执行。
fn log_invocation(
    path: &std::path::Path,
    invocation_id: &str,
    tool: &str,
    project_root: &str,
    ok: bool,
    error_code: Option<WireErrorCode>,
    elapsed: std::time::Duration,
) {
    if path.as_os_str().is_empty() {
        return;
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let line = json!({
        "invocation_id": invocation_id,
        "ts_ms": ts_ms,
        "tool": tool,
        "project_root": project_root,
        "ok": ok,
        // Option<WireErrorCode> 直接走 serde：Some → SCREAMING_SNAKE_CASE，None → null。
        "error_code": error_code,
        "duration_ms": elapsed.as_millis() as u64,
    });
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()))
    {
        eprintln!(
            "[serena] invocation log append failed (path={path:?}): {e}; tool execution unaffected"
        );
    }
}

async fn status_get(State(state): State<AppState>) -> Response {
    crate::reaper::note_activity();
    let loaded = state
        .supervisor
        .loaded_entries()
        .into_iter()
        .map(|k| k.lang.to_string())
        .collect::<Vec<_>>();
    let resp = StatusResponse {
        uptime_secs: state.uptime_secs(),
        pid: std::process::id(),
        loaded_ls: loaded,
        draining: state.draining.load(std::sync::atomic::Ordering::Acquire),
        active_project: state.active_project.lock().unwrap().clone(),
    };
    (StatusCode::OK, Json(resp)).into_response()
}

async fn shutdown_post(State(state): State<AppState>) -> Response {
    // 幂等：重复 /shutdown（stop-all 重试）不叠加 drain 窗口。
    if state.draining.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "message": "already draining"})),
        )
            .into_response();
    }
    // drain 窗口后台跑：notify_waiters 延迟到窗口尽才发——axum 未收到通知前
    // 持续 accept，窗口内新请求在 tools_post 拿 503 DAEMON_DRAINING（客户端
    // 可区分「daemon 在拒绝」vs「已死」）。handler 立即返回：CLI stop-all 对
    // 管理命令只有 3s 超时，不能挂 2s 窗口。
    let st = state.clone();
    tokio::spawn(async move {
        st.wait_drain().await;
        st.shutdown_notify.notify_waiters();
    });
    (
        StatusCode::OK,
        Json(json!({"ok": true, "message": "draining set; will exit when in-flight drains"})),
    )
        .into_response()
}

/// 未知工具名 → 404 transport error。
pub fn not_found_tool(name: &str) -> Response {
    let wire = WireError {
        code: WireErrorCode::Internal,
        message: format!("unknown tool: {name}"),
        ls: None,
        retryable: false,
    };
    (
        StatusCode::NOT_FOUND,
        Json(json!({"ok": false, "error": wire})),
    )
        .into_response()
}

// ── L：POST /batch —— 多工具并行执行（local/plan-l-batch / AI-token §12-L）──

/// 单批上限：防一个请求占满 supervisor 并发（N≤32）。
pub const MAX_BATCH_SIZE: usize = 32;

/// `POST /batch` 请求体。
#[derive(Debug, Deserialize)]
pub struct BatchRequest {
    pub calls: Vec<BatchCall>,
}

#[derive(Debug, Deserialize)]
pub struct BatchCall {
    pub tool: String,
    pub project_root: String,
    /// 工具参数（各工具自行解析；与 /tools 的 ToolRequest.args 同语义）。
    pub args: serde_json::Value,
    pub lang: Option<String>,
}

/// 单条调用结果：`value`/`error` 按 ok 互斥。失败隔离——error 形状与 /tools
/// 的 WireError 一致（9 错误码），客户端复用同一解析路径。
#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub tool: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub results: Vec<BatchResult>,
}

/// `POST /batch`：并行执行工具数组，结果按请求顺序返回。生命周期与
/// tools_post 同套（draining 503 / in_flight / note_activity）；空批与超限
/// 走 200 + `{ok:false}` + 既有 BAD_ARGS 码——wire 契约 9 码不变。
async fn batch_handler(State(state): State<AppState>, Json(req): Json<BatchRequest>) -> Response {
    if state.draining.load(std::sync::atomic::Ordering::Acquire) {
        return draining_response();
    }
    // 尺寸校验在 in_flight 计数前：拒收不占排空窗口。
    let n = req.calls.len();
    if n == 0 || n > MAX_BATCH_SIZE {
        let wire = WireError {
            code: WireErrorCode::BadArgs,
            message: if n == 0 {
                "calls array empty".into()
            } else {
                format!("batch has {n} calls; max {MAX_BATCH_SIZE}")
            },
            ls: None,
            retryable: false,
        };
        return (
            StatusCode::OK, // 工具级失败走 200（A5）；batch 尺寸违规同契约
            Json(json!({"ok": false, "error": wire})),
        )
            .into_response();
    }
    state.in_flight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    crate::reaper::note_activity();
    // batch 可混多项目；active_project 记最后一个请求的 root（「最近请求」语义同 tools_post）。
    if let Some(root) = req.calls.last().map(|c| c.project_root.clone()) {
        *state.active_project.lock().unwrap() = Some(root);
    }
    let results = run_batch(&state, req.calls).await;
    state.in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    (StatusCode::OK, Json(BatchResponse { results })).into_response()
}

/// 并发执行（≤8 同时在飞）+ 顺序恢复：JoinSet 配 Semaphore 限流，收集
/// (idx, result) 后按 idx 排序——JoinSet 完成序 ≠ 请求序。
async fn run_batch(state: &AppState, calls: Vec<BatchCall>) -> Vec<BatchResult> {
    let sem = Arc::new(tokio::sync::Semaphore::new(8));
    let mut set: tokio::task::JoinSet<(usize, BatchResult)> = tokio::task::JoinSet::new();
    for (idx, call) in calls.into_iter().enumerate() {
        let sem = Arc::clone(&sem);
        let sup = Arc::clone(&state.supervisor);
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore never closed");
            (idx, execute_batch_call(&sup, call).await)
        });
    }
    let mut results: Vec<(usize, BatchResult)> = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(pair) => results.push(pair),
            // 不可达：任务体无 panic 源。防御兜底保失败隔离，usize::MAX 沉底
            // 不冒充任何请求位。
            Err(e) => results.push((
                usize::MAX,
                BatchResult {
                    tool: String::new(),
                    ok: false,
                    value: None,
                    error: Some(WireError {
                        code: WireErrorCode::Internal,
                        message: format!("batch task failed: {e}"),
                        ls: None,
                        retryable: false,
                    }),
                },
            )),
        }
    }
    results.sort_by_key(|(idx, _)| *idx);
    results.into_iter().map(|(_, r)| r).collect()
}

/// 单条调用：工具级失败隔离为 `{ok:false}`，绝不短路整批。
async fn execute_batch_call(
    sup: &Arc<dyn supervisor::SupervisorTrait>,
    call: BatchCall,
) -> BatchResult {
    match sup
        .execute_tool(&call.tool, &call.project_root, call.args, call.lang.as_deref())
        .await
    {
        Ok(value) => BatchResult {
            tool: call.tool,
            ok: true,
            value: Some(value),
            error: None,
        },
        Err(e) => BatchResult {
            tool: call.tool,
            ok: false,
            value: None,
            error: Some(wire_error_from_tool_error(&e)),
        },
    }
}

/// draining 503 响应（tools_post / batch_handler 共用；形状被测试断言锁定）。
fn draining_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("Retry-After", "10")],
        Json(json!({
            "ok": false,
            "error": {"code": "DAEMON_DRAINING", "message": "daemon in ShutdownDraining; refusing new requests", "retryable": false}
        })),
    )
        .into_response()
}

/// CLI exit code 取 wire 错误码（ARCH §6.3 表）。
#[allow(dead_code)] // 给 CLI 复用
pub fn cli_exit_from_wire_code(code: WireErrorCode) -> u8 {
    wire_error_code_to_exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as AxStatus};
    use tower::ServiceExt;

    /// mock supervisor：`tokio::sync::Mutex<Option<Box<dyn FnOnce + Send>>>`，
    /// 跨 .await 拿闭包、call 一次即丢。
    #[allow(clippy::type_complexity)]
    struct MockSupervisor {
        /// execute_tool 前的延迟：模拟慢工具，供 in-flight 计数断言。
        delay: Option<std::time::Duration>,
        /// 按工具名回显模式（batch 用）：`slow_x` 延迟 50ms 后 Ok("x")，
        /// `bad_*` → Err(BadArgs)，其余 Ok(tool)。并发下完成序 ≠ 请求序。
        echo: bool,
        result: tokio::sync::Mutex<
            Option<Box<dyn FnOnce() -> Result<serde_json::Value, supervisor::ToolError> + Send>>,
        >,
    }

    impl MockSupervisor {
        fn ok(data: serde_json::Value) -> Self {
            Self {
                delay: None,
                echo: false,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn err(e: supervisor::ToolError) -> Self {
            Self {
                delay: None,
                echo: false,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Err(e)))),
            }
        }
        fn slow(data: serde_json::Value, delay: std::time::Duration) -> Self {
            Self {
                delay: Some(delay),
                echo: false,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn echo_by_tool() -> Self {
            Self {
                delay: None,
                echo: true,
                result: tokio::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl supervisor::SupervisorTrait for MockSupervisor {
        async fn execute_tool(
            &self,
            _tool: &str,
            _root: &str,
            _args: serde_json::Value,
            _lang: Option<&str>,
        ) -> Result<serde_json::Value, supervisor::ToolError> {
            if self.echo {
                if let Some(name) = _tool.strip_prefix("slow_") {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    return Ok(json!(name));
                }
                if _tool.starts_with("bad_") {
                    return Err(supervisor::ToolError::BadArgs {
                        detail: format!("rejected: {_tool}"),
                    });
                }
                return Ok(json!(_tool));
            }
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let f = self.result.lock().await.take().expect("mock called once");
            f()
        }

        fn loaded_entries(&self) -> Vec<supervisor::Key> {
            vec![supervisor::Key {
                root: std::path::PathBuf::from("/mock"),
                lang: Box::from("rust"),
            }]
        }
    }

    fn state(token: &str, mock: MockSupervisor) -> AppState {
        state_with_log(token, mock, std::path::PathBuf::new())
    }

    /// d3a：注入重放日志路径的 fixture（tempdir 即可断言日志内容）。
    fn state_with_log(
        token: &str,
        mock: MockSupervisor,
        invocation_log_path: std::path::PathBuf,
    ) -> AppState {
        AppState {
            supervisor: Arc::new(mock),
            token: Arc::new(token.into()),
            start_ts: std::time::Instant::now(),
            loaded_ls: Arc::new(std::sync::Mutex::new(vec!["clangd".into()])),
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_project: Arc::new(std::sync::Mutex::new(None)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            drain_window: std::time::Duration::from_millis(100),
            invocation_log_path,
        }
    }

    /// d3a：带 X-Invocation-Id header 的请求。
    fn req_post_invocation(
        path: &str,
        token: Option<&str>,
        invocation_id: &str,
        body: serde_json::Value,
    ) -> Request<Body> {
        let mut b = req_post(path, token, body);
        b.headers_mut()
            .insert("X-Invocation-Id", invocation_id.parse().unwrap());
        b
    }

    /// d3a：读重放日志全部行（JSONL）。
    fn read_log(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("log line json"))
            .collect()
    }

    /// 内存请求：免端口绑定；返回 (status, body_json)。
    async fn oneshot_json(
        router: Router,
        req: Request<Body>,
    ) -> (AxStatus, Option<serde_json::Value>) {
        let resp = router.oneshot(req).await.expect("oneshot");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .expect("body");
        let json_val = serde_json::from_slice(&bytes).ok();
        (status, json_val)
    }

    fn req_post(path: &str, token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut b = Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        if let Some(t) = token {
            b.headers_mut().insert("X-Serena-Token", t.parse().unwrap());
        }
        b
    }

    fn req_get(path: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::get(path).body(Body::empty()).unwrap();
        if let Some(t) = token {
            b.headers_mut().insert("X-Serena-Token", t.parse().unwrap());
        }
        b
    }

    #[tokio::test]
    async fn missing_token_returns_403() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, _) = oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                None,
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::FORBIDDEN);
    }

    #[tokio::test]
    async fn wrong_token_returns_403() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, _) = oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                Some("wrong"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::FORBIDDEN);
    }

    #[tokio::test]
    async fn correct_token_with_ok_returns_200_data() {
        let st = state("secret", MockSupervisor::ok(json!([{"name": "main"}])));
        let (status, body) = oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"], json!([{"name": "main"}]));
    }

    // ── d3a：编排 envelope + invocation 重放日志 ──

    /// UUID v4 形状：8-4-4-4-12，版本位 4，变体位 [89ab]。
    fn assert_uuid_v4(id: &str) {
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "not uuid-shaped: {id}"
        );
        assert_eq!(id.len(), 36);
        assert_eq!(parts[2].chars().next(), Some('4'), "version nibble: {id}");
        let variant = parts[3].chars().next().unwrap();
        assert!(matches!(variant, '8' | '9' | 'a' | 'b'), "variant: {id}");
    }

    #[test]
    fn new_invocation_id_uuid_v4_shape_and_uniqueness() {
        let a = new_invocation_id();
        let b = new_invocation_id();
        assert_uuid_v4(&a);
        assert_uuid_v4(&b);
        assert_ne!(a, b, "consecutive ids must differ");
    }

    #[tokio::test]
    async fn envelope_body_id_wins_over_header() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        let st = state_with_log("secret", MockSupervisor::ok(json!(null)), log.clone());
        let (status, body) = oneshot_json(
            router(st),
            req_post_invocation(
                "/tools/overview",
                Some("secret"),
                "from-header-should-lose",
                json!({
                    "project_root": "D:/x",
                    "args": {},
                    "envelope": {"invocation_id": "from-envelope-wins"}
                }),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        assert_eq!(body.unwrap()["ok"], true);
        let rows = read_log(&log);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["invocation_id"], "from-envelope-wins");
    }

    #[tokio::test]
    async fn header_id_used_when_no_body_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        let st = state_with_log("secret", MockSupervisor::ok(json!(null)), log.clone());
        oneshot_json(
            router(st),
            req_post_invocation(
                "/tools/overview",
                Some("secret"),
                "hdr-123",
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        let rows = read_log(&log);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["invocation_id"], "hdr-123");
    }

    /// P0-2：老客户端（无 envelope 无 header）→ 自动生成 v4 + 日志索引可查；
    /// 响应结构不变（wire 无 envelope 泄漏）。
    #[tokio::test]
    async fn missing_invocation_id_generates_uuid_v4_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        let st = state_with_log("secret", MockSupervisor::ok(json!({"n": 1})), log.clone());
        let (status, body) = oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        // 响应结构不动：只有 ok/data（/format），无 envelope 字段。
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"], json!({"n": 1}));
        assert!(body.get("invocation_id").is_none());
        assert!(body.get("envelope").is_none());
        let rows = read_log(&log);
        assert_eq!(rows.len(), 1);
        let logged = rows[0]["invocation_id"].as_str().expect("id logged");
        assert_uuid_v4(logged);
        assert_eq!(rows[0]["tool"], "overview");
        assert_eq!(rows[0]["ok"], true);
        assert!(rows[0]["error_code"].is_null());
    }

    /// P0-3：失败路径同样带 envelope 索引；9 错误码 wire 契约不动。
    #[tokio::test]
    async fn error_path_logged_with_wire_code() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        let st = state_with_log(
            "secret",
            MockSupervisor::err(supervisor::ToolError::BadArgs {
                detail: "missing pattern".into(),
            }),
            log.clone(),
        );
        let (status, body) = oneshot_json(
            router(st),
            req_post_invocation(
                "/tools/find-symbol",
                Some("secret"),
                "err-42",
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK); // 工具级失败走 200（A5 不变）
        let body = body.expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "BAD_ARGS");
        assert!(body.get("envelope").is_none());
        let rows = read_log(&log);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["invocation_id"], "err-42");
        assert_eq!(rows[0]["ok"], false);
        assert_eq!(rows[0]["error_code"], "BAD_ARGS");
    }

    #[tokio::test]
    async fn correct_token_with_tool_err_returns_200_ok_false() {
        let st = state(
            "secret",
            MockSupervisor::err(supervisor::ToolError::BadArgs {
                detail: "missing x".into(),
            }),
        );
        let (status, body) = oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK, "工具级失败走 200");
        let body = body.expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "BAD_ARGS");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn status_returns_uptime_and_loaded_ls() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, body) = oneshot_json(router(st), req_get("/status", Some("secret"))).await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert!(body["uptime_secs"].is_u64());
        assert_eq!(body["loaded_ls"], json!(["rust"]));
        assert_eq!(body["draining"], false);
    }

    #[tokio::test]
    async fn shutdown_sets_draining() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let r = router(st);
        // 1) shutdown
        let req = Request::post("/shutdown")
            .header("X-Serena-Token", "secret")
            .body(Body::empty())
            .unwrap();
        let (status, _) = oneshot_json(r.clone(), req).await;
        assert_eq!(status, AxStatus::OK);
        // 2) 再请求工具：503 + 可区分信号（transport 层 code，非 9 工具错误码）。
        let resp = r
            .oneshot(req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ))
            .await
            .expect("oneshot");
        assert_eq!(resp.status(), AxStatus::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("Retry-After").unwrap(), "10");
        let bytes = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["error"]["code"], "DAEMON_DRAINING");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, _) = oneshot_json(router(st), req_get("/no-such-path", Some("secret"))).await;
        assert_eq!(status, AxStatus::NOT_FOUND);
    }

    /// drain 窗口核心语义：无 in-flight 时 quiet 期（500ms）满即 notify（不傻等窗口）。
    #[tokio::test]
    async fn shutdown_notifies_immediately_when_no_inflight() {
        let mut st = state("secret", MockSupervisor::ok(json!(null)));
        // 窗口设 5s：若实现错误地等满窗口，下面的 2s timeout 必失败。
        st.drain_window = std::time::Duration::from_secs(5);
        let notify = st.shutdown_notify.clone();
        let waiter = tokio::spawn(async move {
            notify.notified().await;
            true
        });
        let req = Request::post("/shutdown")
            .header("X-Serena-Token", "secret")
            .body(Body::empty())
            .unwrap();
        let (status, _) = oneshot_json(router(st), req).await;
        assert_eq!(status, AxStatus::OK);
        let fired = tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await;
        assert!(
            fired.is_ok(),
            "无 in-flight 时 notify 应在 quiet 期（500ms）后发出，不等满窗口"
        );
    }

    /// in-flight 未排空时 notify 必须推迟到窗口尽（窗口内新请求拿 503
    /// DAEMON_DRAINING 而非 connection refused 的前提）。
    #[tokio::test]
    async fn shutdown_defers_notify_until_inflight_drains() {
        let mut st = state("secret", MockSupervisor::ok(json!(null)));
        st.drain_window = std::time::Duration::from_millis(200);
        st.in_flight
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let notify = st.shutdown_notify.clone();
        let mut waiter = tokio::spawn(async move {
            notify.notified().await;
            true
        });
        let req = Request::post("/shutdown")
            .header("X-Serena-Token", "secret")
            .body(Body::empty())
            .unwrap();
        let (status, _) = oneshot_json(router(st), req).await;
        assert_eq!(status, AxStatus::OK);
        // 窗口（200ms）内不得提前 notify。
        let early = tokio::time::timeout(std::time::Duration::from_millis(80), &mut waiter).await;
        assert!(early.is_err(), "in-flight 未归零时 notify 必须推迟到窗口尽");
        let fired = tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await;
        assert!(fired.is_ok(), "窗口尽后必须 notify");
    }

    /// 真实计数路径：请求执行中 in_flight==1，完成后归零。
    #[tokio::test]
    async fn inflight_tracks_active_tool_call() {
        let st = state(
            "secret",
            MockSupervisor::slow(json!(null), std::time::Duration::from_millis(100)),
        );
        let counter = st.in_flight.clone();
        let handle = tokio::spawn(oneshot_json(
            router(st),
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "执行中 in_flight 应为 1"
        );
        let _ = handle.await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "完成后 in_flight 应归零"
        );
    }

    /// 保序：首个 call 最慢（最后完成），结果数组仍必须按请求顺序对应。
    #[tokio::test]
    async fn batch_returns_results_in_request_order() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let (status, body) = oneshot_json(
            router(st),
            req_post(
                "/batch",
                Some("secret"),
                json!({"calls": [
                    {"tool": "slow_alpha", "project_root": "D:/x", "args": {}, "lang": null},
                    {"tool": "beta", "project_root": "D:/x", "args": {}, "lang": null},
                    {"tool": "gamma", "project_root": "D:/x", "args": {}, "lang": null},
                ]}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        let results = body["results"].as_array().expect("results array");
        assert_eq!(results.len(), 3);
        // 完成序是 beta/gamma/…/slow_alpha；若丢 idx 排序，首位必不是 slow_alpha。
        assert_eq!(results[0]["tool"], "slow_alpha");
        assert_eq!(results[1]["tool"], "beta");
        assert_eq!(results[2]["tool"], "gamma");
        // value 回显去前缀的名字——位置 i 的 value 必须来自请求 i 的调用。
        assert_eq!(results[0]["value"], "alpha");
        assert_eq!(results[1]["value"], "beta");
        assert_eq!(results[2]["value"], "gamma");
    }

    /// 失败隔离：一条 Err 不影响其余，各自带 ok 标志与 9 码 wire error。
    #[tokio::test]
    async fn batch_isolates_failures() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let (status, body) = oneshot_json(
            router(st),
            req_post(
                "/batch",
                Some("secret"),
                json!({"calls": [
                    {"tool": "alpha", "project_root": "D:/x", "args": {}, "lang": null},
                    {"tool": "bad_beta", "project_root": "D:/x", "args": {}, "lang": null},
                    {"tool": "gamma", "project_root": "D:/x", "args": {}, "lang": null},
                ]}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        let results = body["results"].as_array().expect("results array");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[1]["ok"], false);
        assert_eq!(results[1]["error"]["code"], "BAD_ARGS");
        assert_eq!(results[1]["error"]["retryable"], false);
        assert_eq!(results[2]["ok"], true);
    }

    /// 超 32 拒收：200 + {ok:false} + 既有 BAD_ARGS（9 码不变，不新发明）。
    #[tokio::test]
    async fn batch_rejects_too_large() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let calls: Vec<_> = (0..=MAX_BATCH_SIZE)
            .map(|i| {
                json!({"tool": format!("t{i}"), "project_root": "D:/x", "args": {}, "lang": null})
            })
            .collect();
        let (status, body) = oneshot_json(
            router(st),
            req_post("/batch", Some("secret"), json!({"calls": calls})),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "BAD_ARGS");
        assert!(body.get("results").is_none(), "拒收响应不含 results");
    }

    /// 空批同契约拒收。
    #[tokio::test]
    async fn batch_rejects_empty() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let (status, body) = oneshot_json(
            router(st),
            req_post("/batch", Some("secret"), json!({"calls": []})),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "BAD_ARGS");
    }
}
