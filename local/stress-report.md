# serena-rust 全功能压测报告（AI-token 12 特性 · S1-S8）

VERDICT: FAIL

唯一失败项：S8 的 I 腿（`search --comments-only`）在锚点 a8a244d **未实现**（计划文档存在、代码不存在，非回归）。其余 S1-S7 全部 PASS。

- 锚点：a8a244d（`git log -1` 验证，HEAD 一致）
- 二进制：`target/release/cli.exe` 在位；daemon 单实例 `127.0.0.1:7860`（lock `%LOCALAPPDATA%/serena/daemon.lock`，`X-Serena-Token` 鉴权）
- 压测脚本：`/tmp/sstress2/`（Windows: `C:\Users\Begonia\AppData\Local\Temp\sstress2`），未进 git；fixtures 零改动（`git status --short fixtures/` 为空）
- 测量纪律：全部 CLI 调用完整文件重定向（python subprocess，stdout+stderr→文件，无管道）；HTTP 走 urllib；无 powershell 管道截断
- 调用量：约 360 次 CLI 调用 + 50 个 batch（10×32 顺序 + 40×32 并行 = 1600 次批内工具执行）+ 校准约 30 次

---

## S1 顺序耐力 — PASS

- 命令：`python /tmp/sstress2/s1.py`（6 工具 × 20 轮 = 120 调用；工具 overview/hover/def/refs/find-symbol/symbol-body，全 --json，rust_demo）
- 结果：**0 意外失败**；每轮 6 调用中位延迟第 1 轮 16.0ms、第 20 轮 17.5ms，**退化比 1.094 ≤ 2**
- 轮中位序列（ms）：16.0, 17.5, 18.0, 16.5, 17.5, 17.5, 17.5, 17.0, 16.5, 15.5, 17.0, 16.0, 16.5, 18.5, 15.0, 19.0, 18.5, 16.0, 17.0, 17.5

## S2 batch 极限 — PASS

- 命令：`python /tmp/sstress2/s2.py`（POST /batch，port/token 读自 lock 文件）
- 顺序：10 轮 × 32-item 批（slot 7 = 未知工具、slot 23 = overview 缺 file 投毒）
  - 每轮 HTTP 200、`results` 长度 32、`results[i].tool == calls[i].tool` 全对、恰好 slot 7/23 `ok:false`（均 BAD_ARGS：`unknown tool` / `missing args.file`），其余 30 全 `ok:true` 且带真实 value
  - 批耗时 30ms → 24ms（无退化）
- 并行：5 轮 × 8 并发批（每批 32 item，坏槽 = 批号，跨批唯一）：**无挂死**（join 全部及时返回）、**无错位**（顺序+ok 模式逐批校验通过）
- 内容抽查：`s2_seq_r01.json` results[0] 含真实 overview items（add/multiply），非空壳

## S3 delta 循环 — PASS

- 命令：`python /tmp/sstress2/s3.py`；副本项目 `/tmp/sstress2/s3proj`（lib.rs 加 1 行注释使 delta hit-key 与 rust_demo 键天然不撞；fixtures 不动）
- 协议：20 轮交替追加/删除 `pub fn delta_probe_i(a,b) -> i32 { add(a, b) }`，每轮后 `refs lib.rs 1 7 --delta --json`
- 结果：**21/21 轮记录 ok，added/removed 与 ground truth 逐轮精确一致**（按 `file:line:col` 集合相等，无多余项），无需重试窗口
- 受控实验（`cx.py`）：prime → 加 → `added=[lib.rs:9:27]` 精确；删 → `removed=[lib.rs:9:27]` 精确
- 观察记录（非判据）：一次性观察到"原位重写文件 + delta 基线经 --delta 存入后，连续 59 次 delta 调用（90s）报 delta:true 空差异"的疑似陈旧窗口；随后受控复现（cx 实验）立即正确，未能复现。诊断过程与探针文件保留在 `/tmp/sstress2/out/`（s3_diag/s3_base/cx_*）

## S4 budget 边界 — PASS

- 命令：`python /tmp/sstress2/s4.py`；大输出目标 = 本仓库 `refs crates/supervisor/src/lib.rs 193 13`（trait `execute_tool`，15 refs；先 warm + settle，测量矩阵恰 3 次调用）
- 结果（3 次全部 **exit 0**）：

| --max-tokens | truncated | items | raw_count | original_count |
|---|---|---|---|---|
| 0 | true | 0 | 15 | 15 |
| 1 | true | 0 | 15 | 15 |
| 100000 | （键缺席） | 15 | 15 | （键缺席） |

- `truncated:true` 仅在真超出时出现；未超出时键缺席（**无假阳性**）。`original_count` 在截断时保持原数。0/1 token 预算截到 0 items 符合 4B≈1token 粒度

## S5 warm+重启 — PASS

- 命令：`python /tmp/sstress2/s5.py`
- warm ×20 幂等：全部 `ready:true, partial:false`，elapsed 16–89ms
- stop-all → 死亡轮询 → 重建 ×5 循环：每循环 `stop rc=0`、进程归零（count_after_stop=0）、**lock 文件已删（无僵尸锁）**、重建 warm `ready:true`、**warm 后首个 overview 14–18ms（< 1s）**、终态恰 1 daemon
- 重启把 RA 子进程一并回收（见 S7 归因）

## S6 冷启动并发 — PASS

- 命令：`python /tmp/sstress2/s6.py`（stop-all 验死亡后，6 个 overview 进程同时 Popen）
- 结果：6/6 `rc=0` 且输出合法 JSON，wall 632ms，**exit-3 风暴 = 0**，终态恰 1 daemon，`status` rc=0
- 无 daemon 时的 6 路 lazy-spawn 竞争被锁仲裁干净收敛（胜者为 daemon，败者 join）

## S7 泄漏哨兵 — PASS

- 采样：S1 基线 + 每轮 + 每阶段后（`snap.py` → events.jsonl）+ 终态空闲 60s×3
- cli.exe（daemon）RSS 序列：

| 时点 | RSS (MB) | 备注 |
|---|---|---|
| S1 前 | 13.77 | pid 2348 |
| S1 后 | 13.84 | 120 调用后 |
| S2 后 | 14.68 | +1600 批内执行 |
| S3 后 | 14.83 | +s3proj 会话 |
| S4 后 | 15.87 | +self-repo 会话 |
| S5 后 | 12.47 | 新 daemon（重启重置） |
| S6 后 | 11.60 | 新 daemon |
| S8 后 | 13.51 | |
| 空闲 60s ×3 | 13.475840 ×3 | **逐字节相同，零爬升** |

  - 全程增幅峰值 +2.1MB（≈15%），远低于 50%/100MB 上限；空闲无单调爬升
- rust-analyzer 子进程归因（CIM 父进程链）：daemon 独有 1–3 个随项目会话生灭；**stop-all 后 daemon 子 RA 全部回收**（final 仅剩用户 IDE "Trae CN" 的 2 个实例，07:49:51 创建、早于本压测 daemon 08:04:16，归因排除）
- **终态 cli.exe = 0** ✓

## S8 冒烟 — FAIL（I 腿未实现）

- K/H/B/E/A/C 各 ×20 循环（rust_demo，全 --json）：**6×20 = 120 循环 0 失败**，形状判据：

| 腿 | 工具 | 形状断言 | ×20 结果 |
|---|---|---|---|
| K | find-symbol | `{compact:true, items:[[name,"f:l:c"]], raw_count≥1}` | 0 坏 |
| H | refs（compact 缺省） | items 全部匹配 `.+:\d+:\d+$` | 0 坏 |
| B | edit-context | 六键齐 + `body.text` 非空（body 为 `{start_line,end_line,text}` 对象） | 0 坏 |
| E | repo-map | `total_symbols≥1, 1≤len(top)≤10, budget_bytes≥1` | 0 坏 |
| A | search | `hits[0].symbol` 非空 + `file/line/text` 键 | 0 坏 |
| C | find-referencing-symbols --grouped | `group_count==len(groups)`, 每组 `samples≤3` | 0 坏 |

- **I（search --comments-only）：未实现**
  - 证据 1（复现命令）：`./target/release/cli.exe --project D:/Project/serena-rust/fixtures/rust_demo search TODO --comments-only --json` → **×20 全部 rc=2**：`error: unexpected argument '--comments-only' found`
  - 证据 2：`grep "comments_only|comments-only" crates/` → **0 命中**（supervisor 无 `SearchArgs.comments_only`、execute_tool 无该分支、CLI 无该 flag）
  - 证据 3：计划文档 `local/plan-i-comments-only.md` 在 local/ 存在，但 HEAD a8a244d 无对应实现 —— **计划≠实现**
  - 根因定性：特性缺口（未实装），非行为回归。若要 S8 全过，需先实现 supervisor `tool_search` 的 comments_only 过滤 + CLI flag 透传（本票只测不修，未动代码）

---

## 失败汇总

| 判据 | 结果 | 失败项 |
|---|---|---|
| S1 | PASS | — |
| S2 | PASS | — |
| S3 | PASS | — |
| S4 | PASS | — |
| S5 | PASS | — |
| S6 | PASS | — |
| S7 | PASS | — |
| S8 | **FAIL** | I 腿：feature 未实现（证据见上） |

## 观察与建议（非判据，只测不修）

1. **`warm ready:true` ≠ 请求级就绪**：S4 首探 refs 返回 0（RA 索引窗口），S3 曾见 90s 级 stale-delta 观察。建议 warm 的 probe 改用 refs 类探针，或在文档标注 warm 就绪语义为"进程级"。
2. **truncated 键缺席语义**：未截断时 `truncated`/`original_count` 键整个缺席。调用方需按"缺席=未截断"判断；建议显式回 `truncated:false`。
3. J delta 空基线健壮性实测通过（空集不缓存 → 冷窗口期重复调用始终返全量，基线不被污染）。
4. 压测产物：`/tmp/sstress2/out/`（逐调用原始 JSON + events.jsonl），验收后可整目录删除。

---

# S8-I 复验（2026-09-22，cherry-pick 04f68d4 回线 + release 重建后）— PASS

- 复验范围：仅 S8-I 腿（S1-S7 未重跑，维持原 PASS）。二进制 `target/release/cli.exe`（serena-cli 0.1.0，含 I），daemon :7860。
- 命令形态：`cli.exe --request-timeout 60000 --project <root> search <pattern> --comments-only --json`，输出/stderr 完整重定向到文件后逐调用断言。脚本 `/tmp/s8i/loop_s8i.py`，正式 20 循环零重试。
- 判据 ×20（rust_demo ×10 阴性 + 本仓库 ×10 阳性，warm daemon）：

| 腿 | 项目 | pattern | rc | 形状（恒在） | wall | 结果 |
|---|---|---|---|---|---|---|
| 阴性 | fixtures/rust_demo | `fn` | ×10 全 0 | `{files_scanned:3, hits:[], truncated:false}` | 15–22ms | **10/10** |
| 阳性 | 本仓库 | `KILL_ON_JOB_CLOSE` | ×10 全 0 | `{files_scanned:343, hits:12, truncated:false}` | 54–67ms | **10/10** |

- **clap rc=2 绝迹**：旧失败形态 `error: unexpected argument '--comments-only'` ×20 全部不再复现，flag 解析+透传（main.rs:159→lib.rs:4366）+ 过滤（lib.rs:4370 retain looks_like_comment）全链路活体可用。
- **阴性 = 过滤器所为，非 pattern 落空**（不带 --comments-only 对照，bash 直跑）：`search "fn"` raw = 4 hits（`pub fn add/multiply`、`fn add/main` 全代码行，symbol=add/multiply/add/main）→ filtered = 0。rust_demo 零注释（grep `//` 0 命中），恒空 hits 是正确语义。
- **阳性逐 hit 断言 ×10**：12 hits 全部命中行 `lstrip()` 后 `//` 前缀（`///` 文档注释与 `//!` inner doc）；`symbol`/`container` 键 ×12 恒在（A 特性组合照常，10/12 symbol 非空、7/12 container 非空——文件顶部 `//!` 无 enclosing symbol 为 null，合理）；20 次调用 hits 集合逐字节稳定。
- **过滤器非空转（阳性对照，bash 直跑）**：`KILL_ON_JOB_CLOSE` raw = 16（12 注释行 + 1 代码行 `limit.limit_kill_on_job_close();` + 3 其他）→ filtered = 12 恰为注释行。
- **结论**：S8-I 判据全过；S8 唯一失败腿翻绿 ⇒ S1–S8 全 PASS。

## 复验观察（非判据，只测不修）

1. **测量通道楔死实锤（新踩坑，已沉淀 `~/.omp/agent/rules/e2e-cli-lsp.md`）**：v1 循环脚本用 python `subprocess.run(capture_output=True)`（PIPE 捕获）做 stop-all 后首个冷启动调用 → **>9min 不返回**；楔死期间 client 进程已消失、仅剩 detached daemon（LISTENING :7860，对新请求 0.5s 正常响应）——python 在等永不到来的管道 EOF。同形态对照：bash 文件重定向 stop-all 后冷启动 0.74s rc=0。**CLI/daemon 零缺陷嫌疑，系测量方法**；lazy-spawn 型 CLI 的 e2e 测量禁 PIPE 捕获（与旧 powershell EPIPE 教训同族），v2 改文件重定向 + 90s kill-switch 后 20/20 秒级通过。
2. **`truncated` 语义（lib.rs:2542,2668）**：指 raw 命中数触及 `max_results`（默认 100，先计数后过滤），且扫描提前 break（files_scanned 同步截断）。超常见词（"the"：42 文件即满 100 raw；"语义"：88 文件满）在 comments-only 下也会 `truncated:true`——判据 `truncated:false` 必须配 raw<100 的 pattern（本次 `KILL_ON_JOB_CLOSE` raw=16，全仓 343 文件扫描）。调用方解读：`truncated:true` + 少量 hits = "还有更多 raw 命中被注释过滤掉"，非误报。
3. **行尾注释被过滤**：`let x = 1; // 注` 型命中被丢弃（looks_like_comment 仅认行首 `//`/`///` 前缀），与 P0 "/// 或 // 前缀" 判据一致，如实记录。
4. 压测产物：`/tmp/s8i/`（loop_s8i.py、loop_out2.txt、out/ 逐调用 JSON + .err + 探针对照件）。
