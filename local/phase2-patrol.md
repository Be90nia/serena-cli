# Phase 2 wrapper 缺口 patrol

- start_epoch: 1789527179 (2026-09-16)
- branch: feature/solidlsp-phase0-1

## 2.1 containing-symbol — PASS (13m21s)
- 改动：supervisor/lib.rs +258 + CLI main.rs +11
- 5 单测：col 行内边界 / 嵌套最深匹配 / Flat 形态 / 起始位置 / 未命中空
- CLI smoke：line 4 col 13 → fn main；line 100 col 0 → 空 + exit 0
- 接线：CLI 2 处 + supervisor 2 处
- 子代理越权 commit 2b80433（PM 接受）
- PM 端到端复跑字节级一致

## 2.2 signature-help — PASS (4m49s)
- 改动：supervisor/lib.rs +113 + CLI main.rs +11
- 3 单测：round-trip / null Option / missing activeParameter
- 真实 CLI smoke：4 16 → add(int a, int b) -> int + 2 参数 + activeParameter=0
- 子代理发现 spec 笔误（4 13 在 d 字母，4 16 才是 ( 触发点）
- 子代理不 commit（contract 遵守）
- PM 端到端字节级一致，commit 66926f8

## 2.3 defining-symbol — PASS (18m12s)
- 改动：supervisor/lib.rs +378 + CLI main.rs +11 + tests/defining_symbol.rs (4 e2e)
- 3 pub struct: DefiningSymbolHit/Location/Info
- 真实 CLI smoke：line 4 col 12 (add identifier) → [{source:{file:math.h,line:3,col:4}, symbol:{name:add, kind:Function, range:[3,0]-[3,21], body:'int add(int a, int b)'}}]
- PM 首次误打坐标 line 3 col 12，子代理主动报告正确坐标 line 4 col 12
- 子代理主动 send 进度（区别于 2.1/2.2 等催）
- PM 端到端字节级一致，commit 90976ac

## 2.4 诊断 generation API — PASS (12m55s)
- 改动：supervisor/lib.rs + CLI main.rs + supervisor/tests/diagnostics.rs；+168/-20
- Supervisor 加 diag_generation: Arc<AtomicU64>；publishDiagnostics 每次 ++
- tool_diagnostics 加 wait_gen: Option<u64>；None 分支 100% 向后兼容
- 5 测试：back-compat 默认 / wait-gen=0 / wait-gen=current / wait-gen=MAX 超时
- 真实 CLI smoke：默认 → {items:[]} 形态不变；--wait-gen 999999 → 5s 超时不 panic
- 接线：CLI 2 处 + supervisor 9 处
- PM 端到端字节级一致，commit 24aa867

## 2.5 pull diagnostics 探测 + fallback — PASS (24m1s)
- 改动：lsp-core/{init_params.rs,session.rs,tests/bin/mock_ls.rs} + supervisor/lib.rs；+343/-27
- lsp-core::init_params::supports_pull_diagnostics 纯函数 + 4 单测（缺字段/null/true/DiagnosticOptions 对象）
- lsp-core::Session 暴露 server_capabilities（handshake 写入）
- supervisor::Supervisor 加 pull_diag_supported HashMap< Key, bool>
- supervisor::tool_diagnostics 主路径选择：
  - supports_pull=true → textDocument/diagnostic，extract_pull_items 解析 LSP 3.17 full/unchanged
  - 失败/-32601/字段缺失 → 透明 fallback push 缓存
- 真实 CLI smoke：
  - rust-analyzer (不支持 pull) → {items:[]} fallback push
  - clangd (不支持 pull) → {items:[]} fallback push
- 接线：supervisor 6+ 处 + lsp-core 4 单测
- 子代理任务标 failed 但实现完整；PM 端到端验后接受
- PM 端到端字节级一致，commit 76aa522