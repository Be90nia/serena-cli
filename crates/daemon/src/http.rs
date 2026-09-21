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
        .route("/status", get(status_get))
        .route("/shutdown", post(shutdown_post))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn tools_post(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<crate::dto::ToolRequest>,
) -> Response {
    if state.draining.load(std::sync::atomic::Ordering::Acquire) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Retry-After", "10")],
            Json(json!({
                "ok": false,
                "error": {"code": "DAEMON_DRAINING", "message": "daemon in ShutdownDraining; refusing new requests", "retryable": false}
            })),
        )
            .into_response();
    }
    state.in_flight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    crate::reaper::note_activity();

    // 记录最近请求的 project_root（供 /status 观察；不区分成败，只要请求到达）。
    *state.active_project.lock().unwrap() = Some(req.project_root.clone());

    let resp = match state
        .supervisor
        .execute_tool(&name, &req.project_root, req.args, req.lang.as_deref())
        .await
    {
        Ok(data) => {
            let resp = ToolResponse::Ok {
                ok: true,
                data,
                format: None,
            };
            (StatusCode::OK, Json(serde_json::to_value(&resp).unwrap())).into_response()
        }
        Err(err) => {
            let wire = wire_error_from_tool_error(&err);
            let status = StatusCode::OK; // 工具级失败走 200（A5）
            let resp = ToolResponse::Err {
                ok: false,
                error: wire,
            };
            (status, Json(serde_json::to_value(&resp).unwrap())).into_response()
        }
    };
    state.in_flight.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    resp
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
        result: tokio::sync::Mutex<
            Option<Box<dyn FnOnce() -> Result<serde_json::Value, supervisor::ToolError> + Send>>,
        >,
    }

    impl MockSupervisor {
        fn ok(data: serde_json::Value) -> Self {
            Self {
                delay: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn err(e: supervisor::ToolError) -> Self {
            Self {
                delay: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Err(e)))),
            }
        }
        fn slow(data: serde_json::Value, delay: std::time::Duration) -> Self {
            Self {
                delay: Some(delay),
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
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
        }
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
}
