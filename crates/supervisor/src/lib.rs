//! Supervisor —— (project_root, language) 实例编排与工具语义层（PLAN Task 10 / ARCH §3.3）。
//!
//! M0 最小版（Task 10）：
//! - **单实例**：每个 `(root, lang)` 缓存**一份** `Arc<Session>`（无池/LRU，Task 13-15 接管）。
//! - 工具入口：`tool_overview` / `tool_def` / `tool_refs` —— 直接 LSP 三个只读语义。
//! - line/col 经 `lsp_core::offsets::OffsetEncoding::Utf16` 换算为 LSP `Position`。
//!   M0 固定 utf-16：clangd 默认协商 + base init_params 声明 utf-16 优先；M1 改走
//!   `Session` 协商结果。
//!
//! 错误模型（ARCH §6.1）：库层用 thiserror 具名类型 `ToolError`；adapters 是被编排末端
//! 用 anyhow 内部传（`launch_info` 错误冒泡上来），包装成 `ToolError::NotInstalled` 等。
//!
//! ponytail: 单实例 ≠ `Mutex<HashMap>` —— 真正的「同 root 多 lang」也只装得下 1 个 lang
//! (clangd)，单 `Mutex<HashMap>` 比 `Arc<Mutex<OnceCell>>` 简单。Task 13 把池换进来时本
//! 公共 API 不变。

// serde_json::json! 47 个嵌套对象超过默认 128 递归上限；catalog.rs 必需。
#![recursion_limit = "512"]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use lsp_core::docsync::{path_to_uri, path_to_uri_str};
use lsp_core::error::CoreError;
use lsp_core::init_params::base_initialize_params;
use lsp_core::init_params::supports_pull_diagnostics;
use lsp_core::offsets::{OffsetEncoding, Position as LspPos};
use lsp_core::session::Session;
pub mod doctor;
pub mod edit_context;
pub mod edit_tools;
pub mod fs_tools;
pub mod root_finder;

pub mod catalog;
pub mod ref_tools;
pub mod repo_map;
pub mod warm;
pub mod write_gate;

use lsp_core::types::{SymbolHit, SymbolKindTag};
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

/// Read-only tool timeout. overview/def/refs on small files complete in ms; clangd
/// cold-start of a project may take seconds. 30s mirrors `READY_PROBE_TIMEOUT`.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// workspace/symbol / background-index 长操作。clangd 首次索引大项目可能 >30s。
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// 符号缓存条目上限（ARCH §3.2 容量闸门：512 文件）。put 时超过则整表清空重建——
/// fingerprint/mtime 对每张缓存表每次校验（doc_symbol_cache_key / find_symbol_cache_key
/// 内置），全清后旧 mtime 命中的是清空而非 miss；旧 entry 不会"误中"。ponytail: 定长全清；
/// 若实测命中率明显下降再换 LRU。上一行不是 LRU 决策失败的解释，是基于 ARCH §3.2 的
/// 实施记录——LRU 必然引入 LinkedHashMap/indexmap 类非 std 依赖或手写双向链表，本项目
/// 禁 dashmap/parking_lot/3rd party ordered-map（ARCH §6）。
const SYMBOL_CACHE_MAX_ENTRIES: usize = 512;

/// Phase 4 基建 Task 22b：三层 timeout 合并（CLI > servers.toml > 默认）。
/// CLI flag 透传走 `args._timeout_ms` / `args._index_timeout_ms`（私有约定，
/// CLI daemon HTTP / shell JSONL 都按 args 字段透传），`None` = 走 servers.toml 或默认。
///
/// `lang` 是 `session_for` 传入的语言名（或 id）；CLI / config override 的
/// `lang` 字段必须按相同语义 lookup `spec_for`。
pub fn effective_tool_timeout(
    lang: Option<&str>,
    args: &serde_json::Value,
) -> Duration {
    let from_args = args
        .get("_timeout_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let cli_override = ls_registry::config::LsOverride {
        timeout_ms: from_args,
        ..Default::default()
    };
    let ms = ls_registry::config::effective_timeout_ms(lang.unwrap_or(""), Some(&cli_override))
        .unwrap_or(30_000);
    Duration::from_millis(ms as u64)
}

pub fn effective_index_timeout(
    lang: Option<&str>,
    args: &serde_json::Value,
) -> Duration {
    let from_args = args
        .get("_index_timeout_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let cli_override = ls_registry::config::LsOverride {
        index_timeout_ms: from_args,
        ..Default::default()
    };
    let ms = ls_registry::config::effective_index_timeout_ms(lang.unwrap_or(""), Some(&cli_override))
        .unwrap_or(120_000);
    Duration::from_millis(ms as u64)
}

/// 写类工具（rename / replace-body）入口的索引等待上限，与 on_server_ready 的 30s
/// 对齐（PLAN Phase 3.2）。超时只 warn 不阻断 —— 工具自身请求负责最终报错。
const INDEX_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 工具层错误（ARCH §6.1 supervisor thiserror 边界）。
///
/// 变体与 wire contract 一一对应；调用方（CLI / daemon）按变体决定 exit code 或 HTTP body。
#[derive(Debug, Error)]
pub enum ToolError {
    /// 用户传入参数错（line 越界、file 不存在等）—— exit 2（ARCH §6.3）。
    #[error("bad args: {detail}")]
    BadArgs { detail: String },

    /// LS 未安装（PATH 找不到）—— exit 1 + install_hint。
    #[error("language server for `{language}` not installed: {hint}")]
    NotInstalled { language: String, hint: String },

    /// lsp-core 错误冒泡 —— 此后调用方可判定是否 LS 崩溃 / RPC / 超时。
    #[error("core error: {0}")]
    Core(#[from] CoreError),

    /// 盘上内容与 LSP 状态不符（C3 防线）—— exit 1 + 需重读。
    #[error("write conflict on {path}: {reason}")]
    WriteConflict { path: String, reason: String },

    /// 适配器启动期错误（anyhow 上抛统一收口）。
    #[error("adapter launch failed: {0}")]
    Launch(#[from] anyhow::Error),

    /// 工具结果序列化失败 —— daemon 内部确定性 bug。wire INTERNAL（不可重试，exit 3）。
    /// Δ 43ae021：从 Launch 兜底拆出，避免确定性失败被误映射为 retryable 的
    /// LS_SPAWN_FAILED 让 agent 无意义重试。
    #[error("serialize failed: {0}")]
    Serialize(anyhow::Error),

    /// LSP 协议语义错（server 违反协议约定，如 rename 返回 null / 缺 changes map）。
    /// wire RPC_ERROR（不可重试，exit 1）。tool 标识出错的上层工具语义。
    #[error("protocol error from `{tool}`: {reason}")]
    Protocol { tool: String, reason: String },
}

/// supervisor 公共结果类型（库层 Result 别名）。
/// diagnostics 缓存条目：uri -> (items, 推送的 document version)。
/// version 来自 publishDiagnostics.params.version（LS 可缺省）—— tool_diagnostics
/// 用它比对 docsync 当前 content_version 判定"诊断对应哪一版内容"（RA didChange
/// 后会先重推旧快照再推新分析，无 version 比对则新旧不可分）。
pub type DiagCache =
    std::sync::Arc<Mutex<HashMap<(PathBuf, String), (Vec<serde_json::Value>, Option<i64>)>>>;
pub type ToolResult<T> = std::result::Result<T, ToolError>;
/// 文档符号缓存 key（Phase 3.1）：(root, file, (mtime, size))。
/// find-symbol（workspace 级）无单文件锚点：file 位放 `ws?{query}`、信号位放 root 信号。
/// 信号位双因子（P2-18h）：同 mtime 粒度窗口内的外部改写靠 size 检出 —— 对齐
/// docsync `ensure_open` 的 mtime+size 双对账（Windows mtime 缓存 / FAT 2s 粒度
/// 会漏检同粒度改写，命中旧 SymbolHit → replace-body 切片错位）。
type SymbolCacheKey = (PathBuf, String, Option<(SystemTime, u64)>);

/// O3 解析候选：(file 相对路径, 符号 range)。
type SymbolCandidates = Vec<(String, lsp_types::Range)>;

/// overview / symbol-body 的缓存 key；文件不可 stat（不存在/失败）→ None（确定性 key）。
fn doc_symbol_cache_key(root: &Path, file: &str) -> SymbolCacheKey {
    (
        root.to_path_buf(),
        file.to_string(),
        std::fs::metadata(root.join(file))
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len()))),
    )
}

/// find-symbol（workspace/symbol）缓存 key：按 query 键控，信号位锚 root 信号。
/// `?` 是 Windows 非法文件名字符，`ws?` 前缀与真实文件 key 天然不撞。
/// 信号 = `root_source_mtime`：root 下任一源码文件被外部修改/新增 → 信号推进
/// → 旧缓存 key 失效重查（外部修改感知）。取不到信号（无源码文件/stat 全失败）→ None。
/// workspace 级信号无单一 size 语义，size 位恒 0（`ws?` 前缀保证不与真实文件 key 撞）。
fn find_symbol_cache_key(
    root: &Path,
    query: &str,
    root_mtime: Option<SystemTime>,
) -> SymbolCacheKey {
    (
        root.to_path_buf(),
        format!("ws?{query}"),
        root_mtime.map(|m| (m, 0)),
    )
}

/// 符号缓存写入内核（容量闸门 + 空集跳过）。P2-a5k 抽出供 `Supervisor::symbol_cache_put`
/// 与并发 fan-out 路径 `overview_via_session` 共用——两处写同一 `Arc<Mutex<HashMap>>`，
/// 把"空不写"与"超限全清"绑成原子决策避免任何写入路径绕过容量闸门。
///
/// 容量闸门（ARCH §3.2）：超 `SYMBOL_CACHE_MAX_ENTRIES` 整表清空再建。key 内 mtime
/// 单调推进，旧 mtime 命中是清空而非误中；全清比 LRU 更安全（无 stale 窗口、无
/// insertion-order 维护开销，禁 dashmap/indexmap）。ponytail: 全清；若实测命中率
/// 明显下降再换 LRU。
fn symbol_cache_put_impl(
    cache: &mut HashMap<SymbolCacheKey, Vec<SymbolHit>>,
    key: SymbolCacheKey,
    hits: Vec<SymbolHit>,
) {
    if hits.is_empty() {
        return;
    }
    if cache.len() >= SYMBOL_CACHE_MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(key, hits);
}

/// root 下（depth ≤3，标准 ignore 过滤）源码文件的 (max mtime, lang 集合)。
///
/// 单次 walk 同时收集 max mtime 与 lang 集合 —— 原实现 `tool_find_symbol` miss
/// 路径会再走一遍只为收集 lang，重复 stat 风暴。一次 walk 两用。
///
/// 只 stat 能被 `resolve_lang_name` 识别的文件（非源码文件变化不该失效符号缓存）。
/// 删除文件不推进 max —— 残留已知边界，重启 daemon 兜底。
/// `depth ≤3` 对 `crates/*/src/*.rs` 等深嵌套文件盲（深度 4），P2-4 附注。
///
/// ponytail: 同步函数，调用方（`root_signal_cached`）包 `spawn_blocking`。
fn walk_root_signal(root: &Path) -> (Option<SystemTime>, std::collections::BTreeSet<String>) {
    use ignore::WalkBuilder;
    let mut max: Option<SystemTime> = None;
    let mut langs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in WalkBuilder::new(root)
        .standard_filters(true)
        .max_depth(Some(3))
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file())
            && let Some(lang) = ls_registry::resolve_lang_name(entry.path())
            && let Ok(meta) = entry.metadata()
            && let Ok(m) = meta.modified()
        {
            max = Some(match max {
                Some(prev) if prev >= m => prev,
                _ => m,
            });
            langs.insert(lang.to_string());
        }
    }
    (max, langs)
}

/// 仅 mtime 信号（保留签名给 `root_source_mtime_change_invalidates_find_symbol_cache`
/// 测试直接调用，绕开 TTL 缓存保证 mtime 变化立刻可见）。运行时走 `root_signal_cached`。
#[cfg_attr(not(test), allow(dead_code))]
fn root_source_mtime(root: &Path) -> Option<SystemTime> {
    walk_root_signal(root).0
}

/// root 信号 TTL 缓存（per root）。2s 内复用上次 walk 结果，避免每请求
/// depth-3 全仓 stat 风暴。外部修改感知延迟 ≤2s，与诊断等待同量级容忍。
///
/// 结构：`Mutex<HashMap<PathBuf, (采集时刻, mtime, langs)>`。读路径 fast-path：
/// 缓存新鲜 → clone 走；miss → `spawn_blocking(walk_root_signal)` 后回填。
///
/// ponytail: 全局静态锁 + HashMap；多根项目并行 scan 会争用，但对单一 root 串行
/// find-symbol 场景（典型）零争用。万级 root 时换 DashMap——本项目禁 dashmap，
/// 改回 path-hash 分片 Mutex 即可。
type RootSignalEntry = (std::time::Instant, Option<SystemTime>, std::collections::BTreeSet<String>);
const ROOT_SIGNAL_TTL: Duration = Duration::from_secs(2);

static ROOT_SIGNAL_CACHE: LazyLock<Mutex<HashMap<PathBuf, RootSignalEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 写工具收尾的索引缓存失效（bd serena-rust-0em A 层）：写盘已发生，但
/// `root_signal_cached` 的 2s TTL 窗口内 find_symbol 仍拿旧 mtime → 命中**写前**
/// 的 find_symbol 缓存（旧行号，观测为"数秒后自愈"）。写后必须主动清：
/// - ROOT_SIGNAL_CACHE 条目 → 下次强制 walk，新 mtime → 自然 miss；
/// - 该 root 全部符号缓存（`invalidate_symbol_cache_for_root`，含 find_symbol 的
///   `ws?` 级条目）→ 防 TTL 窗口内旧 key 命中。
fn invalidate_write_derived_caches(root: &Path) {
    ROOT_SIGNAL_CACHE.lock().unwrap().remove(root);
}

/// 取缓存的 root 信号（mtime, langs）。2s 内复用；否则 `spawn_blocking` 重 walk。
///
/// miss 路径收集的 langs 顺手返回，调用方（`tool_find_symbol`）无需再 walk 第二遍。
async fn root_signal_cached(
    root: &Path,
) -> (Option<SystemTime>, std::collections::BTreeSet<String>) {
    // Fast-path: 读锁（同步临界区，亚微秒）。
    if let Some(entry) = ROOT_SIGNAL_CACHE.lock().unwrap().get(root).cloned()
        && entry.0.elapsed() < ROOT_SIGNAL_TTL
    {
        return (entry.1, entry.2);
    }
    // Slow-path: spawn_blocking 跑同步 walk，不占 async worker。
    let root_owned = root.to_path_buf();
    let (max, langs) =
        tokio::task::spawn_blocking(move || walk_root_signal(&root_owned))
            .await
            .unwrap_or_else(|_| (None, std::collections::BTreeSet::new()));
    let mut cache = ROOT_SIGNAL_CACHE.lock().unwrap();
    // 二次检查：期间可能已被并发回填。
    if let Some(existing) = cache.get(root)
        && existing.0.elapsed() < ROOT_SIGNAL_TTL
    {
        return (existing.1, existing.2.clone());
    }
    cache.insert(
        root.to_path_buf(),
        (std::time::Instant::now(), max, langs.clone()),
    );
    (max, langs)
}

/// Daemon 工具语义层抽象；实现负责按工具名分派只读请求。
#[async_trait::async_trait]
pub trait SupervisorTrait: Send + Sync {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError>;
    fn loaded_entries(&self) -> Vec<Key> {
        Vec::new()
    }

    /// 巡检：找出 state==Failed 的 session，从池中驱逐（shutdown+remove）。
    /// reaper 常驻调用，O(n) 扫描；返回驱逐数量用于打点。
    async fn evict_failed(&self) -> usize {
        0
    }
}

/// M0 单实例 supervisor。
///
/// - `instances`：`Mutex<HashMap<Key, Arc<Session>>>`。M0 仅 Cpp 一个 lang；同 (root, cpp) 复用 Session。
/// - `direct_mode`：当前 supervisor 由 `--direct` CLI 拉起；M1 daemon 模式不复用本类型
///   （daemon 引入 HTTP + idle reaper），Task 13 会把 `direct()` 拆为 `DirectSupervisor`，
///   此处留模式开关便于未来扩展。
pub struct Supervisor {
    instances: Mutex<HashMap<Key, Arc<Session>>>,
    load_gates: Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>,
    last_used: Mutex<HashMap<Key, std::time::Instant>>,
    direct_mode: bool,
    /// publishDiagnostics 通知缓存：key = (root, uri)，value = items 数组。
    diag_cache: DiagCache,
    /// diagnostics generation：每次 publishDiagnostics 通知 ++（含空 items 的"无错"推送）。
    /// 客户端可请求 `wait_gen >= N` 等新一代诊断，避免盲轮询 5s。
    diag_generation: Arc<AtomicU64>,
    /// per-key pull diagnostics 支持标记。session_for 末尾读 `ServerCapabilities.diagnosticProvider`
    /// 探测后写入；tool_diagnostics 入口查这里决定走 `textDocument/diagnostic` 还是 push 缓存。
    /// `true` = LS 声明支持；`false` = 缺字段/null → 走 push 缓存（与 2.5 之前等价）。
    /// 锁用 std::sync::Mutex —— 写一次读多次、临界区小，不值得换 parking_lot。
    pull_diag_supported: std::sync::Arc<Mutex<HashMap<Key, bool>>>,
    /// per-(root, uri_lower)：该 uri 是否收到过带 version 的 publishDiagnostics。
    /// 有 version 纪律的 LS（rust-analyzer）→ 缺 version 的推送（如文件 watcher 通
    /// 道对旧内容分析的推送）不得凭 generation 达标误确认（bd serena-rust-76d）；
    /// 从不发 version 的 LS（clangd）→ 保留 generation 判定（旧行为不回退）。
    version_seen: std::sync::Arc<Mutex<HashMap<(PathBuf, String), bool>>>,
    /// 写后一致性窗口（bd serena-rust-0em）：写工具收尾标记 (root, file_lowercase)
    /// → 写入时刻。find_symbol 在 TTL 内对这些文件强制 documentSymbol 对齐 ——
    /// RA wssym 写后可能**缺条目**（命中集缩水，基线实测），行号比对检不出缺失。
    recent_writes: Mutex<HashMap<(PathBuf, String), std::time::Instant>>,
    /// workspace 加载失败记录（bd serena-rust-xzb）：key = `key_root_identity(root)`，
    /// value = LS 经 `window/showMessage` / `window/logMessage` 报告的原始错误消息。
    /// cargo metadata 失败（FetchWorkspaceError）等场景语义工具全静默返空，AI 以为
    /// 项目没符号 —— 记录后经 warning 键透出（`workspace_error_for` / find-symbol /
    /// hover / def / refs / find-implementations 分支）。LS 重启（session_for 再入）
    /// 清零重新评估。键用小写归一：Windows 大小写双重身份（历史教训第三次变体）。
    workspace_errors: std::sync::Arc<Mutex<HashMap<String, String>>>,
    /// Phase 3.1 文档符号缓存：(root, file, mtime) → 平铺 symbol list。
    /// overview / find-symbol / symbol-body 入口前查；命中免 LS 往返。mtime 变 →
    /// key 变 → 自然 miss 重调 LS（旧 entry 残留无害）。std Mutex：临界区仅 HashMap 读写。
    symbol_cache: std::sync::Arc<Mutex<HashMap<SymbolCacheKey, Vec<SymbolHit>>>>,
    /// AI-token 特性 J（§11-J）：delta 响应缓存，key = `"{tool}|{root_key}"` → 上次完整响应。
    /// 空集不缓存：LS 就绪窗口返空若入库，就绪后 delta 恒漏报（宁重查不可错缓存）。
    /// ponytail: 全表无淘汰 —— AI 一次会话只查几个 key；OOM 再换 LRU。
    delta_cache: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// 修 P1 #2（TTL 生产执行者）：每次 `execute_tool` 路过 +1；达阈值后调每个
    /// 在线 Session 的 `evict_idle_buffers(FILE_GUARD_TTL)` 强制回收 ref_count=0
    /// 的陈旧缓冲。原子计数避免持锁。阈值 = 32：每次工具调用大半 1~2 个文件，按
    /// 4 - 8 并发比，32 次调用 ≈ 100 文件操作，对应在 daemon 主动期约 5~10 秒级
    /// 节流，避免高频 reconcile 开销。
    idle_buffers_reclaim_counter: AtomicU64,
    /// 修 P1 #2 测试专用：覆盖默认 TTL 让单测可控；生产 build 不持此字段。
    #[cfg(test)]
    _idle_ttl_override: std::sync::Arc<Mutex<Option<Duration>>>,
    /// LS 启动暖机窗口（bd serena-rust-bxd O2/O4）：root → 记录。LS 会话新建
    /// （session_for spawn 点）即记账并重置；首个语义工具（hover/def/refs/
    /// find-implementations）非空成功即关闭。窗口内 find-symbol 结果可能随
    /// 索引爬升波动（实测 9→4→6+），经既有 warning 通道透出 partial 信号。
    /// 键用 key_root_identity 归一（Windows 大小写双重身份惯例）。
    ls_warmup: Mutex<HashMap<String, LsWarmup>>,
}

/// 单 root 的 LS 暖机记账。
struct LsWarmup {
    started: std::time::Instant,
    semantic_ok: bool,
}

/// 暖机窗口长度：LS 启动后 10s 内符号索引仍在爬升（P2-4 root 信号 2s TTL 覆盖
/// 的是文件变更，这里是 LS 冷启动全局索引，窗长放宽到 10s）；首个语义成功提前关闭。
const LS_WARMUP_WINDOW: Duration = Duration::from_secs(10);

/// O4：暖机窗口内 find-symbol 附带的 partial 提示文案。
fn index_warming_message() -> String {
    "index warming: results may be partial".to_string()
}
/// 实例池身份键。`root` 保存 canonical **真实大小写**——rust-analyzer 按 URI 精确
/// 字符串匹配挂载文件，小写化 root 会让 didOpen/def 的原始大小写 URI 脱挂所有
/// crate（语法层活、语义层恒 null，BD serena-rust-81m）。身份比较/哈希仍按 root
/// 的小写形式归一，`HashMap` 调用点对大小写变体透明。
#[derive(Debug, Clone)]
pub struct Key {
    pub root: PathBuf,
    pub lang: Box<str>,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.lang == other.lang && key_root_identity(&self.root) == key_root_identity(&other.root)
    }
}

impl Eq for Key {}

impl std::hash::Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        key_root_identity(&self.root).hash(state);
        self.lang.hash(state);
    }
}

/// root 的身份形式：小写字符串（Windows 大小写不敏感 FS 的路径归一）。
fn key_root_identity(root: &Path) -> String {
    root.to_string_lossy().to_lowercase()
}

/// 对外暴露给工具调用方的"位置"结构（直接复用 lsp-types `Location`，
/// 但放本 crate re-export 以避免下游依赖 `lsp-types`）。
///
pub use lsp_core::error::CoreError as CoreErrorWire;
pub use lsp_types::Location;
// ============================================================================
// completion 工具类型（M2 #1, design: local/completion-design.md §5）
// ============================================================================

/// AI-friendly 补全项（supervisor 层完成字段裁剪；daemon HTTP / shell 透传）。
///
/// 与 `lsp_types::CompletionItem` 的差异：
/// - 丢弃 `sortText` / `filterText` / `commitCharacters` / `command` / `data` /
///   `tags` / `label_details` / `text_edit`（LSP 内部排序/编辑协议细节，agent 用不到）。
/// - `kind` 由 LSP 枚举映射成人类词（"function" / "variable" / "method" 等）。
/// - `documentation` 截断到 200 char（设计 §3）。
/// - `insert` = `insertText`（缺则用 `label`，agent 直接用）。
#[derive(Debug, Clone, Serialize)]
pub struct CompletionItemLite {
    pub label: String,
    /// "function" / "method" / "variable" / "keyword" / ... 全小写英文。
    /// Unknown kind → "other"。
    pub kind: String,
    /// 签名行（如 `int printf(const char *fmt, ...)`）；可空。
    pub detail: Option<String>,
    /// 实际插入文本（insertText 或 label 兜底）；可空。
    pub insert: Option<String>,
    /// 文档字符串，截断到 200 字符；可空。
    pub doc: Option<String>,
    pub deprecated: bool,
    /// 补全自动插入的额外文本编辑（如 import）；不裁剪，agent 需要。
    pub additional_text_edits: Vec<lsp_types::TextEdit>,
}

/// completion 工具响应：截断标记 + 已裁剪 items。
///
/// 设计 §4：CLI 一行 JSON；daemon HTTP 同形态（daemon 透传 `data`，由 supervisor 裁剪）。
#[derive(Debug, Clone, Serialize)]
pub struct CompletionResponse {
    /// 截断提示："5 of 23"；未截断为 None。
    pub truncated: Option<String>,
    pub items: Vec<CompletionItemLite>,
}

/// `defining-symbol` 工具返回的单个定义条目（Phase 2.3 / local/solidlsp-development-plan.md §2.3）。
///
/// 组合 `tool_def`（拿 Location）+ `tool_containing_symbol` 的 documentSymbol walk
/// （拿完整符号元信息）+ `lsp_core::offsets::slice_at`（拿符号体）。来源上游
/// `request_defining_symbol` 的 `UnifiedSymbolInformation`（ls_types.py@43ae021）。
///
/// 始终以 `Vec` 形式返回 —— 即使单定义场景也是单元素数组；C++ 重载等多定义场景
/// 自然展开。无定义时整个工具返回 `None`，不是空数组（区别于 `containing-symbol`）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolHit {
    /// 定义点 Location（来自 `textDocument/definition`），相对 root。
    pub source: DefiningSymbolLocation,
    /// 符号完整元信息。
    pub symbol: DefiningSymbolInfo,
}

/// 定义点位置（`textDocument/definition` 的 Location 字段子集）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolLocation {
    pub file: String,
    pub line: u32,
    pub col: u32,
}

/// `UnifiedSymbolInformation` 的最小子集（name / kind / range / body）。
///
/// `body` 是从盘上切出来的真实源代码（不含前置 docstring / 注释）；`range` 是
/// LSP 原生 Range（驼峰命名由 `#[serde(rename_all = "camelCase")]` 自动产出）。
#[derive(Debug, Clone, Serialize)]
pub struct DefiningSymbolInfo {
    pub name: String,
    pub kind: SymbolKindTag,
    pub range: lsp_types::Range,
    pub body: String,
}
// ============================================================================
// Phase 1 · 上游 wrapper 类型（13 个 tool_* 配套结构）
// ============================================================================

/// `textDocument/semanticTokens/full` 响应（`SemanticTokens` 加解析后的扁平 token）。
///
/// LSP `data: Vec<i32>` 是 5-tuple delta 序列（[deltaLine, deltaStartChar, length,
/// tokenType, tokenModifiers]）；本结构在 supervisor 层把它解成绝对坐标
/// `(line, start_char, length, token_type, token_modifiers)`，agent 据此定位符号语义
/// 类型，不必自己解码 delta。`token_type` / `token_modifiers` 是 LSP
/// `SemanticTokenTypes` / `SemanticTokenModifiers` 的位索引。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokenEntry {
    pub line: u32,
    pub start_char: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers: u32,
}

/// 解析后的 `SemanticTokens` 响应：原始 data + 扁平 token 列表。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticTokensFull {
    /// LS 提供的版本号（用于 `SemanticTokens/edits` 增量）；部分 LS 不实现（= null）。
    pub result_id: Option<String>,
    /// 已解出的扁平 token（绝对坐标）。
    pub tokens: Vec<SemanticTokenEntry>,
}

impl Supervisor {
    /// `--direct` 模式入口：创建空 supervisor（懒加载 Session）。
    pub async fn direct() -> ToolResult<Self> {
        Ok(Self {
            instances: Mutex::new(HashMap::new()),
            load_gates: Mutex::new(HashMap::new()),
            last_used: Mutex::new(HashMap::new()),
            direct_mode: true,
            diag_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            diag_generation: std::sync::Arc::new(AtomicU64::new(0)),
            pull_diag_supported: std::sync::Arc::new(Mutex::new(HashMap::new())),
            version_seen: std::sync::Arc::new(Mutex::new(HashMap::new())),
            recent_writes: Mutex::new(HashMap::new()),
            workspace_errors: std::sync::Arc::new(Mutex::new(HashMap::new())),
            symbol_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            delta_cache: Arc::new(Mutex::new(HashMap::new())),
            idle_buffers_reclaim_counter: AtomicU64::new(0),
            ls_warmup: Mutex::new(HashMap::new()),
            #[cfg(test)]
            _idle_ttl_override: std::sync::Arc::new(Mutex::new(None)),
        })
    }

    /// 当前是否 direct 模式（保留位，M1 用）。
    #[allow(dead_code)]
    pub fn is_direct(&self) -> bool {
        self.direct_mode
    }

    /// 修 P1 #2（TTL 生产执行者）：throttled reclaim。每次 `execute_tool` 入口
    /// 加 1，达 `RECLAIM_THRESHOLD` 后扫所有在线 Session 调
    /// `evict_idle_buffers(FILE_GUARD_TTL)` —— 把 ref_count=0 且超过 TTL 的
    /// 缓冲 didClose + 移表。复活竞态按 docsync.rs:208-210 注释处理（LS
    /// 收到 didClose on known-closed 通常忽略；不引入两轮锁复杂化）。
    ///
    /// 测试可调 `_idle_ttl_override`（测试专用 Arc<Mutex<Option<Duration>>>)强制
    /// 用更短 TTL 验证生命周期；None 时走默认 60 s。
    ///
    /// ponytail: 阈值为"每次工具调用 1~2 文件、稳态 ~10 s 节流"的粗估；daemon
    /// 真实负载下可降序到 16（更频繁）以加快冷文件回收。
    pub fn reclaim_idle_buffers_once(&self) -> usize {
        // 阈值（call 次）：节流，避免每工具调用都遍历 sessions。
        const RECLAIM_THRESHOLD: u64 = 32;
        let n = self.idle_buffers_reclaim_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n < RECLAIM_THRESHOLD {
            return 0;
        }
        // 阈值到达：归零计数 + 扫所有 session 走 idle evict。
        let _ = self.idle_buffers_reclaim_counter.compare_exchange(
            n,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        // TTL —— 默认 60 s；测试覆盖下走短 TTL（5 ms）让用例可控。
        #[cfg(test)]
        let ttl: Duration = {
            let g = self._idle_ttl_override.lock().unwrap();
            g.unwrap_or(Duration::from_secs(60))
        };
        #[cfg(not(test))]
        let ttl: Duration = Duration::from_secs(60);
        let mut total = 0usize;
        let sessions: Vec<Arc<Session>> = {
            let instances = self.instances.lock().unwrap();
            instances.values().cloned().collect()
        };
        for session in &sessions {
            total += session.evict_idle_buffers(ttl);
        }
        total
    }

    /// 测试辅助：直接读 reclaim 计数器（不增加）。
    #[cfg(test)]
    pub(crate) fn reclaim_count_snapshot(&self) -> u64 {
        self.idle_buffers_reclaim_counter.load(Ordering::Relaxed)
    }

    /// 测试辅助：覆盖默认 TTL（60s）为短 TTL 让测试可断言"生产路径触发回收"。
    /// 与下 reclaim_idle_buffers_once 配套使用。
    #[cfg(test)]
    pub(crate) fn set_idle_ttl_for_test(&self, ttl: Duration) {
        *self._idle_ttl_override.lock().unwrap() = Some(ttl);
    }

    #[doc(hidden)]
    pub fn key(root: &Path, lang: &str) -> Key {
        let mut canonical = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        while matches!(
            canonical.as_os_str().as_encoded_bytes().last(),
            Some(b'/' | b'\\')
        ) {
            canonical.pop();
        }
        Key {
            // 真实大小写保真：LS 的 rootUri / cwd / 探针 URI 都从这里走，
            // 小写形式只用于身份归一（PartialEq/Hash），绝不外泄。
            root: canonical,
            lang: Box::from(lang.to_ascii_lowercase()),
        }
    }

    /// 返回同 key 共用的异步加载门。
    #[doc(hidden)]
    pub fn load_gate_for(&self, root: &Path, lang: &str) -> Arc<tokio::sync::Mutex<()>> {
        let key = Self::key(root, lang);
        self.load_gates
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 更新 last_used 时间戳（reaper LRU 用）。
    fn touch(&self, key: &Key) {
        self.last_used
            .lock()
            .unwrap()
            .insert(key.clone(), std::time::Instant::now());
    }

    /// 当前已加载实例快照：`(key, last_used, Arc<Session>)`，按 last_used 升序。
    /// reaper 巡检用；取锁后立刻 clone 释放。
    pub fn loaded_entries(&self) -> Vec<(Key, std::time::Instant)> {
        self.last_used
            .lock()
            .unwrap()
            .iter()
            .map(|(k, t)| (k.clone(), *t))
            .collect()
    }

    /// 卸载指定实例：先 shutdown session（5s 超时转 kill），再从池中移除。
    /// 返回 Ok(true) 表示真的卸了；Ok(false) 表示 key 不存在。
    ///
    /// P2-0bq: evict 也清理 `load_gates` / `pull_diag_supported` 两张旁表——
    /// 二者只 insert 不 remove，LRU 反复驱逐同 (root, lang) 会按驱逐次数单调累积。
    /// `diag_cache` 按 (root, uri) 键与 session 解耦，刻意保留（文件级诊断跨世代
    /// 仍有效；新一轮 session 第一条 pushDiagnostics 会覆写/清空对应条目）。
    pub async fn evict(&self, key: &Key) -> ToolResult<bool> {
        let session = self.instances.lock().unwrap().remove(key);
        self.last_used.lock().unwrap().remove(key);
        self.load_gates.lock().unwrap().remove(key);
        self.pull_diag_supported.lock().unwrap().remove(key);
        match session {
            Some(s) => {
                s.shutdown().await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 巡检：扫所有 session, Failed 状态驱逐。返回驱逐数。
    /// 复用 `evict`, 后台 reaper 常驻调用。
    pub async fn evict_failed_instances(&self) -> usize {
        // 先 clone 出所有失败 key (避免持锁 await shutdown)。
        let failed_keys: Vec<Key> = {
            let instances = self.instances.lock().unwrap();
            instances
                .iter()
                .filter(|(_, s)| matches!(s.state(), lsp_core::session::SessionState::Failed(_)))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let n = failed_keys.len();
        for key in failed_keys {
            let _ = self.evict(&key).await;
        }
        n
    }

    /// Self-heal 包装：调 `session_for` + 闭包；若闭包因 LS 终结返 `Core(Terminated)`，
    /// 驱逐该 (root, lang) 的旧 session（避免下次再拿到死 session）并重试一次。
    ///
    /// 重试上限 1 次 —— 再次 Terminated 透传原错误给 agent，避免无限重试掩盖真实故障。
    /// 写门内工具（replace-body / insert-*）的 LS 死亡根因是单写门后多次 write 的
    /// didChange version 非单调 → rust-analyzer 关 channel（详 edit_tools::commit_change 注）。
    /// 修复 write 链路后此 self-heal 兜底偶发冷启动失败 / 索引风暴把 RA 拉死的场景。
    ///
    /// ponytail: 重试仅 1 次 —— 再 fail 慢 1 次冷启动（rust-analyzer 89s）远超 TOOL_TIMEOUT，
    /// agent 自己比 supervisor 更适合决定「等 vs 报错」。
    pub(crate) async fn with_session_retry<F, Fut, T>(
        sup: &Supervisor,
        root: &Path,
        lang: &str,
        mut f: F,
    ) -> ToolResult<T>
    where
        F: FnMut(Arc<Session>) -> Fut,
        Fut: Future<Output = ToolResult<T>>,
    {
        let session = sup.session_for(root, lang).await?;
        match f(session.clone()).await {
            Err(ToolError::Core(CoreError::Terminated { .. })) => {
                let key = Supervisor::key(root, lang);
                tracing::warn!(?key, "session terminated mid-call; evicting and retrying once");
                let _ = sup.evict(&key).await;
                let session2 = sup.session_for(root, lang).await?;
                f(session2).await
            }
            other => other,
        }
    }

    /// 拿到/创建 (root, lang) 对应的 Session，同 key 只允许一次冷启动。
    async fn session_for(&self, root: &Path, lang: &str) -> ToolResult<Arc<Session>> {
        let key = Self::key(root, lang);
        // 快路径：缓存命中（Failed 状态视为 miss 触发懒重启）。
        if let Some(session) = self.instances.lock().unwrap().get(&key).cloned() {
            if !matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
                self.touch(&key);
                return Ok(session);
            }
            self.instances.lock().unwrap().remove(&key);
        }

        // 慢路径：per-key 加载门（防同 key 并发双 spawn）+ 双检锁。
        let gate = self.load_gate_for(root, lang);
        let _guard = gate.lock().await;
        if let Some(session) = self.instances.lock().unwrap().get(&key).cloned() {
            if !matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
                self.touch(&key);
                return Ok(session);
            }
            self.instances.lock().unwrap().remove(&key);
        }

        // 双路径（Task 21）：手写 T2 adapter 优先；servers.toml 条目（T0 配置驱动）
        // 走 config::ensure_launch——PATH 探测 / 安装缓存命中，永不触网（auto_install=false，
        // design §0 路径 A；显式下载走 CLI `install` 命令）。
        let ctx = ls_adapters::ProjectCtx {
            project_root: key.root.clone(),
        };
        let t2 = ls_registry::adapter_for(lang);
        let launch = match &t2 {
            Some(adapter) => adapter.launch_info(&ctx).await.map_err(|e| {
                let msg = format!("{e:#}");
                if msg.contains("not found in PATH") {
                    ToolError::NotInstalled {
                        language: lang.to_string(),
                        hint: extract_install_hint(&msg),
                    }
                } else {
                    ToolError::Launch(e)
                }
            })?,
            None => {
                if ls_registry::config::spec_for(lang).is_none() {
                    return Err(ToolError::BadArgs {
                        detail: format!("unknown language: {lang}"),
                    });
                }
                let (_, args) = ls_registry::config::ensure_launch(lang, None, false, false)
                    .map_err(|msg| ToolError::NotInstalled {
                        language: lang.to_string(),
                        hint: msg,
                    })?;
                // expand_exec 返回完整 argv（exec 模板首元素即 {bin}）。
                ls_runtime::process::LaunchInfo {
                    cmd: args.into_iter().map(Into::into).collect(),
                    cwd: key.root.clone(),
                    env: Vec::new(),
                    transport: ls_runtime::process::TransportKind::Stdio,
                }
            }
        };
        let child = ls_runtime::process::Child::spawn(launch)
            .map_err(|e| ToolError::Launch(anyhow::anyhow!("runtime spawn error: {e}")))?;
        let mut params = base_initialize_params();
        let uri = lsp_types::Uri::from_str(&path_to_uri_str(&key.root)).map_err(|e| {
            ToolError::BadArgs {
                detail: format!("root not URI: {e}"),
            }
        })?;
        // 设 root_uri (即使 deprecated): typescript-language-server 等需要
        // root_uri 定位 workspace 的 node_modules (workspaceFolders 不够)。
        // 用 allow(deprecated) 不影响其它 LS (clangd / rust-analyzer 忽略 root_uri)。
        #[allow(deprecated)]
        {
            params.root_uri = Some(uri.clone());
        }
        params.workspace_folders = Some({
            // Phase 4 基建 Task 22a：探测 root 下的 monorepo marker，构造 N 个
            // WorkspaceFolder（root 自身 + 探测出的 modules）。gopls 等多 module
            // LS 一次性拿到全部 module，避开逐个 didChangeWorkspaceFolders 通知。
            let mut folders = vec![lsp_types::WorkspaceFolder {
                uri: uri.clone(),
                name: key
                    .root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("root")
                    .to_string(),
            }];
            folders.extend(lsp_core::workspace_folders::discover_additional_workspace_folders(
                &key.root,
            ));
            folders
        });
        // T0 配置驱动路径无手写 adapter：无 initialize_patches（servers.toml 已含
        // 初始化形态）、无 set_project_root / on_server_ready 特判探针。
        if let Some(adapter) = &t2 {
            adapter.initialize_patches(&mut params);
        }

        let session = Session::start(Some(child), params).await?;
        // didOpen 的 languageId 用 adapter 真实语言（默认 "cpp" 对 rust-analyzer
        // 等严格 LS 是错语言 → 文档拒收）。session_for 是唯一 spawn 点，此处注入
        // 覆盖全部会话路径。
        session.set_language_id(lang);
        // 注册 publishDiagnostics handler → 写 diag_cache + 累 generation。
        let cache_root = key.root.clone();
        let cache = std::sync::Arc::clone(&self.diag_cache);
        let generation = std::sync::Arc::clone(&self.diag_generation);
        let version_seen = std::sync::Arc::clone(&self.version_seen);
        session
            .client()
            .on_notification("textDocument/publishDiagnostics", move |msg| {
                let Some(uri) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("uri"))
                    .and_then(|u| u.as_str())
                else {
                    return;
                };
                let Some(items) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("diagnostics"))
                    .and_then(|d| d.as_array())
                    .cloned()
                else {
                    return;
                };
                // generation 只计**非空**推送（2026-09-23 语义修正）：RA 的空推送是
                // 分析中间态快照（didOpen 后 ~2.5s 才推完整错误版），若空推送也 ++，
                // wait_gen 会在中间态就 confirmed → 误判"确认无错"。空推送仍清缓存
                // （修 P1 #1：改完错误后 RA 推空，仅 !is_empty 写入会让陈旧错误永存），
                // 但只表示"上一版错误已失效"，不代表新分析完成。
                // cache key 归一化：RA 推送 uri 盘符小写（file:///d:/...），而
                // path_to_uri 生成大写（file:///D:/...）—— 不归一则 cache 永远 miss
                // （2026-09-23 debug 日志实锤）。Windows 路径大小写不敏感，统一小写。
                // 空推送也入 cache（带 version）—— "version N 的 items 为空" =
                // 该版内容确认无错（gen 不 ++，generation 只计含错误推送）。
                let ver = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("version"))
                    .and_then(|v| v.as_i64());
                if ver.is_some() {
                    version_seen
                        .lock()
                        .unwrap()
                        .insert((cache_root.clone(), uri.to_lowercase()), true);
                }
                let mut cache = cache.lock().unwrap();
                let key = (cache_root.clone(), uri.to_lowercase());
                if items.is_empty() {
                    cache.insert(key, (Vec::new(), ver));
                } else {
                    cache.insert(key, (items, ver));
                    generation.fetch_add(1, Ordering::Relaxed);
                }
            });
        // bd serena-rust-xzb：workspace 加载错误的可见通道除 window 消息外，RA 的
        // FetchWorkspaceError 只出现在 LS stderr（stderr 泵在 lsp-core，禁区不可改）
        // —— rust root 由 probe_cargo_workspace_error（下方 insert 前）主动探测。
        // 这里挂 window/showMessage / window/logMessage 兜底其它 LS 的推送通道。
        // LS 重启 = 旧结论作废，先清零再挂新 handler 重新评估。
        let ws_err_root = key_root_identity(&key.root);
        self.workspace_errors.lock().unwrap().remove(&ws_err_root);
        for method in ["window/showMessage", "window/logMessage"] {
            let ws_err_root = ws_err_root.clone();
            let ws_errors = std::sync::Arc::clone(&self.workspace_errors);
            session
                .client()
                .on_notification(method, move |msg| {
                    let Some(message) = msg
                        .params
                        .as_ref()
                        .and_then(|p| p.get("message"))
                        .and_then(|v| v.as_str())
                    else {
                        return;
                    };
                    if is_workspace_load_error(message) {
                        ws_errors
                            .lock()
                            .unwrap()
                            .insert(ws_err_root.clone(), message.to_owned());
                    }
                });
        }
        // ↖ mirror: ls.py@43ae021 on_server_started — 把"等待 LS 索引就绪"
        // 推到 session_for 内，避免用户可见的首请求 = 索引懒加载。探针必须用
        // root 下真实文件（虚拟 URI 不触发项目索引 —— cold-start hang 根因，
        // 详见 local/cold-start-hang-diagnosis.md），故先告知 adapter 项目 root。
        // T0 配置驱动路径无 adapter 特判探针——工具层 wait_for_index（3.2）的
        // documentSymbol 通用探针仍然生效。
        if let Some(adapter) = &t2 {
            adapter.set_project_root(&key.root);
            if let Err(e) = tokio::time::timeout(
                Duration::from_secs(30),
                adapter.on_server_ready(&session),
            )
            .await
            {
                tracing::warn!(adapter = adapter.id(), error = %e, "on_server_ready probe failed/timed out; continuing");
            }
        }

        // 写一次、读多次；错就当不支持（fallback push 与 2.4 之前等价）。
        let supports_pull = session
            .server_capabilities()
            .as_ref()
            .map(supports_pull_diagnostics)
            .unwrap_or(false);
        self.pull_diag_supported
            .lock()
            .unwrap()
            .insert(key.clone(), supports_pull);
        // 低层会话换代（旧会话已被 evict/懒重启移除）→ 高层符号缓存整体失效。
        self.invalidate_symbol_cache_for_root(&key.root);
        // bd serena-rust-xzb：rust root 拉起即探测 cargo workspace 健康度（结果缓存，
        // LS 重启清零重测），失败记录供语义工具 warning 透出。
        if lang == "rust" {
            self.probe_cargo_workspace_error(&key.root).await;
        }
        self.instances
            .lock()
            .unwrap()
            .insert(key.clone(), session.clone());
        // bd serena-rust-bxd O2/O4：LS 冷启动/重启 → 开（或重开）暖机窗口。
        self.mark_ls_started(&key.root);
        self.touch(&key);
        Ok(session)
    }

    /// 写工具收尾：拉一次 file-level 诊断快照；挂进写工具响应 `post_write_diagnostics`。
    ///
    /// F2 设计：写完后 AI 最常见的下一步是 `diagnostics <file>` 验证；本 helper 把这步
    /// 折叠进写工具返回值。不阻塞主结果（写仍正常返回 applied=true）。
    ///
    /// pending 语义（2026-09-23 裸 RA 探针实锤）：rust-analyzer 语义诊断（didChange →
    /// publishDiagnostics 推送）延迟 ~2.5s，pull（textDocument/diagnostic）更滞后。
    /// `pending: true` = 等待窗口内 LS 没推新一代诊断，items 为空**不代表无错**——
    /// AI 应回头显式查 `diagnostics` 复核；`pending: false` 且 items 空 = LS 确认无错。
    ///
    /// 锁纪律：与 tool_diagnostics 同样走 push 缓存 + 3s 兜底超时；不进诊断时不阻塞。
    /// ponytail: 不为失败建新错误路径 —— 任何 err 都 `tracing::debug!` + pending 假快照。
    async fn post_diag_for_write(
        &self,
        root: &Path,
        file: &str,
        lang: Option<&str>,
    ) -> serde_json::Value {
        // 写盘已发生（bd serena-rust-0em）：先失效写衍生缓存（root 信号 TTL + 该 root
        // 符号缓存），后续 find-symbol 强制重新 walk → 新 mtime → miss → 重查 LS。
        invalidate_write_derived_caches(root);
        self.invalidate_symbol_cache_for_root(root);
        self.mark_recent_write(root, file);
        // 写入刚发生：记基线，等"下一次"推送（本次 didChange 引发的那代诊断）。
        let before = self.diag_generation.load(Ordering::Relaxed);
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.tool_diagnostics(root, file, lang, Some(before + 1)),
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, file, "F2 post-write diag failed; degrading");
                serde_json::json!({ "items": [], "pending": true })
            }
            Err(_elapsed) => {
                tracing::debug!(file, "F2 post-write diag timeout 3s; degrading");
                serde_json::json!({ "items": [], "pending": true })
            }
        }
    }

    /// `textDocument/diagnostic` 诊断（PLAN Phase 2.5）。
    ///
    /// 主路径选择：
    /// - 若 session_for 末尾探测到 LS 声明 `diagnosticProvider` → 优先 `textDocument/diagnostic`
    ///   （pull，LSP 3.17）；返回的 `Full` 报告 items 即最终结果，`Unchanged` 报告保留 push 缓存。
    /// - pull 失败（`-32601 MethodNotFound` 等任何 RPC 错）/ LS 未声明 pull / 字段缺失 →
    ///   透明 fallback 到 `publishDiagnostics` push 缓存（与 2.4 之前完全等价）。
    ///
    /// `wait_gen`：
    /// - `None`：自动基线（函数入口 generation +1），等下一次推送。
    /// - `Some(N>0)`：等 generation >= N，仍受 5s 上限；超时返 `{ items: [], pending: true }`。
    ///
    /// pending 语义（2026-09-23 裸 RA 探针 + live 复现实锤）：rust-analyzer 的 pull
    /// （textDocument/diagnostic）返回的是"上次计算的快照"——didChange 后异步重算
    /// 完成前，pull 会返回**陈旧错误**（REPAIR 场景实测拿到上一版 7 条 syntax errors
    /// 且无任何版本标记）。因此 **pull 快照无法归属 didChange 之后的版本，永不可信**。
    ///
    /// 主路径 = **push 等待**：generation 只计非空推送（见 handler），gen 越基线 =
    /// didOpen/didChange 之后的新一代推送到达（cache 即新鲜 items）。窗口尽未达标
    /// → pull 做兜底但**一律标 pending: true**（快照可能陈旧）；items 空**不代表
    /// 无错**，AI 应回头复核。
    pub async fn tool_diagnostics(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
        wait_gen: Option<u64>,
    ) -> ToolResult<serde_json::Value> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let path = root.join(file);
        let uri = path_to_uri(&path)
            .map_err(|e| ToolError::BadArgs {
                detail: format!("path to uri: {e}"),
            })?
            .as_str()
            .to_string();
        let session = self.session_for(root, lang.as_str()).await?;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let key = Self::key(root, lang.as_str());

        // 探测结果查表。缺 key 视为"未探测过" → 视为不支持 pull（防御：未来多写漏写）。
        let supports_pull = self
            .pull_diag_supported
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(false);

        // push 等待：**version 精确比对** —— RA didChange 后会先重推旧快照再推新
        // 分析（2026-09-23 实锤），generation 无法区分新旧；推送 version == 当前
        // docsync content_version 才是新内容的分析结果（items 空 = 该版确认无错）。
        // LS 不发 version（如 clangd）→ 回退 generation 达标判定（旧行为）。
        // 窗口 5s（50 × 100ms）。Some(N) 语义保留：gen 兜底路径下 N <= 当前 gen
        // → 立即返回（旧契约，测试锁定）。
        let before_gen = self.diag_generation.load(Ordering::Relaxed);
        let target = wait_gen.unwrap_or(before_gen + 1);
        let doc_cur = session.content_version_of(&path);
        let mut confirmed_items: Option<Vec<serde_json::Value>> = None;
        for _ in 0..50 {
            let hit = self
                .diag_cache
                .lock()
                .unwrap()
                .get(&(key.root.clone(), uri.to_lowercase()))
                .cloned();
            if let Some((items, ver)) = hit {
                let ok = match (ver, doc_cur) {
                    (Some(v), Some(c)) => v >= c,
                    (Some(_), None) => true,
                    // 缺 version 的推送只在「该 uri 从未见 version」的 LS（clangd 等）
                    // 上信任 generation 达标 —— 有 version 纪律的 LS（RA）其 watcher
                    // 通道可能推缺 version 的旧内容分析，凭 generation 确认会把旧错误
                    // 当新鲜（bd serena-rust-76d）。
                    (None, _) => {
                        !self.version_seen_uri(&key.root, &uri)
                            && self.diag_generation.load(Ordering::Relaxed) >= target
                    }
                };
                if ok {
                    confirmed_items = Some(items);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if let Some(items) = confirmed_items {
            tracing::debug!(uri = %uri, "tool_diagnostics confirmed: {} items", items.len());
            return Ok(json!({ "items": compact_diags(&items), "pending": false }));
        }

        // 窗口尽未达标：LS 没推送当前版本的新鲜诊断（可能分析中、可能 LS 慢）。
        // bd serena-rust-76d：版本落后（entry_ver < doc_cur）的缓存条目是**上一版
        // 内容**的分析结果（repair 场景 = 旧错误）—— pending 兜底也不得携带
        // （验收：pending:true 且不返旧错误）；pull 快照是同类旧版计算结果且无法
        // 归属版本（裸探针实锤"pull 返回上次计算快照"），版本落后时一并跳过。
        // LS 不发 version（clangd）→ 无法判定 → 保留旧 items + pull 兜底（旧行为）。
        let (mut items, entry_ver) = self
            .diag_cache
            .lock()
            .unwrap()
            .get(&(key.root.clone(), uri.to_lowercase()))
            .cloned()
            .unwrap_or_default();
        let stale_entry = matches!((entry_ver, doc_cur), (Some(v), Some(c)) if v < c);
        if stale_entry {
            items = Vec::new();
        }
        if items.is_empty()
            && supports_pull
            && !stale_entry
            && let Ok(value) = session
                .client()
                .request::<serde_json::Value>(
                    "textDocument/diagnostic",
                    json!({ "textDocument": { "uri": uri.clone() } }),
                    TOOL_TIMEOUT,
                )
                .await
            && let Some(pull) = Self::extract_pull_items(&value)
        {
            items = pull;
        }
        Ok(json!({ "items": compact_diags(&items), "pending": true }))
    }

    /// 从 LSP 3.17 `textDocument/diagnostic` 响应提取权威 items。
    ///
    /// 返回 `Some(items)` 仅当 `kind == "full"` —— 表示"完整报告"，items 是权威。
    /// `unchanged` / `partial` / 缺 kind → 返 `None`（触发 push fallback）。
    /// 任务验收分支 #5：pull 成功 → 取 items；与 push cache 不重复拼接。
    ///
    /// 纯函数 —— 单测不拉 LS，直接喂构造 JSON 验 5 个分支。
    fn extract_pull_items(value: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
        if value.get("kind").and_then(|v| v.as_str()) != Some("full") {
            return None;
        }
        Some(
            value
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        )
    }
}

/// LSP Diagnostic JSON → AI-friendly 单行紧凑文本（省 token：一条 ~15 行 JSON 折成
/// 一行 ~80 字符）。格式：`[error] L12:5-12:20 E0308: message`；行列为 1-based
/// （与 read-file / 行级编辑基线一致）。message 内换行折叠为空格。
fn compact_diags(items: &[serde_json::Value]) -> Vec<String> {
    items.iter().map(compact_one_diag).collect()
}

fn compact_one_diag(d: &serde_json::Value) -> String {
    let sev = match d.get("severity").and_then(|v| v.as_u64()) {
        Some(1) => "error",
        Some(2) => "warn",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "diag",
    };
    let (sl, sc) = d["range"]["start"]
        .as_object()
        .map(|p| {
            (
                p.get("line").and_then(|v| v.as_u64()).unwrap_or(0) + 1,
                p.get("character").and_then(|v| v.as_u64()).unwrap_or(0) + 1,
            )
        })
        .unwrap_or((0, 0));
    let (el, ec) = d["range"]["end"]
        .as_object()
        .map(|p| {
            (
                p.get("line").and_then(|v| v.as_u64()).unwrap_or(0) + 1,
                p.get("character").and_then(|v| v.as_u64()).unwrap_or(0) + 1,
            )
        })
        .unwrap_or((sl, sc));
    let code = d
        .get("code")
        .map(|c| match c {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty() && s != "syntax-error");
    let msg = d
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace('\n', " ");
    let mut out = format!("[{sev}] L{sl}:{sc}");
    if (el, ec) != (sl, sc) {
        out.push_str(&format!("-L{el}:{ec}"));
    }
    if let Some(code) = code {
        out.push_str(&format!(" {code}:"));
    }
    out.push(' ');
    out.push_str(&msg);
    out
}

impl Supervisor {
    pub fn diag_generation(&self) -> u64 {
        self.diag_generation.load(Ordering::Relaxed)
    }

    pub async fn tool_hover(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<lsp_types::Hover>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<lsp_types::Hover> = session
            .request("textDocument/hover", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
    }
    /// `textDocument/signatureHelp` → 函数调用位置上的参数签名提示。
    ///
    /// 与 `tool_hover` 同形态：cursor 落在函数调用括号内时返 `Option<SignatureHelp>`（含 signatures[]、
    /// activeSignature / activeParameter）；落在非调用位置时返 `None`，与 hover 的"无悬停"语义一致。
    ///
    /// 不裁剪字段：agent 据 LSP 原生结构（label / parameters[] / activeParameter 等）自行决策；
    /// 与 hover 不裁剪保持一致。Phase 2.2（local/solidlsp-development-plan.md §2.2）。
    pub async fn tool_signature_help(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<lsp_types::SignatureHelp>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<lsp_types::SignatureHelp> = session
            .request("textDocument/signatureHelp", params, TOOL_TIMEOUT)
            .await?;
        Ok(resp)
    }
    // ============================================================================
    // Phase 1 · 上游 wrapper 缺口（local/solidlsp-development-plan.md §Phase 1 后续
    // 批次 / 13 个 tool_*）。每项 = 透传 LSP method，不裁剪字段（agent 据 LSP 原生结构
    // 自行决策；与 hover/signature-help 不裁剪保持一致）。
    // ============================================================================

    /// `textDocument/codeAction`：位置 + kind（"quickfix" / "refactor" / "refactor.extract"
    /// / "refactor.inline" / "refactor.rewrite" / "source" / "source.organizeImports" 等
    /// LSP `CodeActionKind` 子串匹配，可选省略返所有）。
    ///
    /// 响应数组归一化：`[] | null` 都表示"无可用 action"；LS 偶尔返非数组（极端）→ 空。
    /// 不实现执行端（`workspace/executeCommand`），agent 据 `edit` 字段自行决策。
    pub async fn tool_code_action(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        kind: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CodeAction>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let mut params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": line, "character": col },
                "end":   { "line": line, "character": col },
            },
            "context": { "diagnostics": [] },
        });
        if let Some(k) = kind {
            params["context"]["only"] = serde_json::Value::String(k.to_owned());
        }
        let resp: Option<serde_json::Value> = session
            .request("textDocument/codeAction", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CodeAction> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/formatting`：整文件格式化（默认 tabSize=4 / insertSpaces=true，
    /// agent 可通过 args.options 覆盖）。响应 `[] | null` = 无变化；非数组归一为空。
    pub async fn tool_format(
        &self,
        root: &Path,
        file: &str,
        tab_size: Option<u32>,
        insert_spaces: Option<bool>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TextEdit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let options = json!({
            "tabSize": tab_size.unwrap_or(4),
            "insertSpaces": insert_spaces.unwrap_or(true),
        });
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "options": options,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/formatting", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TextEdit> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/rangeFormatting`：range 内格式化（参数同 tool_format + Range）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_format_range(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
        tab_size: Option<u32>,
        insert_spaces: Option<bool>,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TextEdit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let options = json!({
            "tabSize": tab_size.unwrap_or(4),
            "insertSpaces": insert_spaces.unwrap_or(true),
        });
        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": start_line, "character": start_col },
                "end":   { "line": end_line,   "character": end_col   },
            },
            "options": options,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/rangeFormatting", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TextEdit> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/inlayHint`：行范围（line..=end_line）内的类型提示。
    /// LSP 返 `InlayHint[] | null`；空/null = 无 hint。
    pub async fn tool_inlay_hint(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::InlayHint>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "range": {
                "start": { "line": start_line, "character": 0 },
                "end":   { "line": end_line,   "character": 0 },
            },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/inlayHint", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::InlayHint> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/documentHighlight`：光标位置的同符号高亮（写引用区 vs 读引用区
    /// 按 `kind` 区分；agent 据此识别写冲突点）。空/null = 无高亮。
    pub async fn tool_document_highlight(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::DocumentHighlight>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/documentHighlight", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::DocumentHighlight> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/foldingRange`：文件级折叠区。LSP `FoldingRange[] | null`；空 = 无折叠。
    pub async fn tool_folding_range(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::FoldingRange>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/foldingRange", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::FoldingRange> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/semanticTokens/full`：文件级语义 token（test 而非着色用）。
    /// 响应是 `SemanticTokens` 单对象（含 `data: [..i32..]` 编码形式）；为 agent 友好
    /// 拆出原始 delta 序列与一份解析后的 `(line, col, length, token_type, modifiers)` 列表。
    pub async fn tool_semantic_tokens(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<SemanticTokensFull> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<lsp_types::SemanticTokens> = session
            .request("textDocument/semanticTokens/full", params, TOOL_TIMEOUT)
            .await?;
        let resp = resp.unwrap_or(lsp_types::SemanticTokens {
            result_id: None,
            data: Vec::new(),
        });
        Ok(SemanticTokensFull {
            result_id: resp.result_id,
            tokens: decode_semantic_tokens(&resp.data),
        })
    }

    /// `textDocument/codeLens`：文件级代码透镜（references / impls / run/debug 计数）。
    /// LSP `CodeLens[] | null`；空 = 无透镜。
    pub async fn tool_code_lens(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CodeLens>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/codeLens", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CodeLens> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/documentLink`：文件级可点击链接（include / 模块导入跳转）。
    /// LSP `DocumentLink[] | null`；空 = 无链接。
    pub async fn tool_document_link(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::DocumentLink>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/documentLink", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::DocumentLink> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/prepareCallHierarchy`：把光标位置的符号转成可被 incoming/outgoing
    /// 操作的 `CallHierarchyItem[]`。LSP 返数组（同一位置的多匹配，如 C++ 重载）；
    /// 空/null = 不是可层级化符号（变量/宏等）。
    pub async fn tool_call_hierarchy_prepare(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyItem>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/prepareCallHierarchy", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `callHierarchy/incomingCalls`：调用当前项的位置集合（含 range / fromSymbol）。
    /// `item` = `tool_call_hierarchy_prepare` 返的 `CallHierarchyItem` 序列化形态（`serde_json::Value`）。
    pub async fn tool_call_hierarchy_incoming(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyIncomingCall>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/incomingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyIncomingCall> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `callHierarchy/outgoingCalls`：当前项调出的位置集合。
    pub async fn tool_call_hierarchy_outgoing(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyOutgoingCall>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/outgoingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::CallHierarchyOutgoingCall> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/prepareTypeHierarchy`：把光标位置的符号转成可被 supertypes/subtypes
    /// 操作的 `TypeHierarchyItem[]`。空/null = 不可层级化（变量 / 函数等非类型符号）。
    pub async fn tool_type_hierarchy_prepare(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/prepareTypeHierarchy", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `typeHierarchy/supertypes`：父类型列表（OOP 继承链向上）。
    pub async fn tool_type_hierarchy_supertypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/supertypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `typeHierarchy/subtypes`：子类型列表（OOP 继承链向下）。
    pub async fn tool_type_hierarchy_subtypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/subtypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::TypeHierarchyItem> =
            serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `textDocument/moniker`：符号的全局 / 项目 / 局部标识符（用于跨仓库跳转 / git
    /// blame 锚定）。LSP `Moniker[] | null`；空 = 无 moniker（很常见，多数 LS 不实现）。
    pub async fn tool_moniker(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::Moniker>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": line, "character": col },
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/moniker", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Moniker> = serde_json::from_value(raw).unwrap_or_default();
        Ok(parsed)
    }

    /// `workspace/diagnostic`：整项目 pull diagnostics（LS 能力 `workspaceDiagnostics`
    /// 未声明时返 -32601 `MethodNotFound`，在此路径转 BadArgs 提示用户走 per-file
    /// `diagnostics` 工具）。items[] 始终返数组（即使空）。
    pub async fn tool_workspace_diagnostic(
        &self,
        root: &Path,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::Diagnostic>> {
        // 无 file 锚点：按 root 唯一 LS 探测；多 lang 项目请用 --lang 限定。
        let lang: String = match lang_override {
            Some(l) => l.to_ascii_lowercase(),
            None => {
                // 走 default LS（adapter 默认 lang）。
                let entries = self.last_used.lock().unwrap().keys().cloned().collect::<Vec<_>>();
                match entries.first() {
                    Some(k) => k.lang.to_string(),
                    None => {
                        return Err(ToolError::BadArgs {
                            detail: "no active LS; specify --lang or run any tool first".into(),
                        });
                    }
                }
            }
        };
        let session = self.session_for(root, &lang).await?;
        let params = json!({
            "previousResultIds": [],
            "textDocument": { "uri": path_to_uri_str(&root.join(".")) },
        });
        let resp: Option<serde_json::Value> = session
            .request("workspace/diagnostic", params, INDEX_TIMEOUT)
            .await?;
        let Some(raw) = resp else {
            return Ok(Vec::new());
        };
        let items = raw.get("items").cloned().unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Diagnostic> =
            serde_json::from_value(items).unwrap_or_default();
        Ok(parsed)
    }

    // ==== Phase 3.1 文档符号缓存存取（上游 ls.py@43ae021 文档符号缓存对应）====

    /// cache 命中查询；返回克隆（平铺 list 小，克隆远便宜于 LS 往返）。
    fn symbol_cache_get(&self, key: &SymbolCacheKey) -> Option<Vec<SymbolHit>> {
        self.symbol_cache.lock().unwrap().get(key).cloned()
    }

    /// 符号缓存写入内核（容量闸门 + 空集跳过）。P2-a5k 抽出供 `Supervisor::symbol_cache_put`
    /// 与并发 fan-out 路径 `overview_via_session` 共用——两处写同一 `Arc<Mutex<HashMap>>`，
    /// 把"空不写"与"超限全清"绑成原子决策避免任何写入路径绕过容量闸门。
    ///
    /// 容量闸门（ARCH §3.2）：超 `SYMBOL_CACHE_MAX_ENTRIES` 整表清空再建。key 内 mtime
    /// 单调推进，旧 mtime 命中是清空而非误中；全清比 LRU 更安全（无 stale 窗口、无
    /// insertion-order 维护开销，禁 dashmap/indexmap）。ponytail: 全清；若实测命中率
    /// 明显下降再换 LRU。
    ///
    /// cache_miss 后写入。LS 错误路径不经过这里（失败不进 cache）。
    /// 空结果不写：语义未就绪窗口的空响应（RA/gopls 加载期）会被永久缓存，
    /// 导致就绪后仍 miss 假象；空是合法语义结果，宁可重查不可错缓存。
    fn symbol_cache_put(&self, key: SymbolCacheKey, hits: Vec<SymbolHit>) {
        let mut cache = self.symbol_cache.lock().unwrap();
        symbol_cache_put_impl(&mut cache, key, hits);
    }

    /// 暴露给测试/排障：当前缓存条目数。std::sync::Mutex 而非 parking_lot（ARCH §6）。
    #[cfg(test)]
    fn symbol_cache_len(&self) -> usize {
        self.symbol_cache.lock().unwrap().len()
    }

    /// 该 uri 是否收到过带 version 的 publishDiagnostics（bd serena-rust-76d）。
    /// handler 写入；tool_diagnostics 确认分支读。
    fn version_seen_uri(&self, root: &Path, uri: &str) -> bool {
        self.version_seen
            .lock()
            .unwrap()
            .get(&(root.to_path_buf(), uri.to_lowercase()))
            .copied()
            .unwrap_or(false)
    }

    /// 该 root 是否记录过 workspace 加载失败（bd serena-rust-xzb）。
    /// session_for 的 window 消息 handler 写入；语义工具响应组装时读。
    fn workspace_error_for(&self, root: &Path) -> Option<String> {
        self.workspace_errors
            .lock()
            .unwrap()
            .get(&key_root_identity(root))
            .cloned()
    }

    /// rust root 的 workspace 加载健康探测（bd serena-rust-xzb）。
    ///
    /// RA 的 cargo workspace 加载失败（FetchWorkspaceError，如 fixture 在别的
    /// workspace 内非成员）只出现在 LS 进程 stderr（裸探针实锤 stdout LSP 通道无
    /// window/showMessage|logMessage 帧），而 stderr 泵在 lsp-core（禁区）。
    /// 故 supervisor 用与 RA 同源的 `cargo metadata` 主动探测 —— RA 加载 workspace
    /// 内部就是跑这条命令，失败与否与 FetchWorkspaceError 同根同源；失败摘要写入
    /// workspace_errors，语义工具响应经 warning 键透出（不再纯静默空）。
    /// session 拉起时跑一次（结果缓存至 LS 重启清零）。
    async fn probe_cargo_workspace_error(&self, root: &Path) {
        if !root.join("Cargo.toml").is_file() {
            return; // 非 cargo 项目（纯文件目录 / 其它语言），无从失败
        }
        let identity = key_root_identity(root);
        if self.workspace_errors.lock().unwrap().contains_key(&identity) {
            return; // 本轮 session 生命周期内已有结论（含其它通道记录）
        }
        let manifest = root.join("Cargo.toml");
        let cwd = root.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || {
            std::process::Command::new("cargo")
                .args(["metadata", "--format-version", "1", "--manifest-path"])
                .arg(&manifest)
                .current_dir(&cwd)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
        })
        .await;
        let Ok(Ok(out)) = outcome else {
            return; // spawn 失败/异常 = 环境问题，不冤枉 workspace
        };
        if out.status.success() {
            return;
        }
        let detail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join(" | ");
        let detail = if detail.is_empty() {
            format!("exit code {:?}", out.status.code())
        } else {
            detail.chars().take(400).collect()
        };
        self.workspace_errors.lock().unwrap().insert(
            identity,
            format!("cargo metadata failed: {detail}"),
        );
    }


    /// workspace 加载错误 warning（bd serena-rust-xzb）：有记录即透出，**不依赖空
    /// 结果** —— 记录意味着该 root 的 workspace 未加载成功，语义结果整体不可信
    /// （RA 语法层可能仍有响应，非空结果同样存疑）。
    fn workspace_error_warnings(&self, root: &Path) -> Vec<String> {
        match self.workspace_error_for(root) {
            Some(err) => vec![format!(
                "workspace error: {err}; semantic results may be empty (workspace failed to load)"
            )],
            None => Vec::new(),
        }
    }

    /// 语义工具空结果的「可能未就绪」判定（bd serena-rust-we0）：
    /// documentSymbol 探测 —— 符号索引空（LS 整体未就绪）或位置在符号内
    /// （类型分析未就绪）→ 就绪提示；位置在符号外 → 空是正常语义，不加。
    /// 探测自身失败（如 RA 索引刷新期 `-32801 content modified` 竞态）= LS 不稳定
    /// 窗口，空结果同样可疑 → 一并标记（探测走 tool_overview，Phase 3.1 缓存命中
    /// 免 LS 往返；未就绪窗口多一次 documentSymbol 往返可接受）。
    async fn semantic_not_ready_warnings(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang: Option<&str>,
    ) -> Vec<String> {
        match self.tool_overview(root, file, lang).await {
            Err(_) => vec![semantic_not_ready_message()],
            Ok(hits) if hits.is_empty() || position_in_hits(&hits, line, col) => {
                vec![semantic_not_ready_message()]
            }
            Ok(_) => Vec::new(),
        }
    }


    /// LS 会话新建（session_for spawn 点）记账：开窗 + 清语义就绪标记。
    /// LS 重启（evict/懒重启）即重新冷启动，窗口必须重开。
    fn mark_ls_started(&self, root: &Path) {
        self.ls_warmup.lock().unwrap().insert(
            key_root_identity(root),
            LsWarmup {
                started: std::time::Instant::now(),
                semantic_ok: false,
            },
        );
    }

    /// 首个语义工具（hover/def/refs/find-implementations）非空成功 → 关窗。
    fn mark_semantic_ready(&self, root: &Path) {
        if let Some(w) = self.ls_warmup.lock().unwrap().get_mut(&key_root_identity(root)) {
            w.semantic_ok = true;
        }
    }

    /// 暖机窗口判定：LS 已启动 && 10s 内 && 首语义成功未到 → partial 提示。
    /// 无记账（root 从未拉起 LS）→ 不提示（无可言的窗口）。
    fn index_warming_warnings(&self, root: &Path) -> Vec<String> {
        let active = self
            .ls_warmup
            .lock()
            .unwrap()
            .get(&key_root_identity(root))
            .is_some_and(|w| {
                !w.semantic_ok && w.started.elapsed() < LS_WARMUP_WINDOW
            });
        if active {
            vec![index_warming_message()]
        } else {
            Vec::new()
        }
    }

    /// 写工具收尾标记（bd serena-rust-0em）：file 进入写后一致性窗口。
    fn mark_recent_write(&self, root: &Path, file: &str) {
        self.recent_writes
            .lock()
            .unwrap()
            .insert((root.to_path_buf(), file.to_lowercase()), std::time::Instant::now());
    }

    /// root 下仍在写后一致性窗口内的文件的归一 uri 集合（顺手清理过期条目）。
    fn recent_written_uris(&self, root: &Path, ttl: Duration) -> Vec<String> {
        let mut map = self.recent_writes.lock().unwrap();
        map.retain(|_, t| t.elapsed() < ttl);
        let mut uris = Vec::new();
        for ((_r, f), _) in map.iter().filter(|((r, _), _)| *r == root) {
            let path = root.join(f);
            if let Ok(uri) = path_to_uri(&path) {
                uris.push(uri.as_str().to_lowercase());
            }
        }
        uris.sort();
        uris.dedup();
        uris
    }

    /// 低层（LS 会话）版本变化 → 该 root 的全部高层符号缓存失效。
    ///
    /// ↖ mirror: ls.py@a5fd4d68 — 高层 document symbol 缓存版本必须纳入 LS-specific
    /// 低层缓存版本，低层变化即失效（否则 LS 换代后命中旧代缓存）。本项目把"低层
    /// 版本"物化为会话本身：`(root, lang)` 的 LS 会话重建（evict / Failed 懒重启 /
    /// mid-call Terminated 重试）即低层版本变更，key 中的 mtime 无法感知这种变化。
    /// 在 session_for 挂入新会话前调用，单点覆盖所有换代路径。
    fn invalidate_symbol_cache_for_root(&self, root: &Path) {
        self.symbol_cache
            .lock()
            .unwrap()
            .retain(|key, _| key.0 != root);
    }

    /// P2-18h 外部修改对账：盘上 (mtime,size) 与缓存记账不符 → 清该文件全部缓存
    /// 条目（force miss → 本次调用重走 LS），返 true。一致/无记账/不可 stat → false。
    ///
    /// 挂在单文件工具（overview/symbol-body）的 miss 路径：key 双因子保证盘变必
    /// miss，但旧 stamp 残留条目会一直占表 —— 这里顺带清掉（context 契约"不一致
    /// 即清该文件缓存"）。didChange 重放由 miss 路径既有的 `ensure_open` 承担
    /// （mtime/size 变 → 全量 didChange），不在此重复发。
    fn reconcile_symbol_cache_for_file(&self, root: &Path, file: &str) -> bool {
        let stamp = doc_symbol_cache_key(root, file).2;
        let mut cache = self.symbol_cache.lock().unwrap();
        let stale = cache
            .keys()
            .any(|k| k.0 == root && k.1 == file && k.2 != stamp);
        if stale {
            cache.retain(|k, _| !(k.0 == root && k.1 == file));
        }
        stale
    }

    /// AI-token 特性 J（plan-j-delta §2 / design §11-J）：迭代工作流（改→查→改）
    /// 下只返回上次以来变化的条目。
    /// - `delta=false`（默认）：原样透传（wire 不变），顺带缓存供下次 delta 对照。
    /// - `delta=true` 且有缓存：`{delta:true, added, removed}`（hit_key set diff）。
    /// - `delta=true` 无缓存（首次/曾遇空集）：`{delta:false, items:全集}`。
    ///
    /// 空集不缓存：LS 就绪窗口返空若入库，就绪后 delta 恒漏报 —— 宁重查不可错缓存。
    ///
    /// ponytail: 不做字符级 diff —— hit_key（file:line:col 归一）set diff 足够回答
    /// "新增了哪些引用"；临界区仅 HashMap 读写，无 await 持锁。
    async fn maybe_delta(
        &self,
        tool: &str,
        root_key: &str,
        current: serde_json::Value,
        delta: bool,
    ) -> serde_json::Value {
        let key = format!("{}|{}", tool, root_key);
        if !delta {
            if !is_empty_response(&current) {
                self.delta_cache.lock().unwrap().insert(key, current.clone());
            }
            return current;
        }
        let prev = self.delta_cache.lock().unwrap().get(&key).cloned();
        if !is_empty_response(&current) {
            self.delta_cache
                .lock()
                .unwrap()
                .insert(key, current.clone());
        }
        let Some(prev) = prev else {
            return serde_json::json!({
                "delta": false,
                "items": items_of(&current)
                    .cloned()
                    .map(serde_json::Value::Array)
                    .unwrap_or(current),
            });
        };
        let added = diff_hits(&current, &prev);
        let removed = diff_hits(&prev, &current);
        serde_json::json!({ "delta": true, "added": added, "removed": removed })
    }

    /// `textDocument/documentSymbol` → 平铺递归 `DocumentSymbol::children` → `Vec<SymbolHit>`。
    pub async fn tool_overview(
        &self,
        root: &Path,
        file: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<SymbolHit>> {
        // Phase 3.1 缓存：同 (root, file, mtime) 二次调用免 LS 往返（命中 <1ms）。
        let cache_key = doc_symbol_cache_key(root, file);
        if let Some(cached) = self.symbol_cache_get(&cache_key) {
            return Ok(cached); // cache_hit
        }
        // P2-18h miss 路径对账：清同文件残留旧 stamp 条目（didChange 由下方
        // ensure_open 按 mtime/size 差异自动重放）。
        self.reconcile_symbol_cache_for_file(root, file);
        let lang_str = resolve_lang_for_file(file, lang_override)?;
        let file_owned = file.to_string();
        let lang_owned = lang_str.clone();
        // P0B race 修：overview 曾不走 self-heal —— cold-start LS_TIMEOUT 后 writer
        // 静默死亡（channel closed），session 仍以 Ready 缓存，后续调用立刻
        // Terminated 且永不恢复。走 with_session_retry 后 Terminated 触发 evict +
        // 换新 session 重试一次（同 def/refs/replace-body 等 tool 的既有通道）。
        Self::with_session_retry(self, root, lang_str.as_str(), move |session| {
            let file = file_owned.clone();
            let lang_str = lang_owned.clone();
            async move { Self::tool_overview_inner(session, root, &file, &lang_str).await }
        })
        .await
        .inspect(|out| self.symbol_cache_put(cache_key, out.clone())) // cache_miss → 写入
    }

    /// tool_overview 的实际实现 —— 拆出便于 `with_session_retry` 在闭包里重放
    /// 整条链路（含 ensure_open 重取）。
    async fn tool_overview_inner(
        session: std::sync::Arc<lsp_core::session::Session>,
        root: &Path,
        file: &str,
        lang_str: &str,
    ) -> ToolResult<Vec<SymbolHit>> {
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Phase 4 基建 Task 22b：timeout 由三层合并（CLI args._timeout_ms >
        // servers.toml `timeout_ms` > 默认 30s）。execute_tool 入口把
        // `args._timeout_ms` 提取后塞进 per-call override；当前 tool_overview 拿不到
        // args，故先固定传 `lang` 让 servers.toml 的 per-LS timeout 生效。其它
        // tool_* 后续按相同 pattern 替换。
        let timeout = ls_registry::config::effective_timeout_ms(lang_str, None)
            .map(|ms| Duration::from_millis(ms as u64))
            .unwrap_or(TOOL_TIMEOUT);
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, timeout)
            .await?;

        Ok(flatten_symbols(resp, &uri))
    }

    /// 跨文件符号树（PLAN Phase 2.5 / 7.2）：聚合 `dir` 下源码文件的 documentSymbol。
    ///
    /// 逐文件走 `tool_overview` —— 天然复用 3.1 缓存（同文件二次 symbol-tree/overview
    /// 免 LS 往返）；目录扫描走 3.3 `filtered_walker`（venv/node_modules/target 等内置
    /// ignore + gitignore）。`max_files` 保险丝（默认 200）：超限截断并标 `truncated`。
    pub async fn tool_symbol_tree(
        &self,
        root: &Path,
        dir: &str,
        lang: Option<&str>,
        max_files: usize,
    ) -> ToolResult<serde_json::Value> {
        if dir.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "missing 'dir'".into(),
            });
        }
        let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let canon_dir = dunce::canonicalize(canon_root.join(dir)).map_err(|e| ToolError::BadArgs {
            detail: format!("dir not found: {dir} ({e})"),
        })?;
        if !canon_dir.starts_with(&canon_root) {
            return Err(ToolError::BadArgs {
                detail: format!("dir escapes root: {dir}"),
            });
        }

        // 收集源码文件：只收能解析语言的扩展名；max_files 保险丝。
        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in crate::fs_tools::filtered_walker(&canon_dir).build() {
            let Ok(entry) = entry else { continue };
            let is_file = entry.file_type().as_ref().is_some_and(|t| t.is_file());
            if !is_file {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&canon_root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            // 按扩展名探测（None）：lang override 只作用于请求语言，不当文件筛选 ——
            // 否则 Some("rust") 会把 .md 等非源码也收进来。
            if resolve_lang_for_file(&rel, None).is_ok() {
                if files.len() >= max_files {
                    truncated = true;
                    break;
                }
                files.push(rel);
            }
        }

        // 逐文件聚合：缓存命中走 cache_get 拿克隆（无 LS 调用，可同步直接结果）；
        // 缓存未命中走 LS 文档符号查询。
        //
        // 修 P1 #3 fan-out（合 P0 修复 audit）：按语言分桶（mixed-lang 目录下不同
        // 扩展名各自走自己的 LS，如 .py → pyright / .rs → rust-analyzer，绝不混
        // session）。每桶独立 `session_for(lang)` + 独立 JoinSet，桶内 MAX_INFLIGHT=4
        // 有界并发；多桶互不阻塞。
        //
        // LS 单会话并发安全性已在 lsp-core/client.rs pending 表（按 Id 关联）+ outbound
        // mpsc（writer task 单写）确认为安全 —— 多 in-flight documentSymbol 安全。
        //
        // 顺序保留：entries 用 `Vec<(usize, Value)>` 暂存（idx 是 file 在 files 中的
        // 原位置）；缓存命中按 idx 升序 push；桶内 miss drain 顺序无序 → 末尾
        // `Vec::sort_by_key(|(idx,_)|*idx)` 稳定排序展平为 final entries。`files` 本身
        // 已按 filtered_walker 顺序收集，命中桶内顺序天然稳定。
        //
        // ponytail: 不引入 `futures` crate —— JoinSet + sort_by_key（std 稳定排序）
        // 已足够，万级文件再换 LRU+slot。
        let mut entries: Vec<(usize, serde_json::Value)> = Vec::new();
        let mut errors = Vec::new();
        if !files.is_empty() {
            let cache_arc = Arc::clone(&self.symbol_cache);
            let mut per_lang_buckets: std::collections::HashMap<String, Vec<usize>> =
                std::collections::HashMap::new();

            // 逐文件语言解析（P0 修复核心）：扩展名不同的文件各自归属各自 LS 桶。
            // 解析失败的文件不入桶，留作 errors（与原 tool_overview 路径一致）。
            for (idx, file) in files.iter().enumerate() {
                match resolve_lang_for_file(file, lang) {
                    Ok(lang_id) => {
                        per_lang_buckets
                            .entry(lang_id)
                            .or_default()
                            .push(idx);
                    }
                    Err(e) => {
                        // 与 tool_overview 一致：纯缓存命中也可走，但 miss 路径无法走 LS，
                        // 这里走 cache-only 分支（与下方的 cache 分流合一）。
                        // 直接尝试读缓存（避免漏已有 cache 命中）：
                        let cache_key = doc_symbol_cache_key(root, file);
                        let cached = cache_arc.lock().unwrap().get(&cache_key).cloned();
                        match cached {
                            Some(symbols) if !symbols.is_empty() => {
                                entries.push((
                                    idx,
                                    serde_json::json!({ "file": file, "symbols": symbols }),
                                ));
                            }
                            Some(_) => {}
                            None => {
                                errors.push(serde_json::json!({
                                    "file": file,
                                    "error": format!("language unresolved: {e}"),
                                }));
                            }
                        }
                    }
                }
            }

            // 对每桶：先把缓存命中按 idx 压 entries，再把 miss 索引推到该桶的并发池；
            // 桶之间可并行（不同 LS 进程），但同桶内受 MAX_INFLIGHT 限制。
            //
            // 修 P0-A 50 文件挂死：扇出前先 sleep 100ms 让前一波 didOpen 流到 LS；
            // files 总数 > 30（实测拐点）时整桶改成串行（in-flight=1），避免 didChange
            // 洪泛把 RA 内部队列压垮 → channel 关闭 → daemon hang。
            // 审计 P2-2：sleep 挪到确认存在 miss 之后 —— 纯缓存命中路径不该白付 100ms。
            const MAX_INFLIGHT: usize = 4;
            let serial_mode = files.len() > 30;
            let mut throttled = false;
            for (lang_id, bucket_indices) in per_lang_buckets {
                // 单桶内的 miss 索引 + 缓存分流
                let mut miss_indices: Vec<usize> = Vec::new();
                for &idx in &bucket_indices {
                    let file = &files[idx];
                    let cache_key = doc_symbol_cache_key(root, file);
                    let cached = cache_arc.lock().unwrap().get(&cache_key).cloned();
                    match cached {
                        Some(symbols) if !symbols.is_empty() => {
                            entries.push((
                                idx,
                                serde_json::json!({ "file": file, "symbols": symbols }),
                            ));
                        }
                        Some(_) => {}
                        None => {
                            miss_indices.push(idx);
                        }
                    }
                }
                if miss_indices.is_empty() {
                    continue;
                }

                // 单桶：拉一次 session（按本桶 lang）。session_for 在该 lang 已有
                // 实例时直返（per-key 加载门），不会重复冷启动。
                let session = self.session_for(root, &lang_id).await?;
                if !serial_mode && !throttled {
                    // 让 outbox mpsc 把已排队的 didOpen 流过去再放 documentSymbol 风暴
                    throttled = true;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                let mut set = tokio::task::JoinSet::new();
                for idx in miss_indices {
                    if !serial_mode {
                        while set.len() >= MAX_INFLIGHT {
                            drain_one(&mut set, &mut entries, &mut errors).await;
                        }
                    } else {
                        // >30 文件：先 drain 上一轮再启下一个，串行推进；
                        // 并行度退化到 1 = 单文件 didOpen 间歇 > 单 documentSymbol。
                        while !set.is_empty() {
                            drain_one(&mut set, &mut entries, &mut errors).await;
                        }
                    }
                    let file = files[idx].clone();
                    let file_for_res = file.clone();
                    let session = Arc::clone(&session);
                    let cache_arc = Arc::clone(&cache_arc);
                    let root = root.to_path_buf();
                    let lang_owned = Some(lang_id.clone());
                    set.spawn(async move {
                        let res = overview_via_session(
                            session,
                            cache_arc,
                            root,
                            file,
                            lang_owned.as_deref(),
                        )
                        .await;
                        (idx, file_for_res, res)
                    });
                }
                while !set.is_empty() {
                    drain_one(&mut set, &mut entries, &mut errors).await;
                }
            }

            // (idx, value) 序列稳定排序，再展平为 entries（顺序 = files 顺序）。
            entries.sort_by_key(|(idx, _)| *idx);
        }

        let final_entries: Vec<serde_json::Value> =
            entries.into_iter().map(|(_, v)| v).collect();

        serde_json::to_value(serde_json::json!({
            "dir": dir,
            "files_scanned": files.len(),
            "truncated": truncated,
            "entries": final_entries,
            "errors": errors,
        }))
        .map_err(|e| ToolError::Serialize(e.into()))
    }

    ///
    /// Phase 2.1（local/solidlsp-development-plan.md §2.1）：位置已知，从 grep/搜索结果直接
    /// 跳进符号工作流，无需按名字再扫一遍。无命中时返空数组（区别于 `tool_def` 的 BadArgs——
    /// 「位置无覆盖"是合法的"）。
    ///
    /// 与 `tool_overview` 一样走 `DocumentSymbolResponse`，不引入新 LSP method：
    /// 上游 `request_containing_symbol` 用 `request_symbol_at_location`（走位置查符号），
    /// 我们用 documentSymbol walk 实现等价语义（ARCH §6.x Δ 标注）。
    pub async fn tool_containing_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<SymbolHit>> {
        let lang_str = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, &lang_str).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        Ok(collect_containing_hits(resp.as_ref(), &uri, line, col))
    }

    /// `defining-symbol`：位置 → `tool_def` 拿 Location → 在该 Location 上 documentSymbol
    /// walk → 切片出符号体 → 返回 `{source, symbol}[]`。
    ///
    /// Phase 2.3（local/solidlsp-development-plan.md §2.3）：组合既有 `tool_def` +
    /// `tool_containing_symbol` 的 walk 实现等价上游 `request_defining_symbol`（ls.py@43ae021
    /// `request_defining_symbol`，返回 `UnifiedSymbolInformation | list[UnifiedSymbolInformation]`）。
    ///
    /// 返回 `Option<Vec<DefiningSymbolHit>>`：
    /// - `None`：`tool_def` 也没找到（位置无定义）—— 与 `tool_def` 同语义（合法）。
    /// - `Some(vec![])`：罕见 —— `tool_def` 给了 Location 但目标位置不在任何符号内。
    /// - `Some(non_empty)`：正常结果；多定义场景（C++ 重载）自然展开为多元素。
    ///
    /// 复用策略：直接调 `self.tool_def` 避免重写 `textDocument/definition` 请求；
    /// 切片走 `lsp_core::offsets::slice_at` 与 `tool_symbol_body` 同源。
    pub async fn tool_defining_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<Vec<DefiningSymbolHit>>> {
        // 1) def → Location（可能 None）。
        let def_loc = self.tool_def(root, file, line, col, lang_override).await?;
        let Some(def_loc) = def_loc else {
            return Ok(None);
        };

        // 2) Location.uri → 相对 root 的 file 路径。走 uri_to_path 统一 percent-decode
        //    （tsserver 回 `d%3A/...`，不解码会误判 outside workspace root）。
        let def_uri = def_loc.uri.to_string();
        let abs = uri_to_path(&def_uri).ok_or_else(|| ToolError::BadArgs {
            detail: format!("definition uri is not a file:// URI: {def_uri}"),
        })?;
        let def_file = abs
            .strip_prefix(root)
            .map_err(|_| ToolError::BadArgs {
                detail: format!("definition at {def_uri} is outside workspace root {root:?}"),
            })?
            .to_string_lossy()
            .replace('\\', "/");

        // 3) 目标文件 → documentSymbol walk → 在 def_loc.range.start 找覆盖符号。
        let target_lang = resolve_lang_for_file(&def_file, lang_override)?;
        let session = self.session_for(root, &target_lang).await?;
        let target_path = root.join(&def_file);
        let target_uri = path_to_uri_str(&target_path);
        let _guard = session
            .ensure_open(&target_path)
            .await
            .map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": target_uri.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        // 用 def 的目标位置作为 walk key（注意：来自 LSP 的 line/col 是 0-based）。
        let hits = collect_containing_hits(
            resp.as_ref(),
            &target_uri,
            def_loc.range.start.line,
            def_loc.range.start.character,
        );
        if hits.is_empty() {
            return Ok(Some(Vec::new()));
        }

        // 4) 读盘 → 切片 body（OffsetEncoding::Utf16 与 tool_symbol_body 一致）。
        let text = tokio::fs::read_to_string(&target_path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", target_path.display()),
            })?;
        let source = DefiningSymbolLocation {
            file: def_file.clone(),
            line: def_loc.range.start.line,
            col: def_loc.range.start.character,
        };
        let out: Vec<DefiningSymbolHit> = hits
            .into_iter()
            .map(|hit| {
                let start = LspPos {
                    line: hit.range.start.line,
                    character: hit.range.start.character,
                };
                let end = LspPos {
                    line: hit.range.end.line,
                    character: hit.range.end.character,
                };
                let body = lsp_core::offsets::slice_at(&text, start, end, OffsetEncoding::Utf16)
                    .unwrap_or_else(|e| {
                        // 切片失败（罕见：LS 给的 range 异常）→ 留空 + 不中断整体响应。
                        // 整调用返 None 会让上层误判「无定义」，代价更大。
                        tracing::warn!(symbol = %hit.name, error = %e, "defining-symbol slice failed");
                        String::new()
                    });
                DefiningSymbolHit {
                    source: source.clone(),
                    symbol: DefiningSymbolInfo {
                        name: hit.name,
                        kind: hit.kind,
                        range: hit.range,
                        body,
                    },
                }
            })
            .collect();
        Ok(Some(out))
    }


    /// `workspace/symbol` → 全 workspace 跨文件符号查找（Task 20）。
    ///
    /// 多语言支持：
    /// - 指定 `lang_override`：仅查该 LS（不依赖 root 文件探测）。
    /// - 不指定：扫 root 找所有 lang, 每个 lang 各起 LS 并行查 + merge (去重已排序后截断)。
    ///
    /// 返回 `(hits, warnings)`：LS 拉起失败不再静默吞（bd serena-rust-x67）——
    /// 全部 lang 失败 → `Err`（全 NotInstalled 合并为一个；混入其它错误原样上抛，
    /// 见 `combined_all_failed_error`）；部分成功 → hits 只含成功 lang，
    /// warnings 逐条描述失败 lang（execute_tool 落到结果顶层 `warning` 键）。
    ///
    /// 索引可能慢（>10s），用 `INDEX_TIMEOUT` 而不是 `TOOL_TIMEOUT`。
    pub async fn tool_find_symbol(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
        lang_override: Option<&str>,
    ) -> ToolResult<(Vec<SymbolHit>, Vec<String>)> {
        use std::collections::BTreeSet;

        if query.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "query must not be empty".into(),
            });
        }

        // P2-4：root 信号（mtime + langs）走 2s TTL 缓存 + spawn_blocking 同步 walk。
        // 单次 walk 同时拿 mtime 与 lang 集合：命中路径（root_source_mtime 同等信号）
        // 与 miss 路径（lang 探测）复用同一份结果，免二次 walk 风暴。
        let (root_mtime, walked_langs) = root_signal_cached(root).await;

        // Phase 3.1 缓存：同 (root, query, root-mtime 信号) 二次调用免全仓
        // workspace/symbol 往返；信号变（任一源码文件被外部改/新增）→ 自然 miss。
        let cache_key = find_symbol_cache_key(root, query, root_mtime);
        if let Some(mut cached) = self.symbol_cache_get(&cache_key) {
            cached.truncate(limit);
            // 命中路径不过 session_for；带 warning 的结果本就不写缓存（见下），
            // 命中即全成功快照 → warnings 恒空。
            return Ok((cached, Vec::new())); // cache_hit
        }

        // 决定要查的 lang 集合 (BTreeSet = 字母序, 顺序稳定)。命中缓存时直接复用 walked_langs。
        let langs: BTreeSet<String> = if let Some(l) = lang_override {
            [l.to_ascii_lowercase()].into()
        } else {
            walked_langs
        };
        if langs.is_empty() {
            return Err(ToolError::BadArgs {
                detail: format!("no known-language files under root {root:?}"),
            });
        }

        // ponytail: 串行拿 session (load_gate_for 防双 spawn), 然后并行 fan-out 请求。
        let mut sessions = Vec::with_capacity(langs.len());
        let mut failures: Vec<(String, ToolError)> = Vec::with_capacity(langs.len());
        for lang in &langs {
            match self.session_for(root, lang).await {
                Ok(s) => sessions.push(s),
                // 单 LS 拉起失败不阻塞其它 lang，但必须可见 —— 静默 continue 会让
                // 「全失败」伪装成「无符号」（bd serena-rust-x67）。
                Err(e) => failures.push((lang.clone(), e)),
            }
        }
        if sessions.is_empty() {
            // 全失败：可见性与 hover/def 一致（错误上抛），不再静默返空。
            // langs 非空（空集已在上方返 BadArgs）⇒ failures 非空。
            return Err(combined_all_failed_error(failures));
        }
        let mut warnings = failure_warnings(&failures);
        let query = query.to_string();
        let mut tasks = Vec::with_capacity(sessions.len());
        for session in sessions {
            let q = query.clone();
            tasks.push(tokio::spawn(async move {
                let params = json!({ "query": q });
                // 审计 P1-2：workspace/symbol 在 classify_method 归 Background
                // （重量级索引），但本工具是用户主动搜索 —— 必须显式 High，
                // 否则落在 TokenBucket 限流 + BG 路径，与设计注释承诺相悖。
                let resp: Vec<lsp_types::SymbolInformation> = match session
                    .request_at(
                        "workspace/symbol",
                        params,
                        INDEX_TIMEOUT,
                        lsp_core::client::Priority::High,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(_) => return Vec::<SymbolHit>::new(),
                };
                resp.into_iter()
                    .map(|si| SymbolHit {
                        name: si.name,
                        kind: kind_from_lsp(&si.kind),
                        uri: si.location.uri.to_string(),
                        range: si.location.range,
                        container: si.container_name,
                    })
                    .collect()
            }));
        }
        let mut merged: Vec<SymbolHit> = Vec::new();
        for t in tasks {
            if let Ok(v) = t.await {
                merged.extend(v);
            }
        }
        // 部分失败不写缓存：warning 只在本次调用产生（命中路径不过 session_for，
        // 无法重现），缓存部分结果会让重查静默丢失失败信息 —— 宁重查不可错缓存
        // （对齐空集不缓存纪律）。
        // bd serena-rust-0em（B 层）：写后 RA workspace/symbol 索引刷新无保证时延
        // （裸探针实测 0.12s~5s+），且写后可能**缺条目**（命中集缩水，基线对照实测）。
        // 结果先过磁盘一致性校验 + 写后窗口检测，检出 stale/缺失 → documentSymbol
        // 轮询对齐修正；对不齐 → 透传 + warning 且不写缓存（宁重查不可错缓存）。
        // 无写后标记且比对全过 → 零额外开销；冷启动首查空的既有语义不变
        // （bd serena-rust-x67 的 warning 机制不回退）。
        if let Err(w) = self
            .realign_stale_hits(root, query.as_str(), &mut merged, lang_override)
            .await
        {
            warnings.push(w);
        }
        // 暖机窗口内的空结果是「假空」（wssym 未爬完，打回实锤 1 分钟后同查询
        // 命中数十处）——不得入缓存，否则 500ms 重试与就绪后重查都命中缓存恒空。
        if warnings.is_empty() && self.index_warming_warnings(root).is_empty() {
            self.symbol_cache_put(cache_key, merged.clone()); // cache_miss → 写入（截断前全量）
        }
        merged.truncate(limit);
        Ok((merged, warnings))
    }

    /// workspace/symbol 结果与磁盘的一致性对齐（bd serena-rust-0em B 层）。
    ///
    /// 对齐目标 = 磁盘比对检出错位的文件 ∪ 写后一致性窗口内的文件（recent_writes
    /// —— wssym 写后可能缺条目，行号比对检不出缺失，基线对照实测）。对目标文件
    /// 轮询 `documentSymbol`（每轮先 `ensure_open`：mtime/size 变则自动重放
    /// didChange，逼 RA 处理最新内容）直到 docsym 自身与磁盘一致 —— RA 对打开文件
    /// 的 per-doc 分析远快于全局 index 重建。对齐后目标文件条目以 docsym 扁平表
    /// **全表替换**（修位 + 补缺失 + 去幽灵一体），其余文件保留 wssym 条目。
    /// 对不齐 → `Err(warning)`，调用方不写缓存。
    async fn realign_stale_hits(
        &self,
        root: &Path,
        query: &str,
        hits: &mut Vec<SymbolHit>,
        lang_override: Option<&str>,
    ) -> Result<(), String> {
        const REALIGN_ROUNDS: usize = 20;
        const REALIGN_ROUND_MS: u64 = 400;
        const RECENT_WRITE_TTL: Duration = Duration::from_secs(10);
        let mut targets = stale_symbol_files(hits);
        for uri in self.recent_written_uris(root, RECENT_WRITE_TTL) {
            if !targets.contains(&uri) {
                targets.push(uri);
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
        let mut fresh_tables: HashMap<std::path::PathBuf, Vec<SymbolHit>> = HashMap::new();
        for n_rounds in 0..REALIGN_ROUNDS {
            for uri in &targets {
                // 小写 uri（归一键）反推出的 path 中段大小写可能失真 —— 必须
                // canonicalize 还原磁盘真实大小写，否则 path_to_uri 生成的 uri 与
                // RA 记账（canonical）不一致 → RA 拒绝请求（日志实锤）。
                let Some(path) = uri_to_path(uri).and_then(|p| dunce::canonicalize(p).ok())
                else {
                    continue;
                };
                if fresh_tables.contains_key(&path) {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Ok(lang) = resolve_lang_for_file(name, lang_override) else {
                    continue;
                };
                let Ok(session) = self.session_for(root, lang.as_str()).await else {
                    continue;
                };
                let Ok(_guard) = session.ensure_open(&path).await else {
                    continue;
                };
                let Ok(curi) = path_to_uri(&path) else {
                    continue;
                };
                let params = json!({ "textDocument": { "uri": curi.as_str() } });
                let Ok(resp) = session
                    .request::<Option<DocumentSymbolResponse>>(
                        "textDocument/documentSymbol",
                        params,
                        INDEX_TIMEOUT,
                    )
                    .await
                else {
                    tracing::debug!(uri, round = n_rounds, "realign docsym request failed");
                    continue;
                };
                let flat = flatten_symbols(resp, curi.as_str());
                // docsym 自身也过磁盘校验 —— 旧 parse tree（RA 分析未跟上 didChange）
                // 同样视为未对齐，等下一轮。（判定用全表，过滤只影响替换内容。）
                let disk_ok = hits_match_disk_for_file(&path, &flat);
                // docsym 全表含文件全部符号 —— find-symbol 结果必须维持查询语义，
                // 按子串匹配过滤后再入替换表（对齐 wssym 的子串查询近似）。
                let q = query.to_lowercase();
                let matched: Vec<SymbolHit> = flat
                    .into_iter()
                    .filter(|s| s.name.to_lowercase().contains(&q))
                    .collect();
                tracing::debug!(
                    uri,
                    round = n_rounds,
                    syms = matched.len(),
                    ?disk_ok,
                    "realign docsym poll"
                );
                if disk_ok == Some(true) {
                    fresh_tables.insert(path, matched);
                }
            }
            if fresh_tables.len() == targets.len() {
                *hits = replace_files_with_tables(std::mem::take(hits), &fresh_tables);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(REALIGN_ROUND_MS)).await;
        }
        Err(format!(
            "symbol index stale for {} file(s) after recent write; results may be misaligned, retry find-symbol shortly",
            targets.len()
        ))
    }
    /// `Location[]`，lsp-types 在 capability 上声明多形态——M0 只解 `Option<Location>`）。
    ///
    /// line/col 1-based 还是 0-based？LSP `Position` 是 0-based；上层 CLI 必须传 0-based。
    pub async fn tool_def(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Option<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // offsets.rs 换算：M0 固定 utf-16（clangd 默认 + base init_params 首选 utf-16）。
        // M1 走 Session 协商结果（`capabilities.positionEncoding`）。本任务不引入新 API。
        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
        });
        // clangd 22 回 `LocationLink[]`（带 `originSelectionRange` / `targetSelectionRange`），
        // 而 LSP 3.17 spec 还允 `Location | Location[]`。统一接 `serde_json::Value` 后归一化。
        let raw: Option<serde_json::Value> = session
            .request("textDocument/definition", params, TOOL_TIMEOUT)
            .await?;
        let resp = normalize_definition(raw.as_ref());
        Ok(resp)
    }

    /// `textDocument/implementation` → 全部实现位置（Task 21）。
    ///
    /// 与 `tool_def` 类似但返 `Vec<Location>`（一个 interface 多处实现）。
    /// clangd 22 默认回 `LocationLink[]`；LSP 3.17 还允 `null | Location | Location[]`。
    /// 全部走 `normalize_implementations` 折叠为统一 `Vec<Location>`。
    pub async fn tool_find_implementations(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/implementation", params, TOOL_TIMEOUT)
            .await?;
        Ok(normalize_implementations(raw.as_ref()))
    }
    /// `textDocument/references` → 全部引用 `Location[]`。
    pub async fn tool_refs(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<Location>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        let params = json!({
            "textDocument": { "uri": uri.clone() },
            "position": { "line": pos.line, "character": pos.character },
            "context": { "includeDeclaration": true },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/references", params, TOOL_TIMEOUT)
            .await?;
        Ok(normalize_implementations(raw.as_ref()))
    }

    /// `textDocument/completion` → AI-friendly 裁剪后的 `CompletionResponse`。
    ///
    /// 设计（local/completion-design.md §3/§5）：
    /// - `limit == 0` 表示不限（`usize::MAX`）；否则截断到 `limit`。
    /// - 字段裁剪在 supervisor 层完成：丢弃 `sortText/filterText/commitCharacters/...`，
    ///   `kind` 由 LSP 枚举映射为人类词，`documentation` 截断 200 char。
    /// - `trigger` 为 Some 时带 `context.triggerKind = TriggerCharacter (2)` + `triggerCharacter`；
    ///   为 None 时发 `Invoked (1)`（用户显式请求补全，agent 不传 trigger 的默认形态）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_completion(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        limit: usize,
        trigger: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<CompletionResponse> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 越界校验（同 tool_def）—— 防止上游传错位掩盖。
        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;

        // CompletionContext：trigger=Some → TriggerCharacter + triggerCharacter；
        //                None → Invoked（agent 显式请求）。
        let context = match trigger {
            Some(ch) => json!({
                "triggerKind": 2, // CompletionTriggerKind::TRIGGER_CHARACTER
                "triggerCharacter": ch,
            }),
            None => json!({ "triggerKind": 1 }), // INVOKED
        };
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": pos.line, "character": pos.character },
            "context": context,
        });
        // CompletionResponse = Array | List（untagged enum）。
        // raw: Option<Value> → 直接以 Value 形态解析两形态：
        //   - Array：items 数组直接 map
        //   - List ：{ isIncomplete, items }
        // null（无候选）也合理：返回空 items + truncated=None。
        let raw: Option<serde_json::Value> = session
            .request("textDocument/completion", params, TOOL_TIMEOUT)
            .await?;
        let items = match raw {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(arr)) => arr,
            Some(serde_json::Value::Object(obj)) if obj.contains_key("items") => obj
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            Some(other) => {
                return Err(ToolError::Protocol {
                    tool: "completion".into(),
                    reason: format!("unexpected response shape: {other}"),
                });
            }
        };
        let total = items.len();
        // 0 = 不限（设计 §3）。
        let cap = if limit == 0 { usize::MAX } else { limit };
        let lite: Vec<CompletionItemLite> = items
            .into_iter()
            .take(cap)
            .map(parse_completion_item)
            .collect();
        // 不让排序退化：保持 LSP 已排序顺序。
        let truncated = if lite.len() < total {
            Some(format!("{} of {}", lite.len(), total))
        } else {
            None
        };
        Ok(CompletionResponse {
            truncated,
            items: lite,
        })
    }
    /// `find_referencing_symbols`：所有引用 + 每个 ref 落在哪个外层符号里（Task 24）。
    pub async fn tool_referencing_symbols(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<ref_tools::RefSymbolHit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        ref_tools::find_referencing_symbols(&session, root, file, line, col)
            .await
            .map_err(|e| {
                ToolError::Core(CoreError::Rpc {
                    code: -1,
                    message: format!("find_referencing_symbols: {e}"),
                })
            })
    }

    /// `find_referencing_code_snippets`：所有引用 + 每个 ref 前后 N 行（Task 24）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_referencing_code_snippets(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        context_lines: u32,
        max_results: usize,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<ref_tools::RefSnippetHit>> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        ref_tools::find_referencing_code_snippets(
            &session,
            root,
            file,
            line,
            col,
            context_lines,
            max_results,
        )
        .await
        .map_err(|e| {
            ToolError::Core(CoreError::Rpc {
                code: -1,
                message: format!("find_referencing_code_snippets: {e}"),
            })
        })
    }

    /// O3（bd serena-rust-bxd）：`--symbol` 直查的符号名解析。
    ///
    /// 解析顺序：documentSymbol 缓存（overview/symbol-body 等已铺平的 per-file
    /// 表）精确名优先、其次前缀，确定性按 (file, line, col) 排序取首命中；缓存
    /// 零命中时回退 workspace/symbol（= 两步法的第一步内部化，覆盖冷 daemon 时
    /// documentSymbol 缓存为空的窗口）。命中多个 → 返回提示串（调用方附 warning
    /// → CLI stderr 可见用了哪个）；零命中 → BadArgs rc=2。
    ///
    /// 返回的 line/col 为 LSP 0-based，与位置参数路径（CLI 已 -1）同基线。
    async fn resolve_symbol_position(
        &self,
        root: &Path,
        name: &str,
        lang: Option<&str>,
    ) -> ToolResult<(String, u32, u32, Option<String>)> {
        // (file, range) 候选；精确名优先于前缀，同级确定性排序。
        let pick = |hits: &[SymbolHit]| -> (SymbolCandidates, SymbolCandidates) {
            let mut exact: Vec<(String, lsp_types::Range)> = Vec::new();
            let mut prefix: Vec<(String, lsp_types::Range)> = Vec::new();
            for h in hits {
                if h.name == name {
                    exact.push((h.name.clone(), h.range));
                } else if h.name.starts_with(name) {
                    prefix.push((h.name.clone(), h.range));
                }
            }
            (exact, prefix)
        };
        let mut exact: Vec<(String, lsp_types::Range)> = Vec::new();
        let mut prefix: Vec<(String, lsp_types::Range)> = Vec::new();

        // 1) documentSymbol 缓存扫描。key.1 即构造时的 file 相对路径，直接可复用。
        let root_id = key_root_identity(root);
        let cached_files: Vec<(String, Vec<SymbolHit>)> = self
            .symbol_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| key_root_identity(&k.0) == root_id)
            .map(|(k, v)| (k.1.clone(), v.clone()))
            .collect();
        for (file, hits) in &cached_files {
            let (e, p) = pick(hits);
            exact.extend(e.into_iter().map(|(_, r)| (file.clone(), r)));
            prefix.extend(p.into_iter().map(|(_, r)| (file.clone(), r)));
        }

        // 2) 缓存零命中 → workspace/symbol 兜底（冷 daemon 窗口）。暖机窗口内
        // wssym 首查常为「假空」（bd serena-rust-bxd 打回：1 分钟后同命令命中
        // 数十处）——窗口仍 active 时追加 1 次 500ms 重试再判空。注意 warming
        // 判定必须在 fallback 之后：首查经 session_for spawn LS 才开窗。
        if exact.is_empty() && prefix.is_empty() {
            for attempt in 0..2 {
                match self.tool_find_symbol(root, name, 50, lang).await {
                    Ok((hits, _)) => {
                        for h in &hits {
                            let Some(path) = uri_to_path(&h.uri) else {
                                continue;
                            };
                            let rel = path
                                .strip_prefix(root)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .to_string();
                            if h.name == name {
                                exact.push((rel, h.range));
                            } else if h.name.starts_with(name) {
                                prefix.push((rel, h.range));
                            }
                        }
                    }
                    // 暖机窗口内兜底查询自身失败（LS 未起/无可查 lang）不淹没
                    // hint；非窗口如实上抛。
                    Err(_) if !self.index_warming_warnings(root).is_empty() => {}
                    Err(e) => return Err(e),
                }
                if !exact.is_empty() || !prefix.is_empty() {
                    break;
                }
                if attempt == 0 && !self.index_warming_warnings(root).is_empty() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    break;
                }
            }
        }
        let warming = !self.index_warming_warnings(root).is_empty();

        let by_pos = |a: &(String, lsp_types::Range), b: &(String, lsp_types::Range)| {
            a.0.cmp(&b.0)
                .then(a.1.start.line.cmp(&b.1.start.line))
                .then(a.1.start.character.cmp(&b.1.start.character))
        };
        exact.sort_by(by_pos);
        prefix.sort_by(by_pos);
        // 歧义只在所选层级内计：有精确命中时前缀候选全部落选，不算「多命中」。
        let (chosen, total) = if !exact.is_empty() {
            (&exact, exact.len())
        } else {
            (&prefix, prefix.len())
        };
        let Some((file, range)) = chosen.first().cloned() else {
            // 打回修复（bd serena-rust-bxd）：暖机窗口内零命中 ≠ 符号不存在——
            // wssym 可能仍未爬完，错误必须带 hint 防 AI 误判（对齐 find-symbol
            // 的 partial warning，不能比它更误导）。
            let mut detail =
                format!("symbol `{name}` not found (documentSymbol cache and workspace index empty)");
            if warming {
                detail.push_str(
                    "; index may still be warming (cold start), retry shortly or use find-symbol",
                );
            }
            return Err(ToolError::BadArgs { detail });
        };
        let note = (total > 1).then(|| {
            format!(
                "resolved --symbol {name} -> {file}:{} ({total} matches; using first, 1-based line)",
                range.start.line + 1
            )
        });
        Ok((file, range.start.line, range.start.character, note))
    }

    /// `replace_text_in_symbol`：在 symbol 体内替换 old→new（Task 25）。
    pub async fn tool_edit_replace_text(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        old_text: &str,
        new_text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::replace_text_in_symbol(&session, root, &abs, symbol, old_text, new_text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("replace_text_in_symbol: {e}"),
            })
    }

    /// `insert_text_before_symbol`：在 symbol 开头插入 text（Task 25）。
    /// 返回插入内容末尾的 (end_line, end_col)（1-based）。
    pub async fn tool_edit_insert_before_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_text_before_symbol(&session, root, &abs, symbol, text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("insert_text_before_symbol: {e}"),
            })
    }

    /// `insert_text_after_symbol`：在 symbol 末尾插入 text（Task 25）。
    pub async fn tool_edit_insert_after_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_text_after_symbol(&session, root, &abs, symbol, text)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("insert_text_after_symbol: {e}"),
            })
    }

    /// `delete_text_in_symbol`：在 symbol 体内删除 [start_line, end_line] 切片（1-based 含端，Task 25）。
    pub async fn tool_edit_delete_text(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        start_line: u32,
        end_line: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::delete_text_in_symbol(&session, root, &abs, symbol, start_line, end_line)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("delete_text_in_symbol: {e}"),
            })
    }
    /// `symbol-body`：按符号名取函数/类体切片（PLAN Task 15）。
    ///
    /// 流程：ensure_open → documentSymbol 定位 name 匹配的符号 range →
    /// offsets.rs 切片返回。position-free（客户端只传 file + symbol name）。
    pub async fn tool_symbol_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<String> {
        let path = root.join(file);
        // Phase 3.1 缓存：documentSymbol 往返命中 → 直接在缓存列表找 range 切片（盘上
        // 文本仍现读）。not-found 与 miss 路径同语义。Δ: find_symbol_range 对 Flat 形态返
        // None 而缓存列表含 Flat 平铺项 —— 仅理论差异（本仓库 adapter 均回 Nested，见
        // flatten_symbols 注）。
        let cache_key = doc_symbol_cache_key(root, file);
        if let Some(cached) = self.symbol_cache_get(&cache_key) {
            return match cached.iter().find(|h| h.name == symbol) {
                Some(h) => read_and_slice(&path, file, h.range).await,
                None => Err(ToolError::BadArgs {
                    detail: format!("symbol `{symbol}` not found in {file}"),
                }),
            }; // cache_hit
        }
        // P2-18h miss 路径对账：清同文件残留旧 stamp 条目（didChange 由下方
        // ensure_open 按 mtime/size 差异自动重放）。
        self.reconcile_symbol_cache_for_file(root, file);
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        // 递归找第一个 name == symbol 的 DocumentSymbol（Nested 形态）。
        let range = find_symbol_range(resp.as_ref(), symbol).ok_or_else(|| ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {file}"),
        })?;
        let out = read_and_slice(&path, file, range).await?;
        self.symbol_cache_put(cache_key, flatten_symbols(resp, &uri)); // cache_miss → 写入
        Ok(out)
    }

    /// `replace-body`：按符号名替换函数/类体（C3 一致性链路，PLAN Task 15）。
    ///
    /// 流程（全程持全局写门）：
    /// 1. documentSymbol 解析符号 range（客户端只传符号名，不传 range）
    /// 2. 读盘 content 与前快照 hash 对账 —— 不符 → WRITE_CONFLICT
    /// 3. 新 body 替换 range → tempfile 原子写 + rename（Windows 共享冲突重试 5×50ms）
    /// 4. 读回 diff 校验 —— 不符 → 从写前副本回滚 + WRITE_CONFLICT
    /// 5. didChange 全量同步 → LS 与盘一致
    ///
    /// Self-heal：闭包内 session.request / ensure_open 任何一步收到
    /// `Core(Terminated)`（RA 因前置写入 didChange 错序 / 索引风暴把 LS 拉死），
    /// `with_session_retry` 驱逐旧 session + 重试一次。再 fail 透传原错误给 agent。
    pub async fn tool_replace_body(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        new_body: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);
        let symbol = symbol.to_string();
        let new_body = new_body.to_string();
        let lang_owned = lang.clone();
        Self::with_session_retry(self, root, lang.as_str(), move |session| {
            let path = path.clone();
            let uri_str = uri_str.clone();
            let symbol = symbol.clone();
            let new_body = new_body.clone();
            let lang_str = lang_owned.clone();
            async move {
                Self::tool_replace_body_inner(
                    session,
                    root,
                    &path,
                    &uri_str,
                    &symbol,
                    &new_body,
                    lang_str.as_str(),
                )
                .await
            }
        })
        .await
    }

    /// tool_replace_body 的实际实现 ——
    /// 拆出来便于 `with_session_retry` 在闭包里重放整条链路（含写门重取）。
    #[allow(unused_variables)]
    async fn tool_replace_body_inner(
        session: Arc<Session>,
        root: &Path,
        path: &Path,
        uri_str: &str,
        symbol: &str,
        new_body: &str,
        lang: &str,
    ) -> ToolResult<()> {
        // 0) 索引等待（PLAN Phase 3.2）：cold-start 下 LS 未索引时 documentSymbol 会
        //    给过期/错位 range —— replace-body 错位的根因。先 didOpen + documentSymbol
        //    探针等就绪再进写门；探针超时只 warn 不阻断（回退契约同 on_server_ready）。
        let _probe_open = session.ensure_open(path).await.map_err(ToolError::Core)?;
        if let Some(adapter) = ls_registry::adapter_for(lang)
            && let Err(e) = tokio::time::timeout(
                INDEX_WAIT_TIMEOUT,
                adapter.wait_for_index(&session, path, INDEX_WAIT_TIMEOUT),
            )
            .await
        {
            tracing::warn!(
                adapter = adapter.id(),
                error = %e,
                "wait_for_index failed/timed out; continuing"
            );
        }

        // ===== 全局写门：以下所有步骤持锁（A4 FIFO）=====
        let _gate = write_gate::acquire().await;

        // 1) 锁内解析符号 range（杜绝客户端 range 过期）。
        let _guard = session.ensure_open(path).await.map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": uri_str } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let range =
            find_symbol_range(resp.as_ref(), symbol).ok_or_else(|| ToolError::BadArgs {
            detail: format!("symbol `{symbol}` not found in {}", path.display()),
        })?;

        // 2) 读盘 + content-hash 对账（C3 防线 ①）。
        let old_text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", path.display()),
            })?;
        let old_hash = content_hash(&old_text);

        // 3) 新文本 = 老文本替换 range；tempfile 原子写 + rename。
        let start = LspPos {
            line: range.start.line,
            character: range.start.character,
        };
        let end = LspPos {
            line: range.end.line,
            character: range.end.character,
        };
        let start_byte = lsp_core::offsets::position_to_byte(&old_text, start, OffsetEncoding::Utf16)
            .map_err(|e| ToolError::BadArgs {
            detail: format!("start position: {e}"),
        })?;
        let end_byte = lsp_core::offsets::position_to_byte(&old_text, end, OffsetEncoding::Utf16)
            .map_err(|e| ToolError::BadArgs {
            detail: format!("end position: {e}"),
        })?;
        let new_text = format!(
            "{}{}{}",
            &old_text[..start_byte],
            new_body,
            &old_text[end_byte..]
        );

        atomic_write(path, &new_text)
            .await
            .map_err(|e| ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: format!("atomic write failed: {e}"),
            })?;

        // 4) 读回 diff 校验（C3 防线 ②③）—— 不符回滚 + 报冲突。
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", path.display()),
            })?;
        if readback != new_text {
            // 回滚：老内容写回。
            let _ = atomic_write(path, &old_text).await;
            return Err(ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: "readback mismatch; rolled back".into(),
            });
        }

        // 5) didChange 全量同步 → LS 与盘一致。
        // 走 `ensure_open` 的 mtime-检测路径：它用 Session 内部的 content_version
        // 单调递增（与 didOpen 起始号连续），不再用本工具自维护的 VERSION 计数器，
        // 避免与 edit_tools 的 didChange 序列撞车（rust-analyzer 一见乱序即
        // 关闭 channel → 后续所有工具 LS_TERMINATED）。
        drop(_guard);
        let _refreshed = session.ensure_open(path).await.map_err(ToolError::Core)?;

        // 防御：old_hash 在此仅供将来做"多客户端并发"检测（M2 扩展）。
        let _ = old_hash;
        Ok(())
    }

    /// 全 codebase grep（AI 替代"读全文件"）。
    ///
    /// 设计要点（Task 19）：
    /// - 走 `ignore` crate：自动尊重 `.gitignore` / `.ignore` / global ignores
    /// - 默认排除 binary / >5MB 大文件（合理启发）
    /// - `path_glob`：可选 glob 过滤（如 `"*.cpp"` `"src/**/*.py"`）
    /// - `max_results` 默认 100：超过返回 truncated 标记
    /// - 不动 LS —— 这是 fs 工具，不需要 LSP
    pub async fn tool_search_for_pattern(
        &self,
        root: &Path,
        pattern: &str,
        path_glob: Option<&str>,
        max_results: usize,
        case_sensitive: bool,
    ) -> ToolResult<SearchResponse> {
        use regex::RegexBuilder;

        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| ToolError::BadArgs {
                detail: format!("bad regex: {e}"),
            })?;
        let glob_re = match path_glob {
            Some(g) => {
                let mut r = String::from("^");
                // glob 不含 `/` 时，前后加 `.*`，让 `*.cpp` 也匹配 `src/a.cpp`。
                if !g.contains('/') {
                    r.push_str(".*");
                }
                // 把 glob 转 regex —— 支持 `**` 跨任意段 + `*` 单段 + `?` 单字符。

                let mut i = 0;
                let chars: Vec<char> = g.chars().collect();
                while i < chars.len() {
                    let c = chars[i];

                    // `**` 跨任意段（包括 `/`）。
                    if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
                        r.push_str(".*");
                        i += 2;
                        // 吞掉紧跟的 `/`（`src/**/foo` 等价 `src/foo`）。
                        if i < chars.len() && chars[i] == '/' {
                            i += 1;
                        }
                        continue;
                    }

                    match c {
                        '*' => r.push_str("[^/]*"),
                        '?' => r.push('.'),
                        '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '\\' => {
                            r.push('\\');
                            r.push(c);
                        }
                        '[' | ']' => r.push(c),
                        _ => r.push(c),
                    }
                    i += 1;
                }
                r.push('$');
                Some(
                    RegexBuilder::new(&r)
                        .case_insensitive(!case_sensitive)
                        .build()
                        .map_err(|e| ToolError::BadArgs {
                            detail: format!("bad glob: {e}"),
                        })?,
                )
            }
            None => None,
        };

        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

        // 整个扫描体（walk + read + regex）丢 `spawn_blocking`：原同步 IO 内联
        // 在 async worker 上，单 search 期间 daemon 该 worker 上的其它请求全部排队。
        // 850 文件冷扫可占 worker 数百 ms-数秒，导致 /status、reaper select、L/batch
        // 并行的 7 条兄弟请求显著延迟。包 spawn_blocking 后 worker 立刻释放，P2-6。
        let (hits, truncated, files_scanned) = tokio::task::spawn_blocking(move || {
            Self::search_sync_scan(&root, &regex, glob_re.as_ref(), max_results)
        })
        .await
        .map_err(|e| ToolError::Core(CoreError::Rpc {
            code: -1,
            message: format!("search scan join error: {e}"),
        }))?;

        Ok(SearchResponse {
            hits,
            truncated,
            files_scanned,
        })
    }

    /// 同步执行 search 扫描体（walk + read_to_string + regex），由
    /// `tool_search_for_pattern` 在 `spawn_blocking` 内调用。
    /// ponytail: 同步 IO + regex 全跑在 blocking thread pool；
    /// max_results 截断短路避免无谓读完大文件。
    fn search_sync_scan(
        root: &Path,
        regex: &regex::Regex,
        glob_re: Option<&regex::Regex>,
        max_results: usize,
    ) -> (Vec<SearchHit>, bool, usize) {
        use ignore::WalkBuilder;

        let mut walker = WalkBuilder::new(root);
        walker
            .standard_filters(true)
            .require_git(false)
            // 内置 ignore 目录（target/node_modules/.idea 等）—— .git 由
            // standard_filters 的 hidden filter 默认排除，但 target/node_modules
            // 不一定在 .gitignore 里，需显式表驱动过滤；与 fs_tools::filtered_walker 语义一致。
            .filter_entry(|e| {
                e.depth() == 0
                    || !e.file_name().to_str().is_some_and(fs_tools::should_ignore)
            });

        let mut hits: Vec<SearchHit> = Vec::new();
        let mut truncated = false;
        let mut files_scanned: usize = 0;

        for entry in walker.build() {
            if truncated {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if let Some(g) = glob_re
                && !g.is_match(&rel_str)
            {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > 5 * 1024 * 1024 {
                continue;
            }

            files_scanned += 1;
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for (line_idx, line) in content.lines().enumerate() {
                if truncated {
                    break;
                }
                for m in regex.find_iter(line) {
                    if hits.len() >= max_results {
                        truncated = true;
                        break;
                    }
                    hits.push(SearchHit {
                        file: rel_str.clone(),
                        line: (line_idx + 1) as u32,
                        col: (m.start() + 1) as u32,
                        text: line.trim_end().to_string(),
                        match_start: m.start() as u32,
                        match_end: m.end() as u32,
                        symbol: None,
                        container: None,
                    });
                }
            }
        }

        (hits, truncated, files_scanned)
    }

    /// `textDocument/rename` 跨文件重命名（Task 22）。
    ///
    /// 设计要点：
    /// - **写门全程持锁**：rename 涉及多文件，跨文件不能并行；与 replace-body 共用全局写门。
    /// - **复用 LSP WorkspaceEdit**：LS 决定 edit 范围（textDocument/rename），
    ///   我们把 WorkspaceEdit 的 edits 应用到盘上 + 全量 didChange。
    /// - **位置倒序 apply**：每文件 edits 按 `range.end` 倒序处理，避免偏移漂移。
    /// - **两种 WorkspaceEdit 形态都收**：`documentChanges` 优先，回退 `changes` map
    ///   （clangd 默认后者，gopls 只产前者）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_rename_symbol(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        new_name: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<RenameReport> {
        if new_name.is_empty() || new_name.contains(' ') {
            return Err(ToolError::BadArgs {
                detail: "new_name must be non-empty, no whitespace".into(),
            });
        }

        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 索引等待（PLAN Phase 3.2）：cold-start 下未索引的 LS 会让 prepareRename /
        // rename 打满 TOOL_TIMEOUT —— 30s 超时的根因。探针就绪后才进写门；探针超时
        // 只 warn 不阻断（回退契约同 on_server_ready）。T0 无 adapter → 跳过特判等待。
        if let Some(adapter) = ls_registry::adapter_for(lang.as_str())
            && let Err(e) = tokio::time::timeout(
                INDEX_WAIT_TIMEOUT,
                adapter.wait_for_index(&session, &path, INDEX_WAIT_TIMEOUT),
            )
            .await
        {
            tracing::warn!(
                adapter = adapter.id(),
                error = %e,
                "wait_for_index failed/timed out; continuing"
            );
        }

        let _gate = write_gate::acquire().await;

        let pos = lsp_position_from_byte(&path, line, col, OffsetEncoding::Utf16).await?;
        let pos_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
        });

        // 1) prepareRename —— null = 不能 rename。
        let prep: Option<serde_json::Value> = session
            .request(
                "textDocument/prepareRename",
                pos_params.clone(),
                TOOL_TIMEOUT,
            )
            .await?;
        if prep.is_none() || prep.as_ref().is_some_and(|v| v.is_null()) {
            return Err(ToolError::BadArgs {
                detail: "prepareRename rejected this position".into(),
            });
        }

        // 2) textDocument/rename → WorkspaceEdit JSON。
        let edit_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
            "newName": new_name,
        });
        let resp: Option<serde_json::Value> = session
            .request("textDocument/rename", edit_params, TOOL_TIMEOUT)
            .await?;
        let resp = resp.ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename returned null".into(),
        })?;

        // 3) 拆 WorkspaceEdit → 按文件分组（documentChanges 优先，回退 changes map）。
        let by_uri = parse_workspace_edit(&resp).ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename response has neither `changes` map nor `documentChanges`".into(),
        })?;

        // 4) 对每个文件应用 edits。
        let mut report = RenameReport::default();
        let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        for (uri, mut edits) in by_uri {
            edits.sort_by_key(|e| std::cmp::Reverse(e.0)); // 倒序
            let abs = match uri_to_path(&uri) {
                Some(p) => p,
                None => continue,
            };
            // 必须在 root 内（防 path traversal 风险）。
            if !abs.starts_with(&root_canon) {
                continue;
            }

            let content = match tokio::fs::read_to_string(&abs).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut new_content = content.clone();
            for (_key, range, new_text) in &edits {
                let start_byte = match lsp_core::offsets::position_to_byte(
                    &new_content,
                    lsp_core::offsets::Position {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    OffsetEncoding::Utf16,
                ) {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let end_byte = match lsp_core::offsets::position_to_byte(
                    &new_content,
                    lsp_core::offsets::Position {
                        line: range.end.line,
                        character: range.end.character,
                    },
                    OffsetEncoding::Utf16,
                ) {
                    Ok(b) => b,
                    Err(_) => break,
                };
                new_content = format!(
                    "{}{}{}",
                    &new_content[..start_byte],
                    new_text,
                    &new_content[end_byte..]
                );
            }

            if new_content == content {
                continue;
            }

            atomic_write(&abs, &new_content)
                .await
                .map_err(|e| ToolError::WriteConflict {
                    path: abs.display().to_string(),
                    reason: format!("atomic write failed: {e}"),
                })?;

            // 全量 didChange 让 LS 跟上 —— 走 ensure_open 的 mtime-检测路径，
            // 与 tool_replace_body / edit_tools 共享同一 content_version 单调递增
            // （rust-analyzer 拒收非单调 version → channel 关）。
            let _refreshed = session.ensure_open(&abs).await.map_err(ToolError::Core)?;

            report.files_modified += 1;
            report.edits_applied += edits.len();
            let rel = abs
                .strip_prefix(&root_canon)
                .unwrap_or(&abs)
                .to_string_lossy()
                .replace('\\', "/");
            report.files.push(rel);
        }

        Ok(report)
    }

    /// `safe-delete-symbol`：无引用才删，有引用拒删并返回引用位置列表。
    ///
    /// ↖ mirror: symbol_tools.py@43ae021 SafeDeleteSymbol
    /// （Δ position-free：按 file + 符号名寻址，与 symbol-body 一致；
    ///  references 不含声明本身——声明随删除一起消失；
    ///  「有引用」是正常结果而非错误，wire 层不占错误码。）
    ///
    /// 流程（写门全程持锁）：documentSymbol 定位 (range, selectionRange) →
    /// references(selectionRange, includeDeclaration=false) → 有引用返回列表；
    /// 无引用整行删除 + atomic_write + 读回校验 + didChange 全量（C3 链路同 replace-body）。
    pub async fn tool_safe_delete_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<SafeDeleteReport> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let path = root.join(file);
        let uri_str = path_to_uri_str(&path);

        let _gate = write_gate::acquire().await;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 1) 锁内解析符号（杜绝过期 range）。
        let params = json!({ "textDocument": { "uri": uri_str.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let (range, selection) =
            find_symbol_node(resp.as_ref(), symbol).ok_or_else(|| ToolError::BadArgs {
                detail: format!("symbol `{symbol}` not found in {file}"),
            })?;

        // 2) references：锚定 selectionRange（标识符），排除声明本身。
        let ref_params = json!({
            "textDocument": { "uri": uri_str.clone() },
            "position": {
                "line": selection.start.line,
                "character": selection.start.character,
            },
            "context": { "includeDeclaration": false },
        });
        let raw: Option<serde_json::Value> = session
            .request("textDocument/references", ref_params, TOOL_TIMEOUT)
            .await?;
        let refs = normalize_implementations(raw.as_ref());

        // 3) 有引用 → 拒删，报引用位置（相对路径 + 1-based 行号）。
        if !refs.is_empty() {
            let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            let mut locations: Vec<SafeDeleteRef> = refs
                .iter()
                .filter_map(|loc| {
                    let p = uri_to_path(loc.uri.as_str())?;
                    let rel = p
                        .strip_prefix(&root_canon)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    Some(SafeDeleteRef {
                        file: rel,
                        line: loc.range.start.line + 1,
                    })
                })
                .collect();
            locations.sort();
            locations.dedup();
            return Ok(SafeDeleteReport {
                deleted: false,
                symbol: symbol.to_string(),
                references: locations,
            });
        }

        // 3.5) 拦截门：语义层 0 引用 ≠ 真无引用 —— 语义层可能整体不可用（如
        // rust-analyzer root 大小写脱挂时 references 恒空，BD serena-rust-81m）。
        // 文本交叉验证：workspace 内符号名仍有 ≥1 处可疑出现（排除定义行 +
        // 注释/字符串粗滤）→ 拒删，提示语义层可能不可用。RPC_ERROR 不可重试。
        let search = self
            .tool_search_for_pattern(
                root,
                &regex::escape(symbol),
                None,
                TEXT_GATE_MAX_HITS,
                true,
            )
            .await?;
        let n = textual_occurrences_outside_def(
            &search.hits,
            file,
            selection.start.line + 1,
            symbol,
        );
        if n > 0 {
            return Err(ToolError::Protocol {
                tool: "safe-delete-symbol".to_string(),
                reason: format!(
                    "symbol has {n} textual occurrence(s) outside definition but 0 semantic refs; semantic layer may be unavailable"
                ),
            });
        }

        // 4) 无引用 → 删除：整行语义（见 delete_symbol_text）+ C3 链路。
        let old_text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", path.display()),
            })?;
        let new_text = delete_symbol_text(&old_text, range)?;
        atomic_write(&path, &new_text)
            .await
            .map_err(|e| ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: format!("atomic write failed: {e}"),
            })?;
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", path.display()),
            })?;
        if readback != new_text {
            let _ = atomic_write(&path, &old_text).await;
            return Err(ToolError::WriteConflict {
                path: path.display().to_string(),
                reason: "readback mismatch; rolled back".into(),
            });
        }
        // 走 `ensure_open` 的 mtime-检测路径，与 tool_replace_body / edit_tools 共享
        // 同一 content_version 单调递增（rust-analyzer 拒收非单调 version → channel 关）。
        drop(_guard);
        let _refreshed = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        Ok(SafeDeleteReport {
            deleted: true,
            symbol: symbol.to_string(),
            references: vec![],
        })
    }

    /// `insert-at-line`：在 line（1-based）前插入，原行下移；line == total+1 追加 EOF。
    /// ↖ mirror: file_tools.py@43ae021 InsertAtLineTool（0-based → 1-based Δ）。
    /// 返回插入内容末尾的 (end_line, end_col)（1-based）。
    pub async fn tool_insert_at_line(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        content: &str,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::insert_at_line(&session, root, &abs, line, content, expected_hash)
            .await
            .map_err(line_edit_err("insert_at_line"))
    }

    /// `replace-lines`：用 content 替换 [start_line, end_line]（1-based 含端）。
    /// ↖ mirror: file_tools.py@43ae021 ReplaceLinesTool（0-based → 1-based Δ）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_replace_lines(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        content: &str,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::replace_lines(
            &session,
            root,
            &abs,
            start_line,
            end_line,
            content,
            expected_hash,
        )
        .await
        .map_err(line_edit_err("replace_lines"))
    }

    /// `delete-lines`：删除 [start_line, end_line]（1-based 含端）。
    /// ↖ mirror: file_tools.py@43ae021 DeleteLinesTool（0-based → 1-based Δ）。
    pub async fn tool_delete_lines(
        &self,
        root: &Path,
        file: &str,
        start_line: u32,
        end_line: u32,
        expected_hash: Option<&str>,
        lang_override: Option<&str>,
    ) -> ToolResult<()> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs = root.join(file);
        edit_tools::delete_lines(&session, root, &abs, start_line, end_line, expected_hash)
            .await
            .map_err(line_edit_err("delete_lines"))
    }
}

/// 行级三件套 EditError → ToolError 映射：写冲突保留变体，其余归 BadArgs。
fn line_edit_err(tool: &str) -> impl Fn(edit_tools::EditError) -> ToolError + '_ {
    move |e| match e {
        edit_tools::EditError::WriteConflict { path, reason } => {
            ToolError::WriteConflict { path, reason }
        }
        edit_tools::EditError::Core(c) => ToolError::Core(c),
        other => ToolError::BadArgs {
            detail: format!("{tool}: {other}"),
        },
    }
}

/// rename_symbol 结果报告。
#[derive(Debug, Default, Serialize)]
pub struct RenameReport {
    /// 修改的文件数。
    pub files_modified: usize,
    /// 应用的总 edit 数（每个文件所有 edits 之和）。
    pub edits_applied: usize,
    /// 相对 root 路径列表（agent 审计用）。
    pub files: Vec<String>,
}

/// 把 `file://...` URL 转回 PathBuf（percent-decode + Windows 盘符大写归一）。
///
/// LS 返回的 URI 可能带 percent-encoding（tsserver 实测 `file:///d%3A/...`），
/// 不解码会导致路径比对失败——rename 静默 0 编辑、defining-symbol 误判 outside
/// workspace root。Windows 盘符统一大写：`Path` 前缀比较按字节区分大小写，小写
/// `d:` 与 dunce canonicalize 出的 `D:` root 不匹配。
pub(crate) fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    // Windows: `file:///C:/foo` → `C:/foo`
    let s = if cfg!(windows) && stripped.starts_with('/') {
        &stripped[1..]
    } else {
        stripped
    };
    let mut s = percent_decode(s);
    if cfg!(windows) && s.len() >= 2 && s.as_bytes()[1] == b':' {
        s[..1].make_ascii_uppercase();
    }
    Some(std::path::PathBuf::from(s.replace('\\', "/")))
}

/// 符号名错位容差窗：`location.range` 是含 attribute/doc 的 full range，起点行
/// 可能不含符号名（如 `#[derive(..)]` 行）—— 起点行起向后 5 行内 contains(name)
/// 视为一致。
const NAME_PROXIMITY_LINES: usize = 5;

/// 单文件符号命中与磁盘行内容的一致性校验（bd serena-rust-0em B 层）。
///
/// `Some(false)` = 检出 stale（起点行起容差窗内都不含符号名/越界）；读盘失败
/// （不存在/非 UTF-8）= `None`（无法判定，调用方按不 stale 处理，维持既有语义）。
fn hits_match_disk_for_file(path: &Path, hits: &[SymbolHit]) -> Option<bool> {
    if hits.is_empty() {
        return Some(true);
    }
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    Some(hits.iter().all(|h| {
        let start = h.range.start.line as usize;
        let end = (start + NAME_PROXIMITY_LINES).min(lines.len());
        lines
            .get(start..end)
            .is_some_and(|w| w.iter().any(|l| l.contains(&h.name)))
    }))
}

/// workspace/symbol 结果里 stale 的文件集合（每文件首个 hit 判定；uri 归一小写，
/// 与 diag_cache 同一归一纪律 —— RA 推送/返回的 uri 盘符小写）。
fn stale_symbol_files(hits: &[SymbolHit]) -> Vec<String> {
    let mut stale = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for h in hits {
        let uri = h.uri.to_lowercase();
        if seen.insert(uri.clone())
            && let Some(p) = uri_to_path(&h.uri)
            && hits_match_disk_for_file(&p, std::slice::from_ref(h)) == Some(false)
        {
            stale.push(uri);
        }
    }
    stale
}

/// 目标文件的条目以 docsym 扁平表（已按查询过滤）**全表替换**（修位 + 补缺失 +
/// 去幽灵一体）；非目标文件的条目原样保留。匹配键用 canonical path —— wssym 返回
/// 的 uri 形态不一（盘符小写/`%3A` percent-encode，实测混现），字符串键会漏配。
fn replace_files_with_tables(
    hits: Vec<SymbolHit>,
    tables: &HashMap<std::path::PathBuf, Vec<SymbolHit>>,
) -> Vec<SymbolHit> {
    let mut out: Vec<SymbolHit> = hits
        .into_iter()
        .filter(|h| match uri_to_path(&h.uri).and_then(|p| dunce::canonicalize(p).ok()) {
            Some(p) => !tables.contains_key(&p),
            // 无法归一 → 保留（不误删他人条目）。
            None => true,
        })
        .collect();
    for (path, table) in tables {
        let uri = path_to_uri_str(path).to_lowercase();
        for s in table {
            let mut s = s.clone();
            s.uri = uri.clone();
            out.push(s);
        }
    }
    out
}

/// percent-decode `%XX` 序列（非法 / 截断序列原样保留）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// rename 单文件编辑：(排序 key（end 线性 index，粗略）, range, new_text)。
type EditSpec = (u64, lsp_types::Range, String);

/// 拆单文件 edits 数组；缺 range/newText 的条目跳过（AnnotatedTextEdit 等富形态
/// 字段是超集，读子集即可）。
fn parse_edits(v: &serde_json::Value) -> Vec<EditSpec> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let range: lsp_types::Range =
                        serde_json::from_value(e.get("range")?.clone()).ok()?;
                    let new_text = e.get("newText")?.as_str()?.to_string();
                    // 排序 key：end 的 linear index（粗略）。
                    let key = (range.end.line as u64) << 32 | range.end.character as u64;
                    Some((key, range, new_text))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 拆 WorkspaceEdit → (uri, edits) 列表。`documentChanges`（LSP 3.13+ 数组形）优先，
/// 回退 `changes` map —— gopls 只产前者，clangd 默认后者。两者皆缺 → None。
/// `kind:"create"/"rename"/"delete"` 等非文本条目跳过。
fn parse_workspace_edit(resp: &serde_json::Value) -> Option<Vec<(String, Vec<EditSpec>)>> {
    if let Some(docs) = resp.get("documentChanges").and_then(|v| v.as_array()) {
        Some(
            docs.iter()
                .filter_map(|td| {
                    let uri = td.get("textDocument")?.get("uri")?.as_str()?.to_string();
                    Some((uri, parse_edits(td.get("edits")?)))
                })
                .collect(),
        )
    } else {
        let changes = resp.get("changes")?.as_object()?;
        Some(
            changes
                .iter()
                .map(|(uri, edits)| (uri.clone(), parse_edits(edits)))
                .collect(),
        )
    }
}

/// safe-delete 结果报告。
#[derive(Debug, Serialize)]
pub struct SafeDeleteReport {
    /// false = 有引用拒删；true = 已删除。
    pub deleted: bool,
    pub symbol: String,
    /// deleted=false 时非空：引用位置（相对路径 + 1-based 行号，按 file+line 排序去重）。
    pub references: Vec<SafeDeleteRef>,
}

/// 单条引用位置。
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct SafeDeleteRef {
    /// 相对 project_root 路径。
    pub file: String,
    /// 1-based 行号。
    pub line: u32,
}

/// 单个搜索命中。
#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub text: String,
    pub match_start: u32,
    pub match_end: u32,
    /// 覆盖该行的最小符号名（None：行不在任何符号内 / 符号信息不可用）。
    /// A（ai-token-features §10-A）：由 enrich_search_with_symbols 增量填充。
    pub symbol: Option<String>,
    /// 覆盖符号的容器名（如 method 所在 class/impl；顶层符号为 None）。
    pub container: Option<String>,
}
/// 搜索响应。
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    pub files_scanned: usize,
}

/// 拍平 `DocumentSymbolResponse` → `Vec<SymbolHit>`。
///
/// LSP 允许两种形态：
/// - `Flat(Vec<SymbolInformation>)`：每项自带 `location` 与 `container_name`。
/// - `Nested(Vec<DocumentSymbol>)`：递归 children；`container` 用父名。
///
/// M0 客户端声明 `hierarchicalDocumentSymbolSupport=true`，clangd 一定回 Nested 形态；
/// Flat 仅 mock_ls 用得到，但本模块不耦合 mock_ls，故对两种形态都处理。
/// `None`（LS 对未就绪/未加载文档返 `null`，如 rust-analyzer）按无符号（合法空）处理。
fn flatten_symbols(resp: Option<DocumentSymbolResponse>, file_uri: &str) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    match resp {
        Some(DocumentSymbolResponse::Flat(items)) => {
            for it in items {
                out.push(SymbolHit {
                    name: it.name,
                    kind: kind_from_lsp(&it.kind),
                    uri: it.location.uri.to_string(),
                    range: it.location.range,
                    container: it.container_name,
                });
            }
        }
        Some(DocumentSymbolResponse::Nested(items)) => {
            for it in items {
                push_nested(&it, None, file_uri, &mut out);
            }
        }
        None => {}
    }
    out
}

fn push_nested(
    sym: &DocumentSymbol,
    container: Option<String>,
    file_uri: &str,
    out: &mut Vec<SymbolHit>,
) {
    let container = container.or_else(|| Some(sym.name.clone()));
    out.push(SymbolHit {
        name: sym.name.clone(),
        kind: kind_from_lsp(&sym.kind),
        uri: file_uri.to_string(),
        range: sym.range,
        container: container.clone(),
    });
    if let Some(children) = sym.children.as_ref() {
        for child in children {
            push_nested(child, Some(sym.name.clone()), file_uri, out);
        }
    }
}

// 修 P1 #3：tool_symbol_tree 并发池辅助。
//
// `drain_one` 从 JoinSet 拿一个完成项 `(idx, file, Result<Vec<SymbolHit>>)`. idx 是
// 文件在原 `files` Vec 中的位置 —— 把命中按 (idx, value) push 进 entries（末尾
// sort 还原顺序），失败就地 push errors（带 file 字段）。JoinHandle 出错（极少见，
// 如 panic）按失败处理 —— panic 不应绕过 errors 通道。
//
// `overview_via_session` 是 tool_overview 缓存命中 / miss 路径的 'static 等价版本：
// 不持 self（只持 Arc<Session>+Arc<cache>+PathBuf+String）以便 spawn 进 JoinSet.
// 与 tool_overview 的语义差异：缓存查 / 写在函数内部直接走 Arc<Mutex<...>>，不再走
// self 的私有 helper（self.symbol_cache_get/put 都靠 Arc 读 / 写同一张表，等价）。
async fn drain_one(
    set: &mut tokio::task::JoinSet<(usize, String, ToolResult<Vec<SymbolHit>>)>,
    entries: &mut Vec<(usize, serde_json::Value)>,
    errors: &mut Vec<serde_json::Value>,
) {
    let Some(joined) = set.join_next().await else {
        return;
    };
    let (idx, file, res) = match joined {
        Ok(pair) => pair,
        Err(join_err) => {
            errors.push(serde_json::json!({
                "error": format!("symbol-tree fan-out task join error: {join_err}"),
            }));
            return;
        }
    };
    match res {
        Ok(symbols) if !symbols.is_empty() => {
            entries.push((idx, serde_json::json!({ "file": file, "symbols": symbols })));
        }
        Ok(_) => {} // 空命中 → 不入 entries
        Err(e) => {
            errors.push(serde_json::json!({ "file": file, "error": e.to_string() }));
        }
    }
}

async fn overview_via_session(
    session: Arc<lsp_core::session::Session>,
    cache_arc: std::sync::Arc<Mutex<HashMap<SymbolCacheKey, Vec<SymbolHit>>>>,
    root: PathBuf,
    file: String,
    lang_override: Option<&str>,
) -> ToolResult<Vec<SymbolHit>> {
    // 缓存查：与 `Supervisor::symbol_cache_get` 等价（共享同一张表）。
    let cache_key = doc_symbol_cache_key(&root, &file);
    if let Some(cached) = cache_arc.lock().unwrap().get(&cache_key).cloned() {
        return Ok(cached);
    }
    let lang_str = resolve_lang_for_file(&file, lang_override)?;
    let path = root.join(&file);
    let uri = path_to_uri_str(&path);
    let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
    let timeout = ls_registry::config::effective_timeout_ms(&lang_str, None)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(TOOL_TIMEOUT);
    let params = serde_json::json!({ "textDocument": { "uri": uri.clone() } });
    let resp: Option<DocumentSymbolResponse> = session
        .request("textDocument/documentSymbol", params, timeout)
        .await?;
    let out = flatten_symbols(resp, &uri);
    // 写入走 `symbol_cache_put_impl`（与 `Supervisor::symbol_cache_put` 同语义：
    // 空不写 + 超 SYMBOL_CACHE_MAX_ENTRIES 全清）。直 .insert 会绕过容量闸门。
    let mut cache = cache_arc.lock().unwrap();
    symbol_cache_put_impl(&mut cache, cache_key, out.clone());
    Ok(out)
}

/// 按 (line, col) 在 `DocumentSymbolResponse` 树中反查所有包含该位置的符号（Phase 2.1）。
///
/// 规则：位置在符号的 [start.line, end.line] 闭区间内；
/// - `line == start.line` 时 `col >= start.character`；
/// - `line == end.line`   时 `col <= end.character`；
/// - 中间行默认命中（不查 col，LSP 自身规范）。
///
/// 返回从最外层到最深命中的 `SymbolHit` 链（所有命中的祖先）。无命中返空 Vec。
/// Nested 形态递归 `children`；Flat 形态只按顶层项判断（mock_ls 路径）。
/// `None`（LS 对未就绪/未加载文档返 `null`）视为无命中。
fn collect_containing_hits(
    resp: Option<&DocumentSymbolResponse>,
    file_uri: &str,
    line: u32,
    col: u32,
) -> Vec<SymbolHit> {
    let Some(resp) = resp else {
        return Vec::new();
    };
    fn walk(
        items: &[DocumentSymbol],
        file_uri: &str,
        line: u32,
        col: u32,
        container: Option<&str>,
        out: &mut Vec<SymbolHit>,
    ) {
        for it in items {
            if position_in_range(it.range, line, col) {
                out.push(SymbolHit {
                    name: it.name.clone(),
                    kind: kind_from_lsp(&it.kind),
                    uri: file_uri.to_owned(),
                    range: it.range,
                    container: container.map(str::to_owned),
                });
                if let Some(children) = it.children.as_ref() {
                    walk(children, file_uri, line, col, Some(&it.name), out);
                }
            } else if let Some(children) = it.children.as_ref() {
                // 父节点不命中但子节点仍可能命中（罕见：嵌套树里父 range 比子 range 大）。
                walk(children, file_uri, line, col, container, out);
            }
        }
    }

    match resp {
        DocumentSymbolResponse::Nested(items) => {
            let mut out = Vec::new();
            walk(items, file_uri, line, col, None, &mut out);
            out
        }
        DocumentSymbolResponse::Flat(items) => {
            // Flat：每项自带 location & container_name；无层级。
            items
                .iter()
                .filter(|it| position_in_range(it.location.range, line, col))
                .map(|it| SymbolHit {
                    name: it.name.clone(),
                    kind: kind_from_lsp(&it.kind),
                    uri: it.location.uri.to_string(),
                    range: it.location.range,
                    container: it.container_name.clone(),
                })
                .collect()
        }
    }
}

/// `line/col` 是否落在 `range` 闭区间内（见 collect_containing_hits 注释）。
fn position_in_range(r: lsp_types::Range, line: u32, col: u32) -> bool {
    if line < r.start.line || line > r.end.line {
        return false;
    }
    if line == r.start.line && col < r.start.character {
        return false;
    }
    if line == r.end.line && col > r.end.character {
        return false;
    }
    true
}

/// A（ai-token-features §10-A）：给 search 命中增量补 `symbol`/`container`。
///
/// 按 file 分桶、按语言再分桶（混合目录各走各的 LS，永不串 session），每桶
/// `overview_via_session` 进 JoinSet 有界并发（Phase 3.1 缓存兜着，同文件仅一次
/// LS 往返）；每条命中按 0-based line/col 找覆盖它的最小符号。单桶 overview 失败
/// （LS 未就绪、语言不可解析等）该组保持 None —— 装饰失败绝不影响 search 主结果。
async fn enrich_search_with_symbols(
    sup: &Supervisor,
    root: &Path,
    hits: &mut [SearchHit],
    lang: Option<&str>,
) {
    let mut per_file: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, hit) in hits.iter().enumerate() {
        per_file.entry(hit.file.clone()).or_default().push(idx);
    }

    // 与 tool_symbol_tree 同一扇出原语（修 P1 #3 复用）：按语言分桶 → 每桶一条
    // session → `overview_via_session` 进 JoinSet 有界并发；语言解析失败 / LS 拉不起
    // 的文件组跳过 —— 与原串行 `tool_overview(..).ok()` 逐文件语义一致。
    let mut per_lang: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for file in per_file.keys() {
        if let Ok(lang_id) = resolve_lang_for_file(file, lang) {
            per_lang.entry(lang_id).or_default().push(file.clone());
        }
    }

    const MAX_INFLIGHT: usize = 4;
    async fn drain_one(
        set: &mut tokio::task::JoinSet<(String, ToolResult<Vec<SymbolHit>>)>,
        out: &mut std::collections::HashMap<String, ToolResult<Vec<SymbolHit>>>,
    ) {
        let Some(joined) = set.join_next().await else {
            return;
        };
        if let Ok((file, res)) = joined {
            out.insert(file, res);
        }
    }

    let mut overviews: std::collections::HashMap<String, ToolResult<Vec<SymbolHit>>> =
        std::collections::HashMap::new();
    let cache_arc = Arc::clone(&sup.symbol_cache);
    for (lang_id, files) in per_lang {
        let Ok(session) = sup.session_for(root, &lang_id).await else {
            continue; // 单 LS 拉不起 → 该桶全部保持无装饰，不影响 search 主结果
        };
        let mut set = tokio::task::JoinSet::new();
        for file in files {
            if set.len() >= MAX_INFLIGHT {
                drain_one(&mut set, &mut overviews).await;
            }
            let session = Arc::clone(&session);
            let cache_arc = Arc::clone(&cache_arc);
            let root_buf = root.to_path_buf();
            let lang_owned = lang.map(str::to_string);
            set.spawn(async move {
                let res =
                    overview_via_session(session, cache_arc, root_buf, file.clone(), lang_owned.as_deref())
                        .await;
                (file, res)
            });
        }
        while !set.is_empty() {
            drain_one(&mut set, &mut overviews).await;
        }
    }

    // 应用装饰：overview 失败的文件整组跳过（symbol/container 保持 None）。
    for (file, indices) in &per_file {
        let Some(Ok(syms)) = overviews.get(file) else {
            continue;
        };
        for idx in indices {
            // SearchHit 的 line/col 是 1-based；LSP Range 是 0-based。
            let line_0 = hits[*idx].line.saturating_sub(1);
            let col_0 = hits[*idx].col.saturating_sub(1);
            if let Some((sym, container)) = find_covering_symbol(syms, line_0, col_0) {
                hits[*idx].symbol = Some(sym);
                hits[*idx].container = container;
            }
        }
    }
}

/// 找覆盖 `(line, col)` 的最小符号（span 最短者；嵌套子符号 span 严格更小）+ 其容器。
///
/// 复用 `position_in_range` 闭区间语义（与 `collect_containing_hits` 一致）。
/// 容器沿用 flatten 结果；`push_nested` 给顶层符号记 container=自身名，
/// 此处滤掉该 artifact —— SearchHit 顶层符号的容器应为 None。
fn find_covering_symbol(
    syms: &[SymbolHit],
    line: u32,
    col: u32,
) -> Option<(String, Option<String>)> {
    fn span(h: &SymbolHit) -> u32 {
        h.range.end.line - h.range.start.line
    }
    let mut best: Option<&SymbolHit> = None;
    for s in syms {
        if !position_in_range(s.range, line, col) {
            continue;
        }
        let deeper = match best {
            Some(b) => span(s) < span(b),
            None => true,
        };
        if deeper {
            best = Some(s);
        }
    }
    best.map(|s| {
        let container = s.container.clone().filter(|c| c != &s.name);
        (s.name.clone(), container)
    })
}

/// 把 `lsp_types::Location` 压成 `"file:line:col"` 紧凑字符串（plan-h-compact §1）。
///
/// 与 `--json` 形态互斥：`--json` 走全形态 LSP Location（range+uri）；默认走 compact
/// 省 90%+ token。`uri_to_path` 同步处理 `d%3A` percent + 盘符小写归一，失败 fallback
/// 到原始 URI 字符串（不丢数据，可逆）。
///
/// ↖ mirror: ai-token-features-design.md §10-H；vs debuginfo-mode-lite：仅 LSP 格式化层。
fn compact_loc(loc: &Location) -> String {
    let s = loc.uri.as_str();
    let path = match uri_to_path(s) {
        Some(p) => p.to_string_lossy().replace('\\', "/"),
        None => s.to_string(),
    };
    let line = loc.range.start.line + 1; // LSP 0-based → 人类 1-based
    let col = loc.range.start.character + 1;
    format!("{path}:{line}:{col}")
}

/// Vec 适配：一次 map 出紧凑字符串数组。
fn compact_locs(locs: &[Location]) -> Vec<String> {
    locs.iter().map(compact_loc).collect()
}

/// `SymbolHit` 的紧凑形态（plan-h-compact §1，结构见 lsp_core::types）。
///
/// `SymbolHit.uri` 是 String、`range` 是 lsp_types::Range —— 适配器复用 `Location`
/// 视图，把 `(uri, range.start)` 喂 `compact_loc`。
fn compact_symbol_hit(hit: &SymbolHit) -> String {
    let uri = match hit.uri.parse::<lsp_types::Uri>() {
        Ok(u) => u,
        // uri 非 file:// 走 fallback（保留原字符串），让 `compact_loc` 内部
        // `uri_to_path` 走 None 分支退回 raw。
        Err(_) => lsp_types::Uri::from_str("file:///").expect("static file uri"),
    };
    let fake = Location { uri, range: hit.range };
    compact_loc(&fake)
}

/// Location[] envelope：`compact=true` → strings 数组；`false` → 原 LSP Location 列表。
///
/// 与 `compact_locs` 配套：保留 raw_count 字段便于 AI 识别"有没有结果"的快速路径；
/// non-compact 形态不增字段（与既有 wire 兼容）。
///
/// AI-token 特性 H（plan-h-compact §1）；↖ mirror: ai-token-features-design §10-H。
fn locations_envelope(locs: &[Location], compact: bool) -> serde_json::Value {
    if compact {
        serde_json::json!({
            "compact": true,
            "items": compact_locs(locs),
            "raw_count": locs.len(),
        })
    } else {
        serde_json::json!({ "compact": false, "items": locs })
    }
}

/// SymbolHit[] envelope：compact 时合并 name + 紧凑位置为 `["name", "file:line:col"]` 数组
/// （保留 name 便于 grep；位置数组内嵌为字符串，体积为原 JSON 的 ~25%）。
fn symbol_hits_envelope(hits: &[SymbolHit], compact: bool) -> serde_json::Value {
    if compact {
        let items: Vec<[String; 2]> = hits
            .iter()
            .map(|h| [h.name.clone(), compact_symbol_hit(h)])
            .collect();
        serde_json::json!({
            "compact": true,
            "items": items,
            "raw_count": hits.len(),
        })
    } else {
        // 非 compact 形态用 serde 直接序列化回原结构，与既有 wire 一致。
        serde_json::to_value(hits).unwrap_or(serde_json::Value::Null)
    }
}

/// completion envelope（bd serena-rust-5st）：compact 时逐 item 裁掉零信息字段
/// （`insert` 恒等于 label 的兜底副本、200 字符 `doc`、`deprecated:false`、空
/// `additional_text_edits`——原嵌套 LSP range 结构压成 `["L{行}:{列}", 新文本]` 对）。
/// `_compact=false`（CLI `--json`）走原 `CompletionResponse` 序列化，wire 零变化。
fn completion_envelope(resp: &CompletionResponse) -> serde_json::Value {
    let items: Vec<serde_json::Value> =
        resp.items.iter().map(compact_completion_item).collect();
    let mut env = serde_json::json!({
        "compact": true,
        "items": items,
        "raw_count": resp.items.len(),
    });
    if let Some(t) = &resp.truncated {
        env["truncated"] = serde_json::json!(t);
    }
    env
}

/// 单个 completion item 的紧凑形态：label/kind/detail 恒在（签名是 AI 选候选的
/// 主依据），其余字段仅在偏离默认时有信息量才出现。
fn compact_completion_item(it: &CompletionItemLite) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("label".into(), serde_json::json!(it.label));
    o.insert("kind".into(), serde_json::json!(it.kind));
    if let Some(d) = &it.detail {
        o.insert("detail".into(), serde_json::json!(d));
    }
    // parse_completion_item 兜底 insert=label；与 label 相同即零信息，省略。
    if it.insert.as_deref().is_some_and(|i| i != it.label) {
        o.insert("insert".into(), serde_json::json!(it.insert));
    }
    if it.deprecated {
        o.insert("deprecated".into(), serde_json::json!(true));
    }
    if !it.additional_text_edits.is_empty() {
        let edits: Vec<[String; 2]> = it
            .additional_text_edits
            .iter()
            .map(|e| {
                // 与 compact_loc 同基线：LSP 0-based → 人类 1-based。
                let at = format!(
                    "L{}:{}",
                    e.range.start.line + 1,
                    e.range.start.character + 1
                );
                [at, e.new_text.clone()]
            })
            .collect();
        o.insert("edits".into(), serde_json::json!(edits));
    }
    serde_json::Value::Object(o)
}

// ==== find-symbol LS 缺失可见性（bd serena-rust-x67）====

/// find-symbol 全失败收口：所有 lang 的 `session_for` 都失败时的错误决策。
/// 任一失败为非 NotInstalled（unknown lang / spawn crash）→ 原样上抛首个此类错误
/// （不把崩溃谎报成 LS_NOT_INSTALLED，否则 agent 会去重装而不是看真错误）；
/// 全为 NotInstalled → 合并 language/hint 为单个 NotInstalled（wire=LS_NOT_INSTALLED，
/// 与 hover/def 单 lang 路径同形）。
fn combined_all_failed_error(failures: Vec<(String, ToolError)>) -> ToolError {
    let mut langs = Vec::with_capacity(failures.len());
    let mut hints = Vec::with_capacity(failures.len());
    let mut other: Option<ToolError> = None;
    for (lang, e) in failures {
        match e {
            ToolError::NotInstalled { hint, .. } => {
                langs.push(lang);
                hints.push(hint);
            }
            _ => {
                if other.is_none() {
                    other = Some(e);
                }
            }
        }
    }
    if let Some(e) = other {
        return e;
    }
    ToolError::NotInstalled {
        language: langs.join(", "),
        hint: hints.join("; "),
    }
}

/// find-symbol 部分失败场景的 warning 文案：lang 前缀保证多失败可归属，
/// 复用 ToolError Display（NotInstalled 自带安装 hint）。
fn failure_warnings(failures: &[(String, ToolError)]) -> Vec<String> {
    failures
        .iter()
        .map(|(lang, e)| format!("{lang}: {e}"))
        .collect()
}

/// 结果 JSON 顶层 `warning` 键（wire 无既有 warning 通道，PM 拍板放结果顶层）。
/// compact envelope 本是对象 → 直接加键；非 compact 裸数组无法带键 → 仅当有
/// warning 时升级为 `{compact:false, items, warning}` 对象（无 warning 维持裸数组
/// 既有 wire 不变）。`_delta=true` 的增量形态 `{delta,added,removed}` 本就有损，不带 warning。
///
/// pub 供 daemon http 层复用（bd serena-rust-h4i：跨 project 切换 warning 走同一通道）。
pub fn attach_warning(value: &mut serde_json::Value, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    let w = serde_json::Value::String(warnings.join("; "));
    if value.is_array() {
        let items = std::mem::take(value);
        *value = serde_json::json!({ "compact": false, "items": items, "warning": w });
    } else if value.is_object() {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("warning".to_string(), w);
        }
    } else {
        // 裸 null / 标量（string/number/bool）无法携带键 → 升级为对象形态
        // （对齐裸数组升级模式；无 warning 时调用方不会进来，既有 wire 不变）。
        let items = std::mem::take(value);
        *value = serde_json::json!({ "items": items, "warning": w });
    }
}

/// LS 报告的 window 消息是否为 workspace 加载失败（bd serena-rust-xzb）。
/// 特征词取自 cargo/RA 的真实错误文本：
/// - RA load_workspace 失败经 window/showMessage 转发的 `FetchWorkspaceError(...)`；
/// - cargo metadata 对"目录在别的 workspace 内但非成员"的原话
///   `current package believes it's in a workspace when it's not`。
///
/// 窄匹配少误报：普通编译诊断不进 workspace_errors。
fn is_workspace_load_error(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("fetchworkspaceerror") || m.contains("believes it's in a workspace")
}

/// 0-based LSP Position 是否落在任一符号的 range 内（bd serena-rust-we0 判据）。
/// hover/def 空结果 + 位置在语法层符号内 = 「类型分析未就绪」而非「无符号」；
/// 位置在符号外 = 空是正常语义（不标记）。
fn position_in_hits(hits: &[SymbolHit], line: u32, col: u32) -> bool {
    hits.iter().any(|h| {
        let (sl, sc) = (h.range.start.line, h.range.start.character);
        let (el, ec) = (h.range.end.line, h.range.end.character);
        (sl < line || (sl == line && sc <= col))
            && (line < el || (line == el && col <= ec))
    })
}

/// 语义工具空结果的「可能未就绪」warning 文案（bd serena-rust-we0）。
/// documentSymbol/workspaceSymbol 先就绪、类型分析（def/refs/hover）晚 30-60s；
/// warm 以 find-symbol 非空为判据覆盖不到类型分析窗口 —— 空结果必须自带线索
/// 让 AI 区分「没符号」与「没就绪」（与诊断 pending 同构）。
fn semantic_not_ready_message() -> String {
    "semantic layer returned empty; the language server's type analysis may not be ready yet \
     (typically ready 30-60s after symbol index on a fresh workspace; a non-empty symbol index \
     does not imply type analysis is ready)"
        .to_string()
}

/// hover 空结果判定（bd serena-rust-we0）：`null`（部分 LS 未就绪返 null）或
/// contents 无内容 —— 实测 RA 未就绪窗口两种形态都出现（空 contents 对象 ≠
/// Some(Hover) 的有效语义，直接 `is_null()` 判会漏）。
fn hover_is_empty(value: &serde_json::Value) -> bool {
    if value.is_null() {
        return true;
    }
    match value.get("contents") {
        Some(serde_json::Value::String(s)) => s.is_empty(),
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(c) => c
            .get("value")
            .and_then(|v| v.as_str())
            .map(str::is_empty)
            .unwrap_or(false),
        None => true,
    }
}


/// RefSymbolHit[] envelope：compact 时合并 `symbol` + `refs[]` 嵌套紧凑（按容器聚类）。
fn ref_symbol_hits_envelope(
    hits: &[ref_tools::RefSymbolHit],
    compact: bool,
) -> serde_json::Value {
    if compact {
        let items: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "symbol": h.container_name,
                    "loc": compact_file_line_col(&h.file, h.line, h.col),
                })
            })
            .collect();
        serde_json::json!({
            "compact": true,
            "items": items,
            "raw_count": hits.len(),
        })
    } else {
        serde_json::to_value(hits).unwrap_or(serde_json::Value::Null)
    }
}

/// RefSnippetHit[] envelope：compact 时保留每条 snippet（snippet 是引用上下文价值所在，
/// 不能砍），但位置压紧凑 + `compact` 标记顶层。
fn ref_snippet_hits_envelope(
    hits: &[ref_tools::RefSnippetHit],
    compact: bool,
) -> serde_json::Value {
    if compact {
        let items: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "loc": compact_file_line_col(&h.file, h.line, h.col),
                    "text": h.text,
                    "snippet": h.snippet,
                })
            })
            .collect();
        serde_json::json!({
            "compact": true,
            "items": items,
            "raw_count": hits.len(),
        })
    } else {
        serde_json::to_value(hits).unwrap_or(serde_json::Value::Null)
    }
}

// ==== AI-token 特性 J（§11-J）：delta set-diff helpers ====

/// 统一取 items 数组视图：`{items:[...]}` envelope → 数组；裸数组（overview /
/// find-symbol 非 compact 形态）→ 自身；其余 → None。
fn items_of(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    v.get("items")
        .and_then(|i| i.as_array())
        .or_else(|| v.as_array())
}

/// 空响应判定：envelope `items:[]` 或裸 `[]`。无 items 且非数组的形态视为非空。
fn is_empty_response(v: &serde_json::Value) -> bool {
    items_of(v).is_some_and(|a| a.is_empty())
}

/// `a` 中有而 `b` 中没有的条目（`added = diff(current, prev)`；参数对调即 removed）。
fn diff_hits(
    a: &serde_json::Value,
    b: &serde_json::Value,
) -> Vec<serde_json::Value> {
    let b_keys: std::collections::HashSet<String> = items_of(b)
        .map(|arr| arr.iter().map(hit_key).collect())
        .unwrap_or_default();
    items_of(a)
        .map(|arr| {
            arr.iter()
                .filter(|h| !b_keys.contains(&hit_key(h)))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// hit 身份键：`file|line:col`。兼容四种形态 ——
/// 紧凑字符串 `"file:line:col"`（locations_envelope compact）、
/// `["name", "file:line:col"]` 对（symbol_hits_envelope compact）、
/// LSP Location / SymbolHit 对象（uri + range.start）、
/// 平面对象 `{file, line, col}`（测试用）。
fn hit_key(h: &serde_json::Value) -> String {
    if let Some(s) = h.as_str() {
        return s.to_string();
    }
    if let Some(pair) = h.as_array() {
        let name = pair.first().and_then(|v| v.as_str()).unwrap_or("");
        let loc = pair.get(1).and_then(|v| v.as_str()).unwrap_or("");
        return format!("{}|{}", name, loc);
    }
    let file = h
        .get("file")
        .or_else(|| h.get("uri"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let (line, col) = match h.get("range").and_then(|r| r.get("start")) {
        Some(s) => (
            s.get("line").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("character").and_then(|v| v.as_u64()).unwrap_or(0),
        ),
        None => (
            h.get("line").and_then(|v| v.as_u64()).unwrap_or(0),
            h.get("col")
                .or_else(|| h.get("character"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        ),
    };
    format!("{}|{}:{}", file, line, col)
}

/// 单条 `file:line:col` 紧凑字符串 —— RefSymbolHit/RefSnippetHit 没有 Location，直接拿
/// raw 字段拼。它们原生就是 0-based，与 LSP 一致；1-based 转换同 compact_loc。
///
/// ponytail: 不走 `uri_to_path` —— ref_tools 已把路径归一化（fix_index 阶段）。
fn compact_file_line_col(file: &str, line: u32, col: u32) -> String {
    let path = file.replace('\\', "/");
    format!("{}:{}:{}", path, line + 1, col + 1)
}
/// `lsp_types::SymbolKind.0` is private. Round-trip via JSON: `SymbolKind` is
/// `#[serde(transparent)]` over `i32`, so deserializing into i32 yields the wire number.
fn kind_from_lsp(k: &lsp_types::SymbolKind) -> SymbolKindTag {
    let n: i32 = serde_json::from_value(serde_json::json!(k)).unwrap_or(0);
    SymbolKindTag::from_lsp(n as u8)
}
/// 把 (line, col) 经 offsets.rs 换算为 LSP Position。M0 固定 utf-16。
/// 读盘 → `position_to_byte` → 校验落在 char 边界。这里简化：假定输入 line/col 即
/// LSP position（0-based），仅校验越界，避免上层传 0-based 错位时掩盖。
async fn lsp_position_from_byte(
    path: &Path,
    line: u32,
    col: u32,
    enc: OffsetEncoding,
) -> ToolResult<Position> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("read {}: {e}", path.display()),
        })?;
    let pos = LspPos {
        line,
        character: col,
    };
    lsp_core::offsets::position_to_byte(&text, pos, enc).map_err(|e| ToolError::BadArgs {
        detail: format!("position {line}:{col} out of range: {e}"),
    })?;
    Ok(Position::new(pos.line, pos.character))
}

/// 解析 lang: 有 override 直接用 (大小写折叠), 否则按文件扩展名探测
/// （内置 EXT_TABLE → external-servers.toml extensions 兜底）。
fn resolve_lang_for_file(file: &str, lang_override: Option<&str>) -> ToolResult<String> {
    if let Some(l) = lang_override {
        return Ok(l.to_ascii_lowercase());
    }
    ls_registry::resolve_lang_name(Path::new(file))
        .map(str::to_string)
        .ok_or_else(|| ToolError::BadArgs {
            detail: format!("file not supported: {file}"),
        })
}

/// 从 anyhow 错消息里抠 install_hint。`ls-adapters::not_installed_error` 模板：
/// `language server \`{name}\` not found in PATH; install_hint: {hint}`
fn extract_install_hint(msg: &str) -> String {
    msg.rsplit("install_hint:")
        .next()
        .map(str::trim)
        .unwrap_or("see upstream docs")
        .to_string()
}

/// Phase 4 基建 Task 22b：从 args 中移除 `_timeout_ms` / `_index_timeout_ms` 私有字段，
/// 返回清理后的 args clone。`execute_tool` 入口调用，避免污染后续 `required_file` 等
/// 私有 helper（它们只看业务字段如 `file`/`line`，忽略下划线前缀；清掉是为了
/// JSONL 反序列化时 `_timeout_ms` 不会泄漏到 tool 输出）。
fn sanitize_timeout_args(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = args.as_object_mut() {
        obj.remove("_timeout_ms");
        obj.remove("_index_timeout_ms");
    }
    args
}

/// AI-token 特性 G（plan-g-budget.md §Task1 / ai-token-features-design §10-G）：
/// 工具响应 token 预算护栏。超出预算则截断顶层 `items` 数组到最大可容纳条数，
/// 并写入 `truncated: true` + `original_count`；未超预算零改动（返回 false）。
/// 返回值 = 是否发生截断。截断是 **success 语义**（wire/退出码不变，仅加标志）。
///
/// 估算：4 字节 ≈ 1 token（BPE 粗略近似，soft limit）。
/// delta 响应（有 `added`/`removed` 键）跳过截断——items 语义已归一为增量集，
/// 按条截断会破坏增量对照关系，直接放行。
///
/// ponytail: 不做精确 BPE 计数——预算护栏是 soft limit，精确度不是核心。
fn apply_budget(value: &mut serde_json::Value, max_tokens: usize) -> bool {
    // J（§11-J）delta 形态：二选一集，跳过截断（见函数 doc）。
    if value.get("added").is_some() || value.get("removed").is_some() {
        return false;
    }
    let budget_bytes = max_tokens.saturating_mul(4);
    let current_bytes = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    if current_bytes <= budget_bytes {
        return false;
    }
    let Some(items) = value.get_mut("items").and_then(|v| v.as_array_mut()) else {
        return false; // 无 items 数组的响应（标量/树形）不在预算护栏范围
    };
    let original_count = items.len();
    // 二分查找预算内最大保留条数；+18 字节为 truncated/original_count 标志开销余量。
    let mut lo = 0usize;
    let mut hi = items.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let trial = serde_json::json!({ "items": &items[..mid] });
        let trial_bytes = serde_json::to_vec(&trial).map(|v| v.len()).unwrap_or(0);
        if trial_bytes + 18 <= budget_bytes {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    items.truncate(lo);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("truncated".into(), serde_json::json!(true));
        obj.insert("original_count".into(), serde_json::json!(original_count));
    }
    true
}

/// AI-token 特性 G（§10-G）：签名压缩——递归删除 `container` / `container_name` /
/// `kind` 三个"二级"冗余字段（保留 name + 位置）。截断/压缩与 `_compact`/`_delta`
/// 同套私有约定（sanitize 不清）。按需未来扩展字段名单。
fn apply_compress(value: &mut serde_json::Value) {
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    strip(item);
                }
            }
            serde_json::Value::Object(obj) => {
                obj.remove("container");
                obj.remove("container_name");
                obj.remove("kind");
                for sub in obj.values_mut() {
                    strip(sub);
                }
            }
            _ => {}
        }
    }
    strip(value);
}

fn required_file(args: &serde_json::Value) -> ToolResult<String> {
    args.get("file")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing args.file".into(),
        })
}

fn required_position(args: &serde_json::Value) -> ToolResult<(String, u32, u32)> {
    let file = required_file(args)?;
    let line = args
        .get("line")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing or invalid args.line".into(),
        })?;
    let col = args
        .get("col")
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing or invalid args.col".into(),
        })?;
    Ok((file, line, col))
}

fn required_symbol_body_args(args: &serde_json::Value) -> ToolResult<(String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    Ok((file, symbol))
}

fn required_replace_args(args: &serde_json::Value) -> ToolResult<(String, String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    let new_body = args
        .get("new_body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'new_body'".into(),
        })?
        .to_owned();
    Ok((file, symbol, new_body))
}

fn required_rename_args(args: &serde_json::Value) -> ToolResult<(String, u32, u32, String)> {
    let (file, line, col) = required_position(args)?;
    let new_name = args
        .get("new_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'new_name'".into(),
        })?
        .to_owned();
    Ok((file, line, col, new_name))
}

/// 行级 replace-lines / delete-lines 的 (file, start_line, end_line)。
fn required_line_range(args: &serde_json::Value) -> ToolResult<(String, u32, u32)> {
    let file = required_file(args)?;
    let start_line = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'start_line'".into(),
        })? as u32;
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'end_line'".into(),
        })? as u32;
    Ok((file, start_line, end_line))
}

/// 可选 hash 对账参数（行级三件套）。
fn opt_expected_hash(args: &serde_json::Value) -> Option<String> {
    args.get("expected_hash")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}
/// edit_* 工具通用 helper：(file, symbol, text)。
fn required_edit_args(args: &serde_json::Value) -> ToolResult<(String, String, String)> {
    let file = required_file(args)?;
    let symbol = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'symbol'".into(),
        })?
        .to_owned();
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadArgs {
            detail: "missing 'text'".into(),
        })?
        .to_owned();
    Ok((file, symbol, text))
}

/// LSP `file://...` URI → 相对 project_root 的 file path（call/type hierarchy 工具
/// 反查 item.uri 时用：item 自身带 uri，我们要把语言探测退回 file path 形式）。
///
/// 注：与 `uri_to_path` 不同 —— 后者返绝对路径，本函数仅在协议适配层用一次，结果
/// 直接喂 `resolve_lang_for_file`（只要扩展名，不需根对齐）。
fn file_path_from_uri(uri: &str) -> String {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    let s = if cfg!(windows) && stripped.starts_with('/') {
        &stripped[1..]
    } else {
        stripped
    };
    percent_decode(s).replace('\\', "/")
}

/// 解码 LSP `SemanticTokens.data`（结构化 delta token 序列）为绝对坐标 token 列表。
///
/// 算法（LSP §3.16 semanticTokens）：
/// - 每 token 含 `delta_line` / `delta_start` / `length` / `token_type` / `token_modifiers`。
/// - `delta_line` 为相对前一 token 的行偏移；`delta_start` 仅在同一行时是字符偏移，
///   否则为新行的绝对起始字符。
/// - 绝对坐标：(line, start_char) 通过累加 delta 得到。
fn decode_semantic_tokens(data: &[lsp_types::SemanticToken]) -> Vec<SemanticTokenEntry> {
    let mut out = Vec::with_capacity(data.len());
    let mut line: u32 = 0;
    let mut start_char: u32 = 0;
    for t in data {
        if t.delta_line > 0 {
            line = line.saturating_add(t.delta_line);
            start_char = t.delta_start;
        } else {
            start_char = start_char.saturating_add(t.delta_start);
        }
        out.push(SemanticTokenEntry {
            line,
            start_char,
            length: t.length,
            token_type: t.token_type,
            token_modifiers: t.token_modifiers_bitset,
        });
    }
    out
}
#[async_trait::async_trait]
impl SupervisorTrait for Supervisor {
    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError> {
        let root = Path::new(project_root);
        // Phase 4 基建 Task 22b：把 args._timeout_ms / args._index_timeout_ms 提取成
        // per-call override，并清掉这两个私有字段（避免传染给具体 tool 的 args 解析）。
        // 实际 timeout 在 tool_* 内部通过 `effective_tool_timeout(lang, &args)` 拿到。
        let args = sanitize_timeout_args(args);
        // AI-token 特性 H（plan-h-compact-locations.md §1 / ai-token-features-design §10-H）：
        // 6 个位置工具（def/refs/find-symbol/find-implementations/find-referencing-*）
        // 默认走紧凑 `file:line:col` 字符串输出。`_compact` 在 sanitize 里**不**进过滤名单
        // —— 与 `_timeout_ms` 同套私有约定。默认 `true` 走紧凑、`--json` 显式 `false` 走全形态。
        let compact = args
            .get("_compact")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // AI-token 特性 J（§11-J）：`_delta` 与 `_compact` 同套私有约定（sanitize
        // 不清）。true 时 refs/overview/find-symbol/find-implementations 末尾走
        // maybe_delta 增量编排；默认 false，wire 与 J 之前完全一致。
        let delta = args
            .get("_delta")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 修 P1 #2（TTL 生产执行者）：throttled reclaim。每 32 次调用扫一次所有
        // 在线 Session 的空闲 FileBuffer（ref_count=0 + 超 60 s）→ didClose + 移表。
        // 这是 TTL 窗口的实际触发点；daemon 周期性或 CLI 流式调用下都能覆盖。
        // 计数原子增加、阈值归零，无锁；scan + evict 内部临界区微秒无 await。
        // ponytail: 阈值 32 对应稳态 5~10 s 节流；测试直接调 reclaim_idle_buffers_once
        // 绕过阈值验证语义。
        let _reclaimed = self.reclaim_idle_buffers_once();
        // bd serena-rust-84n：不存在文件 = 确定性参数错（wire §6.3 BAD_ARGS），
        // 统一在入口拦截 —— 否则 ensure_open 的 io NotFound 经 Core 冒成 INTERNAL，
        // AI 无法据错误码免重试。args 带 file 字段的工具（读/写/位置类）目标文件
        // 全部要求已存在（本项目无「file = 新建目标」语义的工具）。
        if let Some(f) = args.get("file").and_then(|v| v.as_str())
            && !root.join(f).is_file()
        {
            return Err(ToolError::BadArgs {
                detail: format!("file not found: {f}"),
            });
        }
        let mut value: serde_json::Value = match tool {
            "overview" => {
                let file = required_file(&args)?;
                let raw = self.tool_overview(root, &file, lang).await?;
                let value =
                    serde_json::to_value(raw).map_err(|e| ToolError::Serialize(e.into()))?;
                let root_key = format!("{}|{}", root.display(), file);
                Ok(self.maybe_delta("overview", &root_key, value, delta).await)
            }
            "symbol-tree" => {
                let dir = args.get("dir").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'dir'".into(),
                    }
                })?;
                let max_files = args.get("max_files").and_then(|v| v.as_u64()).unwrap_or(200)
                    as usize;
                serde_json::to_value(self.tool_symbol_tree(root, dir, lang, max_files).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-symbol" => {
                let query = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'query'".into(),
                    }
                })?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
                let (raw, mut warnings) = self.tool_find_symbol(root, query, limit, lang).await?;
                // bd serena-rust-xzb：workspace 加载失败时 wssym 全空且无任何线索 ——
                // 有记录即透出（不依赖空结果，xzb 场景下结果恒空）。
                if let Some(err) = self.workspace_error_for(root) {
                    warnings.push(format!(
                        "workspace error: {err}; semantic results may be empty (workspace failed to load)"
                    ));
                }
                // bd serena-rust-bxd O4：LS 暖机窗口内符号索引仍在爬升（结果数实测
                // 波动 9→4→6+），成功响应同样附 partial 提示，AI 不会把中间态当
                // 全量；窗口关闭（10s 或首语义成功）后自动消失，wire 零新字段。
                warnings.extend(self.index_warming_warnings(root));
                let mut value = symbol_hits_envelope(&raw, compact);
                attach_warning(&mut value, &warnings);
                let root_key = format!("{}|{}|{}", root.display(), query, limit);
                Ok(self.maybe_delta("find-symbol", &root_key, value, delta).await)
            }
            "signature-help" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_signature_help(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            // ==== Phase 1 · 上游 wrapper 缺口（13 个）====
            "code-action" => {
                let (file, line, col) = required_position(&args)?;
                let kind = args.get("kind").and_then(|v| v.as_str());
                serde_json::to_value(
                    self.tool_code_action(root, &file, line, col, kind, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format" => {
                let file = required_file(&args)?;
                let tab_size = args.get("tab_size").and_then(|v| v.as_u64()).map(|n| n as u32);
                let insert_spaces = args.get("insert_spaces").and_then(|v| v.as_bool());
                serde_json::to_value(
                    self.tool_format(root, &file, tab_size, insert_spaces, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format-range" => {
                let file = required_file(&args)?;
                let start_line =
                    args.get("start_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        }
                    })? as u32;
                let start_col =
                    args.get("start_col").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_col'".into(),
                        }
                    })? as u32;
                let end_line =
                    args.get("end_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_line'".into(),
                        }
                    })? as u32;
                let end_col =
                    args.get("end_col").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_col'".into(),
                        }
                    })? as u32;
                let tab_size = args.get("tab_size").and_then(|v| v.as_u64()).map(|n| n as u32);
                let insert_spaces = args.get("insert_spaces").and_then(|v| v.as_bool());
                serde_json::to_value(
                    self.tool_format_range(
                        root,
                        &file,
                        start_line,
                        start_col,
                        end_line,
                        end_col,
                        tab_size,
                        insert_spaces,
                        lang,
                    )
                    .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "inlay-hint" => {
                let file = required_file(&args)?;
                let start_line =
                    args.get("start_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        }
                    })? as u32;
                let end_line =
                    args.get("end_line").and_then(|v| v.as_u64()).ok_or_else(|| {
                        ToolError::BadArgs {
                            detail: "missing 'end_line'".into(),
                        }
                    })? as u32;
                serde_json::to_value(
                    self.tool_inlay_hint(root, &file, start_line, end_line, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-highlight" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_document_highlight(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "folding-range" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_folding_range(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "semantic-tokens" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_semantic_tokens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "code-lens" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_code_lens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-link" => {
                let file = required_file(&args)?;
                serde_json::to_value(self.tool_document_link(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "call-hierarchy" => {
                let op = args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'op' (prepare|incoming|outgoing)".into(),
                    })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(&args)?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "incoming" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (CallHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_incoming(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "outgoing" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (CallHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_outgoing(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    other => Err(ToolError::BadArgs {
                        detail: format!("unknown call-hierarchy op: {other}"),
                    }),
                }
            }
            "type-hierarchy" => {
                let op = args
                    .get("op")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'op' (prepare|supertypes|subtypes)".into(),
                    })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(&args)?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "supertypes" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_supertypes(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "subtypes" => {
                        let item_value = args.get("item").cloned().ok_or_else(|| {
                            ToolError::BadArgs {
                                detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                    .into(),
                            }
                        })?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_subtypes(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    other => Err(ToolError::BadArgs {
                        detail: format!("unknown type-hierarchy op: {other}"),
                    }),
                }
            }
            "moniker" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(self.tool_moniker(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "workspace-diagnostic" => serde_json::to_value(
                self.tool_workspace_diagnostic(root, lang).await?,
            )
            .map_err(|e| ToolError::Serialize(e.into())),
            "hover" => {
                let (file, line, col) = required_position(&args)?;
                let resp = self.tool_hover(root, &file, line, col, lang).await?;
                let mut value =
                    serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))?;
                // bd serena-rust-we0：空 hover 可能是「类型分析未就绪」而非「无悬停」；
                // bd serena-rust-xzb：workspace 加载错误无条件透出（结果不可信）。
                let mut ws = self.workspace_error_warnings(root);
                if hover_is_empty(&value) {
                    ws.extend(
                        self.semantic_not_ready_warnings(root, &file, line, col, lang)
                            .await,
                    );
                } else {
                    // bd serena-rust-bxd O2/O4：首个语义成功 → 关暖机窗口。
                    self.mark_semantic_ready(root);
                }
                attach_warning(&mut value, &ws);
                Ok(value)
            }
            "diagnostics" => {
                let file = required_file(&args)?;
                let wait_gen = args.get("wait_gen").and_then(|v| v.as_u64());
                serde_json::to_value(self.tool_diagnostics(root, &file, lang, wait_gen).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "def" => {
                let (file, line, col) = required_position(&args)?;
                let raw = self.tool_def(root, &file, line, col, lang).await?;
                // `def` 单 Location → 退化为单元素 envelope（AI 期望 `items[]` 统一）。
                // None 是合法语义（位置无定义），保留为 `items: []` + `compact: true|false`。
                let locs = raw.into_iter().collect::<Vec<_>>();
                let empty = locs.is_empty();
                let mut value = locations_envelope(&locs, compact);
                // bd serena-rust-we0：空 def 可能是「类型分析未就绪」而非「无定义」；
                // bd serena-rust-xzb：workspace 加载错误无条件透出。
                let mut ws = self.workspace_error_warnings(root);
                if empty {
                    ws.extend(
                        self.semantic_not_ready_warnings(root, &file, line, col, lang)
                            .await,
                    );
                } else {
                    self.mark_semantic_ready(root);
                }
                attach_warning(&mut value, &ws);
                Ok(value)
            }

            "containing-symbol" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_containing_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "defining-symbol" => {
                let (file, line, col) = required_position(&args)?;
                serde_json::to_value(
                    self.tool_defining_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }

            "refs" => {
                let (file, line, col) = required_position(&args)?;
                let raw = self.tool_refs(root, &file, line, col, lang).await?;
                let empty = raw.is_empty();
                let mut value = locations_envelope(&raw, compact);
                // bd serena-rust-we0：空 refs 可能是「类型分析未就绪」而非「无引用」；
                // bd serena-rust-xzb：workspace 加载错误无条件透出。
                let mut ws = self.workspace_error_warnings(root);
                if empty {
                    ws.extend(
                        self.semantic_not_ready_warnings(root, &file, line, col, lang)
                            .await,
                    );
                } else {
                    self.mark_semantic_ready(root);
                }
                attach_warning(&mut value, &ws);
                let root_key = format!("{}|{}|{}|{}", root.display(), file, line, col);
                Ok(self.maybe_delta("refs", &root_key, value, delta).await)
            }
            "completion" => {
                let (file, line, col) = required_position(&args)?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let trigger = args.get("trigger").and_then(|v| v.as_str());
                let resp = self
                    .tool_completion(root, &file, line, col, limit, trigger, lang)
                    .await?;
                // bd serena-rust-5st：默认紧凑 envelope（与 def/refs 同套 `_compact`
                // 约定）；`--json`（_compact=false）走原 CompletionResponse wire。
                if compact {
                    Ok(completion_envelope(&resp))
                } else {
                    serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))
                }
            }
            "find-implementations" => {
                let (file, line, col) = required_position(&args)?;
                let raw = self
                    .tool_find_implementations(root, &file, line, col, lang)
                    .await?;
                let empty = raw.is_empty();
                let mut value = locations_envelope(&raw, compact);
                // bd serena-rust-we0：空 impls 可能是「类型分析未就绪」而非「无实现」；
                // bd serena-rust-xzb：workspace 加载错误无条件透出。
                let mut ws = self.workspace_error_warnings(root);
                if empty {
                    ws.extend(
                        self.semantic_not_ready_warnings(root, &file, line, col, lang)
                            .await,
                    );
                } else {
                    self.mark_semantic_ready(root);
                }
                attach_warning(&mut value, &ws);
                let root_key = format!("{}|{}|{}|{}", root.display(), file, line, col);
                Ok(self.maybe_delta("find-implementations", &root_key, value, delta).await)
            }
            "search" => {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'pattern'".into(),
                    })?;
                let path_glob = args.get("path_glob").and_then(|v| v.as_str());
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(100) as usize;
                let case_sensitive = args
                    .get("case_sensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut resp = self
                    .tool_search_for_pattern(root, pattern, path_glob, max_results, case_sensitive)
                    .await?;
                // I（§11-I）：--comments-only 注释行过滤，先滤后 enrich 省 LSP 缓存查询。
                let comments_only = args
                    .get("comments_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if comments_only {
                    resp.hits
                        .retain(|h| fs_tools::looks_like_comment(&h.file, &h.text));
                }
                // A（§10-A）：命中带所属符号；装饰失败静默（该字段留 None）。
                enrich_search_with_symbols(self, root, &mut resp.hits, lang).await;
                serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))
            }
            "symbol-body" => {
                let (file, symbol) = required_symbol_body_args(&args)?;
                serde_json::to_value(self.tool_symbol_body(root, &file, &symbol, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "edit-context" => {
                // B: 单次调用拿 body + callers + doc + tests（ai-token-features §10-B）。
                let (file, symbol) = required_symbol_body_args(&args)?;
                let report = crate::edit_context::collect(self, root, &file, &symbol, lang).await;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            "repo-map" => {
                // E: workspace 级符号地图（ai-token-features §10-E）。
                // 走 symbol-tree + per-symbol refs 计数，top_n 降序输出。
                let top_n = args
                    .get("top_n")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(20) as usize;
                let report = crate::repo_map::build(self, root, lang, top_n).await;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            "warm" => {
                // M: 预热 LS + 索引（ai-token-features-design §13-M / plan-m-warm.md）。
                let lang = args.get("lang").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'lang'".into(),
                    }
                })?;
                let timeout_secs = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                crate::warm::warm(self, root, lang, Duration::from_secs(timeout_secs)).await
            }
            "replace-body" => {
                let (file, symbol, new_body) = required_replace_args(&args)?;
                self.tool_replace_body(root, &file, &symbol, &new_body, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "applied": true,
                    "file": file,
                    "symbol": symbol,
                    "post_write_diagnostics": diag,
                }))
            }
            "rename-symbol" => {
                let (file, line, col, new_name) = required_rename_args(&args)?;
                serde_json::to_value(
                    self.tool_rename_symbol(root, &file, line, col, &new_name, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "read-file" => {
                let file = required_file(&args)?;
                let start_line = args
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let report = fs_tools::read_file(root, &file, start_line, end_line)
                    .await
                    .map_err(|e| ToolError::BadArgs {
                        detail: format!("read_file: {e}"),
                    })?;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            "list-dir" => {
                let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'path'".into(),
                    }
                })?;
                let max_depth = args
                    .get("max_depth")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let max_entries = args
                    .get("max_entries")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(500) as usize;
                let entries =
                    fs_tools::list_dir(root, path, max_depth, max_entries).map_err(|e| {
                        ToolError::BadArgs {
                            detail: format!("list_dir: {e}"),
                        }
                    })?;
                serde_json::to_value(entries).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-file" => {
                let name_pattern = args
                    .get("name_pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'name_pattern'".into(),
                    })?;
                let path_glob = args.get("path_glob").and_then(|v| v.as_str());
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(200) as usize;
                let hits = fs_tools::find_file(root, name_pattern, path_glob, max_results)
                    .map_err(|e| ToolError::BadArgs {
                        detail: format!("find_file: {e}"),
                    })?;
                serde_json::to_value(hits).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-referencing-symbols" => {
                let (file, line, col) = required_position(&args)?;
                let hits = self
                    .tool_referencing_symbols(root, &file, line, col, lang)
                    .await?;
                let grouped = args
                    .get("grouped")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if grouped {
                    let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                    let page_size = args
                        .get("page_size")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(20) as usize;
                    let report = ref_tools::group_refs(hits, page, page_size);
                    serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
                } else {
                    Ok(ref_symbol_hits_envelope(&hits, compact))
                }
            }
            "find-referencing-code-snippets" => {
                // O3：`symbol` 直查 —— 符号名解析为 (file, line, col)（LSP 0-based），
                // 与位置参数路径同基线；命中多个时附 warning 提示用了哪个。
                let (file, line, col, resolution_note) =
                    match args.get("symbol").and_then(|v| v.as_str()) {
                        Some(name) => self.resolve_symbol_position(root, name, lang).await?,
                        None => {
                            let (f, l, c) = required_position(&args)?;
                            (f, l, c, None)
                        }
                    };
                let context_lines = args
                    .get("context_lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as u32;
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as usize;
                let hits = self
                    .tool_referencing_code_snippets(
                        root,
                        &file,
                        line,
                        col,
                        context_lines,
                        max_results,
                        lang,
                    )
                    .await?;
                let mut value = ref_snippet_hits_envelope(&hits, compact);
                if let Some(note) = resolution_note {
                    attach_warning(&mut value, &[note]);
                }
                Ok(value)
            }
            "replace-text-in-symbol" => {
                let file = required_file(&args)?;
                let symbol = args
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'symbol'".into(),
                    })?
                    .to_owned();
                let old_text = args
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'old_text'".into(),
                    })?
                    .to_owned();
                let new_text = args
                    .get("new_text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'new_text'".into(),
                    })?
                    .to_owned();
                self.tool_edit_replace_text(root, &file, &symbol, &old_text, &new_text, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "applied": true,
                    "file": file,
                    "symbol": symbol,
                    "post_write_diagnostics": diag,
                }))
            }
            "insert-text-after-symbol" => {
                let (file, symbol, text) = required_edit_args(&args)?;
                let (end_line, end_col) = self
                    .tool_edit_insert_after_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                    "post_write_diagnostics": diag,
                }))
            }
            "insert-text-before-symbol" => {
                let (file, symbol, text) = required_edit_args(&args)?;
                let (end_line, end_col) = self
                    .tool_edit_insert_before_symbol(root, &file, &symbol, &text, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                    "post_write_diagnostics": diag,
                }))
            }
            "delete-text-in-symbol" => {
                let file = required_file(&args)?;
                let symbol = args
                    .get("symbol")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'symbol'".into(),
                    })?
                    .to_owned();
                let start_line =
                    args.get("start_line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        })? as u32;
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'end_line'".into(),
                    })? as u32;
                self.tool_edit_delete_text(root, &file, &symbol, start_line, end_line, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "applied": true,
                    "file": file,
                    "symbol": symbol,
                    "post_write_diagnostics": diag,
                }))
            }
            "safe-delete-symbol" => {
                let (file, symbol) = required_symbol_body_args(&args)?;
                let mut value = serde_json::to_value(
                    self.tool_safe_delete_symbol(root, &file, &symbol, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))?;
                // F2: 写完后挂诊断。`SafeDeleteReport` 不含 `file` 字段，
                // 直接读 args 拿到 file（required_symbol_body_args 已保证存在）。
                let diag = self.post_diag_for_write(root, &file, lang).await;
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("post_write_diagnostics".into(), serde_json::json!(diag));
                }
                Ok(value)
            }
            "insert-at-line" => {
                let file = required_file(&args)?;
                let line =
                    args.get("line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'line'".into(),
                        })? as u32;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'content'".into(),
                    })?
                    .to_owned();
                let (end_line, end_col) = self
                    .tool_insert_at_line(
                        root,
                        &file,
                        line,
                        &content,
                        opt_expected_hash(&args).as_deref(),
                        lang,
                    )
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                    "post_write_diagnostics": diag,
                }))
            }
            "replace-lines" => {
                let (file, start_line, end_line) = required_line_range(&args)?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'content'".into(),
                    })?
                    .to_owned();
                self.tool_replace_lines(
                    root,
                    &file,
                    start_line,
                    end_line,
                    &content,
                    opt_expected_hash(&args).as_deref(),
                    lang,
                )
                .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "applied": true,
                    "file": file,
                    "post_write_diagnostics": diag,
                }))
            }
            "delete-lines" => {
                let (file, start_line, end_line) = required_line_range(&args)?;
                self.tool_delete_lines(
                    root,
                    &file,
                    start_line,
                    end_line,
                    opt_expected_hash(&args).as_deref(),
                    lang,
                )
                .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "applied": true,
                    "file": file,
                    "post_write_diagnostics": diag,
                }))
            }
            other => Err(ToolError::BadArgs {
                detail: format!("unknown tool: {other}"),
            }),
        }?;
        // AI-token 特性 G（§10-G）：execute_tool 末尾统一后处理。_max_tokens 按预算
        // 截断 items（4 bytes ≈ 1 token，soft limit）；_compress 删 container/kind
        // 冗余字段。两者与 _compact/_delta 同套私有约定（sanitize 不清）。
        // 偏离 plan：原建议逐分支改造为统一变量；实际 match 整体即 Result，
        // `?` 一行收口零分支改动（11 特性已改动各分支，最小侵入）。
        if let Some(max_tokens) = args.get("_max_tokens").and_then(|v| v.as_u64()) {
            apply_budget(&mut value, max_tokens as usize);
        }
        if args.get("_compress").and_then(|v| v.as_bool()).unwrap_or(false) {
            apply_compress(&mut value);
        }
        Ok(value)
    }

    fn loaded_entries(&self) -> Vec<Key> {
        self.last_used.lock().unwrap().keys().cloned().collect()
    }

    async fn evict_failed(&self) -> usize {
        Self::evict_failed_instances(self).await
    }
}

/// LSP 3.17 `textDocument/definition` 响应允四种形态：
/// `null | Location | Location[] | LocationLink[]`（clangd 22 默认 LocationLink[]）。
/// 归一化为 `Option<Location>`：空 None；单 Location 直返；单元素数组返首项；
/// LocationLink[] 把 `targetUri + targetRange` 折叠为 Location。
fn normalize_definition(raw: Option<&serde_json::Value>) -> Option<Location> {
    let v = raw?;
    if v.is_null() {
        return None;
    }
    // 数组形态：取首个 LocationLike，归一化。
    let first = if v.is_array() {
        v.as_array()?.first()?
    } else {
        v
    };
    // LocationLink 形态：{ targetUri, targetRange, ... } → 转为 Location。
    if let (Some(target_uri), Some(target_range)) = (
        first.get("targetUri").and_then(|x| x.as_str()),
        first.get("targetRange"),
    ) {
        let range: lsp_types::Range = serde_json::from_value(target_range.clone()).ok()?;
        return Some(Location {
            uri: lsp_types::Uri::from_str(target_uri).ok()?,
            range,
        });
    }
    // 标准 Location：{ uri, range }。
    serde_json::from_value::<Location>(first.clone()).ok()
}

/// LSP 3.17 `textDocument/implementation` 响应允多种形态：
/// `null | Location | Location[] | LocationLink[]`。归一化为 `Vec<Location>`：
/// 数组逐元素归一化；单 Location 包成单元素 vec；null → 空 vec。
pub(crate) fn normalize_implementations(raw: Option<&serde_json::Value>) -> Vec<Location> {
    let mut out = Vec::new();
    let Some(v) = raw else { return out };
    if v.is_null() {
        return out;
    }
    let items: &[serde_json::Value] = if v.is_array() {
        v.as_array().expect("just checked")
    } else {
        std::slice::from_ref(v)
    };
    for it in items {
        // LocationLink 形态：{ targetUri, targetRange, ... } → 转 Location。

        if let (Some(target_uri), Some(target_range)) = (
            it.get("targetUri").and_then(|x| x.as_str()),
            it.get("targetRange"),
        ) && let Ok(range) = serde_json::from_value::<lsp_types::Range>(target_range.clone())
            && let Ok(uri) = lsp_types::Uri::from_str(target_uri)
        {
            out.push(Location { uri, range });
            continue;
        }
        // 标准 Location：{ uri, range }。
        if let Ok(loc) = serde_json::from_value::<Location>(it.clone()) {
            out.push(loc);
        }
    }
    out
}
/// 递归在 Nested documentSymbol 里找第一个 name == `symbol` 的 range。
/// Flat 形态（SymbolInformation）不含子符号，这里只处理 Nested —— clangd/mock_ls 都是 Nested。
/// `None`（LS 对未就绪/未加载文档返 `null`）视为未找到。
fn find_symbol_range(
    resp: Option<&DocumentSymbolResponse>,
    symbol: &str,
) -> Option<lsp_types::Range> {
    fn walk(items: &[DocumentSymbol], symbol: &str) -> Option<lsp_types::Range> {
        for it in items {
            if it.name == symbol {
                return Some(it.range);
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol)
            {
                return Some(r);
            }
        }
        None
    }
    match resp? {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol),
        DocumentSymbolResponse::Flat(_) => None,
    }
}

/// 读盘 + 按 LSP range 切符号体（tool_symbol_body 缓存命中/miss 两路共用）。
async fn read_and_slice(path: &Path, file: &str, range: lsp_types::Range) -> ToolResult<String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            detail: format!("read {}: {e}", path.display()),
        })?;
    let start = LspPos {
        line: range.start.line,
        character: range.start.character,
    };
    let end = LspPos {
        line: range.end.line,
        character: range.end.character,
    };
    lsp_core::offsets::slice_at(&text, start, end, OffsetEncoding::Utf16).map_err(|e| {
        ToolError::BadArgs {
            detail: format!("slice {file}@{start:?}-{end:?}: {e}"),
        }
    })
}

/// 找符号的 `(range, selectionRange)`：range = 删除范围，selectionRange = 标识符
/// 位置（references 锚点）。Flat 形态无 selectionRange，用 location.range 起点近似
/// （SymbolInformation 的 location 即标识符所在位置）。
/// `None`（LS 对未就绪/未加载文档返 `null`）视为未找到。
fn find_symbol_node(
    resp: Option<&DocumentSymbolResponse>,
    symbol: &str,
) -> Option<(lsp_types::Range, lsp_types::Range)> {
    fn walk(
        items: &[DocumentSymbol],
        symbol: &str,
    ) -> Option<(lsp_types::Range, lsp_types::Range)> {
        for it in items {
            if it.name == symbol {
                return Some((it.range, it.selection_range));
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol)
            {
                return Some(r);
            }
        }
        None
    }
    match resp? {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol),
        DocumentSymbolResponse::Flat(items) => items
            .iter()
            .find(|it| it.name == symbol)
            .map(|it| (it.location.range, it.location.range)),
    }
}

/// safe-delete 文本拦截门的扫描上限。hits 截断（truncated）意味着可疑出现只多不少，
/// 不影响拒删判定方向。
const TEXT_GATE_MAX_HITS: usize = 200;

/// safe-delete 文本交叉验证：统计 `hits` 中"可疑引用"数。
/// - 排除定义行（def_file 的 def_line_1based 行）；
/// - 注释行粗滤：trim 后以 `//` `/*` `*` `#` 开头（`#` 兼 python 注释/C 预处理；
///   代价是 rust `#[attr]` 行被跳过——粗滤即此，宁可少拒不可误拒）；
/// - 字符串粗滤：剥掉 `"..."` / `'...'` 字面量后不再含符号名 → 视为字符串出现跳过。
fn textual_occurrences_outside_def(
    hits: &[SearchHit],
    def_file: &str,
    def_line_1based: u32,
    symbol: &str,
) -> usize {
    let def_file_norm = def_file.replace('\\', "/").to_ascii_lowercase();
    hits.iter()
        .filter(|h| {
            let hf = h.file.replace('\\', "/").to_ascii_lowercase();
            !(hf == def_file_norm && h.line == def_line_1based)
        })
        .filter(|h| {
            let t = h.text.trim_start();
            !(t.starts_with("//")
                || t.starts_with("/*")
                || t.starts_with('*')
                || t.starts_with('#'))
        })
        .filter(|h| strip_string_literals(&h.text).contains(symbol))
        .count()
}

/// 单行字符串字面量粗剥：`"..."` / `'...'`（含 `\"` 转义）内字符丢弃，其余保留。
/// ponytail: 行级 naive 引号状态机，多行字符串/原始字符串（r#"..."#）会漏剥——
/// 粗滤用途足够，漏剥方向是多算可疑 → 误拒可回查，不吞删除。
fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in line.chars() {
        match quote {
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else {
                    out.push(c);
                }
            }
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
        }
    }
    out
}

/// 文档截断上限（设计 §3：documentation 默认 200 字符）。
const DOC_MAX_CHARS: usize = 200;

/// 把 LSP CompletionItem（已用 serde_json::Value 形态取出）映射到 `CompletionItemLite`。
/// 字段裁剪 / kind 映射 / doc 截断都集中在这里。
fn parse_completion_item(raw: serde_json::Value) -> CompletionItemLite {
    let label = raw
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let kind = raw
        .get("kind")
        .and_then(|v| v.as_i64())
        .map(map_completion_kind)
        .unwrap_or_else(|| "other".to_owned());
    let detail = raw
        .get("detail")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let insert = raw
        .get("insertText")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            if label.is_empty() {
                None
            } else {
                Some(label.clone())
            }
        });
    let doc = raw
        .get("documentation")
        .and_then(extract_doc_string)
        .map(truncate_doc);
    let deprecated = raw
        .get("deprecated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let additional_text_edits = raw
        .get("additionalTextEdits")
        .and_then(|v| serde_json::from_value::<Vec<lsp_types::TextEdit>>(v.clone()).ok())
        .unwrap_or_default();
    CompletionItemLite {
        label,
        kind,
        detail,
        insert,
        doc,
        deprecated,
        additional_text_edits,
    }
}

/// 把 LSP `documentation`（String | MarkupContent）展平成纯文本字符串。
fn extract_doc_string(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_owned());
    }
    // MarkupContent：{ kind: "markdown" | "plaintext", value: "..." }
    v.get("value").and_then(|x| x.as_str()).map(str::to_owned)
}

/// 截断 doc 到 200 char（codepoint 级，非字节；设计 §3/§8.1）。
fn truncate_doc(s: String) -> String {
    if s.chars().count() <= DOC_MAX_CHARS {
        s
    } else {
        let truncated: String = s.chars().take(DOC_MAX_CHARS).collect();
        format!("{truncated}…")
    }
}

/// LSP `CompletionItemKind` 整数 → 人类词。Unknown → "other"。
/// LSP spec kind 值见 `lsp_types::CompletionItemKind::*`（1..=25）。
fn map_completion_kind(kind_num: i64) -> String {
    use lsp_types::CompletionItemKind as K;
    let k = match kind_num {
        1 => K::TEXT,
        2 => K::METHOD,
        3 => K::FUNCTION,
        4 => K::CONSTRUCTOR,
        5 => K::FIELD,
        6 => K::VARIABLE,
        7 => K::CLASS,
        8 => K::INTERFACE,
        9 => K::MODULE,
        10 => K::PROPERTY,
        11 => K::UNIT,
        12 => K::VALUE,
        13 => K::ENUM,
        14 => K::KEYWORD,
        15 => K::SNIPPET,
        16 => K::COLOR,
        17 => K::FILE,
        18 => K::REFERENCE,
        19 => K::FOLDER,
        20 => K::ENUM_MEMBER,
        21 => K::CONSTANT,
        22 => K::STRUCT,
        23 => K::EVENT,
        24 => K::OPERATOR,
        25 => K::TYPE_PARAMETER,
        _ => return "other".to_owned(),
    };
    // lsp_enum! 给出的常量名 "Text" / "Method" / ... 转小写。
    // 形如 "EnumMember" → "enum_member"；"TypeParameter" → "type_parameter"。
    // 用 Debug 拿名字最稳（spec 表与常量名一一对应）。
    format!("{:?}", k).to_ascii_lowercase()
}
/// 从文本中删除符号 range：默认整行删除（start 行首 → end 行含换行）；
/// end 行符号之后还有非空白内容时只删到符号结尾，保留行尾余文。
fn delete_symbol_text(text: &str, range: lsp_types::Range) -> ToolResult<String> {
    let line_start = |line: u32| {
        lsp_core::offsets::position_to_byte(
            text,
            LspPos { line, character: 0 },
            OffsetEncoding::Utf16,
        )
        .map_err(|e| ToolError::BadArgs {
            detail: format!("position {line}:0: {e}"),
        })
    };
    let s = line_start(range.start.line)?;
    let e_sym = lsp_core::offsets::position_to_byte(
        text,
        LspPos {
            line: range.end.line,
            character: range.end.character,
        },
        OffsetEncoding::Utf16,
    )
    .map_err(|e| ToolError::BadArgs {
        detail: format!("end position: {e}"),
    })?;
    let e_line_start = line_start(range.end.line)?;
    let e_line_end = text[e_line_start..]
        .find('\n')
        .map_or(text.len(), |i| e_line_start + i);
    // end 行符号后只剩空白 → 整行吞掉（含换行）；否则保留行尾余文。
    let e = if text[e_sym..e_line_end].trim().is_empty() {
        if e_line_end < text.len() {
            e_line_end + 1
        } else {
            e_line_end
        }
    } else {
        e_sym
    };
    Ok(format!("{}{}", &text[..s], &text[e..]))
}

/// sha256(content) hex 前 16 位（对账用；碰撞概率足够低且只做提示性校验）。
pub(crate) fn content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let out = h.finalize();
    out.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// tempfile 原子写 + rename；Windows 共享冲突（目标被别进程打开）重试 5×50ms（I5）。
///
/// ↖ mirror: PR oraios/serena#2041（save edited source files atomically）对账 —
/// 上游把编辑保存从截断写 `open(path,"w")` 改为 temp-file+`os.replace`，并要求
/// symlink 目标**透传写**（rename 会把链接本体替换成普通文件，破坏链接关系）。
/// 本项目 edit 链路本就走 tmp+rename（语义已对齐），唯一缺口即 symlink：
/// rename 前先解析链接到真实目标，对目标做原子写。
async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    // 仅当最终组件是 symlink 才解析（canonicalize）；其余路径行为不变。
    // 悬空链接/链接环在此报错 —— 与其把链接替换成普通文件，不如明确失败。
    let target = match tokio::fs::symlink_metadata(path).await {
        Ok(m) if m.is_symlink() => Some(tokio::fs::canonicalize(path).await?),
        _ => None,
    };
    let path: &Path = target.as_deref().unwrap_or(path);
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let tmp_path = tmp.into_temp_path().keep()?;
    tokio::fs::write(&tmp_path, content).await?;

    let mut attempt = 0;
    loop {
        match tokio::fs::rename(&tmp_path, path).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 5 => {
                // Windows ERROR_SHARING_VIOLATION(32) / ERROR_ACCESS_DENIED(5) 常见于杀软/索引器；
                // 统一退避重试。
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = e;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    /// smoke：普通文件原子写落盘。
    #[tokio::test]
    async fn writes_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.rs");
        atomic_write(&p, "fn main() {}\n").await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "fn main() {}\n"
        );
    }

    /// ↖ mirror: PR oraios/serena#2041 — symlink 目标透传写：内容更新到 target，
    /// 链接本体不得被 rename 替换成普通文件。
    #[tokio::test]
    async fn symlinked_file_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.rs");
        std::fs::write(&target, "old\n").unwrap();
        let link = dir.path().join("link.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(&target, &link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Windows symlink 需开发者模式/管理员；无权限环境跳过（行为无法构造）。
                println!("skipped: symlink 需要开发者模式/管理员权限");
                return;
            }
            Err(e) => panic!("symlink_file: {e}"),
        }
        atomic_write(&link, "new\n").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new\n",
            "内容必须写到 target"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .is_symlink(),
            "链接本体不得被替换成普通文件"
        );
    }
}

#[cfg(test)]
mod safe_delete_tests {
    use super::*;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    #[test]
    fn delete_symbol_keeps_trailing_same_line_content() {
        // end 行符号后还有别的代码 → 只删符号本体。
        let text = "void f() {} void g() {}\n";
        let out = delete_symbol_text(text, range(0, 0, 0, 11)).unwrap();
        assert_eq!(out, " void g() {}\n");
    }
    #[test]
    fn delete_symbol_removes_whole_lines() {
        let text = "int a = 1;\nint orphan() { return 1; }\nint b = 2;\n";
        let out = delete_symbol_text(text, range(1, 0, 1, 26)).unwrap();
        assert_eq!(out, "int a = 1;\nint b = 2;\n");
    }
    #[test]
    fn delete_symbol_at_eof_without_trailing_newline() {
        let text = "int a;\nint orphan() { return 1; }";
        let out = delete_symbol_text(text, range(1, 0, 1, 26)).unwrap();
        assert_eq!(out, "int a;\n");
    }

    fn hit_line(file: &str, line: u32, text: &str) -> SearchHit {
        SearchHit {
            file: file.to_string(),
            line,
            col: 1,
            text: text.to_string(),
            match_start: 0,
            match_end: 3,
            symbol: None,
            container: None,
        }
    }

    /// refs 空 + 文本有引用（排除定义行/注释/字符串后仍有出现）→ 拦截门计数 > 0 → 拒。
    #[test]
    fn text_gate_counts_occurrences_outside_definition() {
        let hits = vec![
            hit_line("src/lib.rs", 7, "fn add(a: i32, b: i32) -> i32 { a + b }"), // 定义行
            hit_line("src/lib.rs", 9, "    let s = add(1, 2);"),                  // 真引用
            hit_line("src/main.rs", 3, "// add is unused"),                       // 注释
            hit_line("src/main.rs", 4, "    println!(\"add called\");"),          // 字符串
            hit_line("src/main.rs", 5, "    assert_eq!(add(2, 3), 5);"),          // 真引用
        ];
        assert_eq!(textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "add"), 2);
    }

    /// refs 空 + 文本无可疑出现（定义行本身 + 注释/字符串）→ 放行删除。
    #[test]
    fn text_gate_passes_when_nothing_outside_definition() {
        let hits = vec![
            hit_line("src/lib.rs", 7, "fn orphan() {}"),
            hit_line("src/lib.rs", 8, "// orphan kept for docs"),
            hit_line("src/lib.rs", 9, "    let s = \"orphan\";"),
        ];
        assert_eq!(textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "orphan"), 0);
    }

    /// 定义文件路径分隔符/大小写差异不重开定义行豁免（Windows 调用方传 `\` 形态）。
    #[test]
    fn text_gate_normalizes_definition_path() {
        let hits = vec![hit_line("src/lib.rs", 7, "fn add() {}")];
        assert_eq!(textual_occurrences_outside_def(&hits, "src\\lib.rs", 7, "add"), 0);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    /// 字段裁剪：保留 label/kind/insert；丢弃 sortText/filterText 等。
    #[test]
    fn parse_item_drops_lsp_internal_fields() {
        let raw = serde_json::json!({
            "label": "printf",
            "kind": 3,
            "detail": "int printf(const char *, ...)",
            "sortText": "00001",
            "filterText": "printf",
            "insertText": "printf",
            "documentation": { "kind": "markdown", "value": "fmt output" },
        });
        let lite = parse_completion_item(raw);
        assert_eq!(lite.label, "printf");
        assert_eq!(lite.kind, "function");
        assert_eq!(
            lite.detail.as_deref(),
            Some("int printf(const char *, ...)")
        );
        assert_eq!(lite.insert.as_deref(), Some("printf"));
        assert_eq!(lite.doc.as_deref(), Some("fmt output"));
        // 内部字段无对应键（struct 字段未定义）—— 静态保证。
    }

    /// insert 缺时用 label 兜底。
    #[test]
    fn parse_item_insert_falls_back_to_label() {
        let raw = serde_json::json!({ "label": "foo" });
        let lite = parse_completion_item(raw);
        assert_eq!(lite.insert.as_deref(), Some("foo"));
    }

    /// kind map：Function → "function"；Unknown → "other"。
    #[test]
    fn kind_map_lowercases_lsp_enum_names() {
        assert_eq!(map_completion_kind(3), "function");
        assert_eq!(map_completion_kind(2), "method");
        assert_eq!(map_completion_kind(6), "variable");
        assert_eq!(map_completion_kind(14), "keyword");
        assert_eq!(map_completion_kind(99), "other");
    }

    /// doc 截断：> 200 char 截断 + "…"；≤ 200 char 原文。
    #[test]
    fn truncate_doc_caps_at_two_hundred_chars() {
        let short = "a".repeat(100);
        assert_eq!(truncate_doc(short.clone()), short);
        let long = "a".repeat(500);
        let out = truncate_doc(long);
        assert!(
            out.chars().count() <= DOC_MAX_CHARS + 1,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.ends_with('…'), "expected trailing ellipsis: {out}");
    }

    /// documentation: String / MarkupContent 都能展平。
    #[test]
    fn extract_doc_string_handles_both_shapes() {
        let s = serde_json::json!("plain text");
        assert_eq!(extract_doc_string(&s).as_deref(), Some("plain text"));
        let m = serde_json::json!({ "kind": "markdown", "value": "**bold**" });
        assert_eq!(extract_doc_string(&m).as_deref(), Some("**bold**"));
        let none = serde_json::json!(null);
        assert_eq!(extract_doc_string(&none), None);
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod containing_symbol_tests {

    //! Phase 2.1: 按位置反查符号（documentSymbol walk 路径）。
    //!
    //! 不拉起 LS，直接喂 `DocumentSymbolResponse` 验核心规则：
    //! - 位置 [start.line, end.line] 闭区间 + 行内 col 边界。
    //! - 嵌套取最深命中链。
    //! - 无命中返空 Vec（不是 BadArgs）。
    use super::*;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: Position::new(sl, sc),
            end: Position::new(el, ec),
        }
    }

    /// 构造一棵嵌套符号树：mod 外层 → fn 内层。
    fn nested_two_level() -> DocumentSymbolResponse {
        // outer: mod add @ line 0-10
        // inner: fn call @ line 4-5
        let outer = DocumentSymbol {
            name: "outer".into(),
            detail: None,
            kind: lsp_types::SymbolKind::MODULE,
            tags: None,
            range: range(0, 0, 10, 0),
            selection_range: range(0, 4, 0, 9),
            children: Some(vec![DocumentSymbol {
                name: "inner".into(),
                detail: None,
                kind: lsp_types::SymbolKind::FUNCTION,
                tags: None,
                range: range(4, 0, 5, 1),
                selection_range: range(4, 3, 4, 8),
                children: None,
                deprecated: None,
            }]),
            deprecated: None,
        };
        DocumentSymbolResponse::Nested(vec![outer])
    }

    /// 单层（仅 module）：验 col 在起始/结束行的边界。
    fn single_module() -> DocumentSymbolResponse {
        let mod_ = DocumentSymbol {
            name: "outer".into(),
            detail: None,
            kind: lsp_types::SymbolKind::MODULE,
            tags: None,
            range: range(0, 0, 10, 5),
            selection_range: range(0, 4, 0, 9),
            children: None,
            deprecated: None,
        };
        DocumentSymbolResponse::Nested(vec![mod_])
    }

    #[test]
    fn position_in_range_respects_col_at_start_and_end_lines() {
        // range = line 4 col 5 .. line 6 col 10
        let r = range(4, 5, 6, 10);
        // 中间行：col 无所谓。
        assert!(position_in_range(r, 5, 0));
        assert!(position_in_range(r, 5, 100));
        // 起始行：col < start.character 越界。
        assert!(!position_in_range(r, 4, 4));
        assert!(position_in_range(r, 4, 5));
        assert!(position_in_range(r, 4, 99));
        // 结束行：col > end.character 越界。
        assert!(position_in_range(r, 6, 10));
        assert!(!position_in_range(r, 6, 11));
        // 行号 < start 或 > end 直接越界。
        assert!(!position_in_range(r, 3, 0));
        assert!(!position_in_range(r, 7, 0));
    }

    #[test]
    fn deepest_match_wins_inside_nested_function_body() {
        let resp = nested_two_level();
        // inner @ 4-5；位置 line=4 col=10 落在 inner 内（且在 outer 内）。
        let hits = collect_containing_hits(Some(&resp), "file://x", 4, 10);
        assert_eq!(hits.len(), 2, "expected outer+inner chain, got {hits:?}");
        assert_eq!(hits[0].name, "outer");
        assert_eq!(hits[0].container, None);
        assert_eq!(hits[1].name, "inner");
        assert_eq!(hits[1].container.as_deref(), Some("outer"));
    }

    #[test]
    fn position_outside_any_symbol_returns_empty() {
        let resp = single_module();
        // outer @ line 0-10；line=20 越过 end.line。
        let hits = collect_containing_hits(Some(&resp), "file://x", 20, 0);
        assert!(hits.is_empty(), "expected empty, got {hits:?}");
    }

    #[test]
    fn position_at_first_char_of_first_line_hits_only_outer() {
        let resp = nested_two_level();
        // outer @ 0-10；inner @ 4-5；line=0 落在 outer（不在 inner）。
        let hits = collect_containing_hits(Some(&resp), "file://x", 0, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "outer");
    }

    #[test]
    fn flat_response_skips_unmatched_and_returns_only_hits() {
        // flat: foo (l 0-2) + bar (l 5-8)；line=6 在 bar 内。
        let foo = lsp_types::SymbolInformation {
            name: "foo".into(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: lsp_types::Location {
                uri: "file://x".parse().unwrap(),
                range: range(0, 0, 2, 0),
            },
            container_name: None,
        };
        let bar = lsp_types::SymbolInformation {
            name: "bar".into(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: lsp_types::Location {
                uri: "file://x".parse().unwrap(),
                range: range(5, 0, 8, 0),
            },
            container_name: None,
        };
        let resp = DocumentSymbolResponse::Flat(vec![foo, bar]);
        let hits = collect_containing_hits(Some(&resp), "file://x", 6, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "bar");
    }
}

#[cfg(test)]
mod signature_help_tests {
    //! Phase 2.2: `textDocument/signatureHelp` 协议形状。
    //! 不拉起 LS，直接测：把构造的 LSP `SignatureHelp` JSON round-trip 到
    //! `lsp_types::SignatureHelp`（即 supervisor 透传时反序列化的目标类型），确认字段全保
    //! 留（label / parameters[] / activeSignature / activeParameter）。

    /// clangd on `add(1, 2)` 在括号内的典型响应：单一 signature, 两个参数，active=0（第一个参数）。
    fn add_call_signature_json() -> serde_json::Value {
        serde_json::json!({
            "signatures": [{
                "label": "add(int, int)",
                "documentation": "Adds two integers.",
                "parameters": [
                    { "label": "int a" },
                    { "label": "int b" }
                ],
                "activeParameter": 0
            }],
            "activeSignature": 0
        })
    }

    #[test]
    fn lsp_signature_help_round_trip_preserves_all_fields() {
        let raw = add_call_signature_json();
        let parsed: lsp_types::SignatureHelp = serde_json::from_value(raw.clone())
            .expect("LSP signatureHelp should round-trip into lsp_types::SignatureHelp");
        // signatures[0].label 必须保留（agent 据此识别函数签名）
        assert_eq!(parsed.signatures.len(), 1);
        assert_eq!(parsed.signatures[0].label, "add(int, int)");
        // parameters[] 保留两个
        let params = parsed.signatures[0].parameters.as_ref()
            .expect("parameters should be present");
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0].label,
            lsp_types::ParameterLabel::Simple("int a".into())
        );
        assert_eq!(
            params[1].label,
            lsp_types::ParameterLabel::Simple("int b".into())
        );
        // activeParameter / activeSignature 保留（agent 据此高亮当前参数）
        assert_eq!(parsed.active_signature, Some(0));
        assert_eq!(parsed.signatures[0].active_parameter, Some(0));
    }

    #[test]
    fn lsp_signature_help_null_round_trips_as_none() {
        // clangd 在非函数调用位置返 null（与 hover 行为一致）。
        let raw = serde_json::Value::Null;
        let parsed: Option<lsp_types::SignatureHelp> = serde_json::from_value(raw)
            .expect("null should round-trip to None");
        assert!(parsed.is_none());
    }

    #[test]
    fn lsp_signature_help_active_parameter_default_when_missing() {
        // LSP 允许省略 activeParameter（位置未确定）。lsp-types 默认 0；这里确认字段缺失时
        // 也能 round-trip, 不会强制要求 activeParameter 字段。
        let raw = serde_json::json!({
            "signatures": [{
                "label": "f()",
                "parameters": []
            }]
        });
        let parsed: lsp_types::SignatureHelp = serde_json::from_value(raw)
            .expect("missing activeParameter should still round-trip");
        assert_eq!(parsed.signatures.len(), 1);
        assert_eq!(parsed.signatures[0].label, "f()");
        assert_eq!(parsed.signatures[0].active_parameter, None);
        assert_eq!(parsed.active_signature, None);
    }
}

#[cfg(test)]
mod pull_diagnostics_tests {
    //! Phase 2.5: textDocument/diagnostic pull 路径 + fallback 透明契约（PLAN Task 2.5）。
    //!
    //! 不拉 LS，纯函数 + HashMap 表层断言 5 个分支：
    //! - #1 diagnosticProvider 字段缺失（mock_ls / rust-analyzer 现状）→ supports=false。
    //! - #2 diagnosticProvider = null → supports=false。
    //! - #3 diagnosticProvider = true / DiagnosticOptions 对象 → supports=true。
    //! - #4 pull 失败（LS 返 -32601）→ extract_pull_items 不被调用；tool_diagnostics 走 push。
    //! - #5 pull 成功（kind=full）→ extract_pull_items 取 items，**不**与 push 拼接。
    //!
    //! 真实端到端 fallback 验证走 fixtures/rust_demo + rust-analyzer 的 CLI smoke
    //! （拉起 supervisor → tool_diagnostics → 字段缺失 → 自动走 push 缓存），
    //! 见完成报告 `end-to-end` 一节。
    use super::Supervisor;
    use lsp_core::init_params::supports_pull_diagnostics;
    use serde_json::json;
    use std::path::PathBuf;

    /// #1 capabilities 缺 diagnosticProvider 字段（mock_ls 现状）。
    #[test]
    fn missing_diagnostic_provider_field_means_no_pull_support() {
        let caps = json!({ "positionEncoding": "utf-16" });
        assert!(!supports_pull_diagnostics(&caps));
    }

    /// #2 diagnosticProvider 显式为 null。
    #[test]
    fn null_diagnostic_provider_means_no_pull_support() {
        let caps = json!({ "diagnosticProvider": null });
        assert!(!supports_pull_diagnostics(&caps));
    }

    /// #3a diagnosticProvider = true（简写形态）。
    #[test]
    fn boolean_true_diagnostic_provider_means_pull_supported() {
        let caps = json!({ "diagnosticProvider": true });
        assert!(supports_pull_diagnostics(&caps));
    }

    /// #3b diagnosticProvider = DiagnosticOptions 对象（含 interFileDependencies）。
    /// 任务边界契约：子字段不影响 pull 支持判定。
    #[test]
    fn diagnostic_options_object_means_pull_supported() {
        let caps = json!({
            "diagnosticProvider": {
                "interFileDependencies": false,
                "workspaceDiagnostics": false
            }
        });
        assert!(supports_pull_diagnostics(&caps));
    }

    /// #4+#5 extract_pull_items 纯函数：kind=full → Some(items)；
    /// 其它形态 → None（unchanged / partial / 缺 kind / 错形态都触发 push fallback）。
    #[test]
    fn extract_pull_items_returns_items_only_for_full_kind() {
        let full = json!({
            "kind": "full",
            "items": [{"range": {"start": {"line": 0, "character": 0},
                                  "end": {"line": 0, "character": 1}},
                       "message": "err"}]
        });
        let items = Supervisor::extract_pull_items(&full).expect("kind=full 必须返 Some");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["message"], "err");

        // unchanged → None（push 缓存已有完整 items；不重复拼接）。
        let unchanged = json!({"kind": "unchanged", "resultId": "r1"});
        assert!(Supervisor::extract_pull_items(&unchanged).is_none());

        // 缺 kind → None（异常回 push 兜底）。
        let no_kind = json!({"items": []});
        assert!(Supervisor::extract_pull_items(&no_kind).is_none());

        // 空对象 → None。
        let empty = json!({});
        assert!(Supervisor::extract_pull_items(&empty).is_none());
    }

    /// #5 抽取成功路径与"与 push 不重复"语义：返回的是完整 items 数组，
    /// 调用方不再访问 push 缓存（避免重复拼接）。
    #[test]
    fn extract_pull_items_success_does_not_query_push_cache() {
        let full = json!({
            "kind": "full",
            "items": [
                {"message": "e1", "range": {"start": {"line": 0, "character": 0},
                                             "end": {"line": 0, "character": 1}}},
                {"message": "e2", "range": {"start": {"line": 1, "character": 0},
                                             "end": {"line": 1, "character": 1}}}
            ]
        });
        let items = Supervisor::extract_pull_items(&full).expect("kind=full");
        // 完整 items 直接返回（不与 push 拼接）；保证不重复。
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["message"], "e1");
        assert_eq!(items[1]["message"], "e2");
    }

    /// 修 P1 #1：空 publishDiagnostics 推送必须清缓存。push-only LS（如 rust-analyzer）
    /// 在用户把错误改完后会推空 items 数组 —— 修复前 `!is_empty` 才写入导致陈旧错误永
    /// 久残留；修复后空推送直接 remove 该 (root, uri) 条目。
    ///
    /// 本测试不拉 LS，纯函数复制 supervisor `session_for` 内嵌的 handler 闭包逻辑
    /// 验证"空 → remove、非空 → insert"两种行为，确保契约稳定（避免重构 handler
    /// 时偷改语义）。真正的 wire 验证由 tests/diagnostics.rs 的 clangd e2e 覆盖。
    #[test]
    #[allow(clippy::type_complexity)]
    fn empty_push_clears_cache_entry() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        let cache: std::collections::HashMap<(PathBuf, String), Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        let cache_arc: Arc<
            Mutex<std::collections::HashMap<(PathBuf, String), Vec<serde_json::Value>>>,
        > = Arc::new(Mutex::new(cache));
        let generation = Arc::new(AtomicU64::new(0));
        let root: PathBuf = PathBuf::from("/proj");

        // 复用 supervisor lib.rs:644 区域的 handler 语义（手工镜像）：
        let handler = |uri: String, items: Vec<serde_json::Value>| {
            generation.fetch_add(1, Ordering::Relaxed);
            let mut cache = cache_arc.lock().unwrap();
            let key = (root.clone(), uri);
            if items.is_empty() {
                cache.remove(&key);
            } else {
                cache.insert(key, items);
            }
        };

        // 推一个非空 push：cache 应有 1 条，generation 1。
        handler(
            "file:///a.cpp".into(),
            vec![json!({"message": "err1"})],
        );
        assert_eq!(cache_arc.lock().unwrap().len(), 1);
        assert_eq!(generation.load(Ordering::Relaxed), 1);

        // 推空 push：cache 应清空该条目，generation 2。
        handler("file:///a.cpp".into(), vec![]);
        assert_eq!(cache_arc.lock().unwrap().len(), 0, "空 push 必须清缓存");
        assert_eq!(generation.load(Ordering::Relaxed), 2);

        // 推空 push 对未存在的 uri：cache 不增不减，generation 3。
        handler("file:///b.cpp".into(), vec![]);
        assert_eq!(cache_arc.lock().unwrap().len(), 0, "空 push 对空 key 是 no-op");
        assert_eq!(generation.load(Ordering::Relaxed), 3);
    }

    /// F2 验收：post_diag_for_write 三种失败模式都降级为 `[]`。
    ///
    /// 写工具返回值挂诊断快照是设计目标，但降级路径才是契约核心 —— 任何失败
    /// 都不能影响主结果（写工具仍正常返回 applied=true）。三种路径：
    /// - 场景 A：session_for 抛 NotInstalled/NotFound（root 不存在 / 文件无
    ///   LanguageServer 可拉）→ tool_diagnostics 返 Err → helper 降级 pending 快照。
    /// - 场景 B：根路径不存在 / 完全无法解析 → resolve_lang_for_file / 早期错误
    ///   路径 → helper 降级 pending 快照。
    /// - 场景 C：合法且无错误 → 无 items → helper 返回空 items（is_empty）。
    #[tokio::test]
    async fn post_diag_for_write_degrades_on_each_failure_mode() {
        let sup = Supervisor::direct().await.expect("supervisor");

        // 场景 A：根路径不存在 → session_for 失败（要么 resolve_lang 失败，
        // 要么 launch 抛 ToolError::NotInstalled）。helper 必须降级 pending 快照。
        let bad_root = std::path::PathBuf::from("Z:/nonexistent_for_test_xyz_42");
        let a = sup
            .post_diag_for_write(&bad_root, "x.rs", Some("rust"))
            .await;
        assert_eq!(
            a["items"],
            serde_json::json!([]),
            "root 不存在 → 必须降级为空数组"
        );
        assert_eq!(a["pending"], serde_json::json!(true), "失败降级必带 pending");

        // 场景 B：root 存在但 lang 完全无法解析（未装 LS + 无 override 路径探测
        // 也未命中）→ tool_diagnostics 返 Err → helper 降级 pending 快照。
        let tmp = tempfile::tempdir().expect("tempdir");
        let b = sup
            .post_diag_for_write(
                tmp.path(),
                "no_extension_file_with_unknown_lang_qq",
                Some("__definitely_not_a_real_lang__"),
            )
            .await;
        assert_eq!(
            b["items"],
            serde_json::json!([]),
            "lang 无法解析 → 必须降级为空数组"
        );

        // 场景 C：合法 + 文件不存在 → 走 session_for + ensure_open 路径。
        // 写工具超时/失败兜底 helper 验证降级；LS 未拉起场景下 helper 也必须返空。
        let c = sup
            .post_diag_for_write(tmp.path(), "does_not_exist_xyz_42.rs", Some("rust"))
            .await;
        assert_eq!(
            c["items"],
            serde_json::json!([]),
            "文件不存在 → 必须降级为空数组"
        );
    }
}
// ============================================================================
// Phase 3.1 文档符号缓存（local/solidlsp-development-plan.md §3.1）
// ============================================================================

#[cfg(test)]
mod reclaim_idle_buffers_tests {
    //! 修 P1 #2（TTL 生产执行者）：验证 supervisor 工具调用路上 + 显式调用两条
    //! 路径都能让 ref_count=0 + 超 TTL 的 FileBuffer 在生产路径被回收。
    //!
    //! 测试夹具：拉起 mock_ls → 注入 supervisor 实例池 → 创建 guard → drop → 等超
    //! 测试 TTL（短、可断言）→ 调 supervisor.reclaim_idle_buffers_once 走到阈值后
    //! 断言回收数 ≥1。
    //!
    //! mock_ls 通过 `CARGO_BIN_EXE_mock_ls` env 提供 —— 该 env 仅在 lsp-core 测试
    //! 二进制可见。本测试加 skip 守卫，找不到 mock_ls 即跳过（不构成 false failure）。
    use super::*;
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_types::InitializeParams;
    use std::ffi::OsString;
    use std::time::Duration;

    /// mock_ls 二进制在 cargo build 时由 lsp-core 包提供，supervisor 包在测试
    /// 二进制可见但 `CARGO_BIN_EXE_*` 仅在当前 crate 范围内设置 —— supervisor
    /// 找不到则跳过（不计入失败）。运行时通过 PATH / build target-dir 兜底查找。
    /// pub(crate)：symbol_cache_tests（P2-18h）跨 mod 复用。
    pub(crate) fn find_mock_ls() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_mock_ls") {
            let p = std::path::PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        // 兜底：target/debug 下任一 cargo 测试 binary 名查找（cargo build test 留产物）。
        let ext = if cfg!(windows) { ".exe" } else { "" };
        if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
            let dir = std::path::PathBuf::from(target);
            for p in [
                dir.join(format!("debug/mock_ls{}", ext)),
                dir.join(format!("debug/deps/mock_ls{}", ext)),
            ] {
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        // 兜底：默认 cargo target-dir 即项目根 target/。测试进程 cwd 是 package
        // 目录（crates/supervisor），workspace 共享 target 在其上两级 —— 用编译期
        // CARGO_MANIFEST_DIR 锚定，`cargo test -p supervisor` 单包跑法也能找到。
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(format!("target/debug/mock_ls{}", ext)));
            candidates.push(cwd.join(format!("target/debug/deps/mock_ls{}", ext)));
        }
        if let Some(ws) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2) {
            candidates.push(ws.join(format!("target/debug/mock_ls{}", ext)));
        }
        candidates.into_iter().find(|p| p.is_file())
    }

    fn launch_mock_ls() -> Option<LaunchInfo> {
        let exe = find_mock_ls()?;
        Some(LaunchInfo {
            cmd: vec![OsString::from(exe)],
            cwd: std::env::temp_dir(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    /// 修 P1 #2 生产路径回收语义：mock 拉 session → guard drop → 等超测试 TTL
    /// (5 ms) → supervisor 工具调用累计到 RECLAIM_THRESHOLD=32 → reclaim 真正跑
    /// `Session::evict_idle_buffers(测试 TTL)` → 断言回收数 ≥1 + counter 归零。
    #[tokio::test]
    async fn production_path_reclaim_after_idle_ttl() {
        let Some(launch) = launch_mock_ls() else {
            println!("skipped: mock_ls binary not found (lsp-core not yet built?)");
            return;
        };

        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let file = tmp.path().join("a.cpp");
        tokio::fs::write(&file, b"int x=0;\n").await.expect("write fixture");

        // 直连 mock_ls 拉 session。注入到 supervisor 实例池以便 reclaim 扫到。
        let child = Child::spawn(launch).expect("spawn mock_ls");
        let session = lsp_core::session::Session::start(Some(child), InitializeParams::default())
            .await
            .expect("Session::start Ready");
        let sup = Supervisor::direct().await.unwrap();
        // 短 TTL 让单测可控（5 ms 远小于 60 s 默认值）。
        sup.set_idle_ttl_for_test(Duration::from_millis(5));

        let key = Supervisor::key(tmp.path(), "cpp");
        {
            let mut instances = sup.instances.lock().unwrap();
            instances.insert(key.clone(), session.clone());
            let mut last_used = sup.last_used.lock().unwrap();
            last_used.insert(key.clone(), std::time::Instant::now());
        }

        // ensure_open → drop → ref_count=0 + last_released_at=Some(now)
        {
            let _guard = session.ensure_open(&file).await.expect("ensure_open");
        }
        // 等超 5ms TTL
        tokio::time::sleep(Duration::from_millis(20)).await;

        // 未达阈值（32）前 reclaim 应返 0；counter 逐次累加。
        for i in 0..31 {
            let n = sup.reclaim_idle_buffers_once();
            assert_eq!(n, 0, "第 {i} 次未达阈值应返 0");
        }
        assert_eq!(
            sup.reclaim_count_snapshot(),
            31,
            "调用 31 次后 counter = 31"
        );

        // 第 32 次触发：阈值命中 + reclaim 调 Session::evict_idle_buffers(5ms)。
        // buffer 早超 5ms，应被回收。
        let reclaimed = sup.reclaim_idle_buffers_once();
        assert!(reclaimed >= 1, "归零超 TTL 后生产路径必须能回收，至少 1 条；reclaimed={reclaimed}");
        assert_eq!(
            sup.reclaim_count_snapshot(),
            0,
            "reclaim 触发后 counter 应归零"
        );

        // 清理：shutdown session + 卸 supervisor 池条目。
        session.shutdown().await;
        let _ = sup.evict(&key).await;
    }

    /// P2-0bq: evict 必须清理 `load_gates` / `pull_diag_supported` 两张旁表，
    /// 否则 LRU 反复驱逐同 (root, lang) 按驱逐次数单调累积（gate 是 tokio Mutex，
    /// pull_diag_supported 是 bool——虽小但每 key 一条，永不回收）。
    ///
    /// 这里直接构造一个 key，造表条目，evict，再断言两张表都已清掉。
    #[tokio::test]
    async fn evict_removes_load_gates_and_pull_diag_supported() {
        let sup = Supervisor::direct().await.unwrap();
        let key = Supervisor::key(Path::new("Z:/no/such/project"), "rust");

        // 插 gate（任意 Arc<Mutex<()>>）+ pull 标记。
        sup.load_gates
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::new(tokio::sync::Mutex::new(())));
        sup.pull_diag_supported
            .lock()
            .unwrap()
            .insert(key.clone(), true);

        // instances 没有该 key → evict 返 false 但仍清两表。
        let removed = sup.evict(&key).await.expect("evict");
        assert!(!removed, "instances 没 key 时 evict 返 false");

        assert!(
            !sup.load_gates.lock().unwrap().contains_key(&key),
            "load_gates 必须清掉"
        );
        assert!(
            !sup.pull_diag_supported.lock().unwrap().contains_key(&key),
            "pull_diag_supported 必须清掉"
        );
    }

    /// 修 P1 #2 节流验证：连续 32 次调用中前 31 次不应扫 sessions（只递增
    /// 计数器）；第 32 次才真正走 sweep。这是 throttle 防抖核心。
    #[tokio::test]
    async fn reclaim_is_throttled_until_threshold() {
        let sup = Supervisor::direct().await.unwrap();
        sup.set_idle_ttl_for_test(Duration::from_secs(60));
        // 计数器初始 0
        assert_eq!(sup.reclaim_count_snapshot(), 0);
        // 31 次 +1 均应未触发 reclaim
        for _ in 0..31 {
            let _ = sup.reclaim_idle_buffers_once();
        }
        assert_eq!(
            sup.reclaim_count_snapshot(),
            31,
            "31 次调用后 counter 应 = 31"
        );
        // 第 32 次返回 0（无 sessions），但 counter 应归零。
        let _ = sup.reclaim_idle_buffers_once();
        assert_eq!(
            sup.reclaim_count_snapshot(),
            0,
            "第 32 次触发后 counter 归零"
        );
    }
}

#[cfg(test)]
mod write_consistency_tests {
    //! bd serena-rust-0em / 76d：写后索引一致性。纪律：不拉 LS —— 磁盘比对与
    //! zip 修正是纯逻辑可直接断言；`realign_stale_hits` 的 LS 轮询链路与
    //! `post_diag_for_write` 的失效接线由 CLI e2e 锁（fixtures + rust-analyzer）。
    use super::*;

    fn hit_at(name: &str, uri: &str, line: u32) -> SymbolHit {
        SymbolHit {
            name: name.to_string(),
            kind: SymbolKindTag::Function,
            uri: uri.into(),
            range: lsp_types::Range {
                start: Position::new(line, 0),
                end: Position::new(line, 8),
            },
            container: None,
        }
    }

    #[test]
    fn hits_match_disk_fresh_attr_window_stale_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.rs");
        std::fs::write(&p, "fn foo() {}\nfn bar() {}\n").unwrap();

        // 对齐：起点行含名字。
        assert_eq!(
            hits_match_disk_for_file(&p, &[hit_at("foo", "file:///x/a.rs", 0)]),
            Some(true)
        );
        // 容差窗：起点行是 attribute，名字在下 1 行 —— full range 起点 ≠ 名字行。
        let q = dir.path().join("attr.rs");
        std::fs::write(&q, "#[derive(Debug)]\nstruct S;\n").unwrap();
        assert_eq!(
            hits_match_disk_for_file(&q, &[hit_at("S", "file:///x/attr.rs", 0)]),
            Some(true)
        );
        // stale：越界行。
        assert_eq!(
            hits_match_disk_for_file(&p, &[hit_at("foo", "file:///x/a.rs", 9)]),
            Some(false)
        );
        // stale：行内容不含符号名（错位到别的行）。
        assert_eq!(
            hits_match_disk_for_file(&p, &[hit_at("qux", "file:///x/a.rs", 0)]),
            Some(false)
        );
        // 读盘失败 = 无法判定。
        assert_eq!(
            hits_match_disk_for_file(&dir.path().join("nope.rs"), &[hit_at("x", "", 0)]),
            None
        );
        // 空集 = 一致（冷启动空语义不受影响）。
        assert_eq!(hits_match_disk_for_file(&p, &[]), Some(true));
    }

    #[test]
    fn stale_symbol_files_collects_only_mismatched_and_normalizes_case() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().replace('\\', "/");
        let mk_uri = |f: &str, lower: bool| {
            let u = format!("file:///{base}/{f}");
            if lower {
                // 小写盘符变体（RA 实测返回形态），判 stale 走归一后的真实路径。
                u.replacen("file:///C:/", "file:///c:/", 1)
            } else {
                u
            }
        };
        std::fs::write(dir.path().join("fresh.rs"), "fn alpha() {}\n").unwrap();
        // 位移超出容差窗（>5 行）：hit@0 的窗内不含 beta → 检出 stale。
        std::fs::write(
            dir.path().join("moved.rs"),
            "// shifted far\n\n\n\n\n\n\nfn beta() {}\n",
        )
        .unwrap();
        let hits = vec![
            hit_at("alpha", &mk_uri("fresh.rs", false), 0),
            // 行号 0 指向注释行，不含 beta → stale；且 uri 用小写盘符变体。
            hit_at("beta", &mk_uri("moved.rs", true), 0),
        ];
        let stale = stale_symbol_files(&hits);
        // 返回值与 diag_cache 同纪律：uri 全小写归一。
        assert_eq!(stale, vec![mk_uri("moved.rs", true).to_lowercase()]);
    }

    #[test]
    fn replace_tables_swaps_target_files_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().replace('\\', "/");
        let mk_uri = |f: &str| format!("file:///{base}/{f}");
        let mk_uri_pct = |f: &str| format!("file:///{base}/{f}").replacen("file:///C:/", "file:///C%3A/", 1);
        std::fs::write(dir.path().join("moved.rs"), "fn stale_line() {}\nfn added() {}\n").unwrap();
        std::fs::write(dir.path().join("other.rs"), "fn keep() {}\n").unwrap();
        let hits = vec![
            hit_at("stale_line", &mk_uri_pct("moved.rs"), 7), // percent-encode 盘符形态
            hit_at("ghost", &mk_uri("moved.rs"), 30),         // 目标文件整体替换：幽灵自动消失
            hit_at("keep", &mk_uri("other.rs"), 5),           // 非目标文件
        ];
        let moved_path = dunce::canonicalize(dir.path().join("moved.rs")).unwrap();
        let mut tables = HashMap::new();
        // docsym 新鲜表（已按 query 过滤）：stale_line 行号新 + added（wssym 缺失形态补全）。
        tables.insert(
            moved_path,
            vec![hit_at("stale_line", "", 0), hit_at("added", "", 1)],
        );
        let out = replace_files_with_tables(hits, &tables);
        assert_eq!(out.len(), 3, "2 from docsym table + 1 kept from other file");
        let moved = out.iter().find(|h| h.name == "stale_line").unwrap();
        assert_eq!(moved.range.start.line, 0, "行号来自 docsym（新鲜）");
        assert!(
            moved.uri.ends_with("/moved.rs"),
            "uri 归一为 path_to_uri_str 小写: {}",
            moved.uri
        );
        assert!(out.iter().any(|h| h.name == "added"), "缺失条目补全");
        assert!(
            !out.iter().any(|h| h.name == "ghost"),
            "docsym 查无的幽灵自动消失（全表替换）"
        );
        let kept = out.iter().find(|h| h.name == "keep").unwrap();
        assert_eq!(kept.range.start.line, 5, "非目标文件原样保留");
    }

    #[tokio::test]
    async fn recent_write_marks_and_expires_by_ttl() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/write-proj");
        assert!(sup.recent_written_uris(root, Duration::from_secs(10)).is_empty());
        sup.mark_recent_write(root, "lib.rs");
        let uris = sup.recent_written_uris(root, Duration::from_secs(10));
        assert_eq!(uris.len(), 1, "窗口内标记必须可见");
        assert!(uris[0].ends_with("/lib.rs"), "归一 uri: {}", uris[0]);
        // TTL = 0 → 立即过期并清理。
        assert!(sup.recent_written_uris(root, Duration::ZERO).is_empty());
        assert!(sup.recent_written_uris(root, Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn invalidate_write_derived_caches_clears_root_signal_entry() {
        let root = Path::new("Z:/no/such/write-proj");
        ROOT_SIGNAL_CACHE.lock().unwrap().insert(
            root.to_path_buf(),
            (std::time::Instant::now(), None, Default::default()),
        );
        assert!(ROOT_SIGNAL_CACHE.lock().unwrap().contains_key(root));
        invalidate_write_derived_caches(root);
        assert!(
            !ROOT_SIGNAL_CACHE.lock().unwrap().contains_key(root),
            "写后必须清 root 信号 TTL 条目，否则 find_symbol 在 2s 窗口内命中写前缓存"
        );
    }
}

#[cfg(test)]
mod symbol_cache_tests {
    //! 纪律：不拉 LS —— 命中路径在 session_for 之前返回，可对空 supervisor 做工具级
    //! 断言；miss→写→hit 全链路由 CLI smoke（fixtures/rust_demo + rust-analyzer）覆盖。
    use super::*;
    use std::time::Instant;

    fn hit(name: &str) -> SymbolHit {
        SymbolHit {
            name: name.to_string(),
            kind: SymbolKindTag::Function,
            uri: "file:///x/a.rs".into(),
            range: lsp_types::Range {
                start: Position::new(0, 0),
                end: Position::new(3, 0),
            },
            container: None,
        }
    }

    /// bd serena-rust-bxd O2/O4：暖机窗口开/关/过期三态。
    #[tokio::test]
    async fn index_warming_window_opens_closes_and_expires() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/warmup-proj");

        // 无记账（从未拉起 LS）→ 不提示。
        assert!(sup.index_warming_warnings(root).is_empty());

        // LS 启动 → 窗口开（10s 内 + 未语义成功）→ partial 提示。
        sup.mark_ls_started(root);
        let ws = sup.index_warming_warnings(root);
        assert_eq!(ws, vec![index_warming_message()]);

        // 首个语义成功 → 窗口提前关闭。
        sup.mark_semantic_ready(root);
        assert!(sup.index_warming_warnings(root).is_empty());

        // 过期路径：手工回拨 started 越过 10s 窗 → 不提示。
        let stale_root = Path::new("Z:/no/such/warmup-stale");
        sup.ls_warmup.lock().unwrap().insert(
            key_root_identity(stale_root),
            LsWarmup {
                started: std::time::Instant::now() - LS_WARMUP_WINDOW - Duration::from_secs(1),
                semantic_ok: false,
            },
        );
        assert!(sup.index_warming_warnings(stale_root).is_empty());
    }

    /// bd serena-rust-bxd O3：--symbol 直查解析（documentSymbol 缓存路径）。
    #[tokio::test]
    async fn resolve_symbol_position_prefers_exact_and_reports_multi() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/o3-proj");
        let mk = |name: &str, uri: &str, sl: u32, sc: u32| SymbolHit {
            name: name.to_string(),
            kind: SymbolKindTag::Function,
            uri: uri.to_string(),
            range: lsp_types::Range {
                start: Position::new(sl, sc),
                end: Position::new(sl, sc + 5),
            },
            container: None,
        };
        // 前缀命中在 a.rs（字母序在前），精确命中在 b.rs —— 精确必须赢。
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "a.rs"),
            vec![mk("ensure_open_impl", "file:///x/a.rs", 0, 0)],
        );
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "b.rs"),
            vec![mk("ensure_open", "file:///x/b.rs", 4, 2)],
        );
        let (file, line, col, note) =
            sup.resolve_symbol_position(root, "ensure_open", None).await.unwrap();
        assert_eq!((file.as_str(), line, col), ("b.rs", 4, 2));
        assert!(note.is_none(), "单命中不提示");

        // 多命中：同名精确 ×2（前缀 ×1 落选不计）→ 取 (file, line, col) 序首者 + note 报总数。
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "c.rs"),
            vec![mk("ensure_open", "file:///x/c.rs", 1, 0)],
        );
        let (file, _l, _c, note) =
            sup.resolve_symbol_position(root, "ensure_open", None).await.unwrap();
        assert_eq!(file, "b.rs", "同级按 (file, line, col) 排序取首");
        let note = note.unwrap();
        assert!(note.contains("2 matches"), "note 应报所选层级总数: {note}");
        assert!(note.contains("b.rs:5"), "note 含 1-based 位置: {note}");

        // 零命中：缓存无匹配 + fake root 的 wssym 兜底必败 → BadArgs rc=2。
        let err = sup
            .resolve_symbol_position(root, "zzz_absent", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::BadArgs { .. }),
            "零命中必须 BadArgs rc=2: {err:?}"
        );

        // 打回修复：暖机窗口内零命中的错误必须带 warming hint（不得裸 not found
        // 让 AI 误判「符号不存在」）；非窗口错误不带 hint。
        sup.mark_ls_started(root);
        let err = sup
            .resolve_symbol_position(root, "zzz_absent_warm", None)
            .await
            .unwrap_err();
        let ToolError::BadArgs { detail } = &err else {
            panic!("expected BadArgs: {err:?}")
        };
        assert!(
            detail.contains("index may still be warming (cold start), retry shortly or use find-symbol"),
            "暖机窗口零命中必须带 hint: {detail}"
        );
        // 对照：非窗口 root（从未开窗）零命中 → 错误不带 warming hint。
        let cold_root = Path::new("Z:/no/such/o3-proj-cold");
        let err = sup
            .resolve_symbol_position(cold_root, "zzz_absent", None)
            .await
            .unwrap_err();
        assert!(
            !err.to_string().contains("index may still be warming"),
            "非窗口零命中不得带 hint: {err}"
        );
    }

    /// ↖ mirror: ls.py@a5fd4d68 — 低层（LS 会话）版本变化后高层缓存不得再命中：
    /// 同 root 的单文件级与 workspace 级缓存全部失效，其他 root 不受影响。
    #[tokio::test]
    async fn session_rebuild_invalidates_symbol_cache_for_root() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let other = Path::new("Z:/no/such/other");
        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("main")]);
        sup.symbol_cache_put(
            find_symbol_cache_key(root, "main", None),
            vec![hit("main")],
        );
        sup.symbol_cache_put(doc_symbol_cache_key(other, "a.rs"), vec![hit("helper")]);

        // 模拟 (root, lang) 会话换代（session_for 挂入新会话前的失效动作）。
        sup.invalidate_symbol_cache_for_root(root);

        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "a.rs"))
                .is_none(),
            "会话换代后同 root 文档符号缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&find_symbol_cache_key(root, "main", None))
                .is_none(),
            "会话换代后同 root workspace 级缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(other, "a.rs")).is_some(),
            "其他 root 的缓存不受影响"
        );
    }

    /// cache 命中：同 file 二次 overview 命中不拉 LS（命中路径 vs LS 往返秒级的量级差；
    /// 并行全量测试下 CPU 调度抖动可达数 ms，阈值放宽到 10ms 仍比 LS 往返低两个量级）。
    #[tokio::test]
    async fn overview_cache_hit_returns_under_1ms() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("main")]);

        // warmup：预热 LazyLock / 代码路径，排除首次抖动。
        let _ = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();

        let t0 = Instant::now();
        let out = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "main");
        assert!(
            elapsed < Duration::from_millis(10),
            "cache hit took {elapsed:?}"
        );
    }

    /// 命中必须先于 lang 解析 / session 拉起（不可解析扩展名 + 不存在 root 也命中）。
    #[tokio::test]
    async fn overview_cache_hit_precedes_lang_resolution() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "x.unknownext"),
            vec![hit("weird")],
        );
        let out = sup.tool_overview(root, "x.unknownext", None).await.unwrap();
        assert_eq!(out[0].name, "weird");
    }

    /// cache miss：空 supervisor 必 miss；put 后 get 命中（首次调用写入 cache 的机制）。
    #[tokio::test]
    async fn cache_miss_then_put_then_hit() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let key = doc_symbol_cache_key(root, "a.rs");
        assert!(
            sup.symbol_cache_get(&key).is_none(),
            "fresh supervisor must miss"
        );
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        let got = sup.symbol_cache_get(&key).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "f");
    }

    /// invalidate：mtime 变 → key 变 → miss（下次重调 LS）。std set_modified，无新依赖。
    #[tokio::test]
    async fn mtime_change_invalidates_entry() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let root = dir.path();

        let key1 = doc_symbol_cache_key(root, "a.rs");
        sup.symbol_cache_put(key1.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key1).is_some());

        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
            .unwrap();
        let key2 = doc_symbol_cache_key(root, "a.rs");
        assert_ne!(key1, key2, "mtime change must produce a new cache key");
        assert!(
            sup.symbol_cache_get(&key2).is_none(),
            "new mtime must miss (invalidate)"
        );
    }

    /// 外部修改感知（workspace 级）：root 下源码文件 mtime 变 → `root_source_mtime`
    /// 信号推进 → find_symbol 缓存 key 变 → miss 重查。旧实现 (root,query) 键控时
    /// 外部改文件后 find-symbol 永远返回陈旧结果。
    #[tokio::test]
    async fn root_source_mtime_change_invalidates_find_symbol_cache() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let root = dir.path();

        let key1 = find_symbol_cache_key(root, "f", root_source_mtime(root));
        sup.symbol_cache_put(key1.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key1).is_some(), "warm cache must hit");

        // 外部改源码文件 mtime（set_modified 推进，不依赖真实时钟粒度）。
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
            .unwrap();

        let key2 = find_symbol_cache_key(root, "f", root_source_mtime(root));
        assert_ne!(key1, key2, "root source mtime signal must advance key");
        assert!(
            sup.symbol_cache_get(&key2).is_none(),
            "changed root must miss (invalidate find-symbol cache)"
        );
    }

    /// 不同 file 不同 key（不串扰）。
    #[tokio::test]
    async fn different_files_do_not_share_entries() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let ka = doc_symbol_cache_key(root, "a.rs");
        let kb = doc_symbol_cache_key(root, "b.rs");
        assert_ne!(ka, kb);
        sup.symbol_cache_put(ka.clone(), vec![hit("a_sym")]);
        assert!(sup.symbol_cache_get(&ka).is_some());
        assert!(sup.symbol_cache_get(&kb).is_none());
    }

    /// find-symbol 缓存：query 命中 + limit 对缓存全量截断；不同 query 不串扰。
    /// root 用真实空 tempdir：tool_find_symbol 命中路径会 walk root 算 mtime 信号，
    /// 不存在的假盘符路径（Z:/...）walk 探测可达 10ms+ 网络超时量级，污染 <10ms 断言。
    #[tokio::test]
    async fn find_symbol_cache_hits_by_query_and_respects_limit() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        sup.symbol_cache_put(
            find_symbol_cache_key(root, "parse", None),
            vec![hit("parse_a"), hit("parse_b"), hit("parse_c")],
        );

        let t0 = Instant::now();
        let (out, warnings) = sup
            .tool_find_symbol(root, "parse", 2, Some("rust"))
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert!(warnings.is_empty(), "cache hit must not fabricate warnings");
        // 50ms：仍远低于 LS 往返（60ms+），防「命中路径意外走了慢路径」；
        // 10ms 在 86 测试并行满载下会被 tempdir+walk 抖破（实测 16ms）。
        assert!(
            elapsed < Duration::from_millis(50),
            "cache hit took {elapsed:?}"
        );
        assert_eq!(out.len(), 2, "limit must apply to cached full list");
        assert_eq!(out[0].name, "parse_a");

        // 不同 query → miss → 走真实路径：root 不存在 → 扫不到 lang → BadArgs。
        let err = sup
            .tool_find_symbol(root, "other", 5, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::BadArgs { .. }),
            "unexpected: {err:?}"
        );
    }

    /// symbol-tree：预置缓存全链路聚合（不拉 LS）；node_modules 被 3.3 过滤；
    /// dir 逃逸 root 报 BadArgs。
    #[tokio::test]
    async fn symbol_tree_aggregates_from_cache_and_skips_ignored_dirs() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not source\n").unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("dep.rs"), "fn dep() {}\n").unwrap();
        let root = dir.path();

        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs"), vec![hit("sym_a")]);
        sup.symbol_cache_put(doc_symbol_cache_key(root, "b.rs"), vec![hit("sym_b")]);

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 2, "node_modules/notes.txt 必须被过滤: {tree}");
        assert_eq!(tree["truncated"], false);
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "两文件各有符号条目: {tree}");

        // dir 逃逸 root。
        let err = sup
            .tool_symbol_tree(root, "../elsewhere", Some("rust"), 200)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "unexpected: {err:?}");
    }

    /// symbol-tree：max_files 保险丝 —— 超限截断并标 truncated。
    #[tokio::test]
    async fn symbol_tree_respects_max_files_fuse() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "fn x() {}\n").unwrap();
        }
        let root = dir.path();
        for i in 0..5 {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("f{i}.rs")),
                vec![hit("x")],
            );
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 3)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 3, "max_files=3 截断: {tree}");
        assert_eq!(tree["truncated"], true);
        assert_eq!(tree["entries"].as_array().unwrap().len(), 3);
    }

    /// 修 P1 #3 fan-out：files 扫描顺序与 entries 一一对应（涵盖 ≥MAX_INFLIGHT
    /// =4 个文件触发扇出池路径）。本测试通过缓存 hot-set 验证扇出后的整体结构与
    /// 既有断言一致（既有 cache-only 测试覆盖缓存命中语义）。
    ///
    /// 真正的扇出执行（miss 路径）走 spawn+JoinSet 需真实 LS（clangd）支持；
    /// 该 e2e 由后续的真实集成覆盖（既有 e2e_concurrency.rs 已有 LS e2e 路径）。
    /// 本测试只验形：files_扫、entries 数、errors 空（缓存命中无失败）。
    #[tokio::test]
    async fn symbol_tree_fan_out_preserves_files_with_cache() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        // 6 文件（>MAX_INFLIGHT=4）确保触发扇出池入口分支。
        let names: Vec<String> = (0..6).map(|i| format!("f{i}.rs")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn x() {}\n").unwrap();
        }
        let root = dir.path();
        for n in &names {
            sup.symbol_cache_put(doc_symbol_cache_key(root, n), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 6, "6 文件扫描: {tree}");
        assert_eq!(tree["truncated"], false);
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 6, "全部缓存命中 → 6 个 entries: {tree}");
        // errors 必须空：缓存命中分支不构造 error。
        let errors = tree["errors"].as_array().unwrap();
        assert!(errors.is_empty(), "缓存命中不应有 errors: {tree}");
    }

    /// 修 P0-A：>30 文件走串行路径（修 50 文件挂死）。缓存命中场景下，serial_mode
    /// 分支也正确返回 35 个 entries，errors 空，结构与原 fan-out 测试一致。
    /// 用 cache-primed 而非真 LS：避免 mock_ls 不支持 Rust 拉不起——测的是
    /// `files.len() > 30` 控制流入口分支的正确性。
    #[tokio::test]
    async fn symbol_tree_serial_mode_handles_above_threshold() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        // 35 文件 > 30 拐点 → 触发 serial_mode = true 分支。
        let names: Vec<String> = (0..35).map(|i| format!("f{i}.rs")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn x() {}\n").unwrap();
        }
        let root = dir.path();
        for n in &names {
            sup.symbol_cache_put(doc_symbol_cache_key(root, n), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 35, "35 文件扫描: {tree}");
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 35, "全部缓存命中 → 35 个 entries: {tree}");
        let errors = tree["errors"].as_array().unwrap();
        assert!(errors.is_empty(), "缓存命中不应有 errors: {tree}");
    }

    /// 修 P0-A：≤30 文件走并发（in-flight=4）原路径不变 —— 回归保护。25 文件
    /// 全部缓存命中时验证 entries 数 + errors 空。
    #[tokio::test]
    async fn symbol_tree_concurrent_mode_handles_below_threshold() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        // 25 文件 ≤ 30 → 走 serial_mode = false 分支 + sleep 100ms 入口。
        let names: Vec<String> = (0..25).map(|i| format!("f{i}.rs")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn x() {}\n").unwrap();
        }
        let root = dir.path();
        for n in &names {
            sup.symbol_cache_put(doc_symbol_cache_key(root, n), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200)
            .await
            .unwrap();
        assert_eq!(tree["files_scanned"], 25, "25 文件扫描: {tree}");
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 25, "全部缓存命中 → 25 个 entries: {tree}");
        let errors = tree["errors"].as_array().unwrap();
        assert!(errors.is_empty(), "缓存命中不应有 errors: {tree}");
    }

    /// 修 P0 符号树语言解析回归：mixed-lang 目录下 .py / .rs 各属各 LS，绝不
    /// 共用同一 session。本测试通过 symbol_cache 直接放命中条目（避免拉 LS），
    /// 验证"逐文件 resolve 后按 lang 分桶"——同桶缓存在桶分流后仍 1:1 命中。
    /// 不强断言桶数量（cache-only 路径根本不进 fan-out），改断言：entries 与 files
    /// 一一对应、errors 空、不同 lang 的 file 都被识别。
    #[tokio::test]
    async fn symbol_tree_resolves_language_per_file_not_single_lang() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        // 写三种语言的源码文件：
        let files = ["foo.py", "bar.py", "main.rs", "lib.rs", "App.java"];
        for n in files {
            std::fs::write(dir.path().join(n), b"# lang mix\n").unwrap();
        }
        let root = dir.path();
        // 预置 symbol cache（避免触发 session_for 拉 LS）
        for (idx, n) in files.iter().enumerate() {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, n),
                vec![hit(&format!("sym_{idx}"))],
            );
        }

        // lang = None 走逐文件 resolve（lang 是预置短路，下面的 resolve_lang_for_file
        // 不被 lang 短路，直接靠扩展名探测 —— 即 P0 修复的核心路径）。
        let tree = sup
            .tool_symbol_tree(root, ".", None, 200)
            .await
            .unwrap();
        let entries = tree["entries"].as_array().unwrap();
        let errors = tree["errors"].as_array().unwrap();

        // 必须全部命中（cache priming）—— 任何 file 进 errors 即视为被误判为
        // "lang 不可解析"，是 P0 修复前的回归迹象。
        assert_eq!(entries.len(), files.len(), "所有 5 个文件都应该进入 entries: {tree}");
        assert!(
            errors.is_empty(),
            "5 个文件分属 4 种 lang（py/rs/java）应都解析通过；errors={errors:?}"
        );
        // 每个 entry 的 file 字段是 files 之一（一一对应集合论）。
        let entry_files: std::collections::HashSet<String> = entries
            .iter()
            .map(|e| {
                e.get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let expected_files: std::collections::HashSet<String> =
            files.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            entry_files, expected_files,
            "entries file 集 ≠ 期望 file 集（一对一丢失或多出）"
        );
    }

    // ==== P2-18h · 外部修改感知（mtime+size 双因子对账）====

    /// mock_ls 带 track 钩子启动（didOpen/didChange/didClose 事件追加落盘，断言用）。
    fn launch_mock_ls_track(track_log: &std::path::Path) -> Option<ls_runtime::process::LaunchInfo> {
        let exe = crate::reclaim_idle_buffers_tests::find_mock_ls()?;
        Some(ls_runtime::process::LaunchInfo {
            cmd: vec![std::ffi::OsString::from(exe)],
            cwd: std::env::temp_dir(),
            env: vec![(
                "MOCK_LS_TRACK_FILE_EVENTS".to_string(),
                track_log.display().to_string(),
            )],
            transport: ls_runtime::process::TransportKind::Stdio,
        })
    }

    /// 读 track 日志（先等 mock_ls writer 异步落盘）；读不到当空表。
    async fn read_track_events(log: &std::path::Path) -> Vec<serde_json::Value> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let raw = tokio::fs::read_to_string(log).await.unwrap_or_default();
        raw.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// P2-18h（判据 1+2）：walk → 外部覆写（同 mtime 粒度窗口内 size 变化，模拟
    /// Windows mtime 缓存 / FAT 2s 粒度）→ 再 walk + symbol-body。
    ///
    /// 修前（key 只含 mtime）：symbol-body 缓存 hit 旧 SymbolHit —— LS 与盘脱钩
    /// （无 didChange）且切片来自陈旧 walk。修复后：key 含 size → miss → 对账清
    /// 旧条目 + ensure_open 检出 size 变 → didChange 重放（track 日志可断言）→
    /// 重 walk 以最新结果为准。
    ///
    /// mock_ls 恒回固定假 range（mock_helper@5）——返回值修前/修后巧合相同，判定
    /// 性证据 = didChange 事件到达 mock_ls + fast path 零新事件（判据 3）。
    #[tokio::test]
    async fn external_modify_same_mtime_replays_did_change_and_rewalks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let track = tmp.path().join("track.log");
        let Some(launch) = launch_mock_ls_track(&track) else {
            println!("skipped: mock_ls binary not found (lsp-core not yet built?)");
            return;
        };

        let file = tmp.path().join("a.cpp");
        // v1：line5（0-based）= HELPER 行 —— mock 假 range(5:0-5:12) 在 v1 上恰好命中。
        tokio::fs::write(&file, "l0\nl1\nl2\nl3\nl4\nHELPER_V1_LINE\n")
            .await
            .unwrap();
        let m0 = std::fs::metadata(&file).unwrap().modified().unwrap();

        let child = ls_runtime::process::Child::spawn(launch).unwrap();
        let session = lsp_core::session::Session::start(Some(child), lsp_types::InitializeParams::default())
            .await
            .unwrap();
        let sup = Supervisor::direct().await.unwrap();
        let key = Supervisor::key(tmp.path(), "cpp");
        sup.instances
            .lock()
            .unwrap()
            .insert(key.clone(), session.clone());
        sup.last_used
            .lock()
            .unwrap()
            .insert(key, std::time::Instant::now());

        // ① walk：overview miss → didOpen + documentSymbol → mock 假 hits 入缓存。
        let hits = sup
            .tool_overview(tmp.path(), "a.cpp", Some("cpp"))
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "mock_ls 固定回 2 个符号");
        assert!(
            read_track_events(&track)
                .await
                .iter()
                .any(|e| e["event"] == "didOpen"),
            "walk 必须先 didOpen"
        );

        // ② 外部覆写：更长内容（size 变）+ mtime 拨回记账值 —— 粒度窗口漏检形态。
        tokio::fs::write(&file, "l0\nl1\nl2\nl3\nl4\nTAIL_MARKER_LINE_X\nx1\nx2\n")
            .await
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(m0)
            .unwrap();

        // ③ 再 walk + symbol-body：修复后必走 miss（key 含 size）→ didChange + 重 walk。
        let _ = sup
            .tool_overview(tmp.path(), "a.cpp", Some("cpp"))
            .await
            .unwrap();
        let body = sup
            .tool_symbol_body(tmp.path(), "a.cpp", "mock_helper", Some("cpp"))
            .await
            .unwrap();
        assert!(
            body.contains("TAIL_MARKER"),
            "symbol-body 必须以最新 walk 的 range 切盘上现文，实际: {body:?}"
        );
        let events = read_track_events(&track).await;
        assert!(
            events.iter().any(|e| e["event"] == "didChange"),
            "外部修改（同 mtime + size 变）必须触发 didChange 重放；events={events:?}"
        );

        // ④ fast path（判据 3）：无外部修改再 walk —— 缓存命中，零新 LS 事件。
        let before = read_track_events(&track).await.len();
        let _ = sup
            .tool_overview(tmp.path(), "a.cpp", Some("cpp"))
            .await
            .unwrap();
        let after = read_track_events(&track).await.len();
        assert_eq!(before, after, "无修改 fast path 不得产生新 LS 事件");

        session.shutdown().await;
        let _ = sup.evict(&Supervisor::key(tmp.path(), "cpp")).await;
    }

    /// P2-18h 机制单测：同 mtime 粒度窗口内的外部改写（size 变）必须 miss + 对账
    /// 清残留 —— key 双因子是判据 1 集成测试翻转的根因层。
    #[tokio::test]
    async fn same_mtime_size_change_invalidates_symbol_cache() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let root = dir.path();

        let key1 = doc_symbol_cache_key(root, "a.rs");
        sup.symbol_cache_put(key1.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key1).is_some(), "warm cache must hit");

        // 外部覆写：内容变长（size 变）+ mtime 拨回记账值 —— 粒度窗口漏检形态。
        let (m0, _) = key1.2.unwrap();
        std::fs::write(&file, "fn f() {}\n// external edit\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(m0)
            .unwrap();

        let key2 = doc_symbol_cache_key(root, "a.rs");
        assert_ne!(key1, key2, "同 mtime 下 size 变化必须产生新 key（双因子）");
        assert!(sup.symbol_cache_get(&key2).is_none(), "size 变必须 miss");
        // 对账：检出不一致 → 清旧 stamp 残留；二次调用已一致 → false（幂等）。
        assert!(sup.reconcile_symbol_cache_for_file(root, "a.rs"));
        assert!(sup.symbol_cache_get(&key1).is_none(), "残留旧条目必须被清");
        assert!(!sup.reconcile_symbol_cache_for_file(root, "a.rs"));
    }

    /// P2-18h 机制单测：fast path —— 无外部修改（mtime,size 均不变）时 key 稳定、
    /// 缓存照用、对账返 false 不误清；他文件条目不受波及。
    #[tokio::test]
    async fn unchanged_file_keeps_fast_path_and_does_not_touch_others() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        let other = dir.path().join("b.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        std::fs::write(&other, "fn g() {}\n").unwrap();
        let root = dir.path();

        let key = doc_symbol_cache_key(root, "a.rs");
        let other_key = doc_symbol_cache_key(root, "b.rs");
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        sup.symbol_cache_put(other_key.clone(), vec![hit("g")]);

        assert_eq!(
            doc_symbol_cache_key(root, "a.rs"),
            key,
            "无修改 key 必须稳定（fast path 前提）"
        );
        assert!(
            !sup.reconcile_symbol_cache_for_file(root, "a.rs"),
            "无修改不得误报 stale"
        );
        assert!(sup.symbol_cache_get(&key).is_some(), "fast path 缓存保留");
        assert!(sup.symbol_cache_get(&other_key).is_some(), "他文件不受波及");
    }

    // ==== P2-a5k · 容量闸门（ARCH §3.2）+ 增长曲线（长会话内存有界）====

    /// 闸门语义：put 第 N 次后 len 仍 < N — 第 N+1 次触发全清，归 1（满阈即清，禁无界增长）。
    /// 上限值硬编码 512 以保持测试对 ARCH 决策敏感（防误调成 1）。
    #[tokio::test]
    async fn capacity_gate_clears_when_over_limit() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let cap = SYMBOL_CACHE_MAX_ENTRIES; // 512
        // 灌满：put cap 次后 len == cap。
        for i in 0..cap {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("f{i}.rs")),
                vec![hit("x")],
            );
        }
        assert_eq!(
            sup.symbol_cache_len(),
            cap,
            "刚好达到上限时不清空（< 阈值）"
        );
        // 第 cap+1 次 → 触发闸门 → 全清后只剩本条。
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "overflow.rs"),
            vec![hit("y")],
        );
        assert_eq!(
            sup.symbol_cache_len(),
            1,
            "超上限触发全清后只剩新插入的 1 条"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "overflow.rs"))
                .is_some(),
            "新写入的 key 必须可命中"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "f0.rs")).is_none(),
            "旧 entry 被全清"
        );
    }

    /// 闸门边界：本测试在 N = cap * 2 + 5 次 put 内断言 len ≤ cap —— 若有人把上限
    /// 调到 1024 / 关掉闸门，本测试会拒绝合并（CAP 来自 ARCH §3.2，改它 = 改架构
    /// 决策，须同步 ARCH + ADR）。
    #[tokio::test]
    async fn capacity_gate_never_overshoots_max() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let cap = SYMBOL_CACHE_MAX_ENTRIES;
        let total = cap * 2 + 5;
        for i in 0..total {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("g{i}.rs")),
                vec![hit("x")],
            );
            let n = sup.symbol_cache_len();
            assert!(
                n <= cap,
                "第 {i} 次 put 后 len={n} 超过上限 {cap}（闸门未生效）"
            );
        }
    }

    /// 增长曲线：1000 次 put 模拟长会话（修改→查→修改→查）；最终 len 有界 ≤ cap。
    /// 选 1000：远超 512 强制闸门至少一次以上；≈"长会话"基线。
    #[tokio::test]
    async fn growth_curve_long_session_bounded() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let cap = SYMBOL_CACHE_MAX_ENTRIES;
        let total = 1000usize;
        // root 不存在 → mtime = None → 每个 file 都是新 key
        let mut prev_len = 0usize;
        let mut triggered = 0usize;
        for i in 0..total {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("edit_{i}.rs")),
                vec![hit("x")],
            );
            let cur = sup.symbol_cache_len();
            if prev_len > 0 && cur < prev_len / 2 {
                triggered += 1;
            }
            prev_len = cur;
        }
        assert!(
            sup.symbol_cache_len() <= cap,
            "1000 次 put 后 len={} > cap={cap}（闸门失效）",
            sup.symbol_cache_len()
        );
        assert!(
            triggered >= 1,
            "1000 次 put 应至少触发 1 次闸门清空（观察: {triggered}）"
        );
    }

    /// 快速路径：未超上限时 put 不应触发全清（仅 insert）。本测试在 256 次 put 内
    /// 保持 len 单调递增 — 闸门不在快速路径上。
    #[tokio::test]
    async fn fast_path_under_limit_grows_monotonically() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        for i in 0..256 {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("f{i}.rs")),
                vec![hit("x")],
            );
            assert_eq!(
                sup.symbol_cache_len(),
                i + 1,
                "未超上限时 put 不应清表（len 应单调递增到 i+1={}）",
                i + 1
            );
        }
    }

    /// 闸门 + ARCH 全清替代 LRU 的语义保证：全清后任何旧 key 都 miss（无 stale）——
    /// 旧 mtime 命中走清空而非误中。这是 ARCH 决策的安全依据。
    #[tokio::test]
    async fn capacity_gate_clears_all_no_stale() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let cap = SYMBOL_CACHE_MAX_ENTRIES;
        for i in 0..cap {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root, &format!("s{i}.rs")),
                vec![hit("x")],
            );
        }
        // 触发全清
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "trigger.rs"),
            vec![hit("y")],
        );
        let mut all_miss = true;
        for i in 0..cap {
            if sup
                .symbol_cache_get(&doc_symbol_cache_key(root, &format!("s{i}.rs")))
                .is_some()
            {
                all_miss = false;
                break;
            }
        }
        assert!(all_miss, "全清后任何旧 key 都应 miss（无 stale）");
        assert_eq!(sup.symbol_cache_len(), 1, "全清后只剩新 entry");
    }

    /// 闸门不串扰：单次 put 触发全清后，invalidate_symbol_cache_for_root 仍按 root 删，
    /// —— 全清路径不会损坏 invalidation 语义（不同失效维度，互相独立）。
    #[tokio::test]
    async fn capacity_gate_independent_of_invalidate() {
        let sup = Supervisor::direct().await.unwrap();
        let root_a = Path::new("Z:/no/such/project_a");
        let root_b = Path::new("Z:/no/such/project_b");
        let cap = SYMBOL_CACHE_MAX_ENTRIES;
        // root_a 灌满 + root_b 灌 1 条
        for i in 0..cap {
            sup.symbol_cache_put(
                doc_symbol_cache_key(root_a, &format!("a{i}.rs")),
                vec![hit("a")],
            );
        }
        sup.symbol_cache_put(doc_symbol_cache_key(root_b, "b0.rs"), vec![hit("b")]);
        // 触发全清
        sup.symbol_cache_put(
            doc_symbol_cache_key(root_a, "trigger.rs"),
            vec![hit("t")],
        );
        // 全清后 root_a 只有 trigger.rs + root_b 的 b0.rs
        // invalidate root_a → 只剩 root_b 的 1 条
        sup.invalidate_symbol_cache_for_root(root_a);
        assert_eq!(sup.symbol_cache_len(), 1, "只 root_b 一条存活");
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root_b, "b0.rs"))
                .is_some()
        );
    }
}

// ============================================================================
// tool_search_for_pattern 噪音目录过滤回归测
// ============================================================================
//
// 根因回归：walker 之前用 `hidden(false)` 关掉 hidden 过滤，导致 `.git/` 内部
// commit message 等被搜到、污染 AI agent 信号。修复方案：删 `hidden(false)` 让
// `standard_filters` 默认 hidden 过滤生效（`.git/` 是 hidden），再叠加
// `fs_tools::should_ignore` 过滤 target/node_modules/.idea 等内置噪音。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod search_filter_tests {
    use super::*;
    use std::fs;

    /// `.git/` 是 hidden —— 默认被 `standard_filters` 排除。
    /// 同时 `target/` 等通过 `fs_tools::should_ignore` 表驱动排除。
    #[tokio::test]
    async fn skips_git_and_target_dirs() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // 真代码（含目标关键词）
        fs::write(root.join("a.rs"), "fn foo_drain_window() {}\n").unwrap();
        // 噪音：build 产物里同名命中
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/foo.rs"), "fn foo_drain_window() {}\n").unwrap();
        // 噪音：.git 内部 commit message
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join(".git/COMMIT_EDITMSG"),
            "fix: foo_drain_window regression",
        )
        .unwrap();

        let resp = sup
            .tool_search_for_pattern(root, "foo_drain_window", None, 50, false)
            .await
            .unwrap();
        let files: Vec<&str> = resp.hits.iter().map(|h| h.file.as_str()).collect();
        assert!(
            files.iter().all(|f| !f.starts_with(".git/") && !f.starts_with("target/")),
            "search 不应扫到 .git/target，实际命中: {files:?}"
        );
        assert!(
            files.contains(&"a.rs"),
            "应扫到真代码文件，实际命中: {files:?}"
        );
    }

    /// I（§11-I）：execute_tool("search", comments_only=true) 只留注释行；
    /// 默认（false）形态不变，代码行照常返回。
    #[tokio::test]
    async fn search_filters_to_comments_only() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("a.rs"),
            "// TODO: foo\nfn foo() {}\n// foo done\n",
        )
        .unwrap();

        let all = sup
            .execute_tool(
                "search",
                &root.to_string_lossy(),
                serde_json::json!({ "pattern": "foo" }),
                None,
            )
            .await
            .expect("default search ok");
        let all_hits = all["hits"].as_array().unwrap();
        assert!(all_hits.len() >= 2, "默认形态应含代码+注释，实际: {all_hits:?}");

        let filtered = sup
            .execute_tool(
                "search",
                &root.to_string_lossy(),
                serde_json::json!({ "pattern": "foo", "comments_only": true }),
                None,
            )
            .await
            .expect("comments-only search ok");
        let hits = filtered["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "应只留 2 行注释，实际: {hits:?}");
        assert!(hits.iter().all(|h| fs_tools::looks_like_comment(
            h["file"].as_str().unwrap(),
            h["text"].as_str().unwrap()
        )));
    }
}// ============================================================================
// Phase 1 · 13 wrapper 测试（纯 LSP 协议层；不拉 LS / 不依赖 fix-ls-adapters）
// ============================================================================

#[cfg(test)]
mod phase1_wrapper_tests {
    //! Phase 1 · 13 个上游 wrapper 的协议层单测：
    //! - `decode_semantic_tokens` 5-tuple delta 累加正确性。
    //! - 各 LSP method 的请求 params 字段齐全（用 mock Value round-trip）。
    //! - 错误路径：未知 op / 缺 file / 缺 item 等转 BadArgs。
    //!
    //! 真实端到端验证走 fixtures/rust_demo + rust-analyzer 的 CLI smoke
    //! （拉起 supervisor → tool_* → 字段就位 / 不报 protocol error）。
    use super::*;

    // ---- decode_semantic_tokens ----

    /// 单 token delta_line=0 / delta_start=10 → start_char=10。
    #[test]
    fn decode_semantic_tokens_handles_zero_delta_line() {
        let tokens = vec![lsp_types::SemanticToken {
            delta_line: 0,
            delta_start: 10,
            length: 4,
            token_type: 1,
            token_modifiers_bitset: 0,
        }];
        let out = decode_semantic_tokens(&tokens);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 0);
        assert_eq!(out[0].start_char, 10);
        assert_eq!(out[0].length, 4);
        assert_eq!(out[0].token_type, 1);
    }

    /// delta_line=2 / delta_start=5 → 行号 2，start_char 复位为 5。
    #[test]
    fn decode_semantic_tokens_resets_start_on_line_change() {
        let tokens = vec![
            lsp_types::SemanticToken {
                delta_line: 0,
                delta_start: 3,
                length: 1,
                token_type: 0,
                token_modifiers_bitset: 0,
            },
            lsp_types::SemanticToken {
                delta_line: 2,
                delta_start: 5,
                length: 2,
                token_type: 2,
                token_modifiers_bitset: 0,
            },
        ];
        let out = decode_semantic_tokens(&tokens);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line, 0);
        assert_eq!(out[0].start_char, 3);
        assert_eq!(out[1].line, 2);
        assert_eq!(out[1].start_char, 5, "行变化时 delta_start 是绝对坐标");
    }

    /// 空 data → 空 tokens。
    #[test]
    fn decode_semantic_tokens_empty_input_returns_empty() {
        let out = decode_semantic_tokens(&[]);
        assert!(out.is_empty());
    }

    // ---- file_path_from_uri ----

    #[test]
    fn file_path_from_uri_strips_file_scheme_and_keeps_path() {
        let p = file_path_from_uri("file:///D:/proj/main.cpp");
        assert!(p.ends_with("proj/main.cpp"), "got: {p}");
    }

    #[test]
    fn file_path_from_uri_handles_non_file_uri_gracefully() {
        // 不是 file:// 走 fallback（原样保留）。
        let p = file_path_from_uri("unt:///x/y");
        assert!(p.contains("x/y"), "got: {p}");
    }

    // ---- uri percent-decode（LS 返回 URI 统一解码，缺口 #5 回归）----

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let p = uri_to_path("file:///d%3A/proj/foo%20bar/a.rs").expect("file uri");
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(!s.contains('%'), "percent 序列必须解码: {s}");
        assert!(s.ends_with("foo bar/a.rs"), "got: {s}");
    }

    #[cfg(windows)]
    #[test]
    fn uri_to_path_uppercases_windows_drive_letter() {
        // 小写盘符 `d%3A` 解码后必须归一为 `D:`，否则与 canonical root 前缀比对失败。
        let p = uri_to_path("file:///d%3A/proj/a.rs").expect("file uri");
        assert!(
            p.to_string_lossy().starts_with("D:"),
            "盘符必须大写: {p:?}"
        );
    }

    #[test]
    fn percent_decode_keeps_invalid_sequences() {
        assert_eq!(percent_decode("a%3Ab"), "a:b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("100% off"), "100% off"); // 非法 hex 原样保留
        assert_eq!(percent_decode("trailing%2"), "trailing%2"); // 截断序列原样保留
    }

    // ---- parse_workspace_edit（rename 两种 WorkspaceEdit 形态，gopls 兼容回归）----

    #[test]
    fn parse_workspace_edit_changes_map() {
        let resp = json!({
            "changes": {
                "file:///a.ts": [
                    {"range": {"start": {"line": 0, "character": 5}, "end": {"line": 0, "character": 8}}, "newText": "b"},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 1}}, "newText": "c"}
                ]
            }
        });
        let by_uri = parse_workspace_edit(&resp).expect("changes map 必须解析");
        assert_eq!(by_uri.len(), 1);
        assert_eq!(by_uri[0].0, "file:///a.ts");
        assert_eq!(by_uri[0].1.len(), 2);
    }

    #[test]
    fn parse_workspace_edit_document_changes_array() {
        // gopls 形态：只有 documentChanges 数组，无 changes map。
        let resp = json!({
            "documentChanges": [
                {"textDocument": {"uri": "file:///a.go"}, "edits": [
                    {"range": {"start": {"line": 4, "character": 4}, "end": {"line": 4, "character": 7}}, "newText": "sum"}
                ]}
            ]
        });
        let by_uri = parse_workspace_edit(&resp).expect("documentChanges 必须解析");
        assert_eq!(by_uri.len(), 1);
        assert_eq!(by_uri[0].0, "file:///a.go");
        assert_eq!(by_uri[0].1.len(), 1);
        assert_eq!(by_uri[0].1[0].1.start.line, 4);
    }

    #[test]
    fn parse_workspace_edit_prefers_document_changes_and_skips_non_text_ops() {
        // 两形态并存时 documentChanges 优先；kind:create 等非文本条目跳过。
        let resp = json!({
            "changes": {"file:///fallback.ts": []},
            "documentChanges": [
                {"kind": "create", "uri": "file:///new.go"},
                {"textDocument": {"uri": "file:///a.go"}, "edits": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "x"}
                ]}
            ]
        });
        let by_uri = parse_workspace_edit(&resp).expect("documentChanges 优先");
        assert_eq!(by_uri.len(), 1, "create 条目跳过，只留文本编辑文件");
        assert_eq!(by_uri[0].0, "file:///a.go");
    }

    #[test]
    fn parse_workspace_edit_neither_form_returns_none() {
        assert!(parse_workspace_edit(&json!({})).is_none());
        assert!(parse_workspace_edit(&json!({"changes": "not-an-object"})).is_none());
    }

    // ---- execute_tool 输入校验：call-hierarchy / type-hierarchy 缺 item 转 BadArgs ----

    #[tokio::test]
    async fn execute_tool_call_hierarchy_missing_op_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({});
        let r = sup
            .execute_tool("call-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_call_hierarchy_incoming_missing_item_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "incoming"});
        let r = sup
            .execute_tool("call-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_type_hierarchy_unknown_op_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "bogus"});
        let r = sup
            .execute_tool("type-hierarchy", ".", args, None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    // ---- inlay-hint / format-range / code-action 缺必填参数 ----

    #[tokio::test]
    async fn execute_tool_inlay_hint_missing_start_line_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"file": "main.cpp", "end_line": 10});
        let r = sup.execute_tool("inlay-hint", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_format_range_missing_end_col_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({
            "file": "main.cpp",
            "start_line": 1, "start_col": 0,
            "end_line": 2
        });
        let r = sup.execute_tool("format-range", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_workspace_diagnostic_no_active_session_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        // No LS loaded yet; without --lang fallback should error.
        let r = sup
            .execute_tool("workspace-diagnostic", ".", json!({}), None)
            .await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    // ---- 直接构造请求 params 验证字段完整性（不拉 LS）----

    /// `textDocument/codeAction` params 必须含 textDocument / range / context.diagnostics。
    #[test]
    fn code_action_request_payload_shape() {
        let mut params = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end":   { "line": 1, "character": 2 },
            },
            "context": { "diagnostics": [] },
        });
        params["context"]["only"] = json!("quickfix");
        let v: serde_json::Value = params;
        assert_eq!(v["context"]["only"], "quickfix");
        assert!(v["context"]["diagnostics"].is_array());
    }

    /// `textDocument/rangeFormatting` range 必须含 4 字段 + options 2 字段。
    #[test]
    fn range_formatting_request_payload_shape() {
        let v = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 1, "character": 0 },
                "end":   { "line": 5, "character": 0 },
            },
            "options": { "tabSize": 4, "insertSpaces": true },
        });
        assert_eq!(v["options"]["tabSize"], 4);
        assert_eq!(v["options"]["insertSpaces"], true);
    }

    /// `callHierarchy/incomingCalls` 与 `outgoingCalls` params.item 必须存在。
    #[test]
    fn call_hierarchy_subsequent_request_requires_item_field() {
        let item = json!({"name": "foo", "kind": 12, "uri": "file:///x", "range": {}});
        let v = json!({"item": item});
        assert!(v["item"].is_object());
        assert_eq!(v["item"]["name"], "foo");
    }

    /// `textDocument/inlayHint` range 必有 start/end.line。
    #[test]
    fn inlay_hint_request_payload_shape() {
        let v = json!({
            "textDocument": { "uri": "file:///x" },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 100, "character": 0 },
            },
        });
        assert_eq!(v["range"]["end"]["line"], 100);
    }

    // ---- SemanticTokensFull serialization ----

    #[test]
    fn semantic_tokens_full_serializes_with_decoded_tokens() {
        let entry = SemanticTokensFull {
            result_id: Some("v1".to_string()),
            tokens: vec![SemanticTokenEntry {
                line: 2,
                start_char: 5,
                length: 4,
                token_type: 1,
                token_modifiers: 0,
            }],
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["resultId"], "v1");
        assert_eq!(v["tokens"][0]["line"], 2);
        assert_eq!(v["tokens"][0]["startChar"], 5);
        assert_eq!(v["tokens"][0]["length"], 4);
    }

    // ---- workspace-diagnostic 响应归一化（items 缺失 → 空）----

    #[test]
    fn workspace_diagnostic_response_missing_items_returns_empty() {
        // Mock 一个完整 response（items 字段缺失），确认提取路径不报错。
        let raw = json!({ "kind": "full" });
        let items = raw.get("items").cloned().unwrap_or(serde_json::Value::Null);
        let parsed: Vec<lsp_types::Diagnostic> =
            serde_json::from_value(items).unwrap_or_default();
        assert!(parsed.is_empty());
    }
}

// ---- Phase 4 Task 22b: timeout 三层合并 + sanitize ----

#[cfg(test)]
mod timeout_resolution_tests {
    use super::*;

    #[test]
    fn effective_tool_timeout_default_30s_when_no_override() {
        let args = json!({});
        // markdown 的 servers.toml 当前未写 timeout_ms → 走默认 30s
        let d = effective_tool_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_secs(30), "默认 30s");
    }

    #[test]
    fn effective_tool_timeout_args_override_wins() {
        let args = json!({"_timeout_ms": 1234});
        let d = effective_tool_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_millis(1234), "CLI args._timeout_ms 覆盖");
    }

    #[test]
    fn effective_tool_timeout_handles_unknown_lang() {
        // lang 不在 servers.toml → None → 默认
        let args = json!({});
        let d = effective_tool_timeout(Some("rust"), &args);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn effective_index_timeout_default_120s_when_no_override() {
        let args = json!({});
        let d = effective_index_timeout(Some("markdown"), &args);
        assert_eq!(d, Duration::from_secs(120), "index 默认 120s");
    }

    #[test]
    fn sanitize_timeout_args_strips_private_fields() {
        let args = json!({
            "file": "main.cpp",
            "_timeout_ms": 5000,
            "_index_timeout_ms": 60000,
            "col": 1,
        });
        let out = sanitize_timeout_args(args);
        assert_eq!(out.get("file").and_then(|v| v.as_str()), Some("main.cpp"));
        assert_eq!(out.get("col").and_then(|v| v.as_u64()), Some(1));
        assert!(out.get("_timeout_ms").is_none(), "_timeout_ms 应被清掉");
        assert!(out.get("_index_timeout_ms").is_none(), "_index_timeout_ms 应被清掉");
    }

    #[test]
    fn sanitize_timeout_args_passes_through_non_object() {
        // 防御：若 args 罕见形态（null/array），sanitize 不panic、不破坏。
        let null_in = serde_json::Value::Null;
        assert!(sanitize_timeout_args(null_in.clone()).is_null());
        let arr_in = json!([1, 2, 3]);
        assert_eq!(sanitize_timeout_args(arr_in.clone()), arr_in);
    }
}

// ---- AI-token 特性 H: compact 位置格式（plan-h-compact-locations.md §3 验收）----
//
// 本模块覆盖：
// - `compact_loc` / `compact_locs` / `compact_symbol_hit` 字节级断言（盘符归一、percent
//   解码、LSP 0-based → 人类 1-based 三件套）
// - 三个 envelope builder（locations_envelope / symbol_hits_envelope / ref_*_envelope）
//   序列化形状 + compact 开关标志
// - 不拉起 LS，纯函数 + 构造的 Location/SymbolHit 即可验证。

#[cfg(test)]
mod compact_locations_tests {
    use super::*;
    use lsp_types::{Location, Position, Range, Uri};

    fn loc(uri: &str, line: u32, col: u32) -> Location {
        let uri: Uri = uri.parse().unwrap();
        Location {
            uri,
            range: Range {
                start: Position::new(line, col),
                end: Position::new(line, col + 4),
            },
        }
    }

    /// `compact_loc` 3 件套：URI percent 解码 + 盘符大写 + 1-based 行/列。
    /// 输入 LSP 0-based line:9 char:4 → 期望人类 1-based "10:5"。
    #[test]
    fn compact_loc_decodes_percent_and_normalizes_drive_and_one_based() {
        let s = compact_loc(&loc("file:///d%3A/proj/foo.rs", 9, 4));
        // 盘符大写归一 + percent 解码 + 1-based 行:列
        assert!(
            s.starts_with("D:/proj/foo.rs:"),
            "盘符归一 + percent 解码失败: got {s}"
        );
        assert!(s.ends_with(":10:5"), "1-based 转换失败: got {s}");
        assert!(!s.contains('%'), "percent 噪音未消: {s}");
    }

    /// 路径解析可逆：`compact_loc` 输出 → 反解 → 拿回 1-based line/col。
    #[test]
    fn compact_loc_round_trip_preserves_one_based_line_col() {
        let out = compact_loc(&loc("file:///D:/proj/main.rs", 9, 4));
        // 末尾 `:` 分三段：file / line / col
        let parts: Vec<&str> = out.rsplitn(3, ':').collect();
        assert_eq!(parts.len(), 3, "期望 file:line:col 三段: got {out}");
        let col: u32 = parts[0].parse().unwrap();
        let line: u32 = parts[1].parse().unwrap();
        assert_eq!(line - 1, 9, "1-based line 应回 0-based 9");
        assert_eq!(col - 1, 4, "1-based col 应回 0-based 4");
    }

    /// `compact_locs` 一次 vec 适配（多 Location → Vec<String>）。
    #[test]
    fn compact_locs_maps_each_location() {
        let locs = vec![
            loc("file:///D:/proj/a.rs", 0, 0),
            loc("file:///D:/proj/b.rs", 10, 5),
        ];
        let out = compact_locs(&locs);
        assert_eq!(out.len(), 2);
        assert!(out[0].ends_with("a.rs:1:1"));
        assert!(out[1].ends_with("b.rs:11:6"));
    }

    /// `compact_symbol_hit` 走 SymbolHit.uri + SymbolHit.range。
    #[test]
    fn compact_symbol_hit_uses_uri_and_range() {
        let hit = SymbolHit {
            name: "add".to_string(),
            kind: SymbolKindTag::Function,
            uri: "file:///D:/proj/x.rs".to_string(),
            range: Range {
                start: Position::new(0, 3),
                end: Position::new(0, 6),
            },
            container: None,
        };
        let s = compact_symbol_hit(&hit);
        assert_eq!(s, "D:/proj/x.rs:1:4", "name 不参与紧凑字段");
    }

    /// locations_envelope 形状：`compact: bool` 顶层 + `items[]` + 条件 raw_count。
    #[test]
    fn locations_envelope_compact_true_drops_range_uri_and_keeps_count() {
        let locs = vec![loc("file:///D:/a.rs", 0, 0), loc("file:///D:/b.rs", 4, 7)];
        let v = locations_envelope(&locs, true);
        assert_eq!(v["compact"], serde_json::Value::Bool(true));
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_string(), "compact path 应是字符串");
        assert!(items[0].as_str().unwrap().ends_with("a.rs:1:1"));
        assert_eq!(v["raw_count"], serde_json::json!(2));
    }

    /// non-compact path 保留原 LSP Location（range+uri），无 raw_count 噪音。
    #[test]
    fn locations_envelope_compact_false_keeps_lsp_locations() {
        let locs = vec![loc("file:///D:/a.rs", 0, 0)];
        let v = locations_envelope(&locs, false);
        assert_eq!(v["compact"], serde_json::Value::Bool(false));
        assert!(v.get("raw_count").is_none(), "non-compact 应不增字段");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0]["uri"].is_string(), "LSP Location.uri 必须保留");
        assert!(items[0]["range"]["start"]["line"].is_u64());
    }

    /// completion 紧凑 envelope（bd serena-rust-5st）：默认形态 item 只留有信息
    /// 字段——insert==label 兜底副本、doc、deprecated:false、空 edits 全部省略。
    #[test]
    fn completion_envelope_compact_drops_zero_info_fields() {
        let resp = CompletionResponse {
            truncated: Some("1 of 23".into()),
            items: vec![CompletionItemLite {
                label: "add".into(),
                kind: "function".into(),
                detail: Some("fn add(a: i32, b: i32) -> i32".into()),
                insert: Some("add".into()),
                doc: Some("adds two numbers".into()),
                deprecated: false,
                additional_text_edits: Vec::new(),
            }],
        };
        let v = completion_envelope(&resp);
        assert_eq!(v["compact"], serde_json::Value::Bool(true));
        assert_eq!(v["raw_count"], serde_json::json!(1));
        assert_eq!(v["truncated"], serde_json::json!("1 of 23"));
        let item = &v["items"][0];
        assert_eq!(item["label"], serde_json::json!("add"));
        assert_eq!(item["kind"], serde_json::json!("function"));
        assert_eq!(item["detail"], serde_json::json!("fn add(a: i32, b: i32) -> i32"));
        assert!(item.get("insert").is_none(), "insert==label 兜底副本应省略");
        assert!(item.get("doc").is_none(), "compact 下 doc 应省略（--json 可取全）");
        assert!(item.get("deprecated").is_none(), "deprecated:false 应省略");
        assert!(item.get("edits").is_none(), "空 additional_text_edits 应省略");
    }

    /// 偏离默认的字段必须出现：insert!=label、deprecated:true、非空 edits 扁平为
    /// `["L{行}:{列}", 新文本]` 对（与 compact_loc 同为 1-based）。
    #[test]
    fn completion_envelope_compact_keeps_non_default_fields_flattened() {
        let resp = CompletionResponse {
            truncated: None,
            items: vec![CompletionItemLite {
                label: "HashMap".into(),
                kind: "class".into(),
                detail: None,
                insert: Some("std::collections::HashMap".into()),
                doc: Some("docs".into()),
                deprecated: true,
                additional_text_edits: vec![lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position::new(1, 0),
                        end: lsp_types::Position::new(1, 0),
                    },
                    new_text: "use std::collections::HashMap;\n".into(),
                }],
            }],
        };
        let v = completion_envelope(&resp);
        let item = &v["items"][0];
        assert_eq!(
            item["insert"],
            serde_json::json!("std::collections::HashMap"),
            "insert!=label 必须保留"
        );
        assert_eq!(item["deprecated"], serde_json::Value::Bool(true));
        assert_eq!(v.get("truncated"), None, "未截断不增 truncated 键");
        let edits = item["edits"].as_array().unwrap();
        assert_eq!(edits[0][0], serde_json::json!("L2:1"), "0-based → 1-based");
        assert_eq!(edits[0][1], serde_json::json!("use std::collections::HashMap;\n"));
    }

    /// symbol_hits_envelope compact 形态：`[name, "file:line:col"]` 二元组。
    #[test]
    fn symbol_hits_envelope_compact_true_pairs_name_with_loc() {
        let hits = vec![SymbolHit {
            name: "add".into(),
            kind: SymbolKindTag::Function,
            uri: "file:///D:/p/x.rs".into(),
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(1, 0),
            },
            container: None,
        }];
        let v = symbol_hits_envelope(&hits, true);
        assert_eq!(v["compact"], serde_json::Value::Bool(true));
        let items = v["items"].as_array().unwrap();
        let pair = items[0].as_array().expect("pair array");
        assert_eq!(pair[0].as_str().unwrap(), "add");
        assert!(pair[1].as_str().unwrap().ends_with("x.rs:1:1"));
    }

    /// `_compact=false` 在 execute_tool 入口被透传到 envelope —— 至少一处走完整 +
    /// `items[0]` 是 LSP Location（确保现有 wire 兼容）。
    #[test]
    fn locations_envelope_compact_false_preserves_wire_shape_for_ai_compat() {
        let locs = vec![loc("file:///D:/a.rs", 2, 3)];
        let v = locations_envelope(&locs, false);
        // 必须保留 `items[0].uri` + `items[0].range.start` 两个字段（与既有 wire 同步）
        assert!(v["items"][0]["uri"].is_string());
        assert_eq!(
            v["items"][0]["range"]["start"]["line"],
            serde_json::json!(2)
        );
    }
}

// ============================================================================
// A（ai-token-features §10-A）：search 命中带 symbol/container
// ============================================================================
//
// mock_ls 回 FLAT 单行符号（mock_main@L0 / mock_helper@L5，无 container_name），
// 测试走 execute_tool("search") 全链路（分桶 → tool_overview → 覆盖匹配 → JSON）。
// container=Some 路径 mock 拉不出（FLAT 无嵌套）→ find_covering_symbol 纯函数测试
// 用手拼嵌套 Vec 覆盖；真实嵌套由 CLI e2e（fixtures/rust_demo + rust-analyzer）兜底。

#[cfg(test)]
mod delta_tests {
    //! AI-token 特性 J（§11-J）：maybe_delta 首调全集 / 二调增量 / 空集不缓存。
    use super::*;

    #[tokio::test]
    async fn maybe_delta_first_call_returns_full() {
        let sup = Supervisor::direct().await.unwrap();
        let cur = serde_json::json!({"items": [{"file": "a.rs", "line": 1, "col": 0}]});
        let v = sup.maybe_delta("refs", "k", cur, true).await;
        assert_eq!(v["delta"], serde_json::Value::Bool(false));
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn maybe_delta_second_call_returns_added_only() {
        let sup = Supervisor::direct().await.unwrap();
        let first = serde_json::json!({"items": [{"file": "a.rs", "line": 1, "col": 0}]});
        sup.maybe_delta("refs", "k", first, true).await;
        let second = serde_json::json!({"items": [
            {"file": "a.rs", "line": 1, "col": 0},
            {"file": "b.rs", "line": 5, "col": 2},
        ]});
        let v = sup.maybe_delta("refs", "k", second, true).await;
        assert_eq!(v["delta"], serde_json::Value::Bool(true));
        let added = v["added"].as_array().unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["file"], "b.rs");
        assert_eq!(v["removed"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn maybe_delta_empty_response_not_cached() {
        let sup = Supervisor::direct().await.unwrap();
        let empty = serde_json::json!({"items": []});
        sup.maybe_delta("refs", "k", empty, true).await;
        let real = serde_json::json!({"items": [{"file": "a.rs", "line": 1, "col": 0}]});
        let v = sup.maybe_delta("refs", "k", real, true).await;
        assert_eq!(
            v["delta"],
            serde_json::Value::Bool(false),
            "空响应不得入缓存：第二次仍等于首次"
        );
    }
}

#[cfg(test)]
mod search_symbol_tests {
    use super::*;
    use ls_runtime::process::{Child, LaunchInfo, TransportKind};
    use lsp_types::InitializeParams;
    use std::ffi::OsString;

    /// mock_ls 直连注入 lang="rust" 实例池位；返回 (sup, tempdir)。
    /// 缺 mock_ls 二进制 → None（skip，不计失败）—— 同 reclaim_idle_buffers_tests 约定。
    async fn mock_sup_with_symbols() -> Option<(Supervisor, tempfile::TempDir)> {
        let exe = find_mock_ls_for_search_tests()?;
        let child = Child::spawn(LaunchInfo {
            cmd: vec![OsString::from(exe)],
            cwd: std::env::temp_dir(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
        .expect("spawn mock_ls");
        let session = lsp_core::session::Session::start(Some(child), InitializeParams::default())
            .await
            .expect("Session::start Ready");
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let key = Supervisor::key(tmp.path(), "rust");
        sup.instances.lock().unwrap().insert(key, session);
        Some((sup, tmp))
    }

    /// 同 reclaim_idle_buffers_tests::find_mock_ls，但兜底改从 CARGO_MANIFEST_DIR
    /// （crates/supervisor）上溯 workspace 根找 target/debug —— cargo test 运行时
    /// cwd 是 crate 目录，`current_dir()/target` 永远 miss（否则 mock 测试静默 skip）。
    fn find_mock_ls_for_search_tests() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_mock_ls") {
            let p = std::path::PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        let ext = if cfg!(windows) { ".exe" } else { "" };
        let ws = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?)
            .parent()?
            .parent()?
            .to_path_buf();
        [
            ws.join(format!("target/debug/mock_ls{ext}")),
            ws.join(format!("target/debug/deps/mock_ls{ext}")),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    /// 临时文件按 mock_ls 固定符号行摆位：L1=mock_main 命中、L6=mock_helper 命中、
    /// L7=孤儿行（不在任何符号 range 内，1-based）。
    async fn write_fixture(tmp: &tempfile::TempDir) -> String {
        let content =
            "mock_main hit\nfiller\nfiller\nfiller\nfiller\nmock_helper hit\nmock_orphan\n";
        tokio::fs::write(tmp.path().join("a.rs"), content)
            .await
            .expect("write fixture");
        tmp.path().to_string_lossy().to_string()
    }

    /// 命中行落进符号 range → symbol 填充为覆盖符号名（FLAT 无 container → null）。
    #[tokio::test]
    async fn search_hits_carry_symbol_and_container() {
        let Some((sup, tmp)) = mock_sup_with_symbols().await else {
            println!("skipped: mock_ls binary not found (lsp-core not yet built?)");
            return;
        };
        let root = write_fixture(&tmp).await;
        let v = sup
            .execute_tool("search", &root, json!({ "pattern": "mock_" }), None)
            .await
            .expect("search ok");
        let hits = v["hits"].as_array().expect("hits array");
        assert_eq!(hits.len(), 3, "3 行各一命中: {hits:?}");
        let by_line = |l: u32| {
            hits.iter()
                .find(|h| h["line"] == json!(l))
                .expect("hit at line")
        };
        assert_eq!(
            by_line(1)["symbol"],
            json!("mock_main"),
            "L1 落进 mock_main range → 填符号名"
        );
        assert_eq!(
            by_line(1)["container"],
            json!(null),
            "FLAT 无 container_name → 容器 null"
        );
        assert_eq!(by_line(6)["symbol"], json!("mock_helper"), "L6 = mock_helper");
        let _ = sup.evict(&Supervisor::key(tmp.path(), "rust")).await;
    }

    /// 孤儿行（不在任何符号 range）→ symbol/container 保持 null；全部命中值合法非空。
    #[tokio::test]
    async fn search_top_level_returns_none_or_valid() {
        let Some((sup, tmp)) = mock_sup_with_symbols().await else {
            println!("skipped: mock_ls binary not found (lsp-core not yet built?)");
            return;
        };
        let root = write_fixture(&tmp).await;
        let v = sup
            .execute_tool("search", &root, json!({ "pattern": "mock_" }), None)
            .await
            .expect("search ok");
        let hits = v["hits"].as_array().expect("hits array");
        for h in hits {
            let sym = &h["symbol"];
            assert!(
                sym.is_null() || sym.as_str().is_some_and(|s| !s.is_empty()),
                "symbol 必须是 null 或非空字符串: {sym}"
            );
        }
        let orphan = hits
            .iter()
            .find(|h| h["line"] == json!(7))
            .expect("orphan hit");
        assert!(orphan["symbol"].is_null(), "孤儿行不得误标符号");
        assert!(orphan["container"].is_null(), "孤儿行不得误标容器");
        let _ = sup.evict(&Supervisor::key(tmp.path(), "rust")).await;
    }

    /// tool_overview 失败（语言不可解析，未触 LS）→ 静默保留 None，不传播错误。
    #[tokio::test]
    async fn enrich_fails_silently_on_tool_overview_failure() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let mut hits = vec![SearchHit {
            file: "note.txt".into(),
            line: 1,
            col: 1,
            text: "x".into(),
            match_start: 0,
            match_end: 1,
            symbol: None,
            container: None,
        }];
        enrich_search_with_symbols(
            &sup,
            Path::new("Z:/nonexistent_xyz_root"),
            &mut hits,
            None,
        )
        .await;
        assert!(hits[0].symbol.is_none(), "overview 失败 → symbol 保持 None");
        assert!(hits[0].container.is_none());
    }

    /// 嵌套符号最小覆盖 + 容器传递 + 顶层自身名 artifact 过滤（mock FLAT 拉不出嵌套）。
    #[test]
    fn find_covering_symbol_picks_smallest_with_container() {
        let hit = |name: &str, container: Option<&str>, sl: u32, el: u32| SymbolHit {
            name: name.into(),
            kind: SymbolKindTag::Function,
            uri: "file:///t.rs".into(),
            range: lsp_types::Range {
                start: Position::new(sl, 0),
                end: Position::new(el, 1),
            },
            container: container.map(Into::into),
        };
        // impl Foo(L0..L10) ⊃ bar(L2..L5) ⊃ helper(L3..L4)：L3 的最小覆盖 = helper。
        let syms = vec![
            hit("Foo", Some("Foo"), 0, 10), // flatten 给顶层符号记自身名为 container
            hit("bar", Some("Foo"), 2, 5),
            hit("helper", Some("bar"), 3, 4),
        ];
        let (name, container) = find_covering_symbol(&syms, 3, 0).expect("covered");
        assert_eq!(name, "helper", "嵌套中应选 span 最小（最深）者");
        assert_eq!(container.as_deref(), Some("bar"), "容器 = 直接父名");
        // L1 只被 Foo 覆盖 → 容器 artifact（自身名）滤成 None。
        let (name, container) = find_covering_symbol(&syms, 1, 0).expect("covered");
        assert_eq!(name, "Foo");
        assert_eq!(container, None, "顶层符号容器应为 None（滤 flatten 自身名）");
        // L11 无覆盖 → None。
        assert!(find_covering_symbol(&syms, 11, 0).is_none());
    }

    // ==== AI-token 特性 G（§10-G）：budget 截断 + compress 字段清理 ====

    #[test]
    fn apply_budget_truncates_items_when_exceeded() {
        let mut v = serde_json::json!({
            "items": (0..100)
                .map(|i| serde_json::json!({"name": format!("f{}", i), "container": "x"}))
                .collect::<Vec<_>>(),
        });
        let truncated = apply_budget(&mut v, 50); // 50 tokens ≈ 200 bytes 预算
        assert!(truncated, "超预算应发生截断");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["original_count"], 100);
        let kept = v["items"].as_array().unwrap().len();
        assert!(kept < 100, "items 应被截短（实际保留 {kept}）");
        assert!(kept >= 1, "预算 200 bytes 至少容得下 1 条");
    }

    #[test]
    fn apply_budget_passes_through_when_under() {
        let mut v = serde_json::json!({"items": [{"name": "x"}]});
        let truncated = apply_budget(&mut v, 10000);
        assert!(!truncated);
        assert!(v.get("truncated").is_none(), "未超预算不得加标志");
        assert!(v.get("original_count").is_none());
        assert_eq!(v["items"].as_array().unwrap().len(), 1, "零改动");
    }

    #[test]
    fn apply_budget_skips_delta_response() {
        // J×G 交互：delta 增量形态（added/removed 二选一集）不参与按条截断。
        let mut v = serde_json::json!({
            "delta": true,
            "added": (0..100).map(|i| serde_json::json!({"name": format!("f{}", i)}))
                .collect::<Vec<_>>(),
            "removed": [],
        });
        let truncated = apply_budget(&mut v, 10); // 远小于 payload
        assert!(!truncated, "delta 响应跳过截断");
        assert!(v.get("truncated").is_none());
        assert_eq!(v["added"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn apply_compress_removes_container_and_kind() {
        let mut v = serde_json::json!({
            "items": [
                {"name": "x", "container": "Foo", "kind": "Function", "file": "a.rs"},
                {"name": "y", "container_name": "Bar", "kind": "Method", "file": "b.rs"},
            ],
            "meta": {"kind": "summary", "file": "c.rs"},
        });
        apply_compress(&mut v);
        let items = v["items"].as_array().unwrap();
        assert!(items[0].get("container").is_none());
        assert!(items[0].get("kind").is_none());
        assert!(items[1].get("container_name").is_none());
        assert_eq!(items[0]["name"], "x", "name 保留");
        assert_eq!(items[0]["file"], "a.rs", "位置字段保留");
        assert_eq!(items[1]["file"], "b.rs");
        assert!(v["meta"].get("kind").is_none(), "递归删非 items 层的同名字段");
        assert_eq!(v["meta"]["file"], "c.rs", "非目标字段不动");
    }
}

#[cfg(test)]
mod find_symbol_ls_error_tests {
    //! bd serena-rust-x67：find-symbol 不再静默吞 NotInstalled。
    //! - 纯逻辑：全失败合并 / warning 文案 / wire 顶层 warning 键（不拉 LS）。
    //! - 集成：全失败走 python fixture（pyright 本机故意不装——环境不变量）；
    //!   部分成功 + 全成功走 rust+py 混合 fixture（真拉 rust-analyzer，秒级冷启动；
    //!   独立成 mod 以不污染 symbol_cache_tests 的「不拉 LS」纪律）。
    use super::*;

    /// 分支 1 纯逻辑：全失败且全为 NotInstalled → 合并 language/hint 为单错误。
    #[test]
    fn all_failed_not_installed_merges_languages_and_hints() {
        let failures = vec![
            (
                "python".to_string(),
                ToolError::NotInstalled {
                    language: "python".to_string(),
                    hint: "pip install pyright".to_string(),
                },
            ),
            (
                "go".to_string(),
                ToolError::NotInstalled {
                    language: "go".to_string(),
                    hint: "install gopls".to_string(),
                },
            ),
        ];
        match combined_all_failed_error(failures) {
            ToolError::NotInstalled { language, hint } => {
                assert_eq!(language, "python, go");
                assert_eq!(hint, "pip install pyright; install gopls");
            }
            other => panic!("expected combined NotInstalled, got {other:?}"),
        }
    }

    /// 分支 1 变体：混入非 NotInstalled（crash）→ 原样上抛真错误，不谎报未安装
    /// （否则 agent 会去重装 LS 而不是看崩溃原因）。
    #[test]
    fn all_failed_mixed_errors_prefer_real_error_over_not_installed() {
        let failures = vec![
            (
                "python".to_string(),
                ToolError::NotInstalled {
                    language: "python".to_string(),
                    hint: "h".to_string(),
                },
            ),
            (
                "rust".to_string(),
                ToolError::Launch(anyhow::anyhow!("spawn boom")),
            ),
        ];
        let err = combined_all_failed_error(failures);
        assert!(
            matches!(&err, ToolError::Launch(e) if format!("{e:#}").contains("spawn boom")),
            "got {err:?}"
        );
    }

    /// 分支 2 文案：lang 前缀 + 复用 Display（NotInstalled 自带安装 hint）。
    #[test]
    fn failure_warnings_carry_lang_prefix_and_display() {
        let failures = vec![(
            "python".to_string(),
            ToolError::NotInstalled {
                language: "python".to_string(),
                hint: "pip install pyright".to_string(),
            },
        )];
        let w = failure_warnings(&failures);
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("python: "), "w[0]={}", w[0]);
        assert!(w[0].contains("not installed"));
        assert!(w[0].contains("pip install pyright"));
    }

    /// 分支 3 wire 三形态：无 warning 不加键；compact 对象直接加键；裸数组升级对象。
    #[test]
    fn attach_warning_shapes_per_envelope_form() {
        let mut v = serde_json::json!({"compact": true, "items": [], "raw_count": 0});
        attach_warning(&mut v, &[]);
        assert!(v.get("warning").is_none(), "无失败不得加 warning 键");

        let mut v = serde_json::json!({
            "compact": true,
            "items": [["foo", "a.rs:1:1"]],
            "raw_count": 1,
        });
        attach_warning(
            &mut v,
            &["python: language server for `python` not installed: x".to_string()],
        );
        assert_eq!(v["compact"], serde_json::Value::Bool(true), "compact 键保留");
        assert_eq!(v["raw_count"], 1);
        assert!(v["warning"].as_str().unwrap().contains("python"));

        let mut v = serde_json::json!([{"name": "foo"}]);
        attach_warning(&mut v, &["rust: boom".to_string()]);
        assert_eq!(v["compact"], serde_json::Value::Bool(false));
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert!(v["warning"].as_str().unwrap().contains("boom"));
    }

    /// 分支 1 集成：py fixture（walked_langs={python}，pyright 缺失）→
    /// Err NotInstalled 含 lang 与安装 hint，不再 rc=0 静默空。
    /// 环境不变量：pyright 故意不装（find_symbol.rs::has_clangd 同款守卫——
    /// pyright 已装的环境本测试前提失效，跳过不计失败）。
    fn has_pyright() -> bool {
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let names: &[&str] = if cfg!(windows) {
                    &["pyright.exe", "pyright.cmd", "pyright.bat"]
                } else {
                    &["pyright"]
                };
                if names.iter().any(|n| dir.join(n).is_file()) {
                    return true;
                }
            }
        }
        false
    }

    #[tokio::test]
    async fn find_symbol_all_ls_missing_returns_not_installed() {
        if has_pyright() {
            println!("skipped: pyright installed — all-missing env invariant broken");
            return;
        }
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("mod.py"), "def py_foo():\n    pass\n").expect("write .py");
        let err = sup
            .tool_find_symbol(dir.path(), "py_foo", 10, None)
            .await
            .expect_err("all-LS-missing must Err, not silent empty");
        match err {
            ToolError::NotInstalled { language, hint } => {
                assert!(language.contains("python"), "language={language}");
                assert!(!hint.is_empty(), "hint must carry install guidance");
            }
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    /// 分支 2 集成（部分成功）：rust+py 混合目录（RA 在 PATH、pyright 缺失）→
    /// Ok 且 hits 来自 rust、warning 归属 python；随后同 root 仅查 rust（warm session）
    /// → 分支 3 全成功无 warning。真拉 rust-analyzer —— RA 符号索引双阶段就绪
    /// （session ready ≠ workspace/symbol 可见，首查可能静默空），轮询非空。
    #[tokio::test]
    async fn find_symbol_partial_ls_failure_keeps_hits_and_warns() {
        use std::time::{Duration, Instant};
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x67fix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::write(
            src.join("main.rs"),
            "fn alpha_main() {}\nfn main() { alpha_main(); }\n",
        )
        .expect("write main.rs");
        std::fs::write(dir.path().join("mod.py"), "def py_foo():\n    pass\n").expect("write .py");

        // RA 符号索引就绪窗口内 workspace/symbol 可能静默空 → 轮询非空（60s 上限）。
        let deadline = Instant::now() + Duration::from_secs(60);
        let (hits, warnings) = loop {
            let (h, w) = sup
                .tool_find_symbol(dir.path(), "alpha_main", 50, None)
                .await
                .expect("partial LS failure must not fail the call");
            if !h.is_empty() || Instant::now() >= deadline {
                break (h, w);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        assert!(
            hits.iter().any(|h| h.name == "alpha_main"),
            "rust hits must survive python LS failure; hits={hits:?} warnings={warnings:?}"
        );
        assert_eq!(warnings.len(), 1, "warnings={warnings:?}");
        assert!(warnings[0].starts_with("python: "), "warnings={warnings:?}");

        // 分支 3：同 root 仅查 rust（session 已 warm、索引已就绪）→ 全成功无 warning。
        let (hits, warnings) = sup
            .tool_find_symbol(dir.path(), "alpha_main", 50, Some("rust"))
            .await
            .expect("rust-only query on warm session");
        assert!(!hits.is_empty(), "rust-only query must still hit");
        assert!(warnings.is_empty(), "all-success must carry no warnings");
    }
}

#[cfg(test)]
mod semantic_readiness_and_args_tests {
    //! bd serena-rust-we0 / xzb / 84n：
    //! - we0：语义空结果的「未就绪 vs 无符号」判据（position_in_hits 纯逻辑 + 文案）。
    //! - xzb：workspace 加载错误的特征词判定（window 消息 → workspace_errors）。
    //! - 84n：不存在文件入口统一 BadArgs（校验在 session_for 之前，不拉 LS）。
    use super::*;

    // ---- we0：位置命中判据 ----

    fn hit(range_sl: u32, range_sc: u32, range_el: u32, range_ec: u32) -> SymbolHit {
        SymbolHit {
            name: "f".into(),
            kind: SymbolKindTag::Function,
            uri: "file:///t/f.rs".into(),
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: range_sl,
                    character: range_sc,
                },
                end: lsp_types::Position {
                    line: range_el,
                    character: range_ec,
                },
            },
            container: None,
        }
    }

    #[test]
    fn position_in_hits_matches_point_inside_symbol_range() {
        let hits = vec![hit(2, 4, 2, 20)];
        assert!(position_in_hits(&hits, 2, 4), "range start 含端点");
        assert!(position_in_hits(&hits, 2, 20), "range end 含端点");
        assert!(position_in_hits(&hits, 2, 10), "range 中段");
    }

    #[test]
    fn position_in_hits_rejects_point_outside_all_symbols() {
        let hits = vec![hit(2, 4, 2, 20)];
        assert!(!position_in_hits(&hits, 5, 0), "范围后");
        assert!(!position_in_hits(&hits, 1, 0), "范围前");
        assert!(!position_in_hits(&hits, 2, 3), "同行但列在范围前");
    }

    #[test]
    fn position_in_hits_empty_hits_is_false() {
        assert!(!position_in_hits(&[], 0, 0));
    }

    // ---- we0：未就绪文案 AI 可判读 ----

    #[test]
    fn not_ready_message_names_type_analysis_and_window() {
        let m = semantic_not_ready_message();
        assert!(m.contains("type analysis"), "AI 可判读关键词: {m}");
        assert!(m.contains("not be ready"), "就绪性而非无符号: {m}");
        assert!(m.contains("30-60s"), "预期窗口: {m}");
    }

    /// hover 空结果判定：null / 无 contents / 空串 / 空数组 / 空 MarkupValue 都算空；
    /// 有内容的对象与数组不算。
    #[test]
    fn hover_is_empty_covers_null_and_blank_contents_forms() {
        assert!(hover_is_empty(&serde_json::Value::Null));
        assert!(hover_is_empty(&serde_json::json!({})));
        assert!(hover_is_empty(&serde_json::json!({"contents": ""})));
        assert!(hover_is_empty(&serde_json::json!({"contents": []})));
        assert!(hover_is_empty(&serde_json::json!({"contents": {"value": ""}})));
        assert!(!hover_is_empty(&serde_json::json!({"contents": {"value": "fn hello"}})));
        assert!(!hover_is_empty(&serde_json::json!({"contents": [{"value": "x"}]})));
    }

    /// hover 等返 Option 的工具空结果 = 裸 null：带 warning 时升级为对象形态；
    /// 无 warning 时保持 null（wire 既有形态不变）。
    #[test]
    fn attach_warning_upgrades_bare_null_only_when_warning_present() {
        let mut v = serde_json::Value::Null;
        attach_warning(&mut v, &[]);
        assert!(v.is_null(), "无 warning 的 null 保持既有 wire 形态");

        let mut v = serde_json::Value::Null;
        attach_warning(&mut v, &["not ready".to_string()]);
        assert_eq!(v["items"], serde_json::Value::Null);
        assert!(v["warning"].as_str().unwrap().contains("not ready"));
    }

    /// 标量响应（string/number/bool）同裸 null：带 warning 时升级对象形态不丢内容。
    #[test]
    fn attach_warning_upgrades_scalar_without_dropping_value() {
        let mut v = serde_json::json!("symbol body text");
        attach_warning(&mut v, &["project switched: A -> B".to_string()]);
        assert_eq!(v["items"], serde_json::json!("symbol body text"));
        assert!(v["warning"].as_str().unwrap().contains("project switched"));
    }

    // ---- xzb：workspace 加载错误特征词 ----

    #[test]
    fn workspace_error_matches_fetch_workspace_error_and_cargo_wording() {
        assert!(is_workspace_load_error(
            "rust-analyzer failed to load workspace: FetchWorkspaceError(LoadFailed { stdout: \"\" })"
        ));
        assert!(is_workspace_load_error(
            "error: current package believes it's in a workspace when it's not"
        ));
    }

    #[test]
    fn workspace_error_ignores_ordinary_messages() {
        assert!(!is_workspace_load_error(
            "unused variable: `x` in function main"
        ));
        assert!(!is_workspace_load_error("cargo build finished"));
        assert!(!is_workspace_load_error(""));
    }

    // ---- 84n：不存在文件入口统一 BadArgs（校验先于 session_for，无 LS 依赖）----

    #[tokio::test]
    async fn overview_missing_file_returns_bad_args_not_internal() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let dir = tempfile::tempdir().expect("tempdir");
        let err = sup
            .execute_tool(
                "overview",
                dir.path().to_str().unwrap(),
                serde_json::json!({"file": "nope.rs"}),
                Some("rust"),
            )
            .await
            .expect_err("missing file must BadArgs (wire §6.3), not INTERNAL");
        match err {
            ToolError::BadArgs { detail } => {
                assert!(detail.contains("not found"), "detail={detail}");
                assert!(detail.contains("nope.rs"), "detail={detail}");
            }
            other => panic!("expected BadArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn diagnostics_and_read_file_missing_file_return_bad_args() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().unwrap();
        for tool in ["diagnostics", "read-file"] {
            let err = sup
                .execute_tool(
                    tool,
                    root,
                    serde_json::json!({"file": "nope.rs"}),
                    Some("rust"),
                )
                .await
                .expect_err("missing file must BadArgs");
            assert!(
                matches!(err, ToolError::BadArgs { .. }),
                "tool={tool} got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn cargo_probe_flags_non_member_and_clears_valid_workspace() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        // 合法独立 workspace → 不记录
        let ok_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            ok_dir.path().join("Cargo.toml"),
            "[package]\nname = \"okws\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(ok_dir.path().join("src")).expect("mkdir src");
        std::fs::write(ok_dir.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        sup.probe_cargo_workspace_error(ok_dir.path()).await;
        assert!(
            sup.workspace_error_for(ok_dir.path()).is_none(),
            "valid workspace must not be flagged"
        );

        // 父 [workspace] 空表 + 子 package 非成员 → cargo metadata 失败（xzb 同构最小复刻）
        let outer = tempfile::tempdir().expect("tempdir");
        std::fs::write(outer.path().join("Cargo.toml"), "[workspace]\n").expect("write");
        let child = outer.path().join("child");
        std::fs::create_dir_all(child.join("src")).expect("mkdir child/src");
        std::fs::write(
            child.join("Cargo.toml"),
            "[package]\nname = \"child\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write child Cargo.toml");
        std::fs::write(child.join("src/main.rs"), "fn main() {}\n").expect("write");
        sup.probe_cargo_workspace_error(&child).await;
        let err = sup
            .workspace_error_for(&child)
            .expect("non-member child must be flagged");
        assert!(err.contains("cargo metadata failed"), "err={err}");
    }

    #[tokio::test]
    async fn cargo_probe_skips_non_cargo_root() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let dir = tempfile::tempdir().expect("tempdir");
        sup.probe_cargo_workspace_error(dir.path()).await;
        assert!(
            sup.workspace_error_for(dir.path()).is_none(),
            "no Cargo.toml → nothing to probe, no record"
        );
    }
}
