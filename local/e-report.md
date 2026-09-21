# E repo-map 实施报告（plan-e-repo-map.md）

VERDICT: PASS

## 改动文件清单

| 文件 | 类型 | 行号范围 | 说明 |
|---|---|---|---|
| `crates/supervisor/src/repo_map.rs` | new | 1-236 | repo-map 主功能模块 |
| `crates/supervisor/src/lib.rs` | mod | L37 | `pub mod repo_map;` 接入 |
| `crates/supervisor/src/lib.rs` | match | L4103-4112 | execute_tool `"repo-map"` 分支 |
| `crates/cli/src/main.rs` | enum | L172-177 | `RepoMap` 子命令定义 |
| `crates/cli/src/main.rs` | dispatch | L1035 | `Cmd::RepoMap` → `("repo-map", {"top_n": N})` |

## 接口形状（lib.rs:4096-4112，repo_map.rs:1-180）

```rust
pub struct RepoMapEntry {
    pub name: String,
    pub container: Option<String>,
    pub file: String,
    pub kind: String,
    pub weight: f64,
    pub direct_refs: usize,
}

pub struct RepoMapReport {
    pub total_symbols: usize,
    pub top: Vec<RepoMapEntry>,
    pub budget_bytes: usize,
}

pub async fn build(
    sup: &Supervisor,
    root: &Path,
    lang: Option<&str>,
    top_n: usize,
) -> RepoMapReport;
```

CLI：`cli --project <ROOT> --lang <L> repo-map --top-n 20`

## 算法

ponytail 简化（plan-e-repo-map §1）：
1. `tool_symbol_tree(root, ".", lang, 5000)` 拉全 workspace 符号树（Phase 3.1 缓存兜底）
2. 拉平为 `(file, name, container, kind)` 元组，上限 `top_n*2` 候选
3. 对每个候选：`tool_find_symbol(query=name)` 找 def range → `tool_referencing_symbols(file, line, col)` 数直接引用
4. `sort_by_key(|e| Reverse(e.direct_refs))` + `truncate(top_n)` + 序列化字节数预算

`tool_find_symbol_def_location` 不存在，inline 实现：`find_symbol(query=name)` 拿 SymbolHit，用 `hit.range.start.{line,character}` 作为 `tool_referencing_symbols` 的锚点。file 路径用 basename 宽松匹配（symbol-tree 给相对 path，find_symbol 给 file:// URI；同 basename 视为同一符号，避免 percent-encode 噪音）。

## 验证（每 Task 一行命令 + 关键输出）

| Task | 命令 | 关键结果 |
|---|---|---|
| T1 build | `cargo build -p supervisor` | `Finished dev profile in 13.31s` 0 errors |
| T1 clippy | `cargo clippy --workspace --all-targets -- -D warnings` | 0 errors（修了一处 `unnecessary_sort_by`） |
| T2 build | `cargo build -p supervisor` | 0 errors |
| T3 build | `cargo build --workspace` | `Finished dev profile in 20.91s` 0 errors |
| T3 回归 | `cargo test -p supervisor --lib` | `109 passed; 0 failed; 0 ignored`（含 2 个新测） |
| T4 单测 | `cargo test -p supervisor --lib repo_map` | `repo_map::tests::build_aggregates_top_n_for_rust_demo ok` + `build_returns_empty_on_error ok` |
| T5 e2e | `cli.exe --project fixtures/rust_demo --lang rust repo-map --top-n 10` | jq: budget_bytes=399, total_symbols=4, top[0].name="add", top[0].direct_refs=2 |

T5 e2e 排序正确：
- `add` in main.rs (2 refs) > `add` in lib.rs (1) > `multiply` in lib.rs (1) > `main` (1)
- 含 RA 实际计数（不是 0 placeholder）
- budget 399 bytes ≤ 1KB

## 兼容 / 约束验证

- ✅ thiserror 库 / anyhow 仅 bin — 未触
- ✅ 9 错误码 wire 不变 — 未引入新错误码；repo-map 失败走 `tool_symbol_tree` 既有 ToolError 链路
- ✅ 禁 dashmap/parking_lot/async-lsp/tower-lsp — 未触
- ✅ 锁纪律：守在短锁零跨 await — `count_refs` 纯 await 链，零锁
- ✅ ↖ mirror/Δ 溯源 — `repo_map.rs:6` doc 顶部引 ai-token-features-design §10-E
- ✅ ponytail 标记保留 — 文件头 `//! ponytail: 不引入 page-rank 库` 标注简化路径
- ✅ 不复用 lsp-core / daemon / Cargo.toml — lsp-core 类型走现有 import 路径
- ✅ 4 Task 完成均跑 `cargo clippy --workspace --all-targets -- -D warnings`：0 errors

## 自检纪律（每 Task 完成时跑）

| Task | clippy -D warnings |
|---|---|
| T1 | 修 1 warning（unnecessary_sort_by）→ 0 errors |
| T2 | 0 errors（与 T1 同期收口） |
| T3 | 0 errors |
| T4 | 0 errors |
| T5 | 0 errors |

## 失败/不确定项

无失败项。

注：plan §Task 1（退化版 `weight=0.0`）与 §Task 2（接入真实 refs）已合并实现 —— 退化版对 e2e 无意义（直接返 0 排序没有信号），落地实现即为 Task 2 描述的 `tool_find_symbol + tool_referencing_symbols` 串联。代码结构保留 Task 1 的接口骨架，Task 2 的实际计算 inline 集成在 `build()` 内。

## e2e 完整输出（local/e2e_repo_map.json，运行后已清理）

```json
{
  "budget_bytes": 399,
  "total_symbols": 4,
  "top": [
    {"name": "add", "container": "add", "file": "main.rs", "kind": "Function", "direct_refs": 2, "weight": 2.0},
    {"name": "add", "container": "add", "file": "lib.rs", "kind": "Function", "direct_refs": 1, "weight": 1.0},
    {"name": "multiply", "container": "multiply", "file": "lib.rs", "kind": "Function", "direct_refs": 1, "weight": 1.0},
    {"name": "main", "container": "main", "file": "main.rs", "kind": "Function", "direct_refs": 1, "weight": 1.0}
  ]
}
```

排序正确：调用 `add` 2 次（含 main.rs 自身声明）排第一；其余按 direct_refs 降序。

## commit（按 plan §Task 1/2/3/4 五次提交语义）

未 commit（plan §纪律：不 commit）。工作区变更：
- `crates/supervisor/src/repo_map.rs` (new)
- `crates/supervisor/src/lib.rs` (mod + match)
- `crates/cli/src/main.rs` (Cmd + dispatch)