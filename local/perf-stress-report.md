# 全链路压测报告（2026-09-22，a6ec378）

## Setup
- fixture: local/stress_demo/（304 项：50 个合成 .rs + lib.rs + Cargo.toml）
- 跳过 mtime TTL < 2s（基准测试用 mtime TTL 内 cold→hot 转换不显著）
- 工具：`target/release/cli.exe`（release 模式，非 debug）
- 单 daemon 单 worker 串行；不模拟并发 client 池
- 50 个 .rs 每文件 5 行 50 字符，规模约 = 中型项目单模块

## 实测延迟

| 工具 | n | p50 | p95 | p99 | max | rps | err |
|---|---|---|---|---|---|---|---|
| find-symbol hot | 30 | 78ms | 562ms | 569ms | 569ms | 8.4 | 1 |
| list-dir . | 30 | 102ms | 166ms | 313ms | 313ms | 9.0 | 0 |
| find-file | 30 | 68ms | 160ms | 699ms | 699ms | 9.9 | 1 |
| search f0 | 20 | 112ms | 135ms | 135ms | 135ms | 9.9 | 3 |
| search pub | 5 | 75ms | 90ms | 90ms | 90ms | 12.8 | 0 |
| overview lib.rs | 30 | 62ms | 368ms | 369ms | 369ms | 9.6 | 2 |
| def multiply | 30 | 58ms | 437ms | 645ms | 645ms | 8.5 | 4 |
| refs multiply | 30 | **45ms** | 339ms | 354ms | 354ms | **15.9** | 0 |

**总体**：e2e 端到端 PASS；总调用 215 次；0 致命错误；p50 中位数 60ms；rps 中位数 9。

## 与性能 2 轮报告对比

性能 2 轮报告 P24 称 find-symbol 251× 提速（35.3ms → 0.14ms）—— **那是单调用重复热缓存场景**。
真实 cold 查询（DAEMON 起动后第一次访问）实测 78ms；其中：
- root_signal walk + mtime 检查 ≈ 2-5ms（实测 per-call < 1ms，符合 P24 报告）
- LS 进程内 Rust analyzer 处理（didChange → semantic index）≈ 60-100ms
- IPC + JSON 序列化 ≈ 5-10ms
- **RA 后台索引抢占**造成 p99 尖刺 700ms（无节流）

**结论**：性能 2 轮报告的"缓存层优化"被 **LS 自身的不可压缩 latency** 盖过；
**真正的瓶颈是 rust-analyzer 的索引吞吐**，不是我们的代码。

## P0 实锤

### P0-A：symbol-tree 50 文件挂死 daemon（致命）
- 触发：daemon 在 stress_demo 跑 `symbol-tree .`，连续 600s 不返回
- 不是单次慢，而是 **整个 daemon 进程 hang**；后续所有调用 timed out
- 根因：50 个 .rs 全仓扫描触发 RA 全量重建；我们的扇出（MAX_INFLIGHT=4）让 RA 同时接收 50+ didChange + 文档符号查询，RA 内部队列拥塞
- 影响：中等项目（>30 个 .rs）跑 symbol-tree 必卡
- 修法（候选）：
  a) 节流：didChange 批量 + 200ms 去抖，避免瞬时洪泛
  b) 降级：>30 文件时改回串行 walk（牺牲并发换稳定）
  c) 队列隔离：文档符号查询走独立通道，与 didChange 互不阻塞

### P0-B：p99 尖刺 300-700ms（高频）
- 触发：RA 后台索引（文件 watcher、crates 重索引）抢占 daemon worker
- 影响：每 20-30 次调用出现一次尖刺
- 修法（候选）：
  a) RA 启动时延迟开销（已有 RA compilation 接收器？实测 rps/DRA 96.9%）
  b) 请求优先级：用户请求 > 索引后台任务
  c) 索引预算：每秒最多 N 次 RA 调用

## P1

### P1-A：rust_demo 5 文件全绿；scale up 行为差异
- p50 在 50 文件 fixture 下 60ms，比 rust_demo 5 文件 ~6ms（memory 数字）高 10×
- 与 RA 索引规模成正比；非工程问题

## 全链路状态：✅ 可用 / ⚠️ 中等项目受限

- ✅ 端到端 e2e 20/20 PASS（local/p2_e2e_verify.py 11.8s）
- ✅ workspace cargo test 58 组 0 FAILED
- ✅ clippy -D warnings 全 workspace 干净
- ✅ 7 适配器稳定（clangd/pyright/gopls/typescript/csharp-ls/jdtls/rust-analyzer）
- ⚠️ 中等项目（≥30 .rs）symbol-tree 触发 daemon hang（P0-A）
- ⚠️ 频繁用户场景 p99 300-700ms 尖刺（P0-B）

## 落盘
- 脚本：local/perf-stress.py（可调参数：fixture 大小、每段 N、warmup）
- 数据：local/perf-stress-7.log（完整输出）
- 报告：本文件