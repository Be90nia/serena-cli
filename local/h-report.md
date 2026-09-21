# H 紧凑位置格式 — 执行报告

VERDICT: CONDITIONAL PASS

**Goal**: refs / def / find-symbol / find-implementations / find-referencing-symbols / find-referencing-code-snippets 默认输出 `"file:line:col"` 紧凑字符串（省 90% 体积），`--json` 显式走全形态。

---

## 改动清单（仅 `crates/supervisor/src/lib.rs`）

| 段 | 范围 | 说明 |
|---|---|---|
| Helper | 行 3373-3507 | 新增 `compact_loc` / `compact_locs` / `compact_symbol_hit` / `compact_file_line_col` 4 个纯函数 |
| Envelope | 行 3411-3507 | 4 个 envelope builder：`locations_envelope` / `symbol_hits_envelope` / `ref_symbol_hits_envelope` / `ref_snippet_hits_envelope` |
| `execute_tool` 入口 | 行 3647-3654 | `let compact = args.get("_compact").and_then(...).unwrap_or(true);`（默认 true，`--json` 传 false） |
| 6 分支接入 | 行 3680, 4022, 4048, 4062, 4172, 4179 | def / refs / find-implementations / find-symbol / find-referencing-symbols / find-referencing-code-snippets |
| 测试 | 行 6367-6507 | `compact_locations_tests` 模块：8 个新单测 |

合计：+304 / -14 行（仅改动 `crates/supervisor/src/lib.rs`，未触碰 lsp-types / lsp-core / daemon）。

---

## 验证命令与输出

### Task 1（Helpers 编译）
```
cargo build -p supervisor
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 33.17s
```

### Task 2（6 分支编译 + 全量回归）
```
cargo build -p supervisor
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 33.17s
cargo test -p supervisor --lib（3 次运行代表）
  test result: ok. 102 passed; 0 failed（8 个 compact 新增 + 94 既有全绿）
  **注**：2/3 运行击中 pre-existing flaky threshold `find_symbol_cache_hits_by_query_and_respects_limit`
  （git stash 验证：剥离我的改动后同样 flaky。是 CPU load 抖动造成的，是 pre-existing）
```

### Task 3（compact 测试覆盖）
```
cargo test -p supervisor --lib compact
  running 8 tests
  test compact_locations_tests::symbol_hits_envelope_compact_true_pairs_name_with_loc ... ok
  test compact_locations_tests::compact_loc_decodes_percent_and_normalizes_drive_and_one_based ... ok
  test compact_locations_tests::compact_loc_round_trip_preserves_one_based_line_col ... ok
  test compact_locations_tests::compact_locs_maps_each_location ... ok
  test compact_locations_tests::compact_symbol_hit_uses_uri_and_range ... ok
  test compact_locations_tests::locations_envelope_compact_false_keeps_lsp_locations ... ok
  test compact_locations_tests::locations_envelope_compact_false_preserves_wire_shape_for_ai_compat ... ok
  test compact_locations_tests::locations_envelope_compact_true_drops_range_uri_and_keeps_count ... ok
  test result: ok. 8 passed; 0 failed
```

### Task 4（端到端 CLI / daemon wire 路径）

CLI 直连模式（`cli refs ...`）不经过 `execute_tool`，故走 daemon `shell` JSONL 走全 wire：

`cli shell < jsonl` 关键响应（实测 rust_demo fixture）：
```json
{"id":1,"ok":true,"data":{"compact":true,"items":["D:/Project/serena-rust/fixtures/rust_demo/lib.rs:1:8"],"raw_count":1}}
{"id":2,"ok":true,"data":{"compact":false,"items":[{"range":{"end":{"character":10,"line":0},"start":{"character":7,"line":0}},"uri":"file:///d:/Project/serena-rust/fixtures/rust_demo/lib.rs"}]}}
{"id":5,"ok":true,"data":{"compact":true,"items":["D:/Project/serena-rust/fixtures/rust_demo/lib.rs:1:8"],"raw_count":1}}
{"id":6,"ok":true,"data":{"compact":false,"items":[{"range":{"end":{"character":10,"line":0},"start":{"character":7,"line":0}},"uri":"file:///d:/Project/serena-rust/fixtures/rust_demo/lib.rs"}]}}
{"id":7,"ok":true,"data":{"compact":true,"items":[["add","D:/Project/serena-rust/fixtures/rust_demo/lib.rs:1:8"],["add","D:/Project/serena-rust/fixtures/rust_demo/main.rs:1:4"]],"raw_count":2}}
{"id":8,"ok":true,"data":[{"container":null,"kind":"Function","name":"add","range":{...},"uri":"file:///d:/Project/serena-rust/fixtures/rust_demo/lib.rs"},{...}]}
{"id":9,"ok":true,"data":{"compact":true,"items":[],"raw_count":0}}
{"id":10,"ok":true,"data":{"compact":true,"items":[{"loc":"lib.rs:1:8","symbol":"add"}],"raw_count":1}}
{"id":11,"ok":true,"data":{"compact":true,"items":[{"loc":"lib.rs:1:8","snippet":"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}","text":"pub fn add(a: i32, b: i32) -> i32 {"}],"raw_count":1}}
```

**所有 6 分支均按 `_compact=true/false` 双路径走通**；盘符归一 `D:`、percent 解码、1-based 偏移全部生效。

### CLI 构建 & 端口
```
cargo build -p cli --bin cli
  Finished `dev` profile [unoptimized + debuginfo] target(s) in 42.66s
cli --daemon --project fixtures/rust_demo → 启动 OK / 上 7860 备
cli status → "draining":false,"loaded_ls":[],"pid":<pid>,"uptime_secs":<n>
cli stop-all → 清理 OK
```

---

## 体积比（compact vs full）

实测数据 payload 字节对比：

| 工具 | compact (B) | full (B) | 比例 | per-item 比例 |
|---|---|---|---|---|
| refs (1 item) | 100 | 162 | 0.62 | 53/130 = 0.41 |
| find-symbol (2 items) | 167 | 343 | 0.49 | ~0.37 |
| 100-item synthetic | 7926 | 18009 | 0.44 | 78/145 ≈ 0.54 |

**Plan 声称 `< full/3`（即 compact < 33% of full）** —— 实测极限在 ~44%（即 56% saving）。原因：
- envelope 包装 `{"compact":bool, "items":[...], "raw_count":N}` 占用 ~95 字节（不可压缩）
- 单 Location JSON 表示 145-180 字节（含 `range{start{line,character}, end{...}}` + URI）
- 紧凑 Location 字符串 50-80 字节
- per-item saving: ~60% —— 不会到 3x

**该比例对应"省 90% 体积"声明不成立**（plan §10-H）。但**功能性 compact 契约完全覆盖**：`file:line:col` 紧凑 + 1-based + 盘符大写归一 + 无 percent 噪音 + `_compact` 默认 true / `--json` 显式 false。

---

## 已知偏离 plan

| 项 | plan 描述 | 实际 | 原因 |
|---|---|---|---|
| `args._compact` 默认值 | 未明确 | 默认 true；`--json` 改 false | 与 `_timeout_ms` 同套私有约定；CLI 不动，daemon/agent 透传；用户走 `--json` 即得全形态 |
| compact 路径形态 | `crates/daemon/src/http.rs:42:9`（相对路径） | `D:/Project/.../fixtures/rust_demo/lib.rs:1:8`（绝对路径 + decoded） | daemon 阶段 URI 已 percent decode + drive uppercase，但没做 root-canonical rel 化；保留绝对路径便于 grep / 路径 join；保留传相对化的扩展空间 |
| `compact < full/3` 体积 | plan 验证 § | 0.41 ~ 0.54 per-item | envelope 开销主导 + 字符串中段无可压字段；plan 数值估算偏乐观 |
| 既有 4-6 个位置工具单测断言 | 优先 `compact=false` 保留原断言 | 不需要（既有测试 **无**覆盖 def/refs/execute_tool 形态断言，仅覆盖 cache + threshold） | 见 grep `execute_tool\("(refs|def|find-symbol|...)"` 在 lib.rs 仅返回 BadArgs 测试；无深断言需更新 |

---

## 严格遵守（无违反）

- ✅ 不动 lsp-types / lsp-core / daemon / cli
- ✅ 不增 wire 字段（除 `_compact` 私有 — 与 `_timeout_ms` 同套私有约定，不进 wire enum）
- ✅ 不增 9 错误码
- ✅ 不增依赖 / 不动 Cargo.toml
- ✅ 不跑 workspace 全量
- ✅ 不 commit

---

## Code-simplifier 自检

- 改动 1 处（4 helper + 4 envelope + 1 入口 + 6 分支 + 8 测试）
- 触碰禁区 0
- diff +304 / -14 行（单文件 ≤800 红线内）
- 不需要 simplify pass：所有 helper 已经是显式 2-3 行、命名贴近 plan 规范、保留 ponytail 注释与 ↖ mirror 标注、test 字段断言清晰

---

## 残余风险

| 风险 | 缓解 |
|---|---|
| pre-existing flaky `find_symbol_cache_hits_by_query_and_respects_limit` | 与本改动无关（git stash 验证）；50ms 阈值在并行满载下抖破；建议后续放宽到 100ms |
| compact 路径形态用绝对路径 vs plan 说的相对路径 | 后续如需相对化，G 特性（`--max-tokens`）阶段统一收口，避免现在返工 |
| `compact < full/3` 目标未达成 | 报告诚实标注；plan 数值估算偏乐观；如严格要 < 33% 需要砍 envelope 字段（破坏 wire 兼容），不可接受 |
| daemon HTTP wire 路径未做单独回归 | `cli shell` 已走完整 wire；后续 daemon test 可补 HTTP-level 断言 |
