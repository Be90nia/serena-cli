//! Daemon HTTP 前端（PLAN Task 12 / ARCHITECTURE §6.3）。
//!
//! - `POST /tools/{name}`：执行工具，返回 wire DTO。
//! - `GET /status`：uptime + 进程统计。
//! - `POST /shutdown`：进入 ShutdownDraining。
//!
//! Middleware：`X-Serena-Token` 必须与 lock 文件 token 一致；不符 403。
//! 工具级失败走 200 + `{ok:false}`（A5），transport 错误才用 4xx/5xx。
//! 503 用于 ShutdownDraining 拒绝新请求（I8）。

use std::collections::{HashSet, VecDeque};
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
    StatusRecentError, StatusResponse, ToolResponse, WireError, WireErrorCode,
    wire_error_code_to_exit, wire_error_from_tool_error,
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
    /// bd ts9d：已报过 switched warning 的 (from, to) 对。daemon 生命周期内
    /// 同对只报一次，多 fixture 轮换不逐响应刷 warning；重启后自然重报。
    pub switch_reported: Arc<std::sync::Mutex<HashSet<(String, String)>>>,
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
    /// 7rh：SERENA_NO_TOKEN_ESTIMATE=1 时工具成功响应不附 `~tokens` 估算。
    /// daemon 启动读一次（serve），测试直接注入 bool 保持隔离。
    pub no_token_estimate: bool,
    /// bd 7tk/e1p：观测面（/status 四字段 + invocation 日志增强）共享态。
    pub obs: ObsState,
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

// ==== bd 7tk/e1p：观测面（/status 四字段 + invocations.jsonl 增强字段）====

/// 冷启动窗：daemon 启动后此窗内的调用在重放日志标 `cold_start: true`
/// （audit-prod-replay「spawn 后首调用 5s 内」切片判据）。R10-F03 的冷
/// LS_TIMEOUT 调用耗 30s+，故在请求到达时判定而非落日志时——否则漏标。
const COLD_START_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
/// recent_errors / recent_agents 环容量（status 载荷保持小）。
const OBS_RING_CAP: usize = 8;
/// invocation_id → 历史次数表容量（长寿命 daemon 防泄漏；越过即 FIFO 驱逐，
/// 被驱逐 id 的再次重试重新从 0 计，重放分析以窗口为准）。
const SEEN_MAP_CAP: usize = 4096;

/// 观测共享态。集中成束：AppState 各构造点只需一行 `obs: Default::default()`。
#[derive(Clone, Default)]
pub struct ObsState {
    invocation_count: Arc<std::sync::atomic::AtomicU64>,
    recent_errors: Arc<std::sync::Mutex<VecDeque<StatusRecentError>>>,
    /// 最近 N 个 invocation_id 前 8 字符前缀（多 agent 场景区分编排方；重见即刷新最新）。
    recent_agents: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// 同一 invocation_id 的历史出现次数（retry_count 差分）+ 首见序（FIFO 驱逐）。
    seen: Arc<std::sync::Mutex<SeenInvocations>>,
    /// bd xwi：当前日志代内已追加行数（轮转判据之一，轮转后清零）。
    log_lines: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
struct SeenInvocations {
    counts: std::collections::HashMap<String, u64>,
    order: std::collections::VecDeque<String>,
}

impl ObsState {
    /// log_invocation 唯一记账点：计数 +1、错误/agent 前缀入环；返回该 id 的
    /// 历史出现次数（0 = 首次；重放分析里 retry_count 即此值）。
    fn record(&self, invocation_id: &str, tool: &str, error_code: Option<WireErrorCode>) -> u64 {
        self.invocation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(code) = error_code {
            let mut ring = self.recent_errors.lock().unwrap();
            ring.push_back(StatusRecentError {
                ts_ms: now_ms(),
                tool: tool.to_owned(),
                code,
            });
            while ring.len() > OBS_RING_CAP {
                ring.pop_front();
            }
        }
        let prefix: String = invocation_id.chars().take(8).collect();
        let mut agents = self.recent_agents.lock().unwrap();
        if let Some(pos) = agents.iter().position(|p| *p == prefix) {
            agents.remove(pos);
        }
        agents.push_back(prefix);
        while agents.len() > OBS_RING_CAP {
            agents.pop_front();
        }
        let mut seen = self.seen.lock().unwrap();
        let entry = seen.counts.entry(invocation_id.to_owned()).or_insert(0);
        let prior = *entry;
        *entry += 1;
        if prior == 0 {
            // 只记首见序：重试不重复入队，驱逐时键与序同删不悬空。
            seen.order.push_back(invocation_id.to_owned());
            while seen.order.len() > SEEN_MAP_CAP
                && let Some(oldest) = seen.order.pop_front()
            {
                seen.counts.remove(&oldest);
            }
        }
        prior
    }

    fn invocation_count(&self) -> u64 {
        self.invocation_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn recent_errors_snapshot(&self) -> Vec<StatusRecentError> {
        self.recent_errors.lock().unwrap().iter().cloned().collect()
    }

    fn recent_agents_snapshot(&self) -> Vec<String> {
        self.recent_agents.lock().unwrap().iter().cloned().collect()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

/// in_flight RAII 守卫（audit 竞锁 #10 后半）：handler 入口 fetch_add 后持有，
/// Drop 即 -1——future 取消（客户端断连 drop handler future）时手动 fetch_sub
/// 不执行会让排空窗口计数只增不减。/tools 与 /batch 共用。
struct InFlightGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
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
    state
        .in_flight
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let in_flight = InFlightGuard {
        counter: Arc::clone(&state.in_flight),
    };
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
    // bd serena-rust-h4i：daemon 全局单 project 语义——跨 project 调用会隐式切换
    // active_project（session 池 per (root, lang)，LRU 复用）。切换发生时在响应
    // data 顶层附 warning，让 AI 感知 project 已变；同 project 连续调用零噪音，
    // 同 (from,to) 对只报一次（bd ts9d，多 fixture 轮换不逐响应重报）。
    let prev_project = state
        .active_project
        .lock()
        .unwrap()
        .replace(req.project_root.clone());

    let started = std::time::Instant::now();
    // bd e1p：冷启动标记在到达时判定（冷 LS_TIMEOUT 调用耗 30s+，落日志时判
    // uptime 会漏标）；wait_gen 提前取（execute 消费 args 所有权）；缓存命中
    // 基线供差分（supervisor 默认实现恒 0 → false）。
    let cold_start = state.start_ts.elapsed() < COLD_START_WINDOW;
    let wait_gen = req.args.get("wait_gen").and_then(|v| v.as_u64());
    let cache_before = state.supervisor.cache_hits_total();
    // bd wy1：写类工具（undo::WRITE_TOOLS）断连 detach —— handler future 随客户端
    // 断开被 drop 时，半途取消会让 TxnGuard 兜底 abort，已落盘的写丢账（Q4 根因）。
    // spawn 进运行时独立执行（JoinHandle drop 仅 detach 不取消），事务 commit/abort
    // 必然收口；正常路径 handler 照常 await 结果返回响应，断连客户端重连后经
    // undo/diff 可见该账。InFlightGuard 随任务转移：断连后计数保持到任务真正完成，
    // 排空/空闲自杀判定仍以实际执行为准。
    let exec = {
        let sup = Arc::clone(&state.supervisor);
        let tool = name.clone();
        let root = req.project_root.clone();
        let args = req.args;
        let lang = req.lang.clone();
        async move { sup.execute_tool(&tool, &root, args, lang.as_deref()).await }
    };
    let result = if supervisor::undo::is_write_tool(&name) {
        match tokio::spawn(async move {
            let _detached = in_flight;
            exec.await
        })
        .await
        {
            Ok(r) => r,
            // 仅任务体 panic 时走到；写门/事务自身的错误已在 r 内。
            Err(e) => Err(supervisor::ToolError::Protocol {
                tool: name.clone(),
                reason: format!("detached write task failed: {e}"),
            }),
        }
    } else {
        let _held = in_flight;
        exec.await
    };
    match result {
        Ok(mut data) => {
            if let Some(prev) = prev_project.filter(|p| *p != req.project_root) {
                // bd ts9d：insert 返回 false = 该 (from,to) 对已报过 → 静默跳过。
                let first_report = state
                    .switch_reported
                    .lock()
                    .unwrap()
                    .insert((prev.clone(), req.project_root.clone()));
                if first_report {
                    let switch =
                        format!("project switched: {prev} -> {}", req.project_root);
                    // 工具自身的 warning（如 we0/xzb 就绪标记）不覆盖，拼接保序。
                    let combined = match data.get("warning").and_then(|w| w.as_str()) {
                        Some(existing) => format!("{existing}; {switch}"),
                        None => switch,
                    };
                    supervisor::attach_warning(&mut data, &[combined]);
                    // blindtest v5 P2-E：切换后首个查询的空 items 是「新 project 的
                    // LS 会话未就绪」而非权威空（csharp/angular 实锤 hover 语义通、
                    // ov 空）——禁止静默空数组，结构化标 degraded 让 AI 重查。
                    // 限定 overview：find-symbol 等已有自身 not-ready 降级机器；
                    // list-dir/find-file 空结果是 fs 权威，不在此列。
                    if name == "overview"
                        && data
                            .get("items")
                            .and_then(|v| v.as_array())
                            .is_some_and(|a| a.is_empty())
                    {
                        supervisor::attach_degraded(
                            &mut data,
                            supervisor::Degraded::SemanticPending,
                        );
                    }
                }
            }
            let facts = CallFacts {
                cache_hit: state.supervisor.cache_hits_total() > cache_before,
                wait_gen,
                pending: data.get("pending").and_then(|v| v.as_bool()),
            };
            log_invocation(
                &state,
                InvocationRecord {
                    invocation_id: &invocation_id,
                    tool: &name,
                    project_root: &req.project_root,
                    ok: true,
                    error_code: None,
                    elapsed: started.elapsed(),
                    facts,
                    cold_start,
                },
            );
            // 7rh：token 估算 = 响应序列化字节 / 4（无 tokenizer 依赖）。tokens
            // 依赖最终字节数，只能先计量再发送（两遍 serialize，Value 零 clone：
            // take 出去计量再还回）。SERENA_NO_TOKEN_ESTIMATE=1 时跳过，保持单遍。
            let approx_tokens = if state.no_token_estimate {
                None
            } else {
                let probe = ToolResponse::Ok {
                    ok: true,
                    data: std::mem::take(&mut data),
                    format: None,
                    approx_tokens: None,
                };
                let n = serde_json::to_vec(&probe)
                    .ok()
                    .map(|b| (b.len() / 4) as u64);
                // 计量后把 data 还回（probe 按构造恒为 Ok 变体）。
                if let ToolResponse::Ok { data: measured, .. } = probe {
                    data = measured;
                }
                n
            };
            let resp = ToolResponse::Ok {
                ok: true,
                data,
                format: None,
                approx_tokens,
            };
            // Direct Serialize (no intermediate Value clone). For large responses
            // (search 200+ hits, refs, repo-map) saves ~500µs / 84% vs the prior
            // to_value+serialize double walk — see local/p2-0bq-bench.rs.
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(err) => {
            let wire = wire_error_from_tool_error(&err);
            let facts = CallFacts {
                cache_hit: state.supervisor.cache_hits_total() > cache_before,
                wait_gen,
                pending: None,
            };
            log_invocation(
                &state,
                InvocationRecord {
                    invocation_id: &invocation_id,
                    tool: &name,
                    project_root: &req.project_root,
                    ok: false,
                    error_code: Some(wire.code),
                    elapsed: started.elapsed(),
                    facts,
                    cold_start,
                },
            );
            let status = StatusCode::OK; // 工具级失败走 200（A5）
            let resp = ToolResponse::Err {
                ok: false,
                error: wire,
            };
            (status, Json(resp)).into_response()
        }
    }
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

/// 单次工具调用的观测事实（bd e1p）——invocations.jsonl 增强字段的数据源。
#[derive(Debug, Clone, Copy, Default)]
struct CallFacts {
    /// supervisor 符号缓存命中（调用前后 cache_hits_total 差分）。
    cache_hit: bool,
    /// 请求 args 携带的 diagnostics `wait_gen`（未携带 = None → null）。
    wait_gen: Option<u64>,
    /// 响应 data 的 `pending` 旗（diagnostics 形态超时未确认时 true）。
    pending: Option<bool>,
}

/// log_invocation 入参束（bd e1p：消 9 参超限，字段自释名）。
struct InvocationRecord<'a> {
    invocation_id: &'a str,
    tool: &'a str,
    project_root: &'a str,
    ok: bool,
    error_code: Option<WireErrorCode>,
    elapsed: std::time::Duration,
    facts: CallFacts,
    /// 请求到达时 daemon 是否仍在冷启动窗（bd e1p）。
    cold_start: bool,
}

/// bd xwi：invocations.jsonl 封顶轮转参数。单代大小/条数任一越限即轮转；
/// 留 `KEEP` 代历史（`.1` 最新 … `.N` 最旧），最旧删除。磁盘上界 =
/// `(KEEP+1) × 100MB`。
const INVOCATION_LOG_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// 条数封顶（超短行的极端场景兜底）。
const INVOCATION_LOG_MAX_LINES: u64 = 1_000_000;
/// 保留的轮转代数（.1 ~ .2）。
const INVOCATION_LOG_KEEP: u32 = 2;
/// 轮转探测节流窗：每 N 次追加才做一次 metadata 探测（高频工具调用下
/// 每条都 stat 是纯浪费；8MB 增量粒度足够，越限滞后 ≤ 一个节流窗）。
const INVOCATION_LOG_CHECK_EVERY: u64 = 4096;

/// 第 i 代轮转文件路径（`invocations.jsonl.1` …）。
fn invocation_log_gen(path: &std::path::Path, i: u32) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(format!(".{i}"));
    std::path::PathBuf::from(s)
}

/// 轮转：当前代后移一代，最旧删除。失败只 warn——日志轮转绝不影响工具执行。
/// 逐代先 remove 再 rename：消费端（tail -f 等）占住旧代时 Windows rename
/// 会共享冲突，remove 失败也不阻断后续代次位移。
pub(crate) fn rotate_invocation_log(path: &std::path::Path, keep: u32) {
    let _ = std::fs::remove_file(invocation_log_gen(path, keep));
    for i in (1..keep).rev() {
        let _ = std::fs::rename(invocation_log_gen(path, i), invocation_log_gen(path, i + 1));
    }
    if let Err(e) = std::fs::rename(path, invocation_log_gen(path, 1)) {
        eprintln!(
            "[serena] invocation log rotate failed (path={:?}): {e}; append continues",
            path
        );
    } else {
        tracing::info!(path = %path.display(), keep, "invocation log rotated");
    }
}

/// 越限即轮转（bd xwi）。返回是否轮转了。
/// 大小判据读 metadata（对启动时接管超大旧文件也成立）；条数由调用方累计。
pub(crate) fn rotate_invocation_log_if_oversized(
    path: &std::path::Path,
    max_bytes: u64,
    keep: u32,
) -> bool {
    let oversized = std::fs::metadata(path).map(|m| m.len() >= max_bytes).unwrap_or(false);
    if oversized {
        rotate_invocation_log(path, keep);
    }
    oversized
}

/// serve 启动时的生产参数轮转检查（bd xwi：接管超大旧日志）。
pub(crate) fn rotate_invocation_log_at_startup(path: &std::path::Path) {
    rotate_invocation_log_if_oversized(path, INVOCATION_LOG_MAX_BYTES, INVOCATION_LOG_KEEP);
}

/// 追加一条工具调用记录到重放日志（d3a）。JSONL，行首键即 invocation_id
/// （`grep <id> invocations.jsonl` 即索引）。写失败只 warn 不影响工具执行。
/// bd e1p/dt1：尾部追加 cache_hit/wait_gen/pending/cold_start/retry_count——
/// 旧消费者按键读取不受影响；retry_count = 同 id 在本 daemon 内的历史请求数。
fn log_invocation(state: &AppState, rec: InvocationRecord<'_>) {
    // 记账先于路径判断：日志关断（空路径）时 /status 观测面仍计数。
    let retry_count = state
        .obs
        .record(rec.invocation_id, rec.tool, rec.error_code);
    if state.invocation_log_path.as_os_str().is_empty() {
        return;
    }
    // bd xwi：封顶轮转。代内行数达节流窗倍数才探测一次；大小或条数任一
    // 越限即轮转（本代行数计数随之清零）。
    let lines = state.obs.log_lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if lines % INVOCATION_LOG_CHECK_EVERY == 0 {
        let oversized = std::fs::metadata(&state.invocation_log_path)
            .map(|m| m.len() >= INVOCATION_LOG_MAX_BYTES)
            .unwrap_or(false);
        if oversized || lines >= INVOCATION_LOG_MAX_LINES {
            rotate_invocation_log(&state.invocation_log_path, INVOCATION_LOG_KEEP);
            state
                .obs
                .log_lines
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let line = json!({
        "invocation_id": rec.invocation_id,
        "ts_ms": now_ms(),
        "tool": rec.tool,
        "project_root": rec.project_root,
        "ok": rec.ok,
        // Option<WireErrorCode> 直接走 serde：Some → SCREAMING_SNAKE_CASE，None → null。
        "error_code": rec.error_code,
        "duration_ms": rec.elapsed.as_millis() as u64,
        // bd e1p/dt1 追加字段。
        "cache_hit": rec.facts.cache_hit,
        "wait_gen": rec.facts.wait_gen,
        "pending": rec.facts.pending,
        "cold_start": rec.cold_start,
        "retry_count": retry_count,
    });
    if let Err(e) = crate::lockfile::secure_open()
        .create(true)
        .append(true)
        .open(&state.invocation_log_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()))
    {
        eprintln!(
            "[serena] invocation log append failed (path={:?}): {e}; tool execution unaffected",
            state.invocation_log_path
        );
    }
}

/// `GET /status`：纯诊断查询。不刷 activity 时钟——idle 监控脚本轮询 status
/// 不能让 15min idle 自杀永不触发（bd b40）；真正的负载信号是 in_flight 计数。
async fn status_get(State(state): State<AppState>) -> Response {
    // bd b09i：结构化 {lang, sessions}——同 lang 多 (root,lang) 键折叠计数，
    // 替代旧 ["rust x3"] 字符串形态。
    let mut by_lang: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for k in state.supervisor.loaded_entries() {
        *by_lang.entry(k.lang.to_string()).or_insert(0) += 1;
    }
    let loaded = by_lang
        .into_iter()
        .map(|(lang, sessions)| crate::dto::LoadedLs { lang, sessions })
        .collect::<Vec<_>>();
    let resp = StatusResponse {
        uptime_secs: state.uptime_secs(),
        pid: std::process::id(),
        loaded_ls: loaded,
        draining: state.draining.load(std::sync::atomic::Ordering::Acquire),
        active_project: state.active_project.lock().unwrap().clone(),
        // bd 7tk：观测四字段（wire v1 追加式）。
        in_flight: state.in_flight.load(std::sync::atomic::Ordering::Acquire) as u64,
        invocation_count: state.obs.invocation_count(),
        recent_errors: state.obs.recent_errors_snapshot(),
        recent_agents: state.obs.recent_agents_snapshot(),
        // bd ulq：版本与 binary 路径自检。
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        binary_path: std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
    };
    (StatusCode::OK, Json(resp)).into_response()
}

async fn shutdown_post(State(state): State<AppState>) -> Response {
    // 幂等：重复 /shutdown（stop-all 重试）不叠加 drain 窗口。
    if state
        .draining
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
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
        hint: None,
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
            hint: None,
        };
        return (
            StatusCode::OK, // 工具级失败走 200（A5）；batch 尺寸违规同契约
            Json(json!({"ok": false, "error": wire})),
        )
            .into_response();
    }
    state
        .in_flight
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    // bd wy1 注：batch 的计数 guard 不随写任务转移（整批一把，per-call 转移要
    // 穿 run_batch 两层泛型）——断连时计数提前归零只损观测精度；写账完整性由
    // execute_batch_call 内层 spawn 独立保证。
    let _in_flight = InFlightGuard {
        counter: Arc::clone(&state.in_flight),
    };
    crate::reaper::note_activity();
    // batch 可混多项目；active_project 记最后一个请求的 root（「最近请求」语义同 tools_post）。
    if let Some(root) = req.calls.last().map(|c| c.project_root.clone()) {
        *state.active_project.lock().unwrap() = Some(root);
    }
    let results = run_batch(&state, req.calls).await;
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
                        hint: None,
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
    // bd wy1：写类调用同样 detach —— batch_handler 的 run_batch JoinSet 在
    // 客户端断连（handler drop）时 abort 内含任务，写工具半途取消丢账；
    // tokio::spawn 的独立任务不受 JoinSet abort 影响，跑完 commit/abort 收口。
    let sup2 = Arc::clone(sup);
    let tool = call.tool.clone();
    let root = call.project_root.clone();
    let lang = call.lang.clone();
    let args = call.args;
    let exec = async move { sup2.execute_tool(&tool, &root, args, lang.as_deref()).await };
    let r = if supervisor::undo::is_write_tool(&call.tool) {
        match tokio::spawn(exec).await {
            Ok(r) => r,
            Err(e) => Err(supervisor::ToolError::Protocol {
                tool: call.tool.clone(),
                reason: format!("detached write task failed: {e}"),
            }),
        }
    } else {
        exec.await
    };
    match r {
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
        /// v5 P2-E：可重复 Ok(value)——switch 场景断言需同一 AppState 内多调
        /// （active_project/switch_reported 在 state 里，换 state 即丢记忆）。
        always: Option<serde_json::Value>,
        result: tokio::sync::Mutex<
            Option<Box<dyn FnOnce() -> Result<serde_json::Value, supervisor::ToolError> + Send>>,
        >,
    }

    impl MockSupervisor {
        fn ok(data: serde_json::Value) -> Self {
            Self {
                delay: None,
                echo: false,
                always: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn ok_repeat(data: serde_json::Value) -> Self {
            Self {
                delay: None,
                echo: false,
                always: Some(data),
                result: tokio::sync::Mutex::new(None),
            }
        }
        fn err(e: supervisor::ToolError) -> Self {
            Self {
                delay: None,
                echo: false,
                always: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Err(e)))),
            }
        }
        fn slow(data: serde_json::Value, delay: std::time::Duration) -> Self {
            Self {
                delay: Some(delay),
                echo: false,
                always: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || Ok(data)))),
            }
        }
        fn echo_by_tool() -> Self {
            Self {
                delay: None,
                echo: true,
                always: None,
                result: tokio::sync::Mutex::new(None),
            }
        }
        /// bd wy1：慢写任务 + 完成信号 —— 断连 detach 测试用。
        fn detached_write_probe(
            tx: tokio::sync::mpsc::Sender<&'static str>,
        ) -> Self {
            Self {
                delay: Some(std::time::Duration::from_millis(150)),
                echo: false,
                always: None,
                result: tokio::sync::Mutex::new(Some(Box::new(move || {
                    let _ = tx.try_send("done");
                    Ok(json!("replace-body"))
                }))),
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
            if let Some(v) = &self.always {
                return Ok(v.clone());
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
            switch_reported: Arc::new(std::sync::Mutex::new(HashSet::new())),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            drain_window: std::time::Duration::from_millis(100),
            invocation_log_path,
            // 7rh：默认开估算；关闭开关的测试显式置 true。
            no_token_estimate: false,
            obs: ObsState::default(),
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

    /// bd serena-rust-h4i：跨 project 调用 → 响应附 `project switched` warning；
    /// 首调（active_project 尚为 None）与同 project 连续调用零噪音。
    #[tokio::test]
    async fn cross_project_call_attaches_switch_warning_once() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let router = router(st);
        let call = |root: &str| {
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({ "project_root": root, "args": {} }),
            )
        };
        // 首调：无 warning（echo 标量响应原样透传，不升级）。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-a")).await;
        let body = body.expect("json body");
        assert_eq!(body["data"], json!("overview"), "无 warning 时 wire 零变化");

        // 跨 project：data 升级携带 warning（标量响应走 attach_warning 升级通道）。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        let body = body.expect("json body");
        assert_eq!(body["ok"], true);
        let w = body["data"]["warning"]
            .as_str()
            .expect("warning 键必须存在");
        assert!(w.contains("project switched"), "{w}");
        assert!(w.contains("D:/proj-a"), "{w}");
        assert!(w.contains("D:/proj-b"), "{w}");
        assert_eq!(
            body["data"]["items"],
            json!("overview"),
            "升级形态不丢原响应内容"
        );

        // 同 project 连续调用：无 warning。
        let (_, body) = oneshot_json(router, call("D:/proj-b")).await;
        let body = body.expect("json body");
        assert_eq!(body["data"], json!("overview"), "同 project 无 warning");
    }

    /// blindtest v5 P2-E：切换后首个 overview 空 items = LS 未就绪而非权威空 →
    /// 结构化 degraded + warmup 标记，禁静默空数组；非空结果与重复切换不标。
    #[tokio::test]
    async fn switch_first_empty_overview_gets_degraded_marker() {
        let st = state("secret", MockSupervisor::ok_repeat(json!([])));
        let router = router(st);
        let call = |root: &str| {
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({ "project_root": root, "args": {} }),
            )
        };
        // 首调 proj-a：active_project 尚为 None → 无 switch，data 保持裸数组。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-a")).await;
        let body = body.expect("json body");
        assert_eq!(body["data"], json!([]));
        assert!(body["data"].get("degraded").is_none());

        // 切到 proj-b：warning + degraded + warmup 三标记齐上。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        let body = body.expect("json body");
        assert_eq!(body["data"]["degraded"], json!("semantic-pending"));
        assert_eq!(
            body["data"]["warmup"]["retry_after_warm"],
            json!(true),
            "切换场景可重查（与 rust-no-cargo 的 false 相区隔）"
        );
        assert!(body["data"]["items"].as_array().unwrap().is_empty());

        // 同 project 重复调用（B→B 无 switch）：不再标。
        let (_, body) = oneshot_json(router, call("D:/proj-b")).await;
        let body = body.expect("json body");
        assert!(body["data"].get("degraded").is_none());
    }

    /// bd ts9d：同 (from,to) 对 daemon 生命周期内只报一次——A→B 二次调用
    /// 不再附 warning；反向 (B,A) 是新对，首次仍报。
    #[tokio::test]
    async fn switch_warning_deduped_per_direction_pair() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let router = router(st);
        let call = |root: &str| {
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({ "project_root": root, "args": {} }),
            )
        };
        let warning_of =
            |body: serde_json::Value| body["data"]["warning"].as_str().map(str::to_owned);

        // 首调：零噪音。A→B：首次报。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-a")).await;
        assert_eq!(warning_of(body.expect("json body")), None, "首调零噪音");
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        assert!(
            warning_of(body.expect("json body")).is_some(),
            "首次跨 project 必须报"
        );

        // 同 project：不报。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        assert_eq!(
            warning_of(body.expect("json body")),
            None,
            "同 project 不报"
        );

        // 反向 B→A：新对，首次报；B→A 二次：同对去重不报。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-a")).await;
        assert!(
            warning_of(body.expect("json body")).is_some(),
            "反向 B→A 是新对，首次报"
        );
        let (_, body) = oneshot_json(router, call("D:/proj-a")).await;
        let body = body.expect("json body");
        assert_eq!(
            warning_of(body.clone()),
            None,
            "(B,A) 同对第二次不报（会话级去重）"
        );
        assert_eq!(body["data"], json!("overview"), "去重路径 wire 零变化");
    }

    /// bd ts9d：去重按 (from,to) 对记账且跨切换持续——A→B 报过一次后，经
    /// B→A→B→A 折返，两个方向各至多一条，折返不再重报。
    #[tokio::test]
    async fn switch_warning_dedup_persists_across_alternating_switches() {
        let st = state("secret", MockSupervisor::echo_by_tool());
        let router = router(st);
        let call = |root: &str| {
            req_post(
                "/tools/overview",
                Some("secret"),
                json!({ "project_root": root, "args": {} }),
            )
        };
        let has_warning =
            |body: serde_json::Value| body["data"].get("warning").is_some();

        // 首调 + A→B 首报 + B→A 首报。
        let _ = oneshot_json(router.clone(), call("D:/proj-a")).await;
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        assert!(has_warning(body.expect("json body")), "A→B 首报");
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-a")).await;
        assert!(has_warning(body.expect("json body")), "B→A 首报");

        // 折返 A→B / B→A：两对均已记账，全部静默。
        let (_, body) = oneshot_json(router.clone(), call("D:/proj-b")).await;
        assert!(
            !has_warning(body.expect("json body")),
            "(A,B) 已报过，折返不重报"
        );
        let (_, body) = oneshot_json(router, call("D:/proj-a")).await;
        assert!(
            !has_warning(body.expect("json body")),
            "(B,A) 已报过，折返不重报"
        );
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

    /// 7rh：成功响应附 `~tokens` 整数估算，且 >0（字节/4）。
    #[tokio::test]
    async fn tool_success_carries_token_estimate() {
        let st = state("secret", MockSupervisor::ok(json!({"n": 1})));
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
        let tokens = body
            .get("~tokens")
            .and_then(|v| v.as_u64())
            .expect("~tokens 必须是非负整数");
        assert!(tokens > 0, "估算必须 >0，got {tokens}");
    }

    /// 7rh：no_token_estimate（SERENA_NO_TOKEN_ESTIMATE=1）时字段不上 wire。
    #[tokio::test]
    async fn token_estimate_opt_out_strips_field() {
        let mut st = state("secret", MockSupervisor::ok(json!({"n": 1})));
        st.no_token_estimate = true;
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
        assert!(body.get("~tokens").is_none(), "开关打开时不得附 ~tokens");
    }

    /// 7rh：错误响应（9 码 wire）不带 ~tokens——错误结构零变动。
    #[tokio::test]
    async fn error_response_has_no_token_estimate() {
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
        assert!(body.get("~tokens").is_none(), "错误响应不得附 ~tokens");
    }

    #[tokio::test]
    async fn status_returns_uptime_and_loaded_ls() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let (status, body) = oneshot_json(router(st), req_get("/status", Some("secret"))).await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert!(body["uptime_secs"].is_u64());
        assert_eq!(
            body["loaded_ls"],
            json!([{ "lang": "rust", "sessions": 1 }]),
            "bd b09i：结构化 lang/sessions 对象"
        );
        assert_eq!(body["draining"], false);
    }

    /// b40：status 是纯诊断，不得刷新全局 activity 时钟——否则 idle 监控
    /// 脚本轮询 /status 即可让 15min idle 自杀永不触发。
    /// 全局时钟是进程级单例：并行的 tools/batch 类测试 note_activity 会打穿
    /// before/after 单次取样（与是否回归无关）。轮询到并行噪音静止后再断言——
    /// status_get 若真刷钟则 before==after 永不成立，deadline 处必失败。
    #[tokio::test]
    async fn status_get_does_not_refresh_activity() {
        let st = state("secret", MockSupervisor::ok(json!(null)));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let before = crate::reaper::last_activity();
            let (status, _) =
                oneshot_json(router(st.clone()), req_get("/status", Some("secret"))).await;
            assert_eq!(status, AxStatus::OK);
            let after = crate::reaper::last_activity();
            if before == after {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "status_get 不得刷新全局 activity 时钟（bd b40）"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
        st.in_flight.store(1, std::sync::atomic::Ordering::SeqCst);
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
    async fn write_tool_survives_client_disconnect() {
        // bd wy1：写类请求的 handler 被取消（模拟客户端断连 drop）时，detach 的
        // 执行任务必须跑完（commit/abort 收口），不能被一起取消丢账。
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let st = state("secret", MockSupervisor::detached_write_probe(tx));
        let router = router(st);
        let req = req_post(
            "/tools/replace-body",
            Some("secret"),
            json!({"project_root": "D:/x", "args": {}}),
        );
        let handle = tokio::spawn(router.oneshot(req));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        handle.abort(); // 模拟断连：axum drop handler future
        let done = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
        assert_eq!(
            done,
            Ok(Some("done")),
            "断连后 detach 的写任务必须跑完并发完成信号"
        );
    }

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

    // ── bd e1p/dt1：invocations.jsonl 观测字段 ──

    /// 追加字段形状：cache_hit/wait_gen/pending/cold_start/retry_count 恒在；
    /// wait_gen/pending 从请求 args 与响应 data 透传；同 id 二次请求 retry_count=1。
    #[tokio::test]
    async fn invocation_log_line_has_obs_fields_and_retry_count() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        // MockSupervisor 一次性消费 → 两次请求各用一个 state，但共享同一
        // ObsState（Arc 语义 = 同一 daemon 的观测记账，retry_count 差分成立）。
        let mut st1 = state_with_log(
            "secret",
            MockSupervisor::ok(json!({"items": [], "pending": true})),
            log.clone(),
        );
        let st2 = state_with_log(
            "secret",
            MockSupervisor::ok(json!({"items": [], "pending": true})),
            log.clone(),
        );
        st1.obs = st2.obs.clone();
        let mk = |body| {
            req_post_invocation("/tools/tool_diagnostics", Some("secret"), "agent-x-1234", body)
        };
        oneshot_json(
            router(st1),
            mk(json!({
                "project_root": "D:/x",
                "args": {"file": "a.py", "wait_gen": 3}
            })),
        )
        .await;
        oneshot_json(
            router(st2),
            mk(json!({
                "project_root": "D:/x",
                "args": {"file": "a.py", "wait_gen": 4}
            })),
        )
        .await;
        let rows = read_log(&log);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row["invocation_id"], "agent-x-1234");
            assert_eq!(row["cache_hit"], false, "mock 无缓存语义 → false");
            assert_eq!(row["pending"], true, "响应 data.pending 透传");
            assert!(row["cold_start"].is_boolean());
        }
        assert_eq!(rows[0]["wait_gen"], 3);
        assert_eq!(rows[1]["wait_gen"], 4);
        assert_eq!(rows[0]["retry_count"], 0, "首见 = 0");
        assert_eq!(rows[1]["retry_count"], 1, "同 id 第二次请求 = 1（bd dt1）");
    }

    /// 失败调用：错误码入 recent_errors 环，status 四字段形状（bd 7tk）。
    #[tokio::test]
    async fn status_exposes_obs_fields_after_error() {
        let st = state_with_log(
            "secret",
            MockSupervisor::err(supervisor::ToolError::BadArgs {
                detail: "missing file".into(),
            }),
            std::path::PathBuf::new(),
        );
        let router = router(st);
        let (status, _) = oneshot_json(
            router.clone(),
            req_post_invocation(
                "/tools/hover",
                Some("secret"),
                "err-agent-99",
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK, "工具级失败走 200（A5）");
        let (status, body) = oneshot_json(router, req_get("/status", Some("secret"))).await;
        assert_eq!(status, AxStatus::OK);
        let body = body.expect("json body");
        assert_eq!(body["in_flight"], 0, "请求已落定，无在飞");
        assert_eq!(body["invocation_count"], 1);
        let errs = body["recent_errors"].as_array().expect("errors ring");
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0]["tool"], "hover");
        assert_eq!(errs[0]["code"], "BAD_ARGS");
        assert!(errs[0]["ts_ms"].is_u64());
        let agents = body["recent_agents"].as_array().expect("agents ring");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0], "err-agen", "invocation_id 前 8 字符前缀");
    }

    // ---- bd xwi：invocations.jsonl 封顶轮转 ----

    #[test]
    fn invocation_log_rotate_shifts_generations_and_drops_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        std::fs::write(&log, "gen0\n").unwrap();
        std::fs::write(invocation_log_gen(&log, 1), "gen1\n").unwrap();
        std::fs::write(invocation_log_gen(&log, 2), "gen2\n").unwrap();
        rotate_invocation_log(&log, 2);
        assert!(!log.exists(), "当前代轮转后腾空待续写");
        assert_eq!(
            std::fs::read_to_string(invocation_log_gen(&log, 1)).unwrap(),
            "gen0\n",
            "旧当前代 → .1"
        );
        assert_eq!(
            std::fs::read_to_string(invocation_log_gen(&log, 2)).unwrap(),
            "gen1\n",
            "旧 .1 → .2"
        );
        // keep=2：旧 .2 已删，无 .3。
        assert!(!invocation_log_gen(&log, 3).exists(), "最旧代必须删除");
    }

    #[test]
    fn invocation_log_oversize_rotates_and_small_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        std::fs::write(&log, "x".repeat(64)).unwrap();
        assert!(
            !rotate_invocation_log_if_oversized(&log, 128, 2),
            "未越限不轮转"
        );
        assert!(log.exists(), "未轮转当前代保留");
        assert!(
            rotate_invocation_log_if_oversized(&log, 32, 2),
            "越限（含启动接管 164MB 旧文件场景）即轮转"
        );
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(invocation_log_gen(&log, 1)).unwrap().len(),
            64
        );
    }

    /// log_invocation 的条数封顶轮转：走真实 HTTP 路径把代内行数推到
    /// 「节流窗倍数且 ≥ MAX_LINES」的第一个触发点，断言旧代进 .1、新请求
    /// 续写新当前代、计数清零。
    #[tokio::test]
    async fn invocation_log_rotates_after_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("invocations.jsonl");
        std::fs::write(&log, "old gen\n").unwrap();
        let st = state_with_log("secret", MockSupervisor::ok(json!(null)), log.clone());
        // 触发条件 = lines % CHECK_EVERY == 0 && lines >= MAX_LINES，
        // 预置 counter 到满足两者的最小值减一，单次请求即命中。
        let target = (INVOCATION_LOG_MAX_LINES / INVOCATION_LOG_CHECK_EVERY + 1)
            * INVOCATION_LOG_CHECK_EVERY;
        st.obs
            .log_lines
            .store(target - 1, std::sync::atomic::Ordering::Relaxed);
        let (status, _) = oneshot_json(
            router(st.clone()),
            req_post_invocation(
                "/tools/hover",
                Some("secret"),
                "rot-agent-1",
                json!({"project_root": "D:/x", "args": {}}),
            ),
        )
        .await;
        assert_eq!(status, AxStatus::OK);
        assert_eq!(
            std::fs::read_to_string(invocation_log_gen(&log, 1)).unwrap(),
            "old gen\n",
            "越限轮转：旧当前代整体进 .1"
        );
        let cur = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            cur.lines().count(),
            1,
            "轮转后新当前代只有本次请求一条"
        );
        assert!(
            cur.contains("\"invocation_id\""),
            "新当前代是合法 JSONL 记录"
        );
        assert_eq!(
            st.obs.log_lines.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "轮转后代内行数计数清零"
        );
    }
}
