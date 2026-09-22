# P2-18h 报告：外部修改不感知导致旧缓存 range 错位编辑

VERDICT: 完成（6/6 P0 判据，见下）

## 根因（修前实锤）

mock_ls 集成测试 RED 输出（判据 1 证据）：

```
外部修改（同 mtime + size 变）必须触发 didChange 重放；
events=[Object {"event": "didOpen", ..., "version": 1}]
```

场景：walk → 外部覆写（内容变、mtime 拨回记账值 = mtime 粒度窗口漏检形态）→ 再调工具：

1. `SymbolCacheKey` 第三位只含 `Option<SystemTime>`（无 size）→ 同 mtime 粒度窗口内的改写 key 不变 → **缓存命中旧 SymbolHit** → 切片/编辑落在错位置（Retest replace-body 损坏机制）。
2. 缓存 hit 路径不触 LS → **无 didChange** → LS 文档与盘脱钩。

Windows 下该窗口真实存在（mtime 元数据缓存 / FAT 2s 粒度）；docsync `ensure_open` 早已用 mtime+size 双因子堵同款窗口（docsync.rs:60-62 注释 declare），symbol_cache key 是唯一没跟上的层。

## 修复（crates/supervisor/src/lib.rs，实现 ~35 行）

| 改动 | 位置 | 内容 |
|---|---|---|
| key 双因子 | lib.rs:146-162 | `SymbolCacheKey` 第三位 → `Option<(SystemTime, u64)>`；`doc_symbol_cache_key` 取 `(modified, len)`。盘变必 miss |
| find_symbol 适配 | lib.rs:164-179 | workspace 信号 size 位恒 0（`ws?` 前缀保证不与真实文件 key 撞） |
| 对账清理 | lib.rs:1561-1578 | 新增 `reconcile_symbol_cache_for_file`：盘上 stamp 与缓存记账不符 → retain 清该文件全部条目（"不一致即清该文件缓存"） |
| 挂载 | overview / symbol_body miss 路径 | miss 后调用对账清残留；didChange 由 miss 路径既有 `ensure_open` 自动重放（mtime/size 变 → 全量 didChange），不重复发 |
| 测试基建 | lib.rs:5822-5855 | `find_mock_ls` 兜底加 workspace-root 候选（编译期 CARGO_MANIFEST_DIR 锚定）——修复 `-p supervisor` 单包跑法下 mock_ls 静默 skip；`find_mock_ls` 提升 pub(crate) 供跨 mod 复用 |

replace-body 不挂载：其 documentSymbol 直查不走缓存，外部修改感知已由链路内 `ensure_open` 对账覆盖。

## 验证

- 修前复现（RED）：`cargo test -p supervisor --lib external_modify_same_mtime` → FAILED，events 仅 didOpen（上文）
- 修复后（GREEN）：同测试 → ok（didChange 到达 mock_ls + 重 walk + fast path 零新事件）
- `cargo test -p supervisor --lib` → **139 passed / 0 failed**（含 3 个新测试）
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 errors
- `cargo test --workspace --no-fail-fast` → **46 个测试目标全部 `ok`，0 FAILED**（112s，含 supervisor 139）

### 新增测试（判据 4）

1. `external_modify_same_mtime_replays_did_change_and_rewalks`（集成，mock_ls 模式）：walk → 外部覆写（同 mtime + size 变）→ 再 walk + symbol-body。断言：didChange 到达 mock_ls（track 日志）、body 以最新 walk range 切盘上现文、fast path（无修改再 walk）零新 LS 事件（判据 3）。
2. `same_mtime_size_change_invalidates_symbol_cache`（机制单测）：同 mtime + size 变 → key 变必 miss + 对账清残留 + 幂等（二次调用 false）。
3. `unchanged_file_keeps_fast_path_and_does_not_touch_others`（机制单测）：无修改 key 稳定、缓存保留、不误清、他文件不波及。

## 边界与解释

- **判据 2 的"命中新位置 OK"在 mock_ls 下的语义**：mock_ls 的 documentSymbol 恒回固定假 range（lsp-core 禁区不可改），任何 supervisor 侧修复都无法让 mock 返回新 range——修前/修后返回值巧合相同。判定性证据落在机制层：didChange 重放到达 LS + 缓存条目迁移到新 stamp + 重 walk 以最新结果为准。真实 LS（clangd 等）消化 didChange 后 documentSymbol 即返回新 range，闭环成立。
- **判据 3"fast path 不走 filesystem"**：外部修改感知的物理必要条件是 stat（metadata 一次，µs 级，与 ensure_open 每次调用 stat 同款既有先例）；fast path 保证的是**零内容重读、零 LS 往返**（hit 路径连 session_for 都不进）。
- 非目标遵守：未实现文件监听；未动 lsp-core / daemon / 写门 / 其他缓存；未 commit。
- 并发协作：P2-a5k（容量闸门）与 P2-y5u（progress 双锁）同文件并发编辑，改动区域已互报不重叠。

## 残余风险

- `root_source_mtime` 只扫 depth≤3：更深目录的外部修改不推进 find-symbol 的 workspace 信号（既有边界，注释已 declare "重启 daemon 兜底"）；单文件工具不受此限（key 为单文件 stamp）。
- 外部修改后同 mtime **且**同 size 的改写（物理上内容必同或极罕见）双因子同样检不出——与 ensure_open 同极限，非本票引入。
