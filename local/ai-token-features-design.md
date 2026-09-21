# AI-token 特性集设计（方案三：薄增强 ×N + 聚合 ×1 + 地图 ×1）

> 日期：2026-09-21 ｜ 状态：待用户审查
> 来源：本会话实测 token 数据 + 竞品调研（Aider/Cursor/Sourcegraph/opencode/repomix/Continue）
> bd：8gz(A) 6x5(B) gej(C) bqc(F2) d1d(E) i0l(G)

## 0. 目标与原则

**目标**：AI agent 用 serena-rust 写代码时，更省 token、更高准确率。

**原则**（全特性共用）：
1. 增量兼容——新字段/新 flag/新命令，不改既有输出形状（现有 skill/脚本零迁移）
2. wire 契约不动——HTTP 200 + 9 错误码；新错误一律走既有 BadArgs/NotFound 形态
3. 三入口统一——CLI 子命令 / daemon HTTP / shell JSONL 同步透传（先例：completion）
4. 降级优雅——LS 未就绪/无会话时新字段返 null 或省略，不因增强失败丢基础结果

## 1. A：search 输出标注所属符号（bd 8gz）

**命令**：`search`（无新参数，输出加字段）

**改动**：`SearchHit` 增加可选 `symbol` / `symbol_kind` / `symbol_range`。

**实现**：按命中文件分组 → 每文件一次 `textDocument/documentSymbol`（Phase 3.1 缓存兜住同文件多命中）→ `collect_containing_hits`（lib.rs 已有）取命中位置最深包含符号。

**降级**：无 LS 会话 / documentSymbol null → `symbol: null`，命中照常返回。

**验收**：fixtures/typescript_demo 注释命中标注正确；与 `containing-symbol` 工具结果一致；测试全绿。

## 2. B：edit-context 编辑上下文聚合（bd 6x5）

**命令**：`edit-context <file> <symbol> [--max-callers 20] [--json]`

**输出**（一次调用四段）：
```json
{
  "symbol": "shutdown_post",
  "signature": "async fn shutdown_post(...)",
  "body": "……",                        ← symbol-body 逻辑（documentSymbol 定位 + 切片）
  "doc": "/// …",                       ← hover 逻辑
  "callers": [{"file","line","snippet"}], ← find-referencing-code-snippets 逻辑
  "callers_truncated": false,
  "tests": [{"file","line"}]            ← search 正则，glob 限 **/test*/** 与 *_test.* / test_*
}
```

**实现**：supervisor `execute_tool` 新分支，内部串 4 个既有 tool_*，无新 LSP 请求形态。callers 空集 → `[]`（合法）。全走既有缓存。

**验收**：fixtures modify multiply 场景一次调用拿全；输出 bytes ≤ 手动 4 调用总和 70%；测试全绿。

## 3. C：refs --grouped 分组翻页（bd gej）

**命令**：`refs` / `find-referencing-symbols` 加 `--grouped --offset N --limit M`

**输出**（--grouped 时）：`{total, by_file: {file: count}, sample: [前5条完整 Location], expand_hint}`

**兼容**：不传 `--grouped` 输出逐字节不变；`--offset/--limit` 两种形态均生效。

**验收**：≥3 文件引用场景分组正确；回归确认默认形态不变。

## 4. F2：编辑回执自带诊断（bd bqc，竞品 Top2 opencode 模式）

**改动**：写类工具（`replace-body` / `rename-symbol` / `safe-delete` / `replace-lines` / `delete-lines` / `insert-text-*` / `delete-text-in-symbol`）返回值增加 `post_diagnostics` 字段。

**实现**：写工具已有「等诊断代际推进」机制（DiagCache + diag_generation，2s 超时容忍）；把等待后的该文件增量诊断直接挂回执。超时/无诊断 → `post_diagnostics: []`。

**价值**：AI 改完一次调用即知「改对没」，省掉跑 build → 读报错整轮（数千 token）。

**验收**：mock_ls 单测覆盖改对/改错两态（改错时 post_diagnostics 非空）；不破坏既有回执字段。

## 5. E：repo-map 全库符号地图（bd d1d，竞品 Top1 LSP 版 Aider）

**命令**：`repo-map [budget_tokens=1024] [--focus <file>]`

**算法**：
1. 符号提取：workspace 全量文件 documentSymbol（有 Phase 3.1/3.3 缓存）+ refs 计数（或 documentHighlight 轻量计数）
2. 二部图：引用文件 → 定义文件，边权 `sqrt(引用次数)`，符号被 focus 文件引用 ×50、公开名（非下划线开头）×10
3. personalized PageRank 幂迭代（~50 行，focus 文件为种子；无 focus 均匀种子）
4. token 预算二分：取前 N 个 ranked 定义的签名行，渲染后估算 token，误差 <15% 提前停
5. 输出：`{budget_used, files_covered, map: [{file, line, signature}]}` 按文件分组

**缓存**：图与排序结果按 root_source_mtime 信号缓存（复用 find-symbol 缓存键模式）。

**验收**：本仓 850 文件实测输出 ≤ budget；二次调用带缓存 <1s；fixtures 手工核对排序合理性（被 refs 最多的符号应靠前）。

## 6. G：全局 --max-tokens 预算护栏 + --compress（bd i0l，repomix 模式）

**改动**：context 类工具（refs / find-symbol / overview / search / symbol-tree / semantic-tokens）统一支持 `--max-tokens <n>`：输出超预算即截断，附统一标记 `{truncated: true, expand_hint: "--offset/--limit"}`；CLI 退出码不变（截断是成功语义，不是错误）。

**--compress**：refs / find-referencing-code-snippets 只回符号签名行（去 snippet body）。

**兼容**：不带 flag 逐字节不变。

**验收**：≥3 工具生效、截断标记统一；回归默认形态不变。

## 7. 实施顺序与依赖

| 序 | 特性 | 理由 | 依赖 |
|---|---|---|---|
| 1 | F2 | 最易+ROI 立竿见影（纯接线） | 无 |
| 2 | B | 主路径聚合（doc 段独立于 A，A 是检索标注） | 无 |
| 3 | E | 价值最大、工时最长 | 复用 3.1 缓存 |
| 4 | A | 薄增强 | 无 |
| 5 | C | 薄增强 | 无 |
| 6 | G | 收尾护栏（给前面输出统一加预算） | 在 A-E 后做避免返工 |

A/B/C/E 相互无硬依赖，可并行派单；G 最后收口。

## 8. 明确不做（YAGNI）

- embedding 向量索引（Cursor 路线）：需模型+向量库+同步基建，与本地 CLI 定位冲突
- 子代理隔离检索：agent 框架层职责，非 CLI 工具层
- 任务枚举式元命令（ctx --task "modify"）：edit-context 单命令已覆盖最高频场景，防枚举爆炸

## 9. 风险

| 风险 | 缓解 |
|---|---|
| repo-map PageRank 在 850+ 文件上的性能 | 图构建全走缓存；幂迭代 O(边数×迭代数)，预算二分 ≤10 轮渲染；先本仓实测 |
| edit-context 内部串 4 工具的失败语义 | 任一段失败整体 fail-fast 走既有 9 错误码；callers/tests 段允许空集不失败 |
| F2 挂诊断拖慢写工具 | 复用既有 2s 诊断等待超时，超时返 [] 不阻塞 |
| 新字段撑爆小结果 | G 的预算护栏最后收口统一兜底 |
