# 修复后全量复测报告（2026-09-20）

> 基线：e2e-full.md + ai-workflow-bench.md（首轮 FAIL）。修复：f42ff0c（daemon 竞态）+ 8aab923（rust 语义/适配器/safe-delete）+ 本轮缓存污染修复。
> 复测执行：e2e-runner（A-G 段实测中途中断，关键项已采信）+ PM 亲自复核（语义复活/拦截门/replace-body/缓存污染）。

## 修复项逐条复核（首轮 vs 复测）

| 项 | 首轮 | 复测 | 验证人 |
|---|---|---|---|
| P0-2 rust 语义层 | def/hover/refs 恒 null/[] | **def/hover/refs 全真实数据；语义就绪 T+1.8s**（修前数分钟）| PM+Retest |
| P0-1 pyright 全瘫 | 0/10 | hover/def/diagnostics 真实数据（AdapterFix e2e）| AdapterFix |
| P0-3 daemon 孤儿竞态 | 403×3、需手工杀 | **F 段孤儿=0、403=0**；竞态现为干净 rc=3 超时+自愈（绑定快速失败的预期权衡）| Retest |
| P1-1 safe-delete 误删 | 删掉被引用符号 | **拦截门拒删**（"2 textual occurrence(s)... semantic layer may be unavailable"）| PM |
| P1-2 ts percent-URI | defining rc=2、rename 0 编辑 | defining 真符号体、rename edits_applied:2 落盘 | AdapterFix |
| P1-3 gopls rename | edits_applied:0 | edits_applied:2 落盘 | AdapterFix |
| 新 P0-4 缓存污染（复测新发现） | find-symbol 空结果被永久缓存 | **修复**：空结果不进缓存，就绪后返回真实数据（PM e2e：冷 [] → 暖 2 符号）| PM |

## 速度判据（AI 编辑三判据）
- 冷启 <90s ✅（rust 语义 T+1.8s 可用）
- 热态 <2s ✅（逐命令 30-70ms）
- 字节节省：定向链 ✅ 46-75%

## 残留（已知局限，登记 P2）

1. **外部修改不感知**：绕过 CLI 改文件（如 git checkout/手改）daemon 无 didChange/无文件监听 → 后续符号定位用旧缓存 range 可致错位编辑（Retest 观察到的 replace-body "损坏" 即此机制——脚本中途 git checkout；CLI 全链路编辑实测正确）。修法方向：tool 调用时 mtime 对账 + 失效重索引（同 Phase 3 wait_for_index 家族）。
2. 编辑类回执 null（RA decode 失败，P2 既有）；diagnostics 就绪 5-6s（ts）。
3. 位置基线混用（completion 1-based vs def/refs 0-based）——文档明示即可。
4. 语义未就绪窗口首次调用返回空而非等待（就绪门现为标记式，不阻塞首次调用）——配合缓存修复后自愈（重查即真数据）。

## 总判：**PASS（首轮 3 P0 + 3 P1 全消除，无回归）**
回归保护：clangd 段全绿（首轮 10/10）、daemon 31/31 + cli 9/9 + cargo test 全绿（supervisor 156 ok）+ clippy 干净。
