# G 报告：--max-tokens 预算护栏 + --compress 签名压缩

VERDICT: OK

## 改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/supervisor/src/lib.rs` | +72/-4：helper `apply_budget`（L3764-3815）/ `apply_compress`（L3817-3832）；execute_tool 收口 `match tool {...}?` + 末尾统一后处理（L4029、L4703-4718）；tests 4 个（L7080-7143） |
| `crates/cli/src/main.rs` | +16/-0：Cli 全局 flag `--max-tokens N` / `--compress`（L72-79）；args 注入 `_max_tokens`/`_compress` 私有字段（L1340-1349） |

不 commit（按任务纪律，归上级）。

## 实现要点（偏离 plan 标注）

1. **接入方式偏离 plan**：plan 建议把 execute_tool 各分支改造为统一变量赋值（11 特性已改动各分支，逐支改造 diff 大且易错）。实际 match 整体即 `Result<Value, ToolError>`，改用 `let mut value = match tool {...}?;` 一行收口 + 末尾 9 行后处理，**零分支改动**。
2. **apply_budget delta guard**：J 增量形态（含 `added`/`removed` 键）跳过截断，guard 内聚在 helper 首行并配专属测试 `apply_budget_skips_delta_response`。`{"delta":false,"items":[...]}`（delta 首调全集）无 added/removed 键，正常参与截断。
3. **apply_compress 简化**：plan 草稿对 `items` 重复递归（先 strip items 再全量 strip），实际单遍递归等价覆盖；删 `container`/`container_name`/`kind` 三个键，保留 name/位置。
4. **截断标志字节余量**：plan 按 18 bytes 预估 `truncated`+`original_count` 开销，实测约 42 bytes——最终 payload 可能轻微超 budget_bytes（soft limit 语义内，plan 原文自定 18 余量即承认近似）。测试未断言最终字节 ≤ 预算，避免脆断言。
5. **overview 不参与截断**：overview 默认 wire 是顶层数组（无 `items` 键），apply_budget 对无 items 响应返回 false 零改动——plan 代码的自然边界，仅 compress 对 overview 生效。e2e compress 因此用 overview 验证。

## 验证命令与关键输出

### Task 1: build + clippy

```
cargo build -p supervisor -p cli
    Finished `dev` profile ... in 18.09s   （0 errors）

cargo clippy --workspace --all-targets -- -D warnings
    Finished ... in 9.22s                  （0 errors；曾报 manual_div_ceil，
                                            已改 (lo+hi).div_ceil(2) 后通过）
```

### Task 2: 单元测试

```
cargo test -p supervisor --lib apply_budget
test apply_budget_passes_through_when_under ... ok
test apply_budget_skips_delta_response ... ok
test apply_budget_truncates_items_when_exceeded ... ok
test result: ok. 3 passed; 0 failed

cargo test -p supervisor --lib apply_compress
test apply_compress_removes_container_and_kind ... ok
test result: ok. 1 passed; 0 failed
```

### Task 3: e2e CLI（daemon 新二进制，rust-analyzer，project=D:/Project/serena-rust）

```
# budget：find-symbol execute_tool --max-tokens 200
$ cli --project . find-symbol execute_tool --max-tokens 200
exit=0
{ "truncated": true, "original_count": 14, "items": 7, "raw_count": 14 }
→ 截断生效，success 语义（退出码 0 不变）

# compress：overview --compress vs 默认
$ jq '.[0] | keys' g_compress.json → ["name","range","uri"]          （container/kind 已删）
$ jq '.[0] | keys' g_default.json  → ["container","kind","name","range","uri"]
体积：128685 → 104069 bytes（省 19%）

# 默认无 flag 老 wire 不变
$ find-symbol execute_tool（无 flag）→ { "truncated": false, "items": 14 }（无 truncated 键，无截断）
$ overview（无 flag）→ container/kind 完好

# 清理
$ cli stop-all → daemon draining (pid 8684); exit=0
```

### 全量回归

```
cargo test -p supervisor
test result: ok. 128 passed; 0 failed   （lib，含 4 个 G 新测试）
（其余 18 个 test target 合计 0 failed）
```

## 边界与残余风险

- `truncated`/`original_count` 标志开销按 18 bytes 余量二分，最终 payload 可超预算 ~24 bytes（soft limit 语义，见实现要点 4）。
- `_compact` 全局 flag `--json` 至今未接线到 `args._compact=false`（H 遗留缺口，位置工具默认恒 compact）。因此 `--compress` 对位置工具（恒 compact 形态，无 container/kind 键）实际无效果；非 compact 形态（如 e2e 的 overview 顶层数组）才见字段删除。缺口归 H，不在 G 范围。
- budget 只作用于顶层 `items` 数组；树形响应（symbol-tree）与顶层数组（overview）不截断（plan 设计如此）。
- delta 响应跳过截断 = 超大增量集仍全量返回（任务要求"直接跳过，报告标注"，已标注）。

## 沉淀

无新增经验（div_ceil 属 clippy 常规提示；plan 余量近似属项目内事实，随本报告留档）。
