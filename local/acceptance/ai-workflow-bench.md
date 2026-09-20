# AI 编辑效率实测报告（S1-S5）

**VERDICT: conditional fail** —— 语法层编辑链（overview→symbol-body→replace-body）快速且跨语言可靠（冷启 0.7-2.2s，热态 <50ms，判据全过）；但**语义层验证链不可用**：rust 适配器语义层在 daemon 下永久不就绪并引发 safe-delete 误删事故，TS rename 静默零编辑 ×2 轮，daemon 生命周期缺陷（孤儿 daemon 403）本轮 3 次复现。字节判据不达标（小文件上 serena JSON 输出膨胀 3-13×，定向编辑链反省 46-75%）。

- 日期：2026-09-20
- 二进制：`target/release/cli.exe`（**注**：任务指定的 `cli_run_bin.exe` 仅 6 个子命令 doctor/install-all/install/ext-detect/read-file，不含符号工具链，S2-S4 无法在其上执行；主 cli.exe 承载全部 40+ 子命令，实测正常）
- 环境：Windows 11 / i9-10900F，rust-analyzer 1.95.0（rustup），typescript-language-server（npm 全局）+ typescript 5.x（bench 本地）
- 测试对象：`local/acceptance/bench/{rust_demo,ts_demo}`（fixtures 副本，原件未动）
- 计时口径：bash `date +%s%N` 差值（wall time，含 CLI 进程 spawn）；字节口径：stdout+stderr 合流 `wc -c`（AI agent 实际接收量）
- 编辑任务：rust —— 修 `fn add` 的 `a + b + 999` → `a + b`；TS —— 给 `add` 加有限数校验抛错

## S1 冷启动（daemon lazy-spawn + LS 启动 + 首查询）

| 语言 | 耗时 | 输出字节 | 结果 | 备注 |
|---|---|---|---|---|
| rust | **0.734s** | 638B | 成功 | rust-analyzer spawn+documentSymbol；历史 89s 为缺 Cargo.toml 时代，现 fixture 有独立 workspace |
| ts | **2.156s** | 960B | 成功 | 首跑失败：fixture 缺 typescript 依赖（tsserver initialize 报错，37B）；`npm i typescript` 后通过 |

判据「首次 <90s 可接受」：**两者优**。

## S2 热态全链路 overview→find-symbol→symbol-body→replace-body

| 步骤 | rust 耗时/字节 | ts 耗时/字节 |
|---|---|---|
| overview | 36ms / 638B | 35ms / 960B |
| find-symbol `add` | 45ms / 633B | 47ms / **3B（空）** |
| symbol-body | 30ms / 48B | 36ms / 76B |
| replace-body | 38ms / 5B | 48ms / 5B |
| **合计** | **149ms / 1324B** | **166ms / 1044B** |

两次编辑均真实落盘且内容正确（`+999` 移除；校验函数体 8 行替换成功）。判据「热态逐命令 <2s 优」：**优**（最大 48ms）。注意 find-symbol（workspace/symbol）在 ts 侧热态仍返回空——语义层未就绪的先兆。

## S3 热态验证链 refs→safe-delete-symbol→diagnostics

| 步骤 | rust 耗时/字节 | ts 耗时/字节 |
|---|---|---|
| refs（定义位置） | 44ms / **3B（空！）** | 72ms / 503B（def+调用点，正确） |
| safe-delete-symbol | 48ms / 61B / **deleted:true（误删！）** | 45ms / 174B / deleted:false（正确拒删+列出 L10 引用） |
| diagnostics | 40ms / 18B | **5448ms** / 18B（第二跑 6124ms） |

- **rust 侧事故**：refs 对 `add` 返回 `[]`（main.rs L2 有 `add(1,2)` 调用），safe-delete 依据空 refs 判"无引用"**直接删除了有引用的函数**，文件被改坏（悬空调用）。已人工回滚。干净重启 daemon 后 hover/refs/def/find-symbol 持续为空（等白数分钟），定性：**rust 适配器语义层在 daemon 环境下不就绪**（documentSymbol 语法层正常，workspace 加载疑似失败）。
- ts 语义层就绪延迟 ~20-25s：T+20s hover=null → T+25s refs 正常。就绪后 refs/safe-delete 行为正确。
- 判据「热态逐命令 <2s 优」：ts diagnostics **不达标**（5.4s/6.1s，两跑一致，含 project 切换混杂）。

## S4 rename-symbol 全链路

| 语言 | 耗时/字节 | 结果 |
|---|---|---|
| rust | 38ms / 107B | `RPC_ERROR: No references found at position`（语义层死派生；显式报错） |
| ts | 63ms / 63B | **`edits_applied: 0, rc=0` 静默零编辑**；重启 daemon 后复现；同位置 def/refs 正常 → 定位到 edit 落盘阶段丢失（refs/safe-delete 输出中 uri 为 `d%3A/...` 冒号误编码，嫌疑） |

判据「热态逐命令 <2s 优」：耗时达标，但**功能失败**（一个报错、一个静默无操作）。TS 侧 grep 验证 `addExact` 0 命中。

## S5 传统对照（全文读字节为 token 代理）

| 任务 | 传统路径（读全文） | serena 完整发现链（S2 四步） | serena 定向链（symbol-body+replace-body，已知符号名） |
|---|---|---|---|
| rust 修 +999（99B） | 99B | 1324B（**13.4×**） | 53B（**-46%**） |
| ts add 校验（328B） | 328B | 1044B（**3.2×**） | 81B（**-75%**） |

判据「字节节省 ≥60% 优」：完整链**不达标**（小 fixture 上 JSON 输出超过文件本体）；定向链 rust 达 46%、ts 达 75%（ts 达标）。关键性质：传统路径成本随文件线性增长，符号链成本与文件大小基本无关——**优势在真实规模项目才兑现**。

## 瓶颈定位

| 候选瓶颈 | 实测 | 结论 |
|---|---|---|
| CLI 进程启动 | ~40-100ms/次 | 排除 |
| daemon RTT | 热态 30-72ms | 排除 |
| LS spawn+语法索引 | 0.7-2.2s 冷启动 | 不是瓶颈（documentSymbol 秒级可用） |
| **语义层就绪/同步** | rust：数分钟不就绪（疑似 workspace 加载失败于 daemon 环境）；ts：20-25s 就绪延迟 | **主瓶颈（正确性而非速度）** |
| **daemon 生命周期** | stop-all reaper 延迟删 lock 与下轮 lazy-spawn 竞态 → 孤儿 daemon 占 7860 → 后续全 403 | **次瓶颈（稳定性）** |

## 事故与缺陷记录（均有复现命令）

1. **safe-delete 误删有引用符号**（rust）：refs 空结果直接判"无引用"→ 删除。建议：语义层未就绪（hover/def 同查为空）时 safe-delete 应拒删降级，而非信任空 refs。
2. **rename-symbol 静默零编辑**（ts）：rc=0 + `edits_applied:0`，重启复现。嫌疑：edit 响应 uri percent-encode（`d%3A`）与落盘匹配失败。
3. **孤儿 daemon 403**（×3）：`stop-all` 后 lock 被 reaper 延迟删除，期间 lazy-spawn 的新 daemon 抢不到端口/token 失配，后续所有调用 403，需手工 taskkill。审计 P0（lock 仲裁）的活体链条。
4. doctor 误报 npm MISS（npm 11.12.1 在 PATH 且可用）。

## 结论（判据对照）

- 「首次 <90s 可接受」：rust 0.73s / ts 2.2s —— **过**
- 「热态逐命令 <2s 优」：S2 全部 <50ms —— **过**（S3 ts diagnostics 5.4-6.1s 不过）
- 「字节节省 ≥60% 优」：完整链不过，定向链 ts 过 rust 差 14pp —— **部分过**
- **AI 能否"快速进行编辑"**：能——前提是只走 documentSymbol 语法层工具（定位/取体/替换体/行级三件套）。任何依赖语义层的一步（find-symbol/refs/rename/可信的 safe-delete）当前在 rust 上不可用、在 ts 上需等 ~20s 且 rename 坏。速度不是问题，**语义层正确性是**。
