# serena-rust 全面验收总报告（2026-09-20）

> 三路并行验收：全命令 e2e（E2EFull，5 语言 × 98 项）+ AI 编辑效率（AIBench，S1-S5）+ 设计符合度（DesignAudit）+ PM 亲自复核。
> 分支 feature/solidlsp-phase0-1 @ 4705e7f。详报：e2e-full.md / ai-workflow-bench.md / design-conformance.md。

## 总判：**FAIL（功能面）/ 有条件通过（架构面）**

- **架构契约**：✅ 分层铁律 0 违规、9 错误码守恒（42 工具）、禁入依赖 0 命中、wire/lock/状态机符合设计
- **IDE 核心能力**：❌ 5 语言中 2 语言失去语义层（python 全瘫、rust 语义死）；rename 三语言不可用；safe-delete 有删码风险
- **AI 编辑效率**：✅ 速度全达标（冷启 0.7-2.2s、热态逐命令 <50ms）；❌ 语义层正确性缺陷拖累真实可用性

## 分语言能力矩阵（PM 复核后定级）

| 语言 | 语法层(overview/编辑) | 语义层(def/hover/refs/find-symbol) | completion | diagnostics | rename | 定级 |
|---|---|---|---|---|---|---|
| c/clangd | ✅ | ✅ 全绿 | ✅ | ✅ | ✅ edits_applied:2 落盘 | **A**（唯一全绿）|
| typescript | ✅ | ✅（需 ~20-25s 就绪等待）| ✅ 2526 候选/183ms | ✅（5.4-6.1s 偏慢）| ❌ 静默 0 编辑（URI 嫌疑）| B |
| go/gopls | ✅ | ✅ | ✅ | ✅ | ❌ 不支持 documentChanges | B- |
| rust/RA | ✅ | ❌ **null/[]（PM 亲测 20s 后仍死）** | — | ❌ | ❌ 同语义死 | C |
| python/pyright | ❌ LS 启动即退（缺 --stdio）| ❌ | ❌ | ❌ | ❌ | **D 全瘫** |

## P0（3 项，均有 PM/双 agent 复现证据）

### P0-1 pyright 启动缺 --stdio → python 全瘫
- 证据：pyright.rs:77-84 注释假设「pyright-langserver 无需 flag」实错；独立复现 `Connection input stream is not set` 立即退出
- 修法：一行（pyright-langserver 也必须 --stdio）

### P0-2 rust 语义层死（本项目主场语言！）
- PM 复核链：①daemon 模式 def/hover null、refs []，20s 后仍死 ②--direct 同样 null ③worktree 回退 d24322c 同样 null → 非回归 ④裸 RA 探针（initialize+didOpen lib.rs+等 15s）→ def/hover/refs **全部真实数据** ⑤RA 1.95.0、cargo check 过、workspace 健康
- 结论：RA 正常，**我们 supervisor/RA 交互路径缺陷**（首要嫌疑：请求前未 didOpen / $/progress 索引等待与新 RA 时序不匹配；smoke9 时代 PASS 系当时 fixture 无 Cargo.toml 走单文件模式的环境差异）
- 附带发现：语义未就绪时 CLI 捕获式调用可挂死 120s（无超时兜底）

### P0-3 daemon 孤儿竞态 → 403
- 证据：AIBench 3 次复现（stop-all reaper 延迟删 lock vs lazy-spawn 竞速 → 孤儿 daemon 占 7860，需手工 taskkill）；E2E daemon 生命周期段独立发现同族缺口
- 即审计遗留「lock 仲裁败者误删启动中 daemon 的锁」活体

## P1（3 项）

| # | 内容 | 证据 |
|---|---|---|
| P1-1 | **safe-delete 假阴性删码**：refs 返回空即放行，误删被 main.rs 引用的 add | E2E+AIBench 双复现，数据丢失向量；修法：refs 空+语义可疑时拒删（grep 交叉验证或错误码降级）|
| P1-2 | ts percent-URI：defining-symbol 实锤（%3A 未解码），rename 静默 0 编辑同族嫌疑（uri d%3A）| E2E B 段 |
| P1-3 | gopls rename 不支持 documentChanges → 失败 | E2E E 段；需 WorkspaceEdit 降级路径 |

## P2（4 项）
位置基线混用（completion 1-based vs def/refs 0-based，易误用）/ 编辑类回执 null（RA decode 失败）/ diagnostics 就绪 5-6s 偏慢 / format LF→CRLF 噪音。

## AI 编辑效率结论（AIBench）
- 冷启 0.73s(rust)/2.16s(ts)；热态全链路 149-166ms（定位→编辑→验证逐条 30-48ms）——**速度全优**
- 字节成本：定向链比传统读全文件省 46-75%；完整链反而膨胀 3-13×（需引导 AI 用符号级查询而非 overview 全量）
- 瓶颈不在速度在**正确性**：语义层不可信时 safe-delete/rename 会破坏代码

## 设计符合度（DesignAudit，详表见 design-conformance.md）
- 代码侧全符合；「唯一事实源」ARCHITECTURE.md 9 处过时（doctor/file_detect 整体缺席）+ coverage doc 严重落后（实为 47 子命令/11 适配器，文档 23/7）→ 需一次纯文档回写轮（14 条清单）

## 与「IDE 能力」的对照结论
IDE 四件套现状：语法定位/编辑 ✅ 已达 AI 编辑器水准；**代码提示** ts/go ✅；**代码纠错**（diagnostics+code action）c/ts/go ✅ 但 rust 无、python 无；**语义导航**（def/refs/hover）c ✅ ts/go ✅ rust ❌；**重构**（rename）仅 c ✅。距「全语言 IDE 级」差 P0-1/P0-2/P1-3 三步。
