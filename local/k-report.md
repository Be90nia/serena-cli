# K 报告：find-symbol 兜底降级（workspace/symbol 真空修复）

VERDICT: DONE — 3 Task 全部完成，supervisor lib 122/0 全绿，clippy --workspace -D warnings 0 errors，e2e 双路径实证（rust 正常路径不变 + TS 真空兜底命中 + 裸 LS 探针实锤上游缺陷）。

## 改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/supervisor/src/lib.rs` | 全部改动（+168 行，5 处） |

1. [lib.rs:1969-1975] tool_find_symbol 内兜底触发点：merged 全空 → `fallback_documents_scan()`；Δ 注释标注 §12-K。cache_put/truncate/wire 零变化。
2. [lib.rs:1982-2037] 新增 `async fn fallback_documents_scan()`：filtered_walker 枚举 → 按扩展名解析语言、只扫 langs 集合内文件（不为新语言 spawn LS）→ 500 文件保险丝 → 逐文件 `overview_via_session`（3.1 缓存兜着，复用既有 helper，签名收 session/cache_arc/root/rel）→ `fuzzy_match` 过滤聚合。单文件失败一律 continue，兜底只增广结果不改 wire。
3. [lib.rs:3333-3337] 新增 `fn fuzzy_match()`：大小写不敏感 substring；ponytail 标记（不做子序列/编辑距离）。
4. [lib.rs:6756,6800] 夹具 `mock_sup_with_symbols` / `write_fixture` 提为 pub(crate)（供新测试 mod 复用，零重复）。
5. [lib.rs:6927-7018] 新增 `mod find_symbol_fallback_tests`（4 测试，文件最末尾，符合 items-after-test-module 规则）。

**偏离 plan 标注**：
- plan 伪代码 `parse_workspace_symbol_response`/`SymbolHit` 重构未采用——实际代码已是 `session.request::<Vec<SymbolInformation>>` + 就地 map，无需改造。
- 兜底走 `overview_via_session` 逐文件 `session_for`（同 key 微秒级快路径）而非预持 session：忠实 plan；与 tool_overview 语义等价（同缓存表、同空不写纪律）。

## Task 2 测试与诚实标注

mock_ls 对 workspace/symbol **硬编码回 null**（make_reply 不可配置；lsp-core 禁改）→「workspace/symbol 正常命中时不兜底」无法 mock。按派单授权走替代：

| 测试 | 验证什么 | 状态 |
|---|---|---|
| `fuzzy_match_basic` | 正例 3（前缀/中缀/大小写）+ 反例 2 | PASS |
| `find_symbol_falls_back_on_empty_workspace_symbol` | mock_ls null（=真空形态）→ 兜底 documentSymbol 扫描 → 恰命中 mock_helper | PASS |
| `find_symbol_unknown_lang_fallback_degrades_to_empty`（替代①） | 未知 lang：无会话 + 兜底语言过滤 0 文件 → 优雅 Ok([]) 不报错 | PASS |
| `find_symbol_fallback_result_cached_second_call_no_rescan`（替代②） | 兜底结果入 find_symbol 级缓存，二次调用 <10ms 免重扫 | PASS |

「正常路径不兜底」由 Task 3 e2e 补足（rust-analyzer 命中 2 条、wire 与既有一致，兜底绝不污染非空路径）。

## Task 3 e2e + 真空实锤

```
$ cli stop-all && cli --project fixtures/rust_demo --json find-symbol "add" --lang rust
→ exit=0, 1.03s
{"items":[["add",".../lib.rs:1:1"],["add",".../main.rs:1:1"]],"raw_count":2}   # 正常路径，既有结果不变

$ cli --project fixtures/typescript_demo --json find-symbol "calc" --lang typescript
→ exit=0 → {"items":[["Calculator",".../main.ts:5:1"]],"raw_count":1}
$ cli --project fixtures/typescript_demo --json find-symbol "multiply" ...
→ exit=0 → {"items":[["multiply",".../main.ts:1:1"]],"raw_count":1}
```

**TS 真空实锤**（`local/k_ts_ls_probe.py` 裸 LSP 探针，initialize→workspace/symbol）：
typescript-language-server (TS 5.9.3) 对冷 workspace/symbol 直接回错
`TypeScript Server Error: No Project (navto/getNavigateToItems)`。
→ 冷 daemon 首查 "calc"（无 didOpen、无缓存）唯一可能路径 = workspace/symbol 报错空 → **兜底扫描活体命中**。证据链闭合。

## 验证命令 + 关键输出

| 检查 | 命令 | 结果 |
|---|---|---|
| Task1 build | `cargo build -p supervisor` | `Finished dev ... in 10.10s` 0 errors |
| Task1 clippy | `cargo clippy --workspace --all-targets -- -D warnings` | `Finished` 0 errors |
| Task2 测试 | `cargo test -p supervisor --lib find_symbol` | `6 passed; 0 failed`（4 新 + 2 既有缓存） |
| Task2 clippy | 同上 clippy | 0 errors |
| Task3 build | `cargo build -p cli` | `Finished` 0 errors |
| 回归 | `cargo test -p supervisor --lib` | **`122 passed; 0 failed`** |
| e2e rust | find-symbol "add" --lang rust | raw_count=2（既有不变） |
| e2e ts | find-symbol "calc"/"multiply" --lang typescript | 各命中 1 条 |

## 纪律核对

- 未 commit（按派单）；未动 lsp-core / daemon / Cargo.toml；锁纪律：零新增锁，缓存经既有微临界区 helper。
- wire 不变：9 错误码零触碰；find-symbol 响应 shape 不变；空结果仍 Ok([])。
- code-simplifier 自检：改动 6 处 / 触碰禁区 0 / diff +168 / -0（本文件）；回读核对语义无变形；无 unwrap/expect 生产代码；复述性注释 0，why 注释保留。
- 证据文件：local/k_e2e_rust.json、local/k_e2e_ts.json、local/k_e2e_ts2.json、local/k_ts_ls_probe.py（可复现实锤工具）。

## 残余风险

- 混合语言 root 且 lang=None 时兜底扫所有探测语言文件——500 保险丝 + 缓存兜底，极端大仓首查仍慢（预期内，plan 接受该代价）。
- 兜底扫描串行（plan 伪代码即串行）；若未来大仓超时，升级路径：symbol-tree 的分桶 MAX_INFLIGHT=4 并发。
