# T1 报告：CLI --json 注入 `_compact=false`

VERDICT: DONE

## 改动

- `crates/cli/src/main.rs:1345-1348`（forward 注入段）：`inject_compact_arg(&mut args, cli.json)` —— `--json` 时向 daemon 转发 args 注入 `_compact=false`；无 `--json` 时一字不动，老 wire（紧凑形态）零变化。
- `crates/cli/src/main.rs:1844-1855`：新增 helper `inject_compact_arg`（与 `inject_timeout_args` 同风格；非 object 形态静默跳过）。
- `crates/cli/src/main.rs:1873-1894`：`#[cfg(test)] mod tests` 两个单测。

supervisor 侧零改动（`execute_tool` 已消费 `args._compact`，默认 true，sanitize 不清 —— lib.rs:4007-4013）。daemon/Lsp-core 零改动。wire/错误码零改动。

## 验证

### 单测（P0 #4）

- `cargo test -p cli --bin cli`：`json_flag_injects_compact_false` ok、`no_json_flag_leaves_args_untouched` ok（2 passed / 0 failed）
- `cargo test -p cli`：9 passed（integration）
- `cargo test -p supervisor`：全部 ok（21 across 5 targets, 0 failed）

### e2e 对照（P0 #1/#2/#3，fixtures/rust_demo，daemon 转发路径）

find-symbol `add`：
- 默认：`{"compact": true, "items": [["add", "D:/.../lib.rs:1:8"], ...], "raw_count": 2}`
- --json：原始 SymbolHit 数组，含 `"uri": "file:///d:/Project/.../lib.rs"` + `range.start/end`

refs / def（lib.rs 0 7）：
- 默认：`{"compact": true, "items": ["D:/.../lib.rs:1:8"], "raw_count": 1}`
- --json：`{"compact": false, "items": [{"range": {...}, "uri": "file:///..."}]}`

search：默认与 --json 输出一致（search 是 grep 型工具，不消费 `_compact`；注入被忽略，无副作用）。

### clippy（P0 #6）

- `cargo clippy --workspace --all-targets -- -D warnings`：Finished，0 errors（每步改动后 `cargo clippy -p cli --all-targets` 亦 0 errors）

## 决策说明

- overview 不消费 `_compact`（supervisor overview 分支直接 `to_value(raw)`，lib.rs:4030-4039），--json 对 overview 无形态变化——注入无副作用。
- `_compact` 对不消费的工具被忽略，故注入不按工具白名单（与 `_delta` 需白名单不同，`maybe_delta` 会真触发增量编排）。

## 未做 / 非目标确认

- 未重写 H 逻辑、未改 wire、未动 Lsp-core/daemon。未 commit。
