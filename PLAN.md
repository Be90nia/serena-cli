# serena-rust 开发计划书（PLAN v1.0）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 Rust 复刻 solidlsp，产出单文件 `serena-cli.exe`：agent 经 bash 调用的 position-free 符号工具 CLI + 常驻 daemon（lazy-spawn / 空闲自杀）。

**Architecture:** 7-crate workspace（`ls-runtime` → `lsp-core` → `ls-adapters`/`ls-registry` → `supervisor` → `daemon`/`cli`），唯一事实源为 `ARCHITECTURE.md` v0.1（锚 oraios/serena@`43ae0211`）。本计划的任务签名、类型名、错误码全部以它为准，冲突时以它为准并回改本计划。

**Tech Stack:** tokio / axum / clap(derive) / serde+serde_json / lsp-types 3.17 / async-trait / thiserror+anyhow / tracing / reqwest(blocking,rustls) / win32job / dunce / tempfile / ignore / regex / sha2 / zip+flate2 / toml —— 完整清单及**禁入清单**（dashmap、parking_lot、async-lsp、tower-lsp）见 ARCHITECTURE.md §8，不得增删。

## Global Constraints

- 平台：Windows 优先开发与验收（用户环境 win32 x64）；Unix 等价路径（PDEATHSIG）标注但不阻塞。
- 上游追溯：凡抄译的 quirk/结构必须注释 `↖ mirror: <上游文件>@43ae021 <符号>`；改进处标 `Δ`。
- 错误处理：`lsp-core`/`ls-runtime`/`supervisor` 用 `thiserror` 具名错误（lsp-core 内禁 anyhow）；`ls-adapters`/`ls-registry` 用 `anyhow`（被编排末端）；`daemon`/`cli` 顶层 anyhow 兜底。权威表 = ARCHITECTURE.md §6.1。
- wire 契约：工具级失败 = HTTP 200 + `{ok:false, error{code,message,ls,retryable}}`；9 个错误码、CLI exit 0/1/2/3 见 ARCHITECTURE.md §6.3，不得自行新增。
- 锁纪律：全项目锁清单以 ARCHITECTURE.md §3.4 为唯一权威表；禁止在该表之外引入任何新的锁且不登记。
- 分层铁律：`lsp-core` 不 import `ls-adapters`/`ls-registry`；`supervisor` 不 import axum；`daemon` 不含 LSP 语义。
- 测试纪律：每任务 TDD（先写失败测试）；真 clangd 集成测试在 PATH 无 clangd 时输出 skip 而非 fail；每任务至少一提交。
- 提交规范：`feat:|fix:|test:|chore: <描述>`；每任务收尾必提交。
- 细化策略：本计划 M0/M1 为 bite-sized 可直接执行；M2-M4 为任务级分解，M0 交付后按同模板细化成独立计划文件（`PLAN-M2.md` 等）再执行。

---

## 里程碑总览

| Phase | 交付物 | 任务 | 预估 |
|---|---|---|---|
| M0 | `--direct` 模式：单进程直连 clangd，overview/def/refs 三工具可用 | Task 1-10 | 3-4 天 |
| M1 | 产品壳：daemon+CLI 双模式 exe，全部只读工具 + replace-body，管理命令，CI | Task 11-17 | 1 周 |
| M2 | 下载器 + servers.toml 收录 T0/T1 ~55 个 | Task 18-21 | +1-2 周 |
| M3 | T2 大户逐个（clangd 完整 quirk 已在 M0 起步） | Task 22-24 | 每个1-4天 |
| M4 | symbols 磁盘缓存、--record、--json | Task 25-27 | +1 周 |

---

# Phase M0：lsp-core + clangd 直连打通

### Task 1: workspace 脚手架

**Files:**
- Create: `Cargo.toml`（workspace）、`rust-toolchain.toml`、`crates/{ls-runtime,lsp-core,ls-adapters,ls-registry,supervisor,daemon,cli}/Cargo.toml`、`crates/*/src/{lib.rs,main.rs}`、`.gitignore`

**Interfaces:**
- Produces: 可编译的空 workspace；后续所有任务的路径基准。

- [ ] **Step 1**: `git init` + 写根 `Cargo.toml`：

```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
edition = "2024"
version = "0.1.0"

[workspace.dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "process", "io-util", "sync", "time", "fs", "macros"] }
axum = "0.8"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
lsp-types = "0.97"
async-trait = "0.1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls", "json"] }
toml = "0.8"
sha2 = "0.10"
zip = "2"
flate2 = "1"
tempfile = "3"
ignore = "0.4"
regex = "1"
win32job = "2"
dunce = "1"
windows-sys = { version = "0.59", features = ["Win32_System_Console"] }
bytes = "1"
```

（子 crate `Cargo.toml` 逐个声明依赖，从 workspace 继承：`tokio.workspace = true` 形式。`cli` 为唯一 bin，其余全 lib。）

- [ ] **Step 2**: `rust-toolchain.toml` 写 `channel = "stable"`；`.gitignore` 加 `/target`；根 `Cargo.toml` 加 `[workspace.lints.clippy] all = { level = "deny" }`，仓库根放默认 `rustfmt.toml`（CI 跑 `cargo fmt --check`）。
- [ ] **Step 3**: `cargo build --workspace` → Expected: 编译通过（仅 unused 警告）。
- [ ] **Step 4**: `git add -A && git commit -m "chore: workspace scaffold (7 crates)"`

### Task 2: lsp-core 类型底座（types.rs）

**Files:**
- Create: `crates/lsp-core/src/types.rs`
- Modify: `crates/lsp-core/src/lib.rs`
- Test: `crates/lsp-core/tests/types.rs`

**Interfaces:**
- Consumes: `lsp_types` crate。
- Produces: `pub struct SymbolHit { name: String, kind: SymbolKindTag, uri: String, range: Range, container: Option<String> }`；`pub enum SymbolKindTag { File, Module, Class, Method, Function, Field, Variable, Other(u8) }`（↖ mirror: ls_types.py@43ae021 `UnifiedSymbolInformation` 字段子集）。后续所有工具输出结构以 `SymbolHit` 为准。

- [ ] **Step 1**: 写失败测试 `tests/types.rs`：

```rust
use lsp_core::types::*;
#[test]
fn symbol_kind_maps_number() {
    assert!(matches!(SymbolKindTag::from_lsp(6), SymbolKindTag::Method));
    assert!(matches!(SymbolKindTag::from_lsp(99), SymbolKindTag::Other(99)));
}
#[test]
fn symbol_hit_serializes_flat() {
    let hit = SymbolHit { name: "main".into(), kind: SymbolKindTag::Function,
        uri: "file:///p/main.cpp".into(), range: Default::default(), container: None };
    let j = serde_json::to_string(&hit).unwrap();
    assert!(j.contains("\"name\":\"main\""));
}
```

- [ ] **Step 2**: `cargo test -p lsp-core` → Expected: FAIL（模块不存在）。
- [ ] **Step 3**: 实现 `types.rs`：`from_lsp(u8)` 按 lsp-types `SymbolKind` 映射（6=Method、12=Function、5=Class，其余 Other）；derive `Serialize`（camelCase）。
- [ ] **Step 4**: `cargo test -p lsp-core` → PASS。
- [ ] **Step 5**: `git commit -am "feat(lsp-core): SymbolHit/SymbolKindTag types (mirror ls_types.py subset)"`

### Task 3: framing.rs（JSON-RPC 帧编解码 + 上游两个 quirk）

**Files:**
- Create: `crates/lsp-core/src/framing.rs`
- Test: `crates/lsp-core/src/framing.rs`（内联 `#[cfg(test)]`）

**Interfaces:**
- Produces: `pub fn encode(msg: &JsonRpc) -> Vec<u8>`；`pub fn decode(buf: &mut BytesMut) -> Result<Option<JsonRpc>, FrameError>`（不够一帧返回 `Ok(None)`）。

- [ ] **Step 1**: 写失败测试（断言含上游 quirk）：

```rust
#[test]
fn encodes_content_length_frame() {
    let out = encode(&JsonRpc::request(1, "textDocument/definition", json!({"uri":"u"})));
    let s = String::from_utf8(out).unwrap();
    assert!(s.starts_with("Content-Length: "));
    assert!(s.contains("\r\n\r\n{"));
}
#[test]
fn no_params_methods_omit_params() { // ↖ mirror: _NO_PARAMS_METHODS（shutdown/exit）
    let out = encode(&JsonRpc::request(2, "shutdown", json!(null)));
    assert!(!String::from_utf8(out).unwrap().contains("params"));
}
#[test]
fn split_across_reads() { // 半帧不解码，拼齐才出
    let full = encode(&JsonRpc::request(1, "ping", json!({})));
    let (a, b) = full.split_at(full.len() / 2);
    let mut buf = BytesMut::from(a); assert!(decode(&mut buf).unwrap().is_none());
    buf.extend_from_slice(b); assert!(decode(&mut buf).unwrap().is_some());
}
```

- [ ] **Step 2**: 跑测确认 FAIL。
- [ ] **Step 3**: 实现 `encode/decode`（头解析大小写不敏感、`\r\n\r\n` 分隔、`Content-Length` 精确 read_exact 语义）。↖ mirror: lsp_protocol_handler/server.py@43ae021 `create_message/content_length`。
- [ ] **Step 4**: `cargo test -p lsp-core framing` → PASS；`git commit -am "feat(lsp-core): JSON-RPC framing with no-params quirk"`。

### Task 4: 子进程 spawn（Job Object）+ stdio 泵拓扑

**Files:**
- Create: `crates/ls-runtime/src/process.rs`、`crates/ls-runtime/src/lib.rs`
- Create: `crates/lsp-core/src/transport/mod.rs`、`transport/stdio.rs`
- Test: `crates/ls-runtime/tests/spawn.rs`、`crates/lsp-core/tests/transport.rs`
- Create: `crates/lsp-core/tests/bin/mock_ls.rs`（测试用假语言服务器）

**Interfaces:**
- Consumes: Task 3 `encode/decode`。
- Produces: `ls_runtime::Child::spawn(cmd: LaunchInfo) -> Result<ChildHandle>`（字段 `stdin: ChildStdin, stdout: ChildStdout, stderr: ChildStdout, job: Option<Job>`，Windows 挂 Job Object）；`lsp_core::transport::stdio::pump(child, outbound_rx, on_msg: Arc<dyn Fn(JsonRpc) + Send + Sync>) -> Pumps`（起 3 个 tokio task：writer 独占 stdin / stdout 泵帧循环 / stderr 泵分级日志）。↖ mirror: ls_process.py@43ae021 `ManagedSubprocess`；Δ Job Object（ARCHITECTURE §3.2）。

- [ ] **Step 1**: 写 `mock_ls.rs`：读 stdin 帧，收到 `initialize` 回 capabilities JSON，收到 `textDocument/documentSymbol` 回固定数组——回放脚本式假服务器（后续多任务复用）。
- [ ] **Step 2**: 写失败测试 `transport.rs`：spawn mock_ls → pump → `request("initialize")` 收到响应（oneshot 收到值）。
- [ ] **Step 3**: 跑测 FAIL → 实现 `process.rs`（`std::process::Command` + `creation_flags(CREATE_NO_WINDOW = 0x0800_0000)` on Windows——勿用 0x08，那是 DETACHED_PROCESS，语义不同 + win32job；`tokio::process::Child` 转换）与 `stdio.rs` 泵（stdout 泵内联分发：响应→pending 通知路径先留 `todo!()` 由 Task 5 接管——本任务只断言"响应帧到达回调"）。
- [ ] **Step 4**: 测试 PASS + Windows 手验：spawn 后 drop `ChildHandle` 前 kill daemon 进程组——`tasklist | findstr clangd`（占位：用 mock 进程名）无残留。
- [ ] **Step 5**: `git commit -am "feat(runtime,transport): managed spawn + 3-pump topology (mirror ls_process.py)"`

### Task 5: client.rs（pending 表 / 超时 / 字符串 id 回退 / ContentModified 重试）

**Files:**
- Create: `crates/lsp-core/src/client.rs`
- Test: `crates/lsp-core/tests/client.rs`

**Interfaces:**
- Consumes: Task 4 泵回调（响应帧进入本模块）。
- Produces: `struct Client { request<R>(&self, method, params, timeout: Duration) -> Result<R, CoreError> }`（lsp-core 禁 anyhow，见 Global Constraints）；`notify(&self, method, params)`；`AtomicI64` id 分配；pending 表 `Mutex<HashMap<Id, oneshot::Sender>>`（↖ mirror: LanguageServerInterface._pending_requests）。Id 归一化：响应 id 先按 i64 查、再按字符串查（↖ mirror: `response_id.isdigit()` 回退）。`-32801 ContentModified` 自动重试白名单机制（3 次/200ms，↖ mirror: ls.py@43ae021）。

- [ ] **Step 1**: 失败测试 ×3：① 响应正常关联（mock 回 `{"id":1,...}`）；② **字符串 id 回退**（mock 回 `"id":"1"` 仍能完成请求）；③ 超时返回 `LS_TIMEOUT` 语义错误。
- [ ] **Step 2**: FAIL → 实现 → PASS ×3。
- [ ] **Step 3**: `git commit -am "feat(lsp-core): request/response client with id-normalization + ContentModified retry"`

### Task 6: session.rs（握手 + 状态机 + 就绪门）

**Files:**
- Create: `crates/lsp-core/src/session.rs`、`src/init_params.rs`
- Modify: `src/lib.rs` 导出
- Test: `crates/lsp-core/tests/session.rs`

**Interfaces:**
- Produces: `Session::start(child: ChildHandle, params: InitializeParams) -> Result<Arc<Session>, CoreError>`（审计修订：启动可能失败——spawn/io/init 超时——必须传错；状态 `Uninitialized→Initializing→Ready`，ARCHITECTURE §5；`initialized_notify: Notify` 就绪门——`request()` 在 Ready 前到达则等门）；`Session::request/notify` 转发 client；`shutdown(self)` 走 `timeout(2s, shutdown)` → `exit` 通知 → 关 stdin → `timeout(5s, wait)` → kill（↖ mirror: `_send_shutdown_in_thread`）。失败态 `Failed` 懒重试环由 supervisor Task 13 消费。

- [ ] **Step 1**: 失败测试：对 mock_ls `Session::start` → `Ready`；`request("textDocument/documentSymbol")` 拿到数组；`shutdown` 后 mock 进程退出（`child.wait` 已完成）。
- [ ] **Step 2**: FAIL → 实现（init_params.rs 构造 base InitializeParams：capabilities 声明 hierarchicalDocumentSymbolSupport、staleRequestSupport 与 client.rs 重试机制一致——ARCHITECTURE §3.2 注）。
- [ ] **Step 3**: PASS；`git commit -am "feat(lsp-core): session handshake + state machine + ready gate"`

### Task 7: docsync.rs（FileBuffer / mtime 对账 / ref-count）

**Files:**
- Create: `crates/lsp-core/src/docsync.rs`
- Test: `crates/lsp-core/tests/docsync.rs`

**Interfaces:**
- Consumes: `Session::notify`。
- Produces: `Session::ensure_open(&self, path: &Path) -> Result<FileGuard>`（首次：读盘+mtime 记账+全量 didOpen；已开：mtime 对账，磁盘新→didChange 全量重同步；返回 guard，drop 时 ref_count-1，归零发 didClose）——↖ mirror: ls.py@43ae021 `LSPFileBuffer._open_in_ls / open_file_buffers`（审计 C3 的对账地基）。

- [ ] **Step 1**: 失败测试 ×3：① ensure_open 后 mock 收到 didOpen（full text）；② **外部改文件后再次 ensure_open 触发 didChange**（写临时文件改 mtime）；③ 双 guard 嵌套只发一次 didOpen（ref_count）。
- [ ] **Step 2**: FAIL → 实现 → PASS。
- [ ] **Step 3**: `git commit -am "feat(lsp-core): docsync FileBuffer with mtime reconcile (mirror LSPFileBuffer)"`

### Task 8: clangd adapter（首个 T2，基础 quirk 随 M0——I7 修正）

**Files:**
- Create: `crates/ls-adapters/src/lib.rs`（trait 定稿，逐字照抄 ARCHITECTURE.md §4.1）、`src/clangd.rs`
- Create: `crates/lsp-core/src/offsets.rs`（LSP position ↔ 字节偏移换算）
- Test: `crates/ls-adapters/tests/clangd.rs`、`crates/lsp-core/tests/offsets.rs`


**Interfaces:**
- Consumes: `Session`、`InitializeParamsBuilder`。
- Produces: `ClangdAdapter` 实现 trait：`launch_info` = PATH `which("clangd")`，无则 `LS_NOT_INSTALLED` 错误（下载矩阵 M2 接管）；`initialize_patches` = OffsetEncoding 声明 + clangd 参数（`--background-index`）；`on_server_ready` = 等 `textDocument/documentSymbol` 首次成功或 30s 超时（基础就绪）；`logmap` = clangd stderr 前缀 `I[..]/E[..]` 分级（↖ mirror: clangd_language_server.py `_determine_log_level`）。另产出 `offsets.rs`：按 Session 协商的 PositionEncodingKind（utf-8/16/32）做 LSP position ↔ 字节偏移双向换算——Task 10 的 def/refs 直传与 Task 15 的 range 切片**必须**经它，禁裸算。


- [ ] **Step 1**: 失败测试：PATH 有 clangd 时 `launch_info` 返回可执行路径且无 UNC 前缀（dunce）；无 clangd 时错误码语义正确（测试机无 clangd 则 `#[ignore]`）。
- [ ] **Step 2**: 失败测试（offsets）：对含中文注释+多字节标识符的样本，utf-8/utf-16 两种编码下 position→offset→position 往返恒等、切片逐字符一致；纯 ASCII 等价。
- [ ] **Step 3**: 实现适配器 + trait 定义 + `offsets.rs`。
- [ ] **Step 4**: `cargo test -p ls-adapters -p lsp-core`；PATH 有 clangd 则加跑 `#[ignore]` 集成测试。
- [ ] **Step 5**: `git commit -am "feat(adapters,lsp-core): trait per ARCHITECTURE §4.1 + clangd + offset conversion"`

### Task 9: ls-registry 最小版（扩展名 → adapter）

**Files:**
- Create: `crates/ls-registry/src/lib.rs`
- Test: `crates/ls-registry/tests/resolve.rs`

**Interfaces:**
- Produces: `pub fn resolve(path: &Path) -> Option<LanguageId>`（`.c/.cpp/.cc/.cxx/.h/.hpp → "cpp"`；本任务硬编码表，M2 换 servers.toml）；`pub fn adapter_for(lang: &str) -> Arc<dyn LanguageServerAdapter>`（本任务只认 "cpp"）。

- [ ] **Step 1**: 失败测试：`resolve("x.cpp")=="cpp"`、`resolve("x.py")==None`（M2 前不认识 python）。
- [ ] **Step 2**: 实现 → PASS → `git commit -am "feat(registry): extension->language minimal table"`

### Task 10: supervisor 只读三工具 + `--direct` CLI 端到端

**Files:**
- Create: `crates/supervisor/src/lib.rs`、`src/tools/overview.rs`、`src/tools/def_refs.rs`
- Create: `fixtures/cpp_demo/{main.cpp,math.h,compile_flags.txt}`（compile_flags.txt 让 clangd 免 compile_commands 稳定解析；生成/转换逻辑属 M3 完整化）
- Modify: `crates/cli/src/main.rs`（`--direct --project <root> overview|def|refs <arg>`）
- Test: `crates/supervisor/tests/e2e_direct.rs`

**Interfaces:**
- Consumes: Task 6-9 全部。
- Produces: `supervisor::Supervisor::tool_overview(root, file) -> Vec<SymbolHit>`、`tool_def(root, file, line, col) -> Option<Location>`、`tool_refs(root, file, line, col) -> Vec<Location>`（position 参数版，position-free 组合层 M1 Task 15 扩展）；CLI `--direct` 进程内直调（A7）。本任务完成即 **M0 验收**。

- [ ] **Step 1**: e2e 失败测试：对 `fixtures/cpp_demo`（PATH 需 clangd，否则 skip）：overview 返回含 `main` 的 SymbolHit；def 从 `main.cpp` 调用点跳到 `math.h` 声明；refs 找到 ≥1 引用。
- [ ] **Step 2**: 实现 tools（`session.request("textDocument/documentSymbol")` → 拍平递归 children → `Vec<SymbolHit>`；def/refs 位置参数经 `offsets.rs` 换算为协商编码的 LSP position，禁裸算）。
- [ ] **Step 3**: CLI 启动即 `SetConsoleOutputCP(65001)`（windows-sys；中文系统 conhost/PowerShell/cmd 默认 GBK 码页，不设必乱码）→ `cargo run -p cli -- --direct --project fixtures/cpp_demo overview main.cpp` → Expected: 打印符号列表（中文不乱码），exit 0。
- [ ] **Step 4**: 全 workspace 测试 + clippy：`cargo test --workspace && cargo clippy --workspace` → 全绿。
- [ ] **Step 5**: `git commit -am "feat(m0): direct-mode overview/def/refs e2e over real clangd"` —— **M0 DONE 门槛**。

---

# Phase M1：daemon + CLI 产品壳

### Task 11: lockfile.rs（单例仲裁，C1 修复落地）

**Files:** `crates/daemon/src/lockfile.rs` + tests

**Produces:** `try_become_daemon() -> Outcome { Won(port), Lost{port} }`——`%LOCALAPPDATA%/serena/daemon.lock` 以 `create_new(true)` 原子建，写 `{pid,port,boot_ms,token}`（token=随机 128-bit hex，ARCHITECTURE §2）；败者/后来者走 TCP 探活（connect 超时 500ms）+ boot_ms 比对判活（A6）。测试 ×4：新建成功、二次创建→Lost、残留死 lock（端口无应答）→ 清理重建、lock 内容含可读 token。

- [ ] checkbox 步骤同 TDD 模板（写测→FAIL→实现→PASS→`git commit -am "feat(daemon): singleton lock arbitration (C1)"`）。以下任务同模板，仅列关键步骤与验收。

### Task 12: daemon HTTP 前端（wire 契约）

**Files:** `crates/daemon/src/http.rs`、`src/dto.rs`

**Produces:** `POST /tools/{name}`（DTO 按 ARCHITECTURE §6.3：200+`{ok, data|error}`、9 错误码、503 ShutdownDraining）；**middleware：校验 `X-Serena-Token` 与 lock 一致，不符 403**；`GET /status`（uptime + 已加载 LS + 索引进度位）；`POST /shutdown`。测试：mock supervisor trait 下 200/ok:false/403/404/503 五分支。

### Task 13: supervisor 实例池（per-key 门 + dunce 规范化 + Failed 懒重启）

**Files:** `crates/supervisor/src/lib.rs`

**Produces:** `Mutex<HashMap<(RootKey, LanguageId), Entry>>`，`Entry{Arc<Session>, last_used, in_flight: AtomicU32, load_gate: Arc<tokio Mutex>}`；root 入键前 `dunce::canonicalize`+大小写折叠+去尾分隔（I6）；`LS_TERMINATED` 后下次请求自动重建（↖ mirror: ls.py L1002 restart-and-retry 语义前移）。测试：同 key 并发首查只 spawn 一次（load_gate 生效，计数 mock spawn 次数==1）。

### Task 14: IdleReaper + ShutdownDraining + LRU（内存闸门落地）

**Files:** `crates/daemon/src/reaper.rs`

**Produces:** 30s 巡检 task：单 LS 10min 未用→卸载（前置断言 `in_flight==0`，C4 引用计数门）；超 `max_loaded_ls=3`→LRU 驱逐；全局 15min 空闲→ShutdownDraining（503+Retry-After → 等 in-flight ≤10s → 逐 LS shutdown 5s 超时转 kill → 删 lock → exit 0，I8）。测试：把间隔参数调到 100ms 级做时间加速验证。

### Task 15: write_gate + symbol-body / replace-body（一致性链路，C3 落地）

**Files:** `crates/supervisor/src/write_gate.rs`、`src/tools/symbol_body.rs`

**Produces:** 全局 `tokio::sync::Mutex<()>` 写门（FIFO 即队列，A4）；`symbol-body`：ensure_open → documentSymbol 定位符号 range → **经 offsets.rs 按协商编码换算字节偏移** → 文件切片返回；`replace-body`：acquire 写门 → **锁内**经 documentSymbol 解析符号 range（客户端只传符号名，不传 range）→ 读盘 content-hash 对账（不符→`WRITE_CONFLICT`）→ 新 body 替换 → tempfile 原子写 + rename（共享冲突重试 5×50ms，I5）→ **读回 diff 校验，不符从写前临时副本回滚并报错（DESIGN C3 ③）** → didChange 全量 → 等诊断代际推进（2s 超时只告警）→ 失效符号缓存 → release。测试 ×4：正常替换、hash 不符拒写、并发两写串行化（顺序断言）、写后读回一致。

### Task 16: cli 转发 + lazy-spawn + 管理命令

**Files:** `crates/cli/src/main.rs`

**Produces:** 默认路径 = 读 lock→TCP 探活（500ms 超时）→活着转发（reqwest blocking，带 `X-Serena-Token`）/死了 spawn 自身 `--daemon`（CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP + 关句柄继承 + stdin/stdout→NULL，I5）→轮询 `/status` ≤3s（100ms 间隔，LS 冷启动等待由工具超时覆盖）→转发；子命令 `status/list/stop-all/logs`（logs tail `%LOCALAPPDATA%/serena/daemon.log`，DESIGN §3）；工具命令全集：`overview/find-symbol/symbol-body/refs/def/impls/hover/replace-body/diagnostics`。测试：杀 daemon 后首次调用自动复活并返回正确结果；`find-symbol` regex 模式对 fixtures 命中且截断标注生效（组合层最小实现：workspace/symbol 精确 + documentSymbol 遍历兜底，I3）。

### Task 17: M1 验收——并发压测 + release exe CI

- [ ] 集成测试：3 并发读 + 1 写同文件（断言写串行、读不阻塞、最终一致性）。
- [ ] `cargo build --release`：单 exe 体积记录；Windows 任务管理器人工验证：`taskkill /F /IM serena-cli.exe`（daemon）后 `tasklist | findstr clangd` → **无残留**（C2 Job Object 验收）。
- [ ] GitHub Actions：windows-latest，`cargo fmt --check` + `cargo clippy --workspace -- -D warnings` + `cargo test --workspace` + `cargo build --release` 产物上传。
- [ ] `git commit -am "feat(m1): daemon+cli product shell"` —— **M1 DONE 门槛**。

---

# Phase M2-M4：任务级分解（M0 交付后按同模板细化执行）

| # | 任务 | 验收要点 | 依赖 |
|---|---|---|---|
| 18 | ls-runtime 下载器（zed 式 (Os,Arch)→URL 矩阵 + sha256 + 解压 + PATH 回退 + 安装进度事件） | clangd 免 PATH 安装冒烟；校验和错误拒绝执行 | M1 |
| 19 | servers.toml schema 定稿（nvim-lspconfig 字段 + multilspy initialize_params 声明式 + required_root_patterns）+ ConfigAdapter（T0 零代码） | crystal/json/yaml 等 5 个 T0 冒烟通过 | 18 |
| 20 | T0/T1 批量收录 ~55 个 + multilspy 式每语言一测试文件 + **SKIP 机制**（缺运行时依赖=SKIP 且列清单，CI 只跑 T0 子集，全量冒烟走本机） | 覆盖率报告 ≥75%；逐个冒烟绿或 SKIP 清单 | 19 |
| 21 | 根发现算法（helix find_lsp_workspace 抄译：marker 向上/root_dirs 截停/workspace 兜底） | fixtures 多层目录正确判定 root | 19 |
| 22-24 | T2 大户逐个（jdtls→rust_analyzer→vue→al…按用户频率） | 每个：对照上游 pytest 全绿 + `--record` 对拍 | 26（对拍工具前置） |
| 25 | symbols 磁盘缓存（fingerprint 每次校验，I4 统一） | 重启后 overview 命中缓存（LS 冷启绕过） | M1 |
| 26 | `--record` JSON-RPC 录制/回放（**先于 22-24 执行**：M3 验收依赖它，clangd 会话 M0 即可录制） | 录制 clangd 会话并回放比对 | M1 |
| 27 | `--json` 输出（保证所有工具输出可被 agent 经 jq 解析） | 文本输出对人友好，JSON 给 agent；MCP 不在本项目范围内 |

---

## 验收标准（每阶段 Done 的量化定义）

原则：每条验收 = 可执行命令 + 期望输出；由 PM 或 e2e-runner 在干净状态（kill daemon、删 lock）下执行；skip 只算显式跳过并记录原因，不算通过。

### 全局验收（每阶段收尾追加）

- **架构守护（机器检查）**：`cargo tree -p lsp-core` 输出不含 `ls-adapters`/`ls-registry`；`cargo tree -p supervisor` 不含 `axum`——分层铁律不靠 review 自觉。
- **工程约束**：`cargo fmt --check` + clippy deny warnings 自 M0 起每任务本地执行，CI 侧 Task 17 落地。

### M0（Task 10 完成 = M0 候选）

| # | 验收项 | 命令/方法 | 期望 |
|---|---|---|---|
| A0-1 | 三工具功能 | overview/def/refs 对 `fixtures/cpp_demo` | overview 含 main/math 符号集；def 跳到 math.h 声明；refs ≥1 |
| A0-2 | 非 ASCII 正确性 | fixture 增补含中文注释+中文标识符的文件，重跑三工具 + symbol-body 切片；PowerShell 与 cmd 直跑 | 输出无乱码（65001 码页生效），切片与源文件逐字符一致（依赖偏移换算实现） |
| A0-3 | exit code 契约 | 工具成功；移出 clangd 后重调 | 成功=0；失败=1 且 message 含 install 提示（对 §6.3 表） |
| A0-4 | 测试纪律 | `cargo test --workspace && cargo clippy --workspace` | 全绿 0 warning |

### M1（Task 17 完成 = M1 候选）

| # | 验收项 | 命令/方法 | 期望 |
|---|---|---|---|
| A1-1 | 9 工具全集 | 每工具对 fixture ≥1 条真实调用 | 输出符合语义；replace-body 含 hash 拒写 + 并发串行用例 |
| A1-2 | 错误码面 | 逐个触发 9 个 error.code（BAD_ARGS / LS_NOT_INSTALLED / WRITE_CONFLICT / LS_TERMINATED …） | 每条的 exit code、retryable、message 特征与 ARCHITECTURE §6.3 一致 |
| A1-3 | 性能口径 | 计时：status 端到端；杀 daemon 后首次调用至 /status 200 | CLI 转发 ≤50ms；daemon 冷启动 ≤3s（§2 预算 ≤100ms 的 30 倍容差，预算本身不作为 Done 条件） |
| A1-4 | 生命周期 | 双 CLI 并发冷启动 ×20；idle 自杀（参数调 100ms 级）；taskkill daemon | 恒收敛为 1 daemon；自杀后 lock 删除；`tasklist` 无 clangd 孤儿 |
| A1-5 | 干净机分发 | 无 Rust 工具链的 Windows 上跑 release exe（M2 前 clangd 走 PATH） | status → overview 全通 |
| A1-6 | CI | GitHub Actions windows-latest 全绿 + exe 产物 | 产物可下载、可直接运行 |
| A1-7 | 压力测试 | 见下方「压力测试清单」，脚本驱动，全项零 INTERNAL/5xx、零死锁（300s 内必返回） | 压测后 daemon RSS ≤150MB（不含 LS）；`tasklist` 进程数与实例表一致，无 LS 泄漏 |

**压力测试清单（M1，Task 17 执行；fixture 需第二个项目目录供多键轮换）**：

1. **并发负载**：8 个并发 CLI 进程 × 混合读写（7 读 1 写）× 2 个项目键，持续 60s——读不被写阻塞，写严格串行，无 5xx。
2. **LRU churn**：`max_loaded_ls=3`，轮流查询 5 个 (root, lang) 键 ×10 轮——驱逐持续发生，LS 进程数峰值 ≤3，无僵尸实例。
3. **崩溃恢复循环**：连续 20 轮 taskkill daemon → 下一调用自动复活 → 工具成功——lock 每轮正确重建，无孤儿累积。
4. **大输入**：5MB 单文件 symbol-body；≥1000 引用的 refs 调用——不挂、可截断或完整返回（策略二选一，写进输出）。

### M2

- 覆盖率报告 ≥75%（Task 20）；冒烟必须有 SKIP 机制（依赖未装=SKIP 非 FAIL），CI 冒烟总时长记录在案。
- 逐 T0/T1 冒烟本机全绿，或给出显式跳过清单+原因。

### M4

- `--record` 录制→回放 diff 为空；`--json` 输出可被 `jq` 解析。

## 验证总表（每阶段收尾必跑）

```bash
cargo test --workspace          # 全绿
cargo clippy --workspace        # 0 error
cargo build --release           # 单 exe 产出
# M1 起追加：
# M1 起追加（产物名为 cli —— crates/cli Cargo.toml [[bin]] name="cli"；M1 若改 name="serena-cli" 同步此处）：
./target/release/cli.exe status                 # daemon 起 + JSON 状态
./target/release/cli.exe overview --project fixtures/cpp_demo fixtures/cpp_demo/main.cpp
taskkill /F /IM cli.exe & tasklist | findstr clangd   # 空输出=无孤儿(C2)
```

## 执行交接

计划已保存到 `D:/Project/serena-rust/PLAN.md`。两种执行选项：
1. **subagent-driven（推荐）**——每 Task 派新鲜 subagent + 两阶段审查，Task 间天然并行度低（强依赖链），但审查质量高；
2. **内联执行**——本会话按批次执行带检查点。
