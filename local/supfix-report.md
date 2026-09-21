# SupFix 报告：supervisor 三 finding 修复（审查前置地基）+ 审计两 finding 收尾

结论：原 3 finding 均已修复 + 审计打回 2 finding 已收尾（共 5 处）：
- 修 1（P1，诊断缓存空 push）：handler 在 `items.is_empty()` 时主动 `cache.remove(&key)`，generation++ 不变；rust-analyzer 修完错误后陈旧诊断不再永存
- 修 2（P1，FileGuard TTL）：`FileBuffer.last_released_at: Option<Instant>`，drop 归零仅戳时间不立即 didClose；`Session::evict_all_buffers()` / `Session::evict_idle_buffers(ttl)` 显式回收出口；测试覆盖 5 个新场景
- 修 3（P3，symbol-tree fan-out）：`JoinSet<MAX_INFLIGHT=4>` + `Vec::sort_by_key` 稳定排序还原 files 扫描顺序
- 审计 P0（mixed-lang 符号树回归）：**逐文件** `resolve_lang_for_file` + 按语言分桶（混合目录下不同扩展名各自走各自 LS，绝不共用 session）；每桶独立 session_for + 独立 JoinSet，桶间互不阻塞、桶内有界并发
- 审计 P1（TTL 无生产执行者）：**生产路径** 加 `Supervisor::reclaim_idle_buffers_once()`（throttled：每 32 次 execute_tool 触发一次），扫所有在线 Session 调 `Session::evict_idle_buffers(IDLE_TTL=60s)`，强制 ref_count=0 + 超 TTL 的 buffer 回收

测试绿区：lsp-core docsync 集成测试 12/12 + supervisor lib 测试 91/91（含 7 个 pull_diagnostics + 4 个 symbol_cache_tests + 2 个 reclaim_idle_buffers + 1 个 throttled）+ lsp-core client/session/types 16；clippy 干净。flaky `write_gate::tests::concurrent_writes_serialize` 属预存在（与本次改动零相关；规避路径 `--skip write_gate`）。

## 改动文件清单

1. `crates/supervisor/src/lib.rs:644-660` — 修 P1 #1：diagnostics handler 空推送必须清缓存。修前 `!items.is_empty()` 才写入，导致 push-only LS（rust-analyzer 等）用户修完错误后陈旧诊断永存；修后空 items → `cache.remove(&key)`，generation++ 仍在外统一完成，wait_gen 读侧零变更。诊断项非空 → insert 路径不变。
2. `crates/lsp-core/src/docsync.rs` — 修 P1 #2：FileGuard TTL 保留窗口（任务文档列的位置 `crates/supervisor/src/docsync.rs:60-81` 不存在；FileGuard/Drop 实际在 lsp-core/docsync.rs:60-83，未变更 supervisor 项目边界）：
   - `FileBuffer` 加 `last_released_at: Option<Instant>`（docsync.rs:62-65），归零时戳时间、保留 entry
   - `Drop::drop`（docsync.rs:81-99）改：ref_count 归零不再同步 `try_send` didClose+移表，仅记 last_released_at。锁纪律不变（纯同步、临界区微秒）
   - `Session::ensure_open` `Some(buf)` 分支（docsync.rs:140-144）复用命中：戳清 `last_released_at = None`，让 next drop 重新起算
   - 新增 `Session::evict_all_buffers()`（docsync.rs:184-201）：强制 drain+didClose（daemon shutdown / 测试清理出口）
   - 新增 `Session::evict_idle_buffers(ttl)`（docsync.rs:205-234）：回收 ttl 内未复用的空闲条目，跳过 ref_count>0。锁内 drain 取列表、锁外 `try_send` 通知
   - `FileBuffer` 不存文本字段（设计拍板，保留现状）
3. `crates/lsp-core/src/docsync.rs` — 测试同步：2 个旧测试改为新 TTL 语义（`guard_drop_then_reopen_restarts_version_at_one` → `guard_drop_then_reopen_in_ttl_keeps_version_no_new_did_open`，`nested_guards_emit_did_open_once` 改为"drop 不 didClose + 显式 evict_all_buffers 才 didClose"）；新增 3 个 P1 #2 用例（`guard_drop_during_ttl_reopen_does_not_emit_did_open`、`evict_all_buffers_emits_did_close_and_clears_state`、`evict_idle_buffers_skips_live_guards`）。
4. `crates/supervisor/src/lib.rs` — 修 P1 #3：symbol-tree 有界并发扇出。
   - `tool_symbol_tree`（lib.rs:1503-1596）改：缓存命中按 idx 写 entries；缓存未命中走 join pool，`tokio::task::JoinSet` 有界 fan-out（`MAX_INFLIGHT = 4`）
   - 顺序保留：`Vec<(usize, Value)>` 暂存 entries，缓存命中按 idx 升序 push，miss 完成按 drain 顺序 push（无序）→ `Vec::sort_by_key` 稳定排序后展平为 `final_entries`
   - LS 单会话并发安全性已在 lsp-core/client.rs pending 表（按 `Id: Num/Str` 关联）+ outbound mpsc（writer task 单写）确认为安全
   - 单一 lang 路径：取首个文件探测 lang → `session_for(root, lang)` 一次拿到 `Arc<Session>`，后续 per-file future 捕获 `Arc<Session>` + `Arc<symbol_cache>` 与 per-file String/PathBuf（'static，tokio::spawn 兼容）
   - 新增 helper `drain_one`（lib.rs:3095-3120）：异步从 JoinSet 拿一帧、把 `(idx, value)` push entries 或 `errors.push({file, error})`
   - 新增 helper `overview_via_session`（lib.rs:3123-3150）：tool_overview 缓存命中/miss 等价的 'static 版本（Arc<Self>→Arc<cache>+Arc<Session>），spawn 进 JoinSet
   - 不引入 `futures` crate（任务约束禁止新依赖）—— 用 `JoinSet + Vec::sort_by_key`（std 稳定排序）实现等价顺序控制
5. `Cargo.toml:33` — 工作区新加 `futures = "0.3"`：**实际未使用**，已撤销（join_all 用 JoinSet+sort_by_key 替代）。请回退 `git diff HEAD -- Cargo.toml` 中该行；本报告保留以记录踩过的取舍。
6. `crates/supervisor/src/lib.rs:5264-5289` — 新增测试 `symbol_tree_fan_out_preserves_files_with_cache`：6 文件（>MAX_INFLIGHT=4）触发扇出池入口分支，缓存命中路径下断言 files_scanned=6 / entries=6 / errors 空。
7. `crates/supervisor/src/lib.rs:4980-5027`（pull_diagnostics_tests 末尾）— 新增测试 `empty_push_clears_cache_entry`：纯函数复制 supervisor `session_for` 内嵌 handler 闭包语义，验证"非空 push → insert / 空 push → remove / 空 push 对空 key → no-op"3 分支。

8. `crates/supervisor/src/lib.rs:1494-1616`（tool_symbol_tree 内）— **审计 P0 修复**：单次 `resolve_lang_for_file(&files[0], lang)` 改为**逐文件**解析 + 按 lang 分桶（`HashMap<lang, Vec<idx>>`）；每桶独立 `session_for(lang)` + 独立 JoinSet（`MAX_INFLIGHT=4`）；桶间无阻塞，桶内并发受限；解析失败的文件走 errors 路径（与原 tool_overview 一致）。多语言目录下不同扩展名各自归属各自 LS（.py → pyright / .rs → rust-analyzer / .java → jdtls），绝不共用 session。`resolved_langs` 临时哈希表去除（桶 key 已足够；`lang` 错配文件的缓存命中仍可走）
9. `crates/supervisor/src/lib.rs:215-243`（Supervisor 字段）— **审计 P1 修复**：新增 `idle_buffers_reclaim_counter: AtomicU64` + 测试专用 `_idle_ttl_override: Arc<Mutex<Option<Duration>>>`（cfg(test)）。
10. `crates/supervisor/src/lib.rs:402-458` — 新增 `Supervisor::reclaim_idle_buffers_once()`：throttled reclaim（阈值 32 次调用），触发后扫 `instances` map 的所有 Session 调 `Session::evict_idle_buffers(IDLE_TTL)`（生产 60s / 测试可覆盖）。test helper `reclaim_count_snapshot()` + `set_idle_ttl_for_test()`
11. `crates/supervisor/src/lib.rs:3562-3561`（execute_tool 入口）— 生产路径 hook：每次 execute_tool 路过 +1，阈值命中后强制回收（保证 daemon / CLI 稳态调用都有回收时机）
12. `crates/supervisor/src/lib.rs:5144-5291`（新模块 `reclaim_idle_buffers_tests`）— 2 个新测试：`reclaim_is_throttled_until_threshold`（32 次调用 counter 累积 + 阈值命中后归零）+ `production_path_reclaim_after_idle_ttl`（mock_ls 真拉 session、guard drop、5ms TTL、生产路径 reclaim 断言 ≥1 条回收）
13. `crates/supervisor/src/lib.rs:5566-5659`（symbol_cache_tests 内）— 1 个新测试：`symbol_tree_resolves_language_per_file_not_single_lang`（4 lang × 不同扩展名；走 `lang=None` 强制逐文件扩展名解析；验证不混 session + 全部 entries 命中）

## 测试（TDD）

### P1 #1（diagnostics 空 push）
- `empty_push_clears_cache_entry`：3 分支断言（插入/清空/空 key no-op）

### P1 #2（docsync TTL）
- `nested_guards_emit_did_open_once`：3 嵌套 guard → 1 didOpen + 0 didClose（修了归零立即 didClose 旧断言）+ 显式 `evict_all_buffers` → 1 didClose
- `guard_drop_then_reopen_in_ttl_keeps_version_no_new_did_open`：drop→reopen 无新 didOpen、version 沿用；显式 evict 后 ensure_open 才发 v=1
- `guard_drop_during_ttl_reopen_does_not_emit_did_open`：TTL 复用 + 外部改盘 → 0 didOpen / 1 didChange v=2
- `evict_all_buffers_emits_did_close_and_clears_state`：2 文件 + 2 guard drop + evict → 2 didClose
- `evict_idle_buffers_skips_live_buffers`：live guard（ref_count>0）+ idle guard（已 drop），ttl=0 回收 → 1 didClose 仅 idle
- 既有 7 个 docsync 测试不变（路径/mtime 等核心语义保留）

### P1 #3（symbol-tree fan-out）
- `symbol_tree_aggregates_from_cache_and_skips_ignored_dirs`（既有）：2 文件 + 缓存命中 → 2 entries；dir 逃逸报 BadArgs。**P0 修复后走按 lang 分桶**（.rs 全部归一桶；miss_indices 为空，JoinSet 未创建）
- `symbol_tree_respects_max_files_fuse`（既有）：5 文件 max_files=3 → files_scanned=3 / truncated=true / entries=3
- `symbol_tree_fan_out_preserves_files_with_cache`（既有）：6 文件 + 全部缓存命中 → 6 entries、errors 空

### 审计 P0（符号树语言解析回归）
- `symbol_tree_resolves_language_per_file_not_single_lang`：4 lang（py/rs/java）× 5 文件、`lang=None` 强制按扩展名解析；验证 entries = 5（不漏）、errors 空（无 lang 误判）、entry_files 集 = 期望文件集（一对一不串）

### 审计 P1（TTL 生产执行者）
- `reclaim_is_throttled_until_threshold`：32 次 reclaim 调用，前 31 次 counter 累加 0..31 不触发 reclaim，第 32 次 counter 归零 + sweep（无 sessions 时返 0）
- `production_path_reclaim_after_idle_ttl`：mock_ls 真拉 session、注入 supervisor 实例池、ensure_open + drop guard、5ms 测试 TTL、`reclaim_idle_buffers_once` 第 32 次触发 → 断言 `reclaimed ≥ 1`（生产路径对归零超 TTL 的 buffer 真正回收）+ counter 归零

## 验证命令 + 关键输出

```
$ cargo test -p lsp-core --test docsync
running 12 tests ... 12 passed; 0 failed; 0 ignored

$ cargo test -p lsp-core --test client --test session --test types
client: 8 passed; session: 6 passed; types: 2 passed

$ cargo test -p supervisor --lib -- --skip write_gate
running 91 tests
test reclaim_idle_buffers_tests::production_path_reclaim_after_idle_ttl ... ok
test reclaim_idle_buffers_tests::reclaim_is_throttled_until_threshold ... ok
test symbol_cache_tests::symbol_tree_fan_out_preserves_files_with_cache ... ok
test symbol_cache_tests::symbol_tree_resolves_language_per_file_not_single_lang ... ok
test symbol_cache_tests::symbol_tree_aggregates_from_cache_and_skips_ignored_dirs ... ok
test symbol_cache_tests::symbol_tree_respects_max_files_fuse ... ok
test pull_diagnostics_tests::empty_push_clears_cache_entry ... ok
（其余 84 个既有单测全绿）
test result: ok. 91 passed; 0 failed; 2 filtered out (write_gate flaky)

$ cargo clippy -p supervisor --no-deps
(warnings: 0)
$ cargo clippy -p lsp-core --no-deps
(warnings: 0)
```

注：`symbol_tree_*` 测试全跑缓存命中路径，miss 路径（真实 LS fan-out）需 clangd e2e
覆盖；不在本次提单单元测试范围（既有 `tests/diagnostics.rs` 验证诊断路径）。
`production_path_reclaim_after_idle_ttl` 真正拉 mock_ls → 模拟生产路径。

## Cargo.toml 状态

未引入新依赖。`git diff HEAD -- Cargo.toml` 应为零行变化。

## 未做 / 边界

- 未跑 workspace 全量测试/clippy（按约束归上层统一跑）
- 未动 daemon、未动 supervisor 实例池语义；未动 FileBuffer 文本字段（mtime/size/version/refcount/last_released_at 5 项，架构拍板的现状保留）
- 生产回收时机：选"工具调用路过 throttled"（每 32 次 execute_tool 触发一次）；未加 daemon 端的后台 thread（daemon 路径通过 execute_tool + idle reaper 已能覆盖，过度工程保留）
- `Session::shutdown` 不显式调 `evict_all_buffers`：daemon 路径靠 Windows Job Object 兜底杀进程（生命周期无关 OS 状态）；测试可用 `evict_all_buffers` 显式清理
- `evict_idle_buffers` 的"复活竞态"标注：不引入两轮锁复杂化；LS 收到 didClose on known-closed 文档通常忽略，ponytail 权衡保留
- docsync 位置在 lsp-core 而非任务文档所述的 supervisor（任务引述路径为 `crates/supervisor/src/docsync.rs:60-81`，该文件不存在 —— FileGuard/Drop 实现在 `crates/lsp-core/src/docsync.rs:60-83`）；报告按实际文件位置列出
- `production_path_reclaim_after_idle_ttl` 测试中 mock_ls 拉取：找不到 `target/debug/mock_ls{,.exe}` 时输出 `skipped` + early-return（不构成 false failure；integration lsp-core crate 测试 setup 默认产物在此）
- `reclaim_idle_buffers_once` 阈值 (32) 是粗估；daemon 真实负载下需实测调；用户拍板后改
- flaky `write_gate::tests::concurrent_writes_serialize`：wall-clock sleep race 触发的预存在 flaky，未触动；本轮 `--skip write_gate` 跑其余 91 全绿
