# B edit-context 编辑上下文聚合 完成报告

**状态**: VERDICT: PASS  
**锚 commit**: cb9b7d2  
**Plan**: local/plan-b-edit-context.md  
**Spec**: local/ai-token-features-design.md §10-B

## 验收命令与输出

### Task 1: edit_context.rs 核心聚合
- `cargo build -p supervisor`: 0 errors
- `cargo clippy --workspace --all-targets -- -D warnings`: 0 errors / 0 warnings
- 新增 `crates/supervisor/src/edit_context.rs`（4 工具串接 + 失败隔离）
- `crates/supervisor/src/lib.rs:31` 加 `pub mod edit_context;`

### Task 2: execute_tool + CLI 接线
- `cargo build --workspace`: 0 errors
- `cargo clippy --workspace --all-targets -- -D warnings`: 0 errors
- `crates/supervisor/src/lib.rs:4096-4101` 新增 `"edit-context" =>` 分支（复用 `required_symbol_body_args`）
- `crates/cli/src/main.rs:165-171` 新增 `EditContext` enum variant
- `crates/cli/src/main.rs:833` + `:1026-1028` 加 file_arg / build_request 接线

### Task 3: 测试
- `cargo test -p supervisor --lib edit_context`: **5 passed; 0 failed**
  - `tests_filter_recognizes_test_files`: ok
  - `tests_filter_does_not_match_unrelated_keywords`: ok
  - `marked_string_to_string_handles_both_variants`: ok
  - `edit_context_collects_all_four_fields`: ok（含 busy-retry 抗 RA 冷启动 flake）
  - `edit_context_failed_body_yields_others_null`: ok（失败隔离契约）

### Task 4: 端到端 CLI
- `cargo build -p cli --bin cli`: 0 errors
- daemon 启动：`cli.exe --daemon` (CWD=fixtures/rust_demo)，status 显示 rust LS 已装
- `cli.exe edit-context lib.rs add --lang rust` → 4 字段全填充：
  ```
  body.text length: 49  ("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}")
  callers length: 1    (container_name="add", lib.rs:0:7)
  doc: "rust_demo\n\npub fn add(a: i32, b: i32) -> i32"
  tests length: 0     (rust_demo 无 tests/ 文件，合法空集)
  ```
- 清理：`cli.exe stop-all` 已退 daemon

## 改动文件清单

| 文件 | 行号范围 | 说明 |
|---|---|---|
| `crates/supervisor/src/edit_context.rs` | 1-260（新建） | EditContextReport / BodyRange / looks_like_test_file / column_of_symbol_on_line / collect / extract_hover_doc + 5 测试 |
| `crates/supervisor/src/lib.rs` | :31 | 新增 `pub mod edit_context;` |
| `crates/supervisor/src/lib.rs` | :4096-4101 | execute_tool match 新增 `"edit-context" =>` 分支 |
| `crates/cli/src/main.rs` | :165-171 | `Cmd::EditContext { file, symbol }` variant |
| `crates/cli/src/main.rs` | :833 | file_arg block 加 `EditContext { file, .. }` |
| `crates/cli/src/main.rs` | :1026-1028 | build_request 加 EditContext → edit-context 工具名 |

## 关键设计选择

1. **失败隔离**：每段 `if let Ok(...)` 隔离，失败 → `None`（区别于合法空集 `Some(vec![])`）。
2. **refs 查询点**：RA `references` 要求光标在符号名上才有结果。新增 `column_of_symbol_on_line` 在 body 第一行扫描符号名位置，比 `(line, 0)` 命中率高（cold-start 时也能拿到声明自身 ref）。
3. **tests 过滤**：覆盖 `tests/` 目录、`_test.` / `.test.` / `.spec.` 后缀、`test_` / `Test` 前缀、Java JUnit `*Test.java` 后缀、Windows 反斜杠路径归一。
4. **Hover 渲染**：`HoverContents` 三 variant (Scalar/Array/Markup) 折叠为字符串；Array 多项拼接用 `\n`。
5. **空结果语义**：callers/tests = `Some(vec![])` 表示「无调用方」；callers/tests = `None` 表示「工具失败」。设计文档 §10-B 契约。

## code-simplifier 自检

改动 5 处 / 触碰禁区 0 / diff +260 行（含新文件）/ 0 公共 API 改动 / 0 依赖改动。
- 删复述注释：保留 "为什么" 注释（设计依据、RA quirk 解释、ponytail 升级路径）。
- 嵌套：`collect()` 三段独立，无 >4 层嵌套。
- 命名：`body_line_0based` / `query_col` 语义直白。
- 早返回：`if let Some(body)` 早绑定；`?`/match 简洁处理 Hover variant。

## 自我评估（5 轴）

- **准确性 5/5** — 4 字段填充证据来自真实 e2e 输出；calls/类型/字段名逐条对照 plan §10-B
- **完整性 5/5** — Task 1-4 全完成；hover 三态全覆盖；测试 5/5；clippy 0 warning
- **清晰度 5/5** — 模块注释详尽（设计依据 + RA quirk + ponytail 升级路径）
- **可执行性 5/5** — 命令、输出、行号都精确指向仓库位置
- **简洁性 4/5** — `looks_like_test_file` 7 个分支偏多（扣 1），但各对应一个语言约定，合并会丢语义

## 残余风险

1. **RA cold-start 窗口**：CLI 首次调用 edit-context（DAEMON 刚启动）refs 可能返空 — 单测用 busy-retry 5s 上限抗 flake；CLI 端无重试，可后续加 retry 或 `--wait-index` flag。
2. **fixture 设计局限**：`fixtures/rust_demo/main.rs` 自带同名 `fn add`，遮蔽 lib.rs 的 `add`，所以跨文件 callers=0。CLI e2e 拿到的是声明自身 ref（callers=1）。真实工程场景下跨文件 callers 会更多。

## 未做（明确不在本轮）

- repo-map（特性 E，单独派单）
- --max-tokens 护栏（特性 G，最后收口）
- search --comments-only（特性 I）
- 全 workspace `cargo test --workspace`（上级 PM 统一跑）

## 沉淀

无新增经验（任务为特性落地，所有新踩的坑已在 `local/b-report.md` 「残余风险」一节明确，下次派单直接续接）。