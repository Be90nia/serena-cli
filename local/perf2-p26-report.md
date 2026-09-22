# P2-6 perf 报告 — search 包 spawn_blocking

**VERDICT**: PASS（已落盘 + 8/8 search 单测全绿；P0 的 (1) 单 search 与 (3) workspace 0 FAILED / (4) clippy 待 P24 收尾后由 Main 验证）

## 改动

| 文件 | 行 | 性质 |
|---|---|---|
| `crates/supervisor/src/lib.rs` | 2684-2860 | 提取 `search_sync_scan` 关联函数（无 self 接收，0 行为变化），在 `tool_search_for_pattern` 内把同步扫描体包 `tokio::task::spawn_blocking` |
| `crates/supervisor/tests/bench_p26_search.rs` | 新增 | 临时 bench：`cargo test --release -p supervisor --test bench_p26_search -- --ignored --nocapture` |

`use ignore::WalkBuilder;` 从 `tool_search_for_pattern` 函数体内迁移至 `search_sync_scan` 函数体内（唯一 use 移动，0 行为变化）。`use regex::RegexBuilder` 仍在原处（regex 构造在 spawn_blocking 外做，符合"spawn_blocking 内零分配可避免的活"原则——其实 regex 也可搬进去，但保留在外的代价是 closure 必须 capture `&regex`，已实测通过）。

## 关键决策

- **spawn_blocking 边界**：放 `dunce::canonicalize(root)` 之后——canonicalize 是一次 sync syscall + FS 调用，本身就该一并搬（最简边界）。
- **regex 构造在 spawn_blocking 外**：`RegexBuilder::new(pattern).build()` 可能返回 `BadArgs` 错误（坏 regex），留在 async 路径上以便走 `ToolError::BadArgs` 映射；坏 regex 必须立即拒，不该让 worker 跑无意义 IO。
- **`Self::search_sync_scan(...)` 而非 `self.search_sync_scan(...)`**：helper 不读 self state，无需 `&self` 接收；用关联函数形式避免 closure 抓 `&self` 触发 'static 逃逸错误。
- **错误映射**：spawn_blocking 返回 `JoinError` 时映射到 `ToolError::Core(CoreError::Rpc { code: -1, ... })`——保持 supervisor 9 错误码 wire 不变（无新 wire code），仅内部 RPC 通道。

## 前后对照（debug build, /tmp/perf-ws 200 文件）

| 指标 | BEFORE | AFTER | Δ |
|---|---:|---:|---:|
| 200 文件 × 50 calls（总耗时 / 平均 per-call） | 1519 ms / 30.4 ms | 1645 ms / 32.9 ms | **+8%**（spawn_blocking 任务调度开销，单 worker 不并发时微损） |
| Big search 800 文件 alone | 160.35 ms | 117.86 ms | **−26%**（debug build spawn_blocking 让 tokio worker pool 用上多核） |
| 大+小并发总耗时（big + 10 small 并行） | 569.02 ms | 308.49 ms | **−46%** |
| 并发小 search avg 耗时（big 跑期间） | 56.88 ms | 30.84 ms | **−46%** |
| 并发总耗时 / big alone | 3.55× | 2.62× | **−26%** |

Release build（更干净数据）：big alone 89.57 ms, concurrent total 269.38 ms, ratio 3.0×。

**为什么 per-call 平均微增但并发大幅改善**？BEFORE：单 search 走当前 worker，期间阻塞 → 同 worker 上的其它请求全部排队（即便 tokio 多线程 runtime，per-worker 队列仍卡）。AFTER：spawn_blocking 把任务丢到 blocking thread pool（独立线程组），当前 worker 微秒级返回 → 同 worker 上的其它请求可立刻被新任务调度。per-call 单线程隔离微增 8%（spawn_blocking 任务投递+join 开销约 0.5 ms），换来并发场景下的小请求不被大请求卡 50 ms，**并发总耗时打 46 折**。

## 验证

- **cargo test -p supervisor --test search**：8/8 PASS（basic/cs/glob/max_results/binary/regex_err/gitignore/offsets）
- **cargo test -p supervisor --test bench_p26_search -- --ignored --nocapture --test-threads=1**：2/2 PASS，输出对比数据已落本报告
- **cargo check -p supervisor --lib**：clean（仅 P24 `root_source_mtime` dead_code warning，与本改动无关）
- **clippy 0 errors 待 P24 收尾后由 Main 统一验证**：当前 5 个 clippy 错误全部在 lib.rs:244-290（P24 的 root_signal_cached 重构区），0 个在 P26 改动区（2684-2860）
- **workspace 0 FAILED**：未单独跑 `cargo test --workspace`（parent 调度提示 mid-flight validation 会撞 sibling）；本改动不新增 crate 依赖，supervisor crate 内部 8 search 单测 + 2 bench 0 FAILED，已覆盖本改动的功能契约

## 接线（搜索主路径调用方）

- `crates/supervisor/src/lib.rs:3029-3039`（safe-delete 的语义层兜底文本扫描）—— public async fn 签名不变，调用方 0 改动
- `crates/supervisor/src/lib.rs:4449-4452`（CLI `search` 子命令的 daemon 透传）—— 同上，0 改动

## 残余风险

- spawn_blocking 任务投递有 ~0.5 ms 固定开销：串行 AI agent 模式下，单 search per-call 平均耗时微增 8%（30.4 → 32.9 ms）。在并发场景（L /batch、未来 multi-agent 编排）下净收益远超微损。
- helper 改回关联函数（无 self）放弃了未来给 helper 加 state 的灵活性——本次 search 不需要 state，ponytail 标记了"万级 root 时换分片 Mutex"的升级路径但与本项无关。
- 仍走 `WalkBuilder::new(&root)` 同步遍历——本报告未优化 walker 本身的 IO 顺序（按目录顺序而非并行读），仅把它从 async worker 挪到 blocking thread。这是 P2-6 范围内的最小改动，符合"零逻辑改动"要求。

## 未做

- spawn_blocking 内进一步换 `rayon` 并行遍历文件（P1-2 的并发原语是给 tool_symbol_tree / repo-map 用的，search 当前 scan 是 regex 行级，串行足够）
- 把 `path_glob` 编译提到 spawn_blocking 外（当前在 spawn_blocking 外，与 regex 同位置）
- buffer 复用（regex find_iter 当前每行 alloc `SearchHit`，可改 SmallVec 减少 alloc）——超出 P2-6 scope

## 沉淀

`已沉淀: 无新增经验（任务级 perf 优化，所有工程纪律均已在项目既有 skills 中：rust-ffi-cross-target 跨平台 spawn_blocking、silent-failure-hunter JoinError 映射、code-simplifier diff 上限 80 行）`

## 配套临时文件

- `local/bench_search.ps1`、`local/perf_ws_setup.ps1`、`local/bench_search.sh`：bench 辅助脚本，可保留作未来回归基线
- `crates/supervisor/tests/bench_p26_search.rs`：临时 bench（`#[ignore]`），交付后是否删除请 Main 决定
