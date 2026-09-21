# M-warm 执行报告（plan-m-warm.md / ai-token-features-design §13-M）

VERDICT: DONE — 3 Task 全部完成；build/clippy/test 三门全绿；e2e 实测 warm 后首个 overview 44ms（冷启动 838ms，19×），重复 warm 幂等 3ms。1 项验收标注（refs 首调窗口为 RA 既有 quirk，非 warm 层可修，证据见下）。

## 改动文件清单

| 文件 | 改动 |
|---|---|
| `crates/supervisor/src/warm.rs` | **新增**。warm 主入口：find_entry_file（filtered_walker + LanguageId 扩展名表，限深 4）→ session_for（LS spawn + on_server_ready 根探针）→ ensure_open 入口文件 → busy-retry documentSymbol 非空即 ready；超时返 `{ready:false, partial:true}`；上限 1h 防 Instant 溢出 panic。含 5 个测试（2 纯 fs + 1 BadArgs 拒绝 + 2 真实 RA 集成） |
| `crates/supervisor/src/lib.rs` | +`pub mod warm;`；execute_tool 新增 `"warm"` 分支（读 args.lang / timeout_secs 默认 30） |
| `crates/cli/src/main.rs` | Cmd 新增 `Warm { lang, timeout_secs }` 子命令；forward() 新增 `("warm", json!({"lang","timeout_secs"}))` 转发分支 |

零改动：lsp-core / daemon / Cargo.toml / 既有公共 API / wire 错误模型（NotInstalled、BadArgs 走既有 9 错误码形态）。

## 【验证命令 + 关键输出】

**Task 1 门（build + clippy）**
```
cargo build --workspace    → Finished `dev` profile ... 0 errors
cargo clippy --workspace --all-targets -- -D warnings → Finished, 0 errors
```

**Task 2 门（单测）**
```
cargo test -p supervisor warm
  test warm::tests::entry_file_matches_by_language_id_table ... ok
  test warm::tests::find_entry_file_finds_rust_source       ... ok
  test warm::tests::warm_rejects_lang_without_source_files  ... ok   (BadArgs 且不 spawn LS)
  test warm::tests::warm_partial_on_zero_timeout            ... ok   (timeout=0 → ready:false + partial:true)
  test warm::tests::warm_ready_then_first_overview_nonempty ... ok   (真 RA：warm ready:true → 首个 overview 非空)
  test result: ok. 5 passed; 0 failed

cargo test -p cli -p supervisor → 全绿：supervisor lib 121 passed（含 warm 5）+ 全部集成测试 bin，0 failed（无回归）
```

**Task 3 e2e（bash 全重定向 + `time`；每段前 stop-all + 轮询 daemon 死亡避免陈旧 lock 竞态）**
```
=== COLD: 全新 daemon 首个 overview ===
real 0m0.838s  → 615 bytes（add/multiply 两符号，非空）
=== warm rust ===
real 0m0.857s（承担整个冷启动：lazy-spawn daemon + RA + 索引确认）
{"elapsed_ms":170,"lang":"rust","partial":false,"probe_file":"lib.rs",
 "project":"D:\\Project\\serena-rust\\fixtures\\rust_demo","ready":true}
=== WARMED: 立即 overview ===
real 0m0.044s → 615 bytes 非空        （19× 加速，T_WARM < T_COLD/2 达标）
=== 重复 warm（幂等秒回）===
real 0m0.065s → {"ready":true,"elapsed_ms":3,...}
=== 清理 ===
stop-all → daemon fully dead
```

**acceleration**: 838ms → 44ms（19×，要求 <2×）✓

## 与 plan 的偏离（plan 与实际不符以实际为准）

1. **warm 落点 CLI→supervisor**：plan 写 CLI 侧 `handle_warm` + `build_supervisor`。实际 CLI 默认是转发模式——CLI 进程内建 Supervisor 预热的是 CLI 自己的 LS，进程退出即死，预热不了 daemon。改为 supervisor 工具（`warm.rs` + execute_tool `"warm"` 分支），CLI 只转发——与 B(edit-context)/E(repo-map) 特性同款落点，daemon HTTP 层通用 dispatch 零改动。
2. **子命令形态**：plan 草稿变体内 `#[arg(long)] lang/project`。实际遵循 CLI 既有惯例：全局 `--project` + 位置 `lang`，即 `cli --project X warm rust`。注意 plan e2e 写法 `cli.exe warm rust --project rust_demo` 实测被 clap 拒（exit 2，`--project` 非 global arg 必须在子命令前）；plan 的 `overview --project rust_demo lib.rs` 同样不成立，正确语法已实测验证。
3. **就绪判定**：plan 的 `Session::is_ready()` / generation 推进方案不存在；按派单采用 busy-retry 真语义查询（documentSymbol 非空 = overview 就绪语义，即 K 特性「二调出数」的出数判据）。
4. **返回字段**：按派单 `{lang, project, ready, elapsed_ms, partial}`，另加 `probe_file`（1 字段，定位探针文件，单测/e2e 均消费）。
5. **shell JSONL 入口未加 warm**：shell 白名单本就缺 edit-context/repo-map（B/E 先例），跟随惯例未动；shell 用户可经 daemon HTTP `POST /tools/warm` 使用。

## 验收标注（1 项）：「warm 后立即 refs → 非空」

实测结论：refs 首调空是 **RA `references` 请求级的既有首调窗口**，与 warm 无关、warm 原理上无法消除：

```
hover main.rs 0:3（同文件）         → 首调即有数据（"fn add" range 3-6）✓
overview main.rs 预热后 refs 首调   → {"items":[],"raw_count":0}（仍空！）
同一会话 refs 二调                  → 2 items（main.rs:4:13 调用点 + 1:4 定义）✓
```

即 documentSymbol 级探针预热不能前置 RA 的 references 分析（overview 预热过也照空）。零引用符号返空是正确语义（lib.rs 的 add 本就无引用），不能也不应被 warm「修掉」。warm 的就绪判定按 plan Task 1 原文采用 overview/documentSymbol 语义；§13-M 的主验收「warm 后首个 overview <1s 热路径」以 44ms 达标。若后续要消除 refs 首调窗口，需在 refs 工具层做 references 请求级 busy-retry（属 refs 工具任务，非 warm 范围）。

## code-simplifier 自检

改动 3 文件 / 触碰禁区 0（lsp-core、daemon、Cargo.toml、测试断言、错误信息字符串均未动）/ 新增 ~250 行（新功能本体，无清扫对象）。循环内 request Err → 250ms 重试不吞错（对齐 wait_for_index 回退契约）；探针超时 → `partial:true` 显式上报不假装 ready；无源文件 → BadArgs 显式拒绝不静默空 warm；无源文件检查先于 session_for，防错误 lang 白烧 LS spawn。

## 自我评估

- 准确性 5/5 — 全部声明有命令输出佐证（warm.json 报文、time 计时、121+5 测试结果）；每条偏离经实测验证
- 完整性 4/5 — 扣分：shell JSONL 入口未加 warm（跟随 B/E 既有先例，偏离节已标注）；refs 验收项以证据标注为非 warm 层问题
- 清晰度 5/5 — 偏离 5 条逐条给出实际形态与理由；refs 窗口附完整证据链
- 可执行性 5/5 — 复现命令齐备（语法 `cli --project X warm rust`、门命令、e2e 序列）
- 简洁性 4/5 — 报告偏长，但偏离清单与 refs 证据链均为必要交付

## 沉淀

`stop-all` 后 reaper 异步删 lock，立即发起下一 CLI 调用会撞陈旧 lock 拿到 exit 3 的静默假象（表象酷似新命令 bug）——e2e 序列必须在 stop 后轮询 daemon 死亡再继续。本次仅写入报告踩坑节供上级分流（daemon 测量纪律已有记忆条目部分覆盖）。
