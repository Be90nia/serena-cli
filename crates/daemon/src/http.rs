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
}

impl AppState {
    pub fn uptime_secs(&self) -> u64 {
        self.start_ts.elapsed().as_secs()
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
                "error": {"code": "SHUTTING_DOWN", "message": "daemon in ShutdownDraining; refusing new requests"}
            })),
        )
            .into_response();
    }

    match state
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
    }
}

async fn status_get(State(state): State<AppState>) -> Response {
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
    };
    (StatusCode::OK, Json(resp)).into_response()
}

async fn shutdown_post(State(state): State<AppState>) -> Response {
    state
        .draining
        .store(true, std::sync::atomic::Ordering::Release);
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
        result: tokio::sync::Mutex<
            Option<Box<dyn FnOnce() -> Result<serde_json::Value, supervisor::ToolError> + Send>>,
        >,
    }

    impl MockSupervisor {
        fn ok(data: serde_json::Value) -> Self {
            Self {
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn err(e: supervisor::ToolError) -> Self {
            Self {
                result: tokio::sync::Mutex::new(Some(Box::new(move || Err(e)))),
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
        // 2) 再请求工具：应 503
        let (status, _) = oneshot_json(
            r,
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, _) = oneshot_json(router(st), req_get("/no-such-path", Some("secret"))).await;
        assert_eq!(status, AxStatus::NOT_FOUND);
    }
}
