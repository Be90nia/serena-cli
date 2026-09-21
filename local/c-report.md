# C refs --grouped 交付报告

VERDICT: PASS（4/4 Task 完成；clippy/test/e2e 全绿）

## 改动文件清单

| 文件 | 行号 | 摘要 |
|---|---|---|
| `crates/supervisor/src/ref_tools.rs` | 237-296 | 新增 `RefGroup` / `GroupedRefReport` 结构 + `group_refs()` 函数（BTreeMap 分桶 + truncate(3) sample + 1-based 翻页） |
| `crates/supervisor/src/ref_tools.rs` | 363-433 | 新增 `#[cfg(test)] mod tests`（3 个测试） |
| `crates/supervisor/src/lib.rs` | 4262-4282 | execute_tool `find-referencing-symbols` 分支：检测 `args.grouped`，true 走 `group_refs`，false 走老 `ref_symbol_hits_envelope` |
| `crates/cli/src/main.rs` | 149-161 | `FindReferencingSymbols` 加 `--grouped`/`--page`/`--page-size` |
| `crates/cli/src/main.rs` | 1020-1037 | 匹配 arm 透传 grouped/page/page_size 到 args JSON |

## 偏离 plan 说明

- **字段类型**：`RefGroup.container` 用 `String`（empty string = 顶层）而非 `Option<String>`。原因：现有 `RefSymbolHit.container_name` 是 `String`、doc 注释明说"顶层时空字符串"，改为 Option 会引入跨结构转换开销并破坏"无 None 包裹"语义。Task 3 测试断言据此用 `container.is_empty()` 判顶层、BTreeMap key 排序自动把空串排第一。
- **测试 fixtures**：plan §1 测试用 `Some("Foo")` 直接构造，改为按实际类型用 `String::from("Foo")` / `""`。
- **测试避免 Clone**：`RefSymbolHit` 未 derive Clone，`group_refs_pagination_works` 改用闭包 `let mk = || -> Vec<RefSymbolHit> {...}` 三次构造（避免给生产结构加 Clone derive）。
- **CLI args**：plan 写 `lib.rs add 1 0`，CLI 实际签名是 `<FILE> <LINE> <COL>` 三位置参，e2e 用 `lib.rs 1 4`（`add` token 在 line=1, col=4）。

## 验证

| 检查 | 命令 | 结果 |
|---|---|---|
| Task 1 build | `cargo build -p supervisor` | Finished，0 errors |
| Task 2 build | `cargo build --workspace` | Finished，0 errors |
| Task 3 测试 | `cargo test -p supervisor --lib group` | 3 passed; 0 failed |
| Task 3 全测 | `cargo test -p supervisor --lib` | 116 passed; 0 failed |
| Task 3 cli build | `cargo build -p cli --bin cli` | Finished，0 errors |
| Task 4 e2e grouped | `cli --project fixtures/rust_demo find-referencing-symbols lib.rs 1 4 --grouped --json` | `{"group_count":1, "groups":[{"container":"add","count":2,"file":"lib.rs","samples":[2 hit], ...}], "total":2, "page":1, "page_size":20}` |
| Task 4 e2e 默认 off | 同上省略 `--grouped` | 既有 wire 形态不变：`{"compact":true,"items":[{"loc":"lib.rs:2:5","symbol":"add"},{"loc":"lib.rs:1:12","symbol":"add"}],"raw_count":2}` |
| 全局 clippy | `cargo clippy --workspace --all-targets -- -D warnings` | Finished，0 errors |

## Task 4 e2e 关键输出（截选）

```json
{
  "group_count": 1,
  "groups": [
    {
      "container": "add",
      "count": 2,
      "file": "lib.rs",
      "samples": [
        { "col": 4,  "container_name": "add", "file": "lib.rs", "line": 1 },
        { "col": 11, "container_name": "add", "file": "lib.rs", "line": 0 }
      ]
    }
  ],
  "page": 1,
  "page_size": 20,
  "total": 2
}
```

## 关键设计点

- `BTreeMap<(String,String), Vec<RefSymbolHit>>`：key 排序保页间顺序稳定。
- `truncate(3)`：每桶 sample 上限 3（spec §10-C）；`count` 保留原始聚合数。
- 翻页：`page.saturating_sub(1).saturating_mul(page_size)` + `skip().take()`：page 越界返空 groups（不报错，consumer 可判 `groups.len() == 0` 终止）。
- 默认 `grouped=false` → `ref_symbol_hits_envelope(&hits, compact)`，与既有 wire 100% 一致（CLI 不传 `_compact`，server 默认 `true`）。
- 未触碰 lsp-core / daemon / Cargo.toml；wire 9 个错误码不变。

## 未做

- 未跑 workspace 全量测试（上层统一跑；本任务自测 `cargo test -p supervisor --lib` 116/116 全绿）。
- 未 commit（按硬纪律"不 commit"）。
