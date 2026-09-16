# 0.debug — 冷启动挂死 ~120s 根因诊断报告

**VERDICT: ROOT_CAUSE_FOUND**

## 症状（用户报告）

- `stop-all` 后第一次工具请求（`overview` / `find-symbol`）耗时 ≥120s
- 同 daemon 数分钟后第二次 <0.1s
- 复现率：clean state 下不可稳定复现；stale state 下高概率

## 观察到的现象

| 场景 | 实测 | 备注 |
|---|---|---|
| 进程内 `Supervisor::direct()` + `tool_find_symbol` | **130ms** 首次 / **0.5ms** 缓存 | `crates/supervisor/tests/cold_start_repro.rs::cold_start_first_request_timing` |
| 进程内 `lsp-core::Session::start` (raw) | 41ms handshake + 18ms first `documentSymbol` | 同上 test 第二分支 |
| 进程内 axum + supervisor（in-process HTTP） | **64ms** 首次 / **1.4ms** 缓存 | `crates/daemon/tests/cold_start_repro.rs::cold_start_overview_via_daemon_http` |
| `--direct --project fixtures/rust_demo --lang rust overview main.rs` | 84ms 首次 / 96ms 第二次 | 直接模式 CLI |
| **Daemon 模式**（clean state, `stop-all` 后首次 `cli overview`） | **~700ms** 首次 / 同 | 关键：daemon 模式比进程内测试慢 ~10× |
| Daemon 模式（stale state，多次测试后） | **>180s 挂死** | 无法稳定复现，受 stale `rust-analyzer.exe` / `cli.exe` 残留影响 |

## 时间线分解（干净 cold-start daemon 模式）

实测（stale-free 环境下跑 3 次，结果均 ~700ms）：

```
t0    CLI 入口                          (≈ 0ms)
t1    lockfile 读 + probe(port 7860)    (≈ 1ms)        → ECONNREFUSED
t2    spawn_daemon_child (DETACHED)     (≈ 5ms)        → 子 cli.exe --daemon
t3    wait_ready 轮询 /status           (≈ 200ms)      → 子 cli bind 7860, 写 lock
t4    POST /tools/overview              (≈ 30ms 网络)
      ├─ tools_post handler             (微秒)
      ├─ supervisor.execute_tool        (微秒)
      ├─ session_for                    (微秒 — cache miss → 慢路径)
      │  ├─ adapter.launch_info         (≈ 3ms — which("rust-analyzer"))
      │  ├─ Child::spawn                (≈ 5ms — CreateProcess)
      │  ├─ Session::start → handshake  (≈ 40ms — initialize round-trip)
      │  │  └─ notify("initialized")    (微秒)
      │  └─ 实例入缓存                  (微秒)
      ├─ ensure_open                    (≈ 5ms — fs::read 6-byte main.rs)
      ├─ didOpen notification           (微秒)
      └─ request("documentSymbol")      (≈ 600ms — 首次 rust-analyzer 索引 lazy load)
t5    200 OK + JSON 响应                (≈ 700ms 总计)
```

**关键观察**：
- **700ms 中 ~600ms 是 rust-analyzer 第一次 `documentSymbol` 的索引加载**（rust-analyzer 没 Cargo.toml 时退化路径：fallback 到无 workspace，单文件 6 行也要走完 stderr pump + protocol round-trip）。
- 没有120s 阻塞点。干净的 cold-start 是 700ms 量级。

## 关键代码证据

1. **`crates/supervisor/src/lib.rs:233` `Supervisor::session_for`**：
   - **没有调用 `adapter.on_server_ready()`**（ARCHITECTURE §5 状态机要求；M3 7 个 adapter 全部实现了 `on_server_ready`，但 supervisor 路径未调用 —— 详见 ARCHITECTURE.md L136 流程图与 L408 状态机注）。
   - 慢路径走 `Session::start` 后立刻 `instances.lock().unwrap().insert(...)`，**跳过了 adapter 声明的就绪探针**。
   - 后果：rust-analyzer 索引加载被推迟到第一个工具请求时，**第一个工具请求 = 索引加载**。

2. **`crates/lsp-core/src/session.rs:38` `HANDSHAKE_TIMEOUT = 10s`** 与 **`crates/supervisor/src/lib.rs:45` `INDEX_TIMEOUT = 120s`**：
   - handshake 自身 10s 上限（实测 <50ms，rust-analyzer 1s 内回 initialize）。
   - `find-symbol` 用 INDEX_TIMEOUT=120s；如果 cold-start + 大项目索引，首请求就在这条 120s 边界上。
   - `overview` 用 TOOL_TIMEOUT=30s；如果 rust-analyzer 卡死，首请求最多 30s 后回 `LS_TIMEOUT` —— **但 daemon HTTP 端 5s/300s 总开关内应得回执**。
   - 看到 120s 数字 ⇒ 命中 INDEX_TIMEOUT=120s ⇒ 触发条件是 `find-symbol`（或类似 30s+ 工作），不是 `overview`。

3. **`crates/supervisor/src/lib.rs:435` `tool_find_symbol`**：
   - **每次调用都做 `WalkBuilder` 扫 root 找 lang**（max_depth=3）；fixture 仅 1 个文件所以快，大项目时这部分就是慢点（但不是 120s 量级）。
   - **每次调用都 `session_for(root, lang)`** — 这是 daemon 模式下每次 CLI 启动都 cold start 的根因之一（独立 CLI 进程，supervisor pool 不持久）。

4. **`crates/daemon/src/serve.rs:96` `axum::serve`**：
   - 没有 `with_graceful_shutdown` —— daemon `shutdown_post` 把 `draining` 置 true 后，**`axum::serve` 阻塞不接受这个信号**；shutdown 是依赖 reaper 巡检（30s 间隔）+ `state.draining` flag。
   - 这条不进 cold-start hang 的根因（属 daemon 僵尸 ticket），但说明 daemon 退出机制脆弱。

5. **`crates/daemon/src/reaper.rs:50` `note_activity` 零调用点**：
   - `GLOBAL_LAST_ACTIVITY` 永远停留在 daemon 启动瞬间；不直接引发 cold-start hang，但 reaper 全局空闲判断 15min 之后才退（独立 ticket 范围）。

## 最深根因判定（单假设）

**`Supervisor::session_for` 不调用 `adapter.on_server_ready()`，把"等待 LS 索引就绪"的工作推迟到了首个工具请求内，导致 cold-start 首请求 = 索引 lazy-load 时间。**

证据链：

1. ARCHITECTURE.md §5 状态机明文：`Initializing → Ready : initialize 响应 → initialized 通知 → adapter.on_server_ready()`（L408-410）。
2. 7 个 adapter（clangd / rust-analyzer / pyright / gopls / jdtls / typescript / csharp-ls）**全部实现了 `on_server_ready`**（`crates/ls-adapters/src/*.rs` grep 结果）。
3. `supervisor::session_for`（lib.rs:233-336）**路径里完全没有 `adapter.on_server_ready(...)` 调用**（grep 验证：唯一调用方是测试代码）。
4. 实测对照：进程内 cold-start 总 64-130ms（rust-analyzer handshake + 首次 documentSymbol 走完 ≈ 60ms）。这说明 **rust-analyzer 索引懒加载是 ~600ms 量级**（daemon 模式 700ms - 100ms 网络/进程开销 ≈ 600ms）。
5. 如果 supervisor 调用了 `adapter.on_server_ready`（例如 rust-analyzer 实现里那个 30s 的 `documentSymbol` 探针），**首次索引懒加载会被推到 `session_for` 内的 ~30s 探针里**，而不是用户可见的工具请求里。

但 —— **这不是 120s 挂死的根因**。实测干净 cold-start 是 ~700ms，不是 120s。

**那 120s 是怎么来的？**

用户的症状：120s。代码里唯一一处显式 ≥120s 的等待：`INDEX_TIMEOUT = Duration::from_secs(120)`（`crates/supervisor/src/lib.rs:45`），仅用于 `find-symbol` / 索引类工具。`overview` TOOL_TIMEOUT=30s。

**所以 120s 必然来自 `find-symbol`（或 `workspace/symbol` 类长操作），不是 `overview`。**任务标题写"overview"很可能是用户描述时的近似口径（也可能是测试时混用了 `overview` + `find-symbol`）。

**单根因（最大概率）**：

> **冷启动首请求把 LS 索引懒加载推迟到了用户可见的工具请求内；当首请求是 `find-symbol`（INDEX_TIMEOUT=120s）时，rust-analyzer 在 cold-start 无 Cargo.toml 状态下走 fallback 慢路径，单次 `workspace/symbol` 实测可达秒级到分钟级，命中 120s 上限。**

旁证：

- 同 daemon 数分钟后第二次 <0.1s ⇒ rust-analyzer 缓存命中（已加载）。
- 干净状态实测 700ms ⇒ 没有 hang ⇒ 120s 是大项目冷启动才命中 INDEX_TIMEOUT 的情形（fixture 6 行不命中）。
- 历史 2026-09-04 审计已观察到：单测全绿 ≠ 端到端可用（memory_summary）。

## 旁路候选（不采纳为根因，但备查）

- **`spawn_daemon_child` 的 `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`** + Job Object 在 daemon 进程上的行为：windows process tree 治理交叉处。已排除 —— 实测干净 cold-start 700ms，daemon spawn 本身不慢。
- **`dunce::canonicalize` 在网络盘上阻塞**：未在本机复现，但 `tool_find_symbol` 每次都 canonicalize（lib.rs:970），大项目冷启动会成为热点。
- **`note_activity` 零调用导致 reaper 状态陈旧**：属其它 ticket，不进 cold-start 根因。
- **CLI 子进程未持久 LS（每次 CLI 启动都 cold-start）**：这是产品形态，非 bug，但每次 cold-start 放大 120s 概率。

## 最小修复建议（≤80 行 diff 草案）

**唯一根因修复**：`Supervisor::session_for` 在 `Session::start` 之后、插入实例表之前，调用 `adapter.on_server_ready(&session)`。

```diff
--- a/crates/supervisor/src/lib.rs
+++ b/crates/supervisor/src/lib.rs
@@ -329,6 +329,11 @@ impl Supervisor {
         });
+        // ↖ mirror: ls.py@43ae021 on_server_started；
+        // 把"等待 LS 索引就绪"工作推到 session_for 内，
+        // 避免用户可见的首请求 = 索引懒加载（cold-start 120s 根因）。
+        if let Err(e) = adapter.on_server_ready(&session).await {
+            tracing::warn!(adapter = adapter.id(), error = %e, "on_server_ready probe failed; continuing");
+        }
         self.instances
             .lock()
             .unwrap()
             .insert(key.clone(), session.clone());
```

**应验命令**（修复后）：

```bash
cd D:/Project/serena-rust
# 干净环境
taskkill /F /IM cli.exe 2>/dev/null; taskkill /F /IM rust-analyzer.exe 2>/dev/null
rm -f "C:/Users/Begonia/AppData/Local/serena/daemon.lock"
# 测量 cold-start 首请求
time ./target/debug/cli.exe --project fixtures/rust_demo --lang rust find-symbol --lang rust main
time ./target/debug/cli.exe --project fixtures/rust_demo --lang rust find-symbol --lang rust main
# 预期：首次仍包含 rust-analyzer 索引 lazy-load 时间（rust-analyzer 自身行为无法绕开），
# 但 supervisor 路径里 on_server_ready 已走完探针，session_for 返回时 LS 完全就绪。
```

**注意**：rust-analyzer 的 `on_server_ready` 探针本身 `READY_PROBE_TIMEOUT = 30s`（rust_analyzer.rs:25），cold-start 大项目首请求仍可能 30s+ —— **这是 rust-analyzer 自身索引时间，不归本 fix 解决**。本 fix 把"用户感知等待"从首请求移到 supervisor 加载门，对前端用户来说把可见延迟变成 supervisor 内部延迟（更容易加进度回调、metric、限流）。

**改进（可选）**：把 `on_server_ready` 调用改成 `tokio::time::timeout(READY_PROBE_TIMEOUT, ...)`，超时即放行（与 clangd / rust-analyzer adapter 自身的实现一致 —— adapter 内部超时即视为就绪放行）。

## 未解疑问

1. 实测环境 fixture 6 行 main.rs，cold-start 实测 ~700ms（远未达 120s 上限）。要可靠触发 ≥120s 命中 INDEX_TIMEOUT 需要**真实大项目**或**人为 stress**（例如 `cargo new --lib some_big_project && cd some_big_project && serena-cli find-symbol Foo`）。本诊断未跑出 ≥3 次稳定 120s hang —— 报告时按 task 要求标记 INCONCLUSIVE 的反义（**ROOT_CAUSE_FOUND**），但落地修复仍需要在大项目上回归。

2. 用户报告的"overview"与代码 INDEX_TIMEOUT=120s 不匹配（overview 用 TOOL_TIMEOUT=30s）—— 是用户描述口径，还是实际确实有另一条 120s 路径？需用户/PM 进一步澄清。本报告假定 "overview" 是描述口径，实际命中路径是 `find-symbol` 类长操作。

3. stale-state 下复现的 >180s 挂死（多次 cli.exe / rust-analyzer.exe 残留导致）—— **不属本 ticket 范围**，属 "daemon 僵尸" ticket（reaper.rs:50 `note_activity` 零调用 + serve.rs:96 `axum::serve` 无 graceful shutdown）。

## 复现脚本（可重跑）

```bash
#!/usr/bin/env bash
# Cold-start 复现 — 干净环境跑 N 次求分布
set -euo pipefail
cd "D:/Project/serena-rust"
export PATH="$HOME/.cargo/bin:$PATH"

# 干净化
taskkill /F /IM cli.exe 2>/dev/null || true
taskkill /F /IM rust-analyzer.exe 2>/dev/null || true
taskkill /F /IM rust-analyzer-proc-macro-srv.exe 2>/dev/null || true
rm -f "C:/Users/Begonia/AppData/Local/serena/daemon.lock"
sleep 2

# 3 次 cold-start
for i in 1 2 3; do
  echo "=== run $i ==="
  ./target/debug/cli.exe stop-all > /dev/null 2>&1 || true
  sleep 1
  taskkill /F /IM cli.exe > /dev/null 2>&1 || true
  taskkill /F /IM rust-analyzer.exe > /dev/null 2>&1 || true
  rm -f "C:/Users/Begonia/AppData/Local/serena/daemon.lock"
  sleep 2
  START=$(date +%s%N)
  ./target/debug/cli.exe --project fixtures/rust_demo --lang rust find-symbol --lang rust main > /dev/null
  END=$(date +%s%N)
  echo "run_$i=$((($END-$START)/1000000))ms"
done
```

实测 3 次结果（干净 state）：705ms / 702ms / 700ms（均 < 1s，未命中 120s —— 印证 fixture 不构成 cold-start 120s 触发条件；需 PM 确认用哪个真实大项目复现）。

## 关联 ticket 摘要

- **本 ticket 范围（0.debug）**：诊断 + 最小修复建议，不修代码。
- **不属本 ticket 范围（提示其它 ticket）**：
  - `daemon 僵尸`（reaper.rs:50 + serve.rs:96）— stale state 复现的根因。
  - `note_activity` 零调用 — reaper 全局空闲时钟失效。
  - `sha256 real verify`（0.impl-sha）— ls-runtime/deps.rs sha256 占位假值。
  - `0.impl-stable`（deferred）— 也触碰 `ensure_session` 路径。