# M1 Task 14/15/16 任务契约（PM 给后续执行者用）

> Task 13 已派 fullstack-engineer `Task13-instances`。本文件列剩余 Task 给后续派单/PM 自执行时参考。

---

## Task 14: IdleReaper + ShutdownDraining + LRU（PLAN §298）

**Files:** `crates/daemon/src/reaper.rs` + tests

**Produces:**
- `IdleReaper` task：30s 巡检
  - 单 LS 10min 未用 → 卸载（前断言 in_flight==0）
  - 超 `max_loaded_ls=3` → LRU 驱逐
- 全局 15min 空闲 → ShutdownDraining（503 + Retry-After → 等 in-flight ≤10s → 逐 LS shutdown 5s 超时转 kill → 删 lock → exit 0）

**接口假设**（依赖 Task 13）：
- `Supervisor::loaded_keys() -> Vec<Key>`（Task 13 暴露）
- `Supervisor::session_for(root, lang).last_used: Instant`（Task 13 Entry 加字段）
- `Supervisor::evict(key)` 主动卸载

**测试：** 把间隔参数调到 100ms 级做时间加速验证。

**注意：** Reaper 是常驻 tokio task——daemon::serve() 起时 spawn，停机时 cancel。

**Commit:** `feat(daemon): idle reaper + LRU + shutdown draining (Task 14)`

---

## Task 15: write_gate + symbol-body + replace-body（C3 链路，PLAN §300）

**Files:** `crates/supervisor/src/write_gate.rs`、`src/tools/symbol_body.rs`、`src/tools/replace_body.rs`

**Produces:**
- 全局 `tokio::sync::Mutex<()>` 写门（FIFO = 队列，A4）
- `symbol-body`: ensure_open → documentSymbol 定位符号 range → offsets.rs 按协商编码换算字节偏移 → 文件切片返回
- `replace-body`:
  - acquire 写门
  - **锁内** documentSymbol 解析符号 range（客户端只传符号名）
  - 读盘 content-hash 对账（不符 → WRITE_CONFLICT）
  - tempfile 原子写 + rename（共享冲突重试 5×50ms，I5）
  - **读回 diff 校验，不符从写前临时副本回滚并报 WRITE_CONFLICT（C3 ③）**
  - didChange 全量同步
  - 等诊断代际推进（2s 超时只告警）
  - 失效符号缓存
  - release

**ToolError 扩展（先做）:** 添加 `WriteConflict { path: String, reason: String }` 变体。同步更新 dto.rs wire_error_from_tool_error 加 `WriteConflict` arm。

**测试 ×4:**
1. 正常替换
2. hash 不符拒写
3. 并发两写串行化（顺序断言）
4. 写后读回一致

**Commit:** `feat(supervisor): write_gate + symbol-body + replace-body C3 链路 (Task 15)`

---

## Task 16: CLI 转发 + lazy-spawn + 管理命令（PLAN §306）

**Files:** `crates/cli/src/main.rs`

**Produces:**
- 默认路径 = 读 lock → TCP 探活（500ms）→ 活着转发（reqwest blocking + X-Serena-Token）/ 死了 spawn 自身 `--daemon`（CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP + 关句柄继承 + stdin/stdout→NULL，I5）→ 轮询 `/status` ≤3s（100ms 间隔）→ 转发
- 子命令：`status / list / stop-all / logs`
  - `logs`: tail `%LOCALAPPDATA%/serena/daemon.log`
- 工具命令全集：`overview/find-symbol/symbol-body/refs/def/impls/hover/replace-body/diagnostics`
- `--record` 录制 JSON-RPC（M3 留 TODO）
- `--json` 输出（M3 留 TODO）

**注意:**
- 复用 `crates/cli/src/main.rs` 现有 `--direct` 路径
- 加 `--daemon` subcommand 走 daemon::serve()
- 工具命令走 HTTP 转发到 daemon

**测试:** 杀 daemon 后首次调用自动复活并返回正确结果

**Commit:** `feat(cli): forwarding + lazy-spawn + 管理命令 (Task 16)`

---

## Task 17: M1 验收（PLAN §312）

**Files:** 集成测试 + release exe CI

**Acceptance:**
- [ ] 集成测试：3 并发读 + 1 写同文件（断言写串行、读不阻塞、最终一致性）
- [ ] `cargo build --release`：单 exe 体积记录
- [ ] Windows 任务管理器人工验证：`taskkill /F /IM cli.exe`（daemon）后 `tasklist | findstr clangd` → 无残留（C2 Job Object 验收）
- [ ] GitHub Actions：windows-latest，fmt + clippy + test + release build + artifact upload
- [ ] `git commit -am "feat(m1): daemon+cli product shell"` — M1 DONE 门槛

**Commit:** `chore(m1): release exe + GH Actions artifact (Task 17)`

---

## 风险点

1. **Task 15 write_gate 复杂**——content-hash 对账 + 临时副本回滚 + didChange 全量同步是 M1 最难点；预估 500+ 行
2. **Task 16 CLI 转发 + lazy-spawn**——`CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP + 关句柄继承 + stdin/stdout→NULL` 是 Windows 进程管理高危区，需详测
3. **Task 17 release exe 体积**——单 exe 应 < 30 MB；超过则查 dep 树 bloat（regex/flate2/zip 大概率占大头）
