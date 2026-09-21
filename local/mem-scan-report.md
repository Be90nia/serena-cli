# 内存/拷贝/正确性审查报告（MemScan）

> 锚 c4f355ac4422b5715edcb48fa20be6a1a15c1953（2026-09-21）｜reviewer 只读审查
> 搭档报告：local/perf-scan-report.md（性能维度）

## VERDICT: 不可整体放行——1 P1 正确性缺陷 + 3 P2 内存债

对 12 项新特性（local/ai-token-features-design.md）的影响：
- finding 1（P1 诊断空 push）**阻塞 F2 及一切诊断类特性**
- finding 2/3 的模式会被新缓存/就绪等待类特性复制（先定缓存上限与驱逐策略）
- finding 4 会被大 payload 特性（repo-map E）放大
- 其余 P3 不阻塞

## P1：诊断缓存空 push 不清除（阻塞 F2）

- 位置：crates/supervisor/src/lib.rs:644-648（写侧）+ :782-786（读侧）
- 问题：publishDiagnostics handler 仅在 `!items.is_empty()` 时写 diag_cache，空推送不动作；
  tool_diagnostics push 路径直接读缓存。**用户修完所有错误后，LS 推空 items，旧错误永存
  直到 daemon 重启**。「缓存不得缓存空结果」模式对符号缓存无害，对诊断把『清除』变成不可
  观察事件。
- 影响：高（rust-analyzer 等未声明 diagnosticProvider 的 push-only LS 上 diagnostics 永久过期）
- 修法：空 push 时按 (root, uri) 删缓存条目并保留 generation++（就绪窗口由现有 5s 轮询
  present 判定兜底）；或缓存条目附 generation、读取时校验

## P2-1：symbol_cache 只失效不删除（无界增长）

- 位置：lib.rs:1372-1377
- 问题：失效靠「mtime 变 → key 变 → 自然 miss」，旧条目永不删除；find-symbol 缓存更糟
  （key 含 root_source_mtime，任一文件变更即孤立全部旧 query 条目），增长速率 = 编辑次数 ×
  query 数，每条携带整文件平铺 SymbolHit
- 修法：symbol_cache_put 前 retain 掉同 (root, file位) 旧 mtime 条目，或条目上限 + LRU；
  落成公共 helper 供新缓存复用

## P2-2：Session Arc 环残留世代泄漏 + 进度表无界

- 位置：crates/lsp-core/src/session.rs:241-248（handler 捕获 Arc<Session> 成环）+
  :259-263（progress_resolved 无消费者单调累积）+ :375 附近（注释依赖的 drop 永不发生）
- 后果：每次 evict/LRU/懒重启泄漏一个完整 Session 世代；rust-analyzer 索引/flycheck
  progress token 会话期单调累积
- 修法：handler 改 Weak<Session>（upgrade 失败静默返回）或 shutdown 清注册表断环；
  progress 表加显式清理并删错误注释

## P2-3：framing 无长度上限 + O(n²) 帧头重扫

- 位置：crates/lsp-core/src/framing.rs:121-130（decode）+ transport/stdio.rs:124-147（泵双层循环）
- 问题：Content-Length 无上限（异常 LS 可致 capacity overflow panic、会话僵死）；
  windows(4) 每 8KB chunk 从零重扫，5MB 级响应累计扫描 GB 级字节，泵 task 被饿死
- 修法：超上限（如 64MB）返 FrameError::TooLarge；记录已扫描偏移，body 累积期跳过重扫

## P3（不阻塞，随批处理）

| # | 问题 | 位置 | 修法 |
|---|---|---|---|
| 5 | execute_tool ~40 臂 typed→Value→HTTP 二次序列化 | lib.rs:3317+ | to_string 一次成型 |
| 6 | 微 clone 批：push_nested 死 clone / json! uri.clone() ~20 处 / search 逐条 clone rel_str / 首次尝试 clone params | lib.rs:2983-2990 等 | 逐点 move，一次小 PR |
| 7 | AppState.loaded_ls 生产死字段 | serve.rs:78-86 | 删 |
| 8 | evict 只清 2 表：load_gates/pull_diag_supported/diag_cache 单调累积 | lib.rs:416-423,446-448 | evict 顺带 remove 同 Key 三表；与 P1/P2-1 合并设计 |
| 9 | root_source_mtime 每请求全 root walk，命中也付；miss 时双遍 walk | lib.rs:156-165,1676,1690-1705 | 一次 walk 双产出 或 信号 TTL |

## 正面确认（无问题证据）

- 生产请求路径 panic 面干净（98 处 unwrap/expect 几乎全在 #[cfg(test)]）
- 错误路径所有权干净（ToolError/CoreError 无 Clone derive，wire 全按引用）
- Arc 无新环（AppState 无回边；六把锁细粒度、临界区无 await）
- docsync FileBuffer 不存文本，无泄漏
- required_*/json! 微分配与 LS 往返差 4-5 数量级，不值得修（与 PerfScan 结论一致）
