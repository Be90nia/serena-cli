# serena-rust 性能维度审查报告（只读）

- **VERDICT**: 无阻塞性架构硬伤——锁纪律干净（全部短临界区、零持锁跨 await）、LS 传输帧协议正确。真实性能债集中在 **4 个可执行修法**上：① FileGuard drop 即 didClose 导致每次工具调用全量重发 didOpen（全局放大器）；② symbol-tree / repo-map 冷路径逐文件串行 LS 往返；③ HTTP 响应 Value 深拷贝（一行修）；④ find-symbol 每请求双次全仓 walk。修掉 ①② 后，12 个新特性中工时最长的 E（repo-map）在本仓 850 文件上的首跑成本可从分钟级降到秒级；其余特性均不阻塞。
- 锚：`c4f355a`（2026-09-21，工作区有未提交 bead 记录类改动，与代码无关）
- 方法：静态通读 supervisor/lib.rs（5536 行）、lsp-core/{session,client,docsync,framing,transport/stdio}.rs、daemon/{http,reaper,serve}.rs、cli/main.rs；量化基于代码路径推导，未做运行时 profiling（标注处为估算）。

---

## Findings（按严重度降序）

### P1-1【高】FileGuard drop 即 didClose → 每次工具调用全量 didOpen 重放（全局放大器）

- **位置**: `crates/lsp-core/src/docsync.rs:60-81`（Drop → ref_count==0 → 立即 `map.remove` + didClose）；`docsync.rs:87-150`（ensure_open：buffer 不存在 → `fs::read_to_string` 全文 + didOpen）
- **问题**: 每个工具调用 `ensure_open` 拿 guard，调用结束 guard drop → 缓冲立即移除 + didClose。下一次对同一文件的任何工具调用都重新走：磁盘全文读 → JSON 编码全文 → stdin 管道写 → **LS 重新解析整文件**（RA/clangd 重入 VFS、重算诊断并重新 push）。全仓约 30 处 `ensure_open` 调用点（supervisor/lib.rs 22 处 + edit_tools.rs 5 处 + ref_tools.rs 2 处）无一幸免。AI 编辑回路（overview→定位→写→诊断→再看 overview）典型 5-6 次连续调用同一文件，5 次重放开销全是纯浪费。
- **影响量化**: 单次（50KB 文件、rust-analyzer）：读 ~0.1-1ms + 管道写 ~0.1ms + **LS 重解析 1-20ms**（[估算]）+ diagnostics 重推。编辑回路 ×6 调用 ≈ 5 次浪费往返。这是 P1-2（串行树扫）和特性 E/K 冷路径的成本乘数。
- **修法**: buffer 保留策略替代 ref_count 归零即关：TTL（如 60s 无访问才 didClose）+ 容量上限 LRU（如 32 文件，超限关最久未用）+ session shutdown 全关。mtime/size 双因子失效检查已在 `docsync.rs:113-121`，直接复用，改动收敛在 docsync.rs 单文件。注意丢弃 buffer 时才发 didClose 的现有语义保留。
- **阻塞 12 特性？**: 不阻塞正确性；但 **E（repo-map 850 文件首跑）与 K（fallback 扫 200 文件）的冷启动成本被此项放大 2-3 倍**，建议在 E 开工前修。

### P1-2【高】symbol-tree / repo-map 冷路径：逐文件串行 LS 往返（200-850 次串行 RTT）

- **位置**: `crates/supervisor/src/lib.rs:1483-1494`（`for file in &files { self.tool_overview(...).await }`——顺序循环，注释自认"daemon 内 LS 请求本就串行"）
- **问题**: tool_symbol_tree 对收集到的 ≤200 个文件逐个 await documentSymbol。每个文件 = 1 次 didOpen（见 P1-1）+ 1 次 documentSymbol 往返 + 1 次 didClose。LS stdin 管道天然支持请求流水线（client.rs pending 表本就是多请求并发设计，`client.rs:88`），串行纯属调用侧没并发。特性 E 的算法第 1 步"workspace 全量文件 documentSymbol"复用 tool_overview，会继承同一串行模式放大到 850 文件。
- **影响量化**: [估算] 200 文件 × 5-30ms/RTT ≈ **1-6s 冷路径**；850 文件（E）≈ 4-25s。并发 4-8 路 + P1-1 修复后 ≈ 亚秒-2s。
- **修法**: `FuturesUnordered` + `buffer_unordered(4..8)` 扇出 tool_overview；保留单文件失败不炸整树语义（collect 结果按序分组）。LS 端无需改动。feature K 的 fallback 逐文件扫描（设计 §13，max_files=200）直接复用同一并发原语。
- **阻塞 12 特性？**: **E 的验收标准"本仓 850 文件实测输出 ≤ budget"实质上依赖此项**（首跑分钟级不可接受）；K 同理。建议与 E 同批实现或作为 E 的前置。

### P2-3【中高】shell JSONL 主循环串行 dispatch：读一行 → 等完整往返 → 才读下一行

- **位置**: `crates/cli/src/main.rs:1496-1552`（`while let Ok(Some(line)) = lines.next_line().await` 循环体内直接 `.await dispatch_shell_cmd` 后才继续读）
- **问题**: AI agent 连发 3 条只读命令时，墙钟 = 三次 RTT 之和而非最慢一条。stdin 本身可缓冲多行，循环却逐条背靠背等待。这正是设计文档特性 L（POST /batch）要解的问题——本 finding 确认该设计的性能动机成立，且给出更小的替代修法。
- **影响量化**: 典型 LS 热往返 20-200ms/条；5 条命令 ≈ 100ms-1s 串行浪费。对 agent 迭代体感明显（每轮省 50-80%，与设计 §12 的 J 目标重叠但机制不同）。
- **修法**: 两档：(a) 最小改——循环体改为"读到的行立刻 spawn 任务 + 有序回写"，用 channel 保输出顺序（改动 ~30 行，client 已复用单 reqwest 实例 `main.rs:1491`）；(b) 完整版 = 特性 L 的 /batch 端点。建议 (a) 先行，L 的批量端点按原计划。
- **阻塞 12 特性？**: 不阻塞；L 落地后本项自然消解。M（warm）的"幂等秒回"不受影响。

### P2-4【中】find-symbol 每请求成本：root_source_mtime 全仓 walk（命中也走）+ miss 时第二次 walk

- **位置**: `crates/supervisor/src/lib.rs:1677`（`find_symbol_cache_key(root, query, root_source_mtime(root))`——缓存**命中路径**也先付 walk 成本）；`lib.rs:165-185`（root_source_mtime：`ignore::WalkBuilder` depth-3 遍历取 max mtime，同步阻塞）；`lib.rs:1686-1697`（cache miss 后为发现 lang 集合**再走一遍**同样的 walker）
- **问题**: ① 每次 find-symbol（无论命中与否）都付一次 depth-3 全仓 stat 风暴，且是**同步 IO 直接跑在 async 执行线程上**；② miss 时同函数内重复走第二遍只为收集 lang 集合——两遍可合一；③ 缓存 key 里 `root.to_path_buf()` + `format!("ws?{query}")` 每次分配（次要）。
- **影响量化**: [估算] Windows 温热 FS：数百次 stat ≈ 5-20ms/次；冷 cache 数十 ms。对交互频率最高的 find-symbol 是每请求固定税。
- **修法**: (a) mtime 信号加 TTL 节流（如 2s 内复用上次信号值，存 `Mutex<(Instant, Option<SystemTime>)>`——外部修改感知延迟 ≤2s，与诊断等待同量级容忍）；(b) 两遍 walk 合一：单次遍历同时收集 max mtime + lang 集合；(c) walk 包 `spawn_blocking`。三项改动都收敛在 lib.rs 头部 ~40 行。
- **阻塞 12 特性？**: **E 直接依赖**（设计 §5"图与排序结果按 root_source_mtime 信号缓存"——若不节流，每次 repo-map 调用都全仓 walk）；J（--delta handle）同样以该信号为失效键。不节流则 E/J 每次调用附加固定税。建议随 E 落地。
- **附注（正确性相邻，静态推断）**: `max_depth(3)` 使信号对深度 >3 的文件盲（本仓 `crates/*/src/*.rs` 在深度 4，**不参与** max mtime）——深层文件外部修改不会推进信号，E/J 的缓存失效设计若依赖"任一源码文件修改即失效"需知此边界；代码注释只自认了"删除不推进"（lib.rs:164），depth 截断未提及。

### P2-5【中】HTTP 响应 Value 深拷贝：大响应被完整复制一次再序列化

- **位置**: `crates/daemon/src/http.rs:153` 与 `:162`（`Json(serde_json::to_value(&resp).unwrap())`——`to_value(&resp)` 对含 `data: Value` 的 ToolResponse **深拷贝整个 data 树**，随后 axum 再序列化一次）；上游 `lib.rs:3316` 等每个 execute_tool 分支已做过一次 typed→Value 转换
- **问题**: 大响应（semantic-tokens 全量、find-symbol 50 命中、refs 200+ 条、850 文件 repo-map）生命周期 = 工具内构建 Value（第 1 份）→ to_value 深拷贝（第 2 份）→ 序列化字节（第 3 遍遍历）。中间那份深拷贝纯属浪费：`Json(resp)` 直接序列化 ToolResponse 即可（它本就实现 Serialize）。
- **影响量化**: 100KB 响应 ≈ 数千个 Value 节点克隆 ≈ 1-3ms CPU + 2-3× 瞬时内存（[估算]）。小响应（<5KB）可忽略——修法零风险，但收益只在大响应场景。
- **修法**: 一行：`Json(resp)` 替代 `Json(serde_json::to_value(&resp).unwrap())`（Ok/Err 两分支，http.rs:153/162）。顺带消灭 `.unwrap()`。
- **阻塞 12 特性？**: 不阻塞；G（--max-tokens）与 H（紧凑位置格式）的"输出 bytes 对比"验收在大响应上做时受益于减少噪声，但无依赖关系。

### P2-6【中】search 全仓扫描同步 IO 跑在 async 线程：walk + 逐文件 read_to_string 内联

- **位置**: `crates/supervisor/src/lib.rs:2336-2347`（`WalkBuilder` 同步遍历，daemon 请求线程上执行）；`lib.rs:2377`（`std::fs::read_to_string(path)` 同步读每个 ≤5MB 文件）
- **问题**: 850 文件 × 读 + 正则扫全部内联在一个 async fn 里，执行期间该 tokio worker 线程完全阻塞——期间 daemon 在该 worker 上的其它请求（含 /status、reaper select 分支）排队。单 worker 阻塞数百 ms-数秒（冷 FS）。
- **影响量化**: [估算] 温热 50-200ms、冷数秒的单 worker 独占。当前 AI 串行使用模式下无感；特性 L（/batch 并行 8 条）落地后，一条 search 会拖慢同批 7 条的调度。
- **修法**: 整个扫描体包 `tokio::task::spawn_blocking`（walk + 读 + 正则都是同步代码，搬进去零改动）。~5 行。
- **阻塞 12 特性？**: 不阻塞；A/I 改 search 时顺手包上（A 的"按命中分组 documentSymbol"会让 search 变慢，届时更不该占着 worker）。

### P3-7【低】诊断等待 = 100ms 盲轮询 ×50；F2 接线时建议换 Notify

- **位置**: `crates/supervisor/src/lib.rs:755-777`（push 路径 `for i in 0..50 { sleep(100ms); 查 cache/generation }`）
- **问题**: `diag_generation: AtomicU64`（lib.rs:224，publish handler 每次 ++）已具备精确唤醒的全部条件，等待方却轮询。平均引入 ~50ms 量化延迟，最坏 100ms。
- **影响量化**: 每次 tool_diagnostics / 未来 F2 写回执诊断 ≈ +50ms 平均。功能层面无害。
- **修法**: `tokio::sync::watch<u64>` 或 per-generation `Notify`，publish handler send，等待方 `changed().await`。改动 ~20 行，收敛在 lib.rs。
- **阻塞 12 特性？**: 不阻塞。**F2 实施时的推荐路径**：设计已定"复用 2s 超时"，轮询也能过验收，但 watch 让写回执平均快 50ms 且少 20 次锁竞争。另注意：F2 的诊断等待**必须在 write_gate 释放之后**做（设计 §17 风险行已提"复用既有 2s 超时"，未明确门内/门外——门外，否则全局写门被诊断等待拖住 2s，见 P3-8）。

### P3-8【低】全局写门（static tokio Mutex）持锁跨 LS 往返——设计使然，标注 F2 约束

- **位置**: `crates/supervisor/src/write_gate.rs:14-21`（`static WRITE_GATE: Mutex<()>`）；持锁范围 `lib.rs:2179-2183`（replace-body：ensure_open → locate_symbol LS 往返 → 读盘 → 写 → didChange 全程持门）、`lib.rs:2462-2465`（rename）、`edit_tools.rs:165/186/210/235/413`（行级编辑五件套同构）
- **问题**: 不同文件、不同项目的写操作全局串行，且每次持门含 1 次 LS 往返（正确性动机成立：锁内 range 对账防过期，A4 FIFO）。AI 写入天然串行，当前无争用。**真正的风险是未来改动**：若 F2 把诊断等待放进门内，所有写工具排队 +≤2s。
- **影响量化**: 当前模式（串行写）≈ 零实际损耗；错误实现 F2 时 = 每并发写 +2s。
- **修法**: 不改门（ponytail 注释已预留按 project_root 分键的升级路径）。只立规矩：**F2 的 post_diagnostics 等待放门释放后**。
- **阻塞 12 特性？**: 不阻塞；F2 实施纪律项。

### P3-9【低】LS 帧解码 O(n²/chunk) 头部重扫 + 8KB 双拷贝

- **位置**: `crates/lsp-core/src/framing.rs:123`（`buf.windows(4).position(|w| w == b"\r\n\r\n")`——每次 decode 从 0 重扫全缓冲）；`crates/lsp-core/src/transport/stdio.rs:102-108`（8192 栈 chunk → `extend_from_slice` 拷入 BytesMut）；`stdio.rs:85-88`（出站帧 encode 新 Vec 后再 copy 进 buf_out，双拷贝）
- **问题**: 大帧（多 MB 的 workspace/symbol、大文件 semantic-tokens）以 8KB 分块到达时，每次 decode 都对已累积缓冲做全量 `windows(4)` 扫描。2MB 帧 ≈ 256 次读 × 平均 1MB 扫描 ≈ 上亿次字节比较（[估算] 50-150ms CPU/帧）。
- **影响量化**: 仅大响应场景可测；常规帧（<64KB，1-8 次读）开销 <1ms。
- **修法**: decode 记住上次扫描终点（`scan_from = buf.len().saturating_sub(3)` 起扫）——~5 行；出站双拷贝可让 encode 直接写入 buf_out（可选，收益微小）。
- **阻塞 12 特性？**: 不阻塞。E 落地后 repo-map 响应大，但那是 daemon→CLI 方向（HTTP），不经此路径；此路径是 LS→daemon，仅 find-symbol/semantic-tokens 大响应受益。

### 无问题项（已排查，附证据）

| 维度 | 结论 | 证据 |
|---|---|---|
| instances/load_gates/last_used/symbol_cache/DiagCache/pull_diag_supported 锁 | 全部短临界区，无持锁跨 await，无 IO/LS 往返在锁内 | lib.rs:216-233、379-386；session_for 快路径 511-530（clone Arc 后即放锁）；evict_failed_instances lib.rs:460-463 显式"先 clone key 再 await" |
| client.rs pending 表 / 通知 handler 表 | 临界区微秒级，请求走 oneshot，写路径 mpsc 无锁 | client.rs:8-10、88、222-224、264-281 |
| reaper GLOBAL_LAST_ACTIVITY | `Mutex<Instant>` 锁内纯赋值/读取，tick 周期性短锁；note_activity 已在 http 层接线（旧审计"无调用点"已修复） | reaper.rs:46-52、104；http.rs:131/177（note_activity 调用） |
| daemon per-request Arc/Atomic 开销 | AppState clone = 2 个 Arc；in_flight 两次 fetch_*；active_project String clone——合计亚微秒，相对 ms 级 LS 往返是噪声 | http.rs:122-165 |
| reqwest 复用 | CLI 每进程建 1 个 client（进程模型决定，跨进程本就无法复用连接池）；shell 会话内单 client 贯穿复用；daemon 请求路径无 reqwest；ls-runtime 的 blocking client 仅分钟级下载用 | main.rs:31-36、895/928/1408/1434/1491；install.rs:9 |
| 必填参数解析（required_file/required_position/sanitize_timeout_args） | 每次 1 个 String 分配 + 若干 Value 查找，~百 ns 量级；`json!` 重建 LSP params 同量级 | lib.rs:3137-3165；**结论：不值得优化**，与 LS 往返差 4-5 个数量级，明确不建议动 |
| 3.1 文档符号缓存 key 构造成本 | `doc_symbol_cache_key` 每次 1 次 `fs::metadata` stat（lib.rs:144）——但同一请求里 ensure_open（docsync.rs:89）本来就要 stat 同一文件，实际增量 ≈ 1 次多余 stat/请求，**不值得单独修**；真正的固定税在 find-symbol 侧（P2-4） | lib.rs:140-148；docsync.rs:89 |

## Findings × 12 特性矩阵

| 特性 | 受影响 finding | 阻塞判定 |
|---|---|---|
| F2 post_diagnostics | P3-7（建议换 watch）、P3-8（等待必须门外） | 不阻塞，实施纪律项 |
| H 紧凑位置格式 | P2-5（大响应少一次拷贝，验收对比更干净） | 不阻塞 |
| B edit-context | P1-1（4 段聚合对同文件多轮往返，churn 修复直接提速） | 不阻塞 |
| K find-symbol 兜底 | **P1-2**（200 文件扫描需并发扇出）、P1-1 | K 的扫描体实现时应带并发，否则 200×RTT |
| I search --comments-only | P2-6（顺手 spawn_blocking） | 不阻塞 |
| E repo-map | **P1-1 + P1-2 + P2-4**（三者叠加决定 850 文件首跑是分钟级还是秒级） | **实质阻塞 E 的验收体验，建议前置** |
| A search 标注符号 | P2-6、P1-1（分组 documentSymbol 走 3.1 缓存可兜） | 不阻塞 |
| C refs --grouped | 无 | 不阻塞 |
| L /batch | P2-3（shell 侧并发化）、P2-6（并行后 search 占线程问题显性化） | L 本身即修法 |
| M warm | P1-1（warm 后持续保持文件打开更佳，非必需） | 不阻塞 |
| G --max-tokens | P2-5（截断在大 Value 上做更省） | 不阻塞 |
| J --delta | **P2-4**（delta handle 失效键 = root_source_mtime，节流 + depth-3 盲区须知） | handle 设计需知 P2-4 附注 |

## 建议实施序（性能债视角）

1. **P2-5**（一行，立即做）
2. **P1-1** buffer TTL/LRU（docsync.rs 单文件，B/K/E/M 全受益）
3. **P2-4** walk 合一 + mtime 信号节流（E/J 前置）
4. **P1-2** 并发扇出原语（与 K/E 同批）
5. P2-6 / P3-7 / P3-9 随对应特性顺手做（A/I、F2、大响应出现时）

预估总改动量：核心 4 项 ≈ 150-250 行，全部收敛在 docsync.rs / lib.rs 头部 / http.rs / stdio.rs+framing.rs，无跨 crate 接口变更。
