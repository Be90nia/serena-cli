VERDICT: 有条件通过——架构契约代码侧符合；「唯一事实源」文档多段过时，coverage v4 数字全面落后；分层铁律零违规

# 设计符合度审计：ARCHITECTURE.md / DESIGN.md / PLAN.md / local/serena-feature-coverage.md 声明 vs 代码实际

审计方式：只读（read/grep/glob）；分层核实为 manifest 级（含传递闭包推理），未运行 cargo tree（scout 无 shell）。

## 【符合度表】

### A. ARCHITECTURE.md（自称唯一事实源）

| 声明（位置） | 代码实际（文件:符号/行） | 判定 |
|---|---|---|
| 7-crate workspace（§0/§1） | crates/ 恰含 ls-runtime / lsp-core / ls-adapters / ls-registry / supervisor / daemon / cli | ✅ 符合 |
| 铁律① lsp-core 不 import ls-adapters/ls-registry | lsp-core/Cargo.toml 依赖仅 ls-runtime（无 adapters/registry/anyhow）；file_detect.rs 头注显式按此铁律选位 | ✅ 符合 |
| 铁律② supervisor 不 import axum | supervisor/Cargo.toml 无 axum（axum 仅在 daemon） | ✅ 符合 |
| 铁律③ daemon 不含 LSP 语义 | daemon/Cargo.toml 仅依赖 supervisor，无 lsp-core/ls-* | ✅ 符合 |
| 9 错误码 wire 契约（§6.3） | daemon/dto.rs `WireErrorCode` 恰 9 变体（SCREAMING_SNAKE_CASE）；`wire_error_from_tool_error` 覆盖 ToolError 全变体；单测逐码断言 | ✅ 符合 |
| 疑点②：新增工具后 9 码守恒？ | supervisor lib.rs:3155 `match tool` 共 42 个工具分支（overview…delete-lines）共用同一映射；新增 ToolError::Serialize/Protocol 折入既有 INTERNAL/RPC_ERROR（dto.rs，Δ 注释自述「从 Launch 兜底拆出」）；未知工具 fallback → BadArgs | ✅ 守恒成立，零新增码 |
| 工具级失败=200+{ok:false}；404 未知工具；503 draining（§6.3/A5） | daemon/http.rs：503+Retry-After（:101-108）、404（:180-184）、tools_post 200 | ✅ 符合（transport body 例外见违规清单#3） |
| thiserror/anyhow 边界（§6.1） | ls-runtime RuntimeError（process.rs:17-18 thiserror）✅；lsp-core error.rs thiserror、manifest 无 anyhow ✅；ls-adapters 全 anyhow ✅；supervisor ToolError thiserror（lib.rs:97-98）✅ | ✅ 符合（§6.1 表内容过时→过时清单#6） |
| 禁 dashmap/parking_lot/async-lsp/tower-lsp（§8，PLAN「不得增删」） | 全部 7 个 crate manifest 全文 0 命中 | ✅ 符合 |
| servers.toml 单表 + include_str! 内嵌（§1） | ls-registry/config.rs:22-24 `include_str!("../servers.toml")` → LazyLock；servers.toml 顶层仅 [servers.*] 家族（50+ 条目，无第二顶级表） | ✅ 符合 |
| external-servers 机制（§4.2 外部段，疑点④） | 路径 %APPDATA%\serena / ~/.config/serena（config.rs:53-61）✅；运行时解析不内嵌 ✅；文件缺失/不可读/坏表 → 静默当空表（load_external config.rs:71-89）✅；merge_pick priority 大者胜、并列（含缺省 0）external 胜（config.rs:202；spec.rs priority i32 缺省 0、负值让位）✅；冲突逐条 warn（warn_external_conflicts config.rs:95-124）✅；EXT_TABLE 未命中 → external extensions → languages[0]（ls-registry/lib.rs:94-104）✅ | ✅ 逐条符合 |
| §4.2「id 冲突时 builtin(T2) 优先」 | session_for 双路径：ls_registry::adapter_for（T2 手写）优先，否则 config::spec_for + ensure_launch（supervisor lib.rs:455-513，「手写 T2 adapter 优先；servers.toml 条目走 config::ensure_launch」注释） | ✅ 功能等价（无字面 Registry 结构体，双路径实现） |
| lock {pid,port,boot_ms,token} + create_new 竞速 + 500ms 探活（§2） | daemon/lockfile.rs:49-113（LockEntry 四字段、OpenOptions create_new、probe_alive 500ms） | ✅ 符合 |
| §2「token 为 daemon 随机 128-bit hex」 | gen_token（lockfile.rs:57-82）= 时钟纳秒+pid+原子计数器拼接，注释自认「非加密安全」 | ⚠️ 偏离（见违规清单#2） |
| §4.1 trait 七方法 | id / languages / launch_info(async+Result) / initialize_patches / on_server_ready / request_hooks / supports_implementation 全在（ls-adapters/lib.rs:166-248） | ✅ 符合（另加 set_project_root / wait_for_index 两方法未登记） |
| §5 状态机 Failed→懒重试 | session_for 将 Failed 视为 miss 摘除重建（supervisor lib.rs:457-459）+ evict_failed 巡检 | ✅ 符合 |
| quirk mirror / Δ 标记约定（头部追溯约定） | ↖ mirror / Δ 遍布各 crate（clangd/pyright/typescript/jdtls/rust_analyzer/jedi/ty/pyre/basedpyright/ls-runtime/lsp-core 抽查 30+ 处，多数锚 @43ae021） | ✅ 符合 |
| §4.2 类图 ServerSpec 字段 + §4.3 schema 草案 | 实装 schema = auto-install-design v0.4 §2：install 七类（download/path_only/npm/uvx/dotnet/gem/source）+ exec {bin} 模板 + timeout_ms/index_timeout_ms/priority/source_commit（spec.rs ServerSpec）；无 id/case_sensitive_ext/experimental/init_overrides/quirks/launch.argv；T2 语言不进表（servers.toml 头注） | ❌ 过时（§4.3 草案被 v0.4 推翻未标注；§4.2 类图字段失真） |
| §1 lsp-core 布局含 diagnostics.rs / symbols.rs | 两文件不存在；诊断缓存与符号缓存在 supervisor（lib.rs DiagCache / symbol_cache / pull_diag_supported） | ❌ 过时（缓存归属上移，§3.4 锁表「位置」列同样失真） |
| §1 supervisor 布局 tools/{find_symbol.rs…} | tools/ 为空目录；工具实现在 edit_tools.rs / ref_tools.rs / fs_tools.rs / doctor.rs / root_finder.rs + lib.rs | ⚠️ 过时（轻） |
| §0「CLI 常规路径只触及 daemon 的 DTO 类型与 reqwest」 | cli/Cargo.toml 依赖 supervisor / ls-registry / lsp-core / daemon（doctor、install、file_detect、--direct 均直连） | ❌ 过时（A7 只覆盖 --direct 一条） |
| §6.3 LS_TIMEOUT「默认 300s」 | per-LS 默认 30s（ServerSpec.timeout_ms / CLI --request-timeout），索引类 120s（--index-timeout）；300s 仅是 CLI 转发超时 FORWARD_TIMEOUT | ❌ 过时 |
| 头部追溯锚 43ae0211 | mirror 注释普遍 @43ae021 ✅；typescript.rs 锚 @28e866b5（PR#1990，已注明理由）；2026-09-20 上游同步基线已推进至 c4dc91a7（local/upstream-sync-2026-09-20.md），ARCHITECTURE 头部未更新 | ⚠️ 过时（锚未随 sync 更新） |

### B. DESIGN.md / PLAN.md

| 声明 | 实际 | 判定 |
|---|---|---|
| DESIGN §3 lock 仲裁 / Job Object / ShutdownDraining / wire 格式 | 与 lockfile.rs / http.rs / reaper.rs 一致 | ✅ 符合 |
| DESIGN §6 工具面 9 命令 | 现 47 子命令（演进结果） | ⚠️ 过时-演进（里程碑文档，低优先） |
| PLAN Global Constraints（9 码不得自行新增 / 分层铁律 / 禁入清单 / §3.4 锁表权威） | 全部仍成立 | ✅ 符合 |
| PLAN「冲突时以 ARCHITECTURE 为准并回改本计划」 | doctor.rs:2 与 file_detect.rs:2 头注均引「PLAN Task 17」，而 PLAN Task 17 = M1 验收压测；Task 16 工具集仍为 9 命令清单 | ❌ 过时（计划未回改，任务锚位错位） |

### C. local/serena-feature-coverage.md v4（疑点①：v3/v4 是否过时）

| 声明 | 实际 | 判定 |
|---|---|---|
| 「本项目 CLI 面核实：Cmd 枚举 23 子命令」 | main.rs:71-343 实为 **47** 子命令（42 工具 + status/stop-all/install/shell/doctor） | ❌ 严重过时 |
| 「悬空接线：completion 调用必失败，M3 最高优」 | supervisor lib.rs:3444 已有 "completion" 分支 | ❌ 过时（已修复） |
| 「wrapper 缺口：signature-help / containing-symbol / defining-symbol / 跨文件 symbol-tree / 诊断 generation / pull diagnostics」 | 六项全落地：lib.rs:3182 / :3422 / :3430 / :3161 / :3410-3413（wait_gen）/ :3401（workspace-diagnostic） | ❌ 过时（已修复） |
| 「73 适配器落地 7 个」 | ls-adapters/src 实为 **11** 个适配器（+basedpyright / ty / pyre / jedi） | ❌ 过时 |
| 「基建缺口：文档符号缓存（每次重跑 documentSymbol）」 | symbol_cache Phase 3.1 已建（doc_symbol_cache_key，lib.rs:190-197 区段） | ❌ 过时 |
| 「deps.rs sha256 假校验」 | install.rs 有 sha 门 + UnsignedRefused（Δ 注释），servers.toml 真值锚（头注 2026-09-19 GitHub assets[].digest）；deps.rs 本体本轮未复查 | ⚠️ 部分过时（部分核实） |

## 疑点核销

- 疑点①（coverage doc 过时）：**证实**。v4 数字面全面落后（23→47 子命令、7→11 适配器、已修缺口仍标 ❌/缺）。
- 疑点②（9 错误码守恒）：**否定风险**。42 个工具零新增 wire 码；新增 ToolError 变体折叠进既有码且有 Δ 注释与单测锚定。
- 疑点③（file_detect/doctor/external 破分层）：**否定**。file_detect 置于 ls-registry 且注释写明分层理由；doctor 置于 supervisor、daemon lock 路径由 CLI 注入（不引入 daemon 依赖）；external 全在 ls-registry。唯一图外边 = supervisor→ls-adapters 直连（§0 依赖图未画该边，但铁律三条均未禁）→ 图不完整，非违规。
- 疑点④（ARCHITECTURE §4.2 新段 vs 实现）：**半证实**。§4.2 新增「外部 LS 注册」段与实现逐条一致（路径/静默容错/合并语义/warn/路由回落）；但同节类图与 §4.3 schema 是被 auto-install-design v0.4 取代的旧草案，未回写。

## 【过时文档清单】（需更新的具体段落）

ARCHITECTURE.md：
1. §1 lsp-core 布局：删 diagnostics.rs/symbols.rs（缓存实际在 supervisor/src/lib.rs）；补 error.rs / offsets.rs / recording.rs / workspace_folders.rs。
2. §1 supervisor 布局：tools/ 子目录实为 edit_tools/ref_tools/fs_tools/doctor/root_finder；**doctor 与 file_detect 两个功能在 ARCHITECTURE 全文零提及——「唯一事实源」最大缺口**。
3. §2 token 句（「随机 128-bit hex」）与 lockfile.rs gen_token 二选一对齐（改文案或改实现）。
4. §4.1 trait：补 set_project_root / wait_for_index 两方法。
5. §4.2 类图 ServerSpec 字段 + §4.3 schema 草案：按 auto-install-design v0.4 实装 schema 重写；删「[servers.clangd] T2 也保留条目」句（T2 语言现不进表，扩展名路由归 EXT_TABLE）。
6. §6.1 supervisor 行 ToolError 变体表：{BadArgs, Core, Runtime, WriteConflict, NotInstalled} → 实际 {BadArgs, NotInstalled, Core, WriteConflict, Launch(anyhow), Serialize, Protocol}；anyhow 内嵌一并登记。
7. §6.3 LS_TIMEOUT 行：默认 300s → 30s（普通）/120s（索引类）双轨；300s 标注为 CLI 转发超时。
8. §0 依赖图补 SUP→ADP 边；§0 CLI 句补 doctor/install/file_detect 对 supervisor/ls-registry 的直连路径。
9. 头部锚 43ae0211 → c4dc91a7（或双锚标注 + 指向 local/upstream-sync-2026-09-20.md）。

其他文档：
10. coverage v4 → v5：23→47 子命令、7→11 适配器、completion + wrapper 六缺口 + 文档符号缓存改 ✅、总账与「待做排期」刷新。
11. PLAN.md Task 16/17 与 doctor.rs/file_detect.rs 头注的任务锚位对齐（或改引实际计划文件）。
12. crates/cli/src/main.rs --request-timeout 帮助文本提到 servers.toml「[defaults].timeout_ms」——该表不存在（顶层仅 [servers.*]）。
13. doctor.rs 头注 exit「0 全绿 / 1 有 MISS / 2 致命错」vs `exit_code()` 实产 0/1（"2 致命错"不可达）。
14. DESIGN.md §6 工具表（低优先，演进性过时）。

## 【违规清单】

1.【轻】PLAN 铁律「ARCH §8 技术清单不得增删」：supervisor 引入 `glob = "0.3"`（supervisor/Cargo.toml）；workspace 级 `bytes` 亦未见于 §8 表。属未登记而非滥用，补登记即可。
2.【轻·安全相关】ARCH §2 承诺 token「随机 128-bit hex」；gen_token（lockfile.rs:57-82）为时钟+pid+计数器，可预测。本机威胁模型下风险低（lock 文件本机可读 = token 本可得，校验只防跨会话误连），但文档-实现必须二选一对齐。
3.【观察】transport 层 body 出现 9 码之外的 code：403 body code "FORBIDDEN"（http.rs:61-68）、404 body 借用 "INTERNAL"（http.rs not_found_tool）。ARCH 未定义 transport 层 body schema，不判违规；但 404→INTERNAL 的 retryable=false 语义易误导 agent，建议 404 改用 BAD_ARGS 语义或文档明示 transport body 不属 9 码表。
4.【观察·反向】ls-registry 实际未用 anyhow（spec::parse 返 Result<_, String>），与 §6.1「ls-registry 用 anyhow」不符——比声明更收敛，登记即可。
5.【无】分层铁律三条：manifest 级零违规（lsp-core 传递闭包仅 ls-runtime；axum 仅 daemon；daemon 无任何 LSP crate）。

## 结论

代码忠实于架构精神：铁律、9 码 wire、错误边界、禁入清单、单表配置、外部表静默容错全部成立且新增 42 工具/11 适配器/external/file_detect/doctor 未破坏任何一条契约。「唯一事实源」ARCHITECTURE.md 落后实现约一个里程碑（9 处待回写，其中 doctor/file_detect 整体缺席）；coverage doc v4 需升 v5。建议安排一次纯文档回写轮，按上表 14 条逐项落实。
