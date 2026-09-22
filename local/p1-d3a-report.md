# P1 d3a：编排 envelope + invocationId — 交付报告

分支 feature/solidlsp-phase0-1（未 commit）。bd: serena-rust-d3a。

## VERDICT

**DONE** — 全部 P0 判据满足（P0-8 workspace 全量见文末验证节）。

## 改动文件

| 文件 | 改动 |
|---|---|
| `crates/daemon/src/dto.rs` | +`WIRE_PROTOCOL_VERSION`、`InvocationEnvelope`、`CompatEvidence`；`ToolRequest.envelope: Option<_>`（default+skip，老 body 兼容）；+3 测试 |
| `crates/daemon/src/http.rs` | `AppState.invocation_log_path`；`tools_post` 加 `HeaderMap` + invocation 三级解析 + 成功/失败双路径记日志；`new_invocation_id()`（UUID v4，std 熵）；`log_invocation()`；+4 测试 |
| `crates/daemon/src/serve.rs` | +`default_invocation_log_path()`（daemon.lock 同目录 `invocations.jsonl`）；serve 注入生产路径 |
| `crates/cli/src/main.rs` | `--invocation-id` 全局 flag；forward 构造 body envelope + `X-Invocation-Id` header（含 token 刷新重发路径） |
| 测试 fixture | `reaper.rs` / `tests/cold_start_repro.rs` 空路径占位（不写日志）——0bq 代补 |

新依赖：0。UUID 走 std 熵（时间纳秒 + pid + AtomicU64 计数器，同 lockfile `gen_token` 惯例）。

## 设计决策

1. **响应结构 0 改动**：envelope 仅请求侧（body 顶层可选 `envelope` 字段 + `X-Invocation-Id` header）。成功/失败响应 wire 完全不变（e2e 实测：`{"ok":false,"error":{"code":"BAD_ARGS",...}}` 无 envelope 泄漏）。orca review §2.1 建议的响应回显 `X-Invocation-Id` header **未做**（任务约束"envelope 只附加在请求侧"，判据未要求）。
2. **invocation_id 三级来源**：body `envelope.invocation_id` > `X-Invocation-Id` header > 自动生成。body 优先：编排器幂等键语义完整（带版本+证据），header 是轻量透传。
3. **重放日志索引化**：JSONL append，行首键即 `invocation_id`，`grep <id> invocations.jsonl` 即索引（零新抽象，无内存表）。成功/失败都记；`error_code` 走 serde（`Option<WireErrorCode>` → `null`/`"BAD_ARGS"`）。写失败仅 warn 不影响工具执行。
4. **写工具幂等去重未做**：orca §2.1 判据 3，不在本任务 P0 清单。
5. **并发协调**：http.rs 与 0bq（Json(resp) 直传优化）同函数交汇——保留其优化形态，经 hub 划界（serve.rs/reaper.rs fixture 归 0bq，http.rs 测试区归 d3a）。

## P0 判据逐条

| # | 判据 | 证据 |
|---|---|---|
| 1 | /tools/* header 透传 | e2e C：`X-Invocation-Id: d3a-curl-header-003` → 日志行同 id |
| 2 | 缺省自动生成 | e2e B/E：`18d789ab-ff4c-4000-9357-89abd74cff4c`（v4 形状：版本位 4、变体 9）|
| 3 | body envelope 3 键，成功失败都带 | dto 测试 `envelope_roundtrip_with_compat_evidence`；失败路径 e2e 5 条日志含 error_code；成功路径单测 `missing_invocation_id_generates_uuid_v4_and_logs`（ok:true 行） |
| 4 | CLI --invocation-id 覆写 | e2e D：`d3a-cli-e2e-002` 落日志 |
| 5 | e2e curl + cli 双路径 | curl A/B/C + cli D/E，全部 200/BAD_ARGS 符合 wire，日志 5 行见下 |
| 6 | 9 错误码不动 | e2e 响应 `{"ok":false,"error":{"code":"BAD_ARGS",...}}`；dto 错误码表零改动 |
| 8 | workspace 0 FAILED | 见验证节 |

e2e 日志实录（%LOCALAPPDATA%/serena/invocations.jsonl）：

```jsonl
{"invocation_id":"d3a-curl-envelope-001",...,"ok":false,"error_code":"BAD_ARGS"}   ← envelope 赢 header
{"invocation_id":"18d789ab-ff4c-4000-9357-89abd74cff4c",...}                        ← 自动生成 v4
{"invocation_id":"d3a-curl-header-003",...}                                         ← header 兜底
{"invocation_id":"d3a-cli-e2e-002",...,"tool":"find-symbol"}                        ← CLI 覆写
{"invocation_id":"18d789b6-cbac-4000-936c-89b66373cbac",...,"tool":"find-symbol"}   ← CLI 默认
```

## 验证

- `cargo check -p daemon -p cli`：绿
- `cargo test -p daemon --lib`：46 passed / 0 failed（含 7 个新 d3a 测试）
- `cargo clippy -p daemon -p cli --all-targets -- -D warnings`：0 errors
- `cargo test --workspace`：见 Main 收口轮（P0-8 由全量套件仲裁）
- e2e：真 daemon（port 7860）+ curl 3 路径 + cli 2 路径，日志 5 行全对

## 未做（明确不在本轮）

- 写工具幂等键去重（orca §2.1 判据 3；需 supervisor 入口签名动 InvocationId，本任务范围 d3a→daemon）
- 响应回显 `X-Invocation-Id`/`X-Schema-Version` header（orca 建议但任务约束请求侧-only）
- `/batch` 的 envelope 支持（P0 判据仅 /tools/*）
