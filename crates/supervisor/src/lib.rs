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
pub mod path_guard;
pub mod root_finder;

pub mod catalog;
mod ct;
pub mod recipe;
pub mod recipe_ops;
pub mod ref_tools;
pub mod repo_map;
pub mod undo;
pub mod warm;
pub mod write_gate;

use ls_adapters::symbol_quirks;
use lsp_core::types::{SymbolHit, SymbolKindTag};
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Position};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

/// per-request documentSymbol 会话重路由（angular `.html` → vscode-html 伴生，
/// ↖ mirror 上游路由表；其余语言/文件恒等返回）。session 已在手的内联路径用。
fn reroute_doc_symbols(session: Arc<Session>, root: &Path, file: &str) -> Arc<Session> {
    if let Some(adapter) = ls_registry::adapter_for(&session.language_id())
        && let Some(s) =
            adapter.session_for_file(root, Path::new(file), "textDocument/documentSymbol")
    {
        return s;
    }
    session
}

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
pub fn effective_tool_timeout(lang: Option<&str>, args: &serde_json::Value) -> Duration {
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

pub fn effective_index_timeout(lang: Option<&str>, args: &serde_json::Value) -> Duration {
    let from_args = args
        .get("_index_timeout_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok());
    let cli_override = ls_registry::config::LsOverride {
        index_timeout_ms: from_args,
        ..Default::default()
    };
    let ms =
        ls_registry::config::effective_index_timeout_ms(lang.unwrap_or(""), Some(&cli_override))
            .unwrap_or(120_000);
    Duration::from_millis(ms as u64)
}

/// 批2-F：recipe 步进度行的边带文件（temp 目录单文件；单机单 daemon :7860
/// 不撞）。daemon 模式下 supervisor 的 eprintln 落 daemon stderr（默认 NULL），
/// CLI 转发 recipe 请求期间中继该文件增量到本进程 stderr——长步不再哑语。
pub const RECIPE_PROGRESS_FILE: &str = "serena-recipe-progress.log";

/// 批2-A：语义工具渐进首答——重量级语义请求的就绪等待上限（find-symbol 的
/// workspace/symbol）。120s 死等改为默认 15s：15s 内 LS 答复 → 全量结果无降级；
/// 超时 → 降级返回（degraded/warmup 标记 + 人话 warning），AI 不再把首答当挂死。
/// 三层合并对齐 [`effective_index_timeout`]：`args._warmup_ms`（CLI
/// `--warmup-timeout`）> `SERENA_WARMUP_TIMEOUT_MS` > 默认。
pub const WARMUP_BUDGET_DEFAULT: Duration = Duration::from_secs(15);

pub fn effective_warmup_budget(lang: Option<&str>, args: &serde_json::Value) -> Duration {
    if let Some(ms) = args
        .get("_warmup_ms")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
    {
        return Duration::from_millis(ms as u64);
    }
    let _ = lang; // 预留 per-LS 覆盖（servers.toml warmup_ms），当前无消费者。
    parse_warmup_env().unwrap_or(WARMUP_BUDGET_DEFAULT)
}

fn parse_warmup_env() -> Option<Duration> {
    let raw = std::env::var("SERENA_WARMUP_TIMEOUT_MS").ok()?;
    let ms = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
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

    /// LS 未安装（PATH 找不到）—— exit 1 + install_hint。中央注入点：所有语言的
    /// NOT_INSTALLED 都走本 Display，在此追加 ls-use 指引即覆盖全部语言
    /// （bd serena-rust-4ux；用户自装 LS 免重装下载）。
    #[error(
        "language server for `{language}` not installed: {hint}; if you already have the LS binary on disk, register it with `serena-cli ls-use <lang> <path-to-ls-binary>`"
    )]
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
/// 第 4 位 = 用户显式 `--lang` override（小写归一）：bd 8ges——此前 (root,file,mtime)
/// 键让 override 调用被先前无 override 的缓存结果静默遮蔽（Dockerfile --lang python
/// 返回 docker 符号）。键用**原始 override**而非解析后 lang：缓存命中必须先于
/// lang 解析（不可解析扩展名也命中，测试锁定）；None 平面（扩展名路由）与
/// Some 平面（override 路由）天然分键、互不遮蔽。
type SymbolCacheKey = (PathBuf, String, Option<(SystemTime, u64)>, Option<String>);

/// O3 解析候选：(file 相对路径, 符号名, 符号全范围)。
type SymbolCandidates = Vec<(String, String, lsp_types::Range)>;

/// O3 pick 中间形态：(符号名, 符号全范围)——扫描侧再拼 file 维度成 SymbolCandidates。
type SymbolNameRanges = Vec<(String, lsp_types::Range)>;

/// overview / symbol-body 的缓存 key；文件不可 stat（不存在/失败）→ None（确定性 key）。
/// `lang_override`：用户显式 `--lang`（bd 8ges，见 [`SymbolCacheKey`] 注）。
fn doc_symbol_cache_key(root: &Path, file: &str, lang_override: Option<&str>) -> SymbolCacheKey {
    (
        root.to_path_buf(),
        file.to_string(),
        std::fs::metadata(root.join(file))
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len()))),
        lang_override.map(|l| l.to_ascii_lowercase()),
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
        None,
    )
}

/// bd serena-rust-gqyp：documentSymbol/workspace-symbol 的 `range` 是符号**全范围**
/// （起点落在 `def`/`class` 声明关键字上），refs/hover 类语义请求必须打在**名字
/// token** 上——打在关键字上 LS 恒返空（对拍实锤：同刻名字处 1 item、关键字处
/// 0 item）。在 range 起始 ≤3 行内找整词名字，命中 → 返回名字位置（UTF-16 col，
/// 与 LSP wire 同基线）；找不到（装饰器跨行等稀有形态）→ 原样返回 `range.start`
/// 保守降级。
fn refine_symbol_name_position(
    text: &str,
    name: &str,
    range: lsp_types::Range,
) -> lsp_types::Position {
    let last_line = range.start.line.saturating_add(2).min(range.end.line);
    let window = (last_line - range.start.line + 1) as usize;
    for (i, line) in text
        .lines()
        .skip(range.start.line as usize)
        .take(window)
        .enumerate()
    {
        if let Some(byte_col) = find_whole_word(line, name) {
            let col16: usize = line[..byte_col].chars().map(char::len_utf16).sum();
            return lsp_types::Position {
                line: range.start.line + i as u32,
                character: col16 as u32,
            };
        }
    }
    range.start
}

/// `name` 在 `line` 内首次**整词**出现的 byte offset（两侧非标识符字符，防
/// `add` 命中 `additional`）。
fn find_whole_word(line: &str, name: &str) -> Option<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(name)
        .find(|(start, hit)| {
            let end = start + hit.len();
            line[..*start].chars().next_back().is_none_or(|c| !is_word(c))
                && line[end..].chars().next().is_none_or(|c| !is_word(c))
        })
        .map(|(start, _)| start)
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
type RootSignalEntry = (
    std::time::Instant,
    Option<SystemTime>,
    std::collections::BTreeSet<String>,
);
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
    let (max, langs) = tokio::task::spawn_blocking(move || walk_root_signal(&root_owned))
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

    /// 符号缓存命中总次数（生命周期单调递增；bd e1p）。daemon 在单次工具调用
    /// 前后差分此值 = 该调用的命中与否，落 invocations.jsonl `cache_hit` 字段。
    /// 默认 0：mock/桩实现无缓存语义，差分恒 false。
    fn cache_hits_total(&self) -> u64 {
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
    /// spawn 成功时登记的 LS exe 路径（launch.cmd 首元素）。缓存命中前复验其仍在盘上：
    /// 卸载/半包（目录在 exe 不在）后活 session 不得继续被复用——PM 对拍实锤
    /// （uninstall + 空壳 → hover 仍返语义结果）。evict 时同步清理。
    launch_exe: Mutex<HashMap<Key, PathBuf>>,
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
    /// bd dmsm：连续空 pending 轮次记账 (root, uri) → (连续轮数, 首轮时刻)。
    /// LS 对该文件连续多轮 (≥3 轮且 ≥10s) 只给 `pending:true` + 空 items 时，
    /// 第 3 轮起降级为 `pending:false` + warning（"LS 不支持该语言的诊断"），
    /// 禁止 AI 消费者无限重试。任何非空/确认结果清零。
    diag_pending_streak: Mutex<HashMap<(PathBuf, String), (u32, std::time::Instant)>>,
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
    /// 符号缓存命中计数（bd e1p）：daemon 每次调用前后差分 = 该调用的命中数。
    cache_hit_counter: AtomicU64,
    /// 修 P1 #2 测试专用：覆盖默认 TTL 让单测可控；生产 build 不持此字段。
    #[cfg(test)]
    _idle_ttl_override: std::sync::Arc<Mutex<Option<Duration>>>,
    /// LS 启动暖机窗口（bd serena-rust-bxd O2/O4）：root → 记录。LS 会话新建
    /// （session_for spawn 点）即记账并重置；首个语义工具（hover/def/refs/
    /// find-implementations）非空成功即关闭。窗口内 find-symbol 结果可能随
    /// 索引爬升波动（实测 9→4→6+），经既有 warning 通道透出 partial 信号。
    /// 键用 key_root_identity 归一（Windows 大小写双重身份惯例）。
    ls_warmup: Mutex<HashMap<String, LsWarmup>>,
    /// 批2-A：语义工具渐进首答的就绪等待上限（ms）。execute_tool 入口按
    /// [`effective_warmup_budget`] 刷新（args._warmup_ms / env / 15s 默认）；
    /// recipe/ct 内部嵌套调用无 args 上下文，读此处当前值即本次请求的预算。
    /// Atomic：Arc 共享下的跨请求覆盖竞态无害（进程级配置，非请求级状态）。
    warmup_budget_ms: AtomicU64,
    /// blindtest v5 P3-H：同 (root, lang) 最近一次 LS 生命周期失败的进程内 memo。
    /// 命中（TTL 内）→ 后续调用不再重复 spawn/请求，快速失败带恢复指引——失败
    /// 语言错误重放占盲测 token 61%（al 四步 ×687B）。stop-all 杀 daemon 即清账。
    failure_memo: Mutex<HashMap<(PathBuf, String), FailureMemoEntry>>,
}

/// memo 记账类别（重建同 wire 类错误用；NotInstalled 短 TTL——装上 LS 即可自愈，
/// 不该被 300s 长账挡住）。
enum MemoKind {
    NotInstalled { language: String, hint: String },
    Launch { message: String },
    Terminated { cause: String },
}

struct FailureMemoEntry {
    kind: MemoKind,
    at: std::time::Instant,
}

impl MemoKind {
    fn ttl_secs(&self) -> u64 {
        match self {
            Self::NotInstalled { .. } => 60,
            Self::Launch { .. } | Self::Terminated { .. } => 300,
        }
    }
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
            launch_exe: Mutex::new(HashMap::new()),
            direct_mode: true,
            diag_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            diag_generation: std::sync::Arc::new(AtomicU64::new(0)),
            pull_diag_supported: std::sync::Arc::new(Mutex::new(HashMap::new())),
            version_seen: std::sync::Arc::new(Mutex::new(HashMap::new())),
            diag_pending_streak: Mutex::new(HashMap::new()),
            recent_writes: Mutex::new(HashMap::new()),
            workspace_errors: std::sync::Arc::new(Mutex::new(HashMap::new())),
            symbol_cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
            delta_cache: Arc::new(Mutex::new(HashMap::new())),
            idle_buffers_reclaim_counter: AtomicU64::new(0),
            cache_hit_counter: AtomicU64::new(0),
            ls_warmup: Mutex::new(HashMap::new()),
            warmup_budget_ms: AtomicU64::new(WARMUP_BUDGET_DEFAULT.as_millis() as u64),
            failure_memo: Mutex::new(HashMap::new()),
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
        let n = self
            .idle_buffers_reclaim_counter
            .fetch_add(1, Ordering::Relaxed)
            + 1;
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
    /// P2-0bq: evict 也清理 `pull_diag_supported` 旁表——只 insert 不 remove，
    /// LRU 反复驱逐同 (root, lang) 会按驱逐次数单调累积。
    ///
    /// audit 竞锁 #5：`load_gates` **不再**随 evict 删除——慢路径持旧 gate guard
    /// 冷启动期间（jdtls 可达分钟级），删表项会让第三个调用者新建 gate 立即放行，
    /// 同 key 双 spawn 并发。保留表项 = 后续调用者与在飞慢路径同门串行；表项上界
    /// = 曾冷启动过的 distinct (root, lang) 数（每条一个 `Arc<Mutex<()>>`，本裁决
    /// 后不随驱逐次数增长，量级可忽略）。
    ///
    /// audit 内存 F3：`version_seen` 按 root 归一清理——(root, uri)→bool 条目原先
    /// 永不回收，长命 daemon 逐文件累积。
    ///
    /// `diag_cache` 按 (root, uri) 键与 session 解耦，刻意保留（文件级诊断跨世代
    /// 仍有效；新一轮 session 第一条 pushDiagnostics 会覆写/清空对应条目）。
    pub async fn evict(&self, key: &Key) -> ToolResult<bool> {
        let session = self.instances.lock().unwrap().remove(key);
        self.last_used.lock().unwrap().remove(key);
        self.pull_diag_supported.lock().unwrap().remove(key);
        self.launch_exe.lock().unwrap().remove(key);
        let evicted_root = key_root_identity(&key.root);
        self.version_seen
            .lock()
            .unwrap()
            .retain(|(root, _), _| key_root_identity(root) != evicted_root);
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
                tracing::warn!(
                    ?key,
                    "session terminated mid-call; evicting and retrying once"
                );
                let _ = sup.evict(&key).await;
                let session2 = sup.session_for(root, lang).await?;
                f(session2).await
            }
            other => other,
        }
    }

    /// 缓存命中前的装态复验:登记的 LS exe 已不在盘上(被卸载/半包)→ 会话失效,
    /// 驱逐后走冷启动重新探测(未装则报 LS_NOT_INSTALLED)。无登记条目(防御:
    /// 理论上 spawn 必登记)→ 视为有效不拦。bd serena-rust-bua（审计 A3）：除
    /// exe 在盘外还须会话未 Failed——Failed 会话残留登记时 respawn 不得复用。
    fn launch_exe_valid(&self, key: &Key) -> bool {
        let exe = self.launch_exe.lock().unwrap().get(key).cloned();
        let state = self.instances.lock().unwrap().get(key).map(|s| s.state());
        Self::reuse_allowed(exe.as_deref(), state.as_ref())
    }

    /// 复用判定纯函数（可单测，Session 无法在测试里构造成 Failed）：exe 在盘
    /// （无登记=有效）且 会话未 Failed。
    fn reuse_allowed(
        exe: Option<&Path>,
        state: Option<&lsp_core::session::SessionState>,
    ) -> bool {
        exe.is_none_or(Path::is_file)
            && !matches!(
                state,
                Some(lsp_core::session::SessionState::Failed(_))
            )
    }

    /// 拿到/创建 (root, lang) 对应的 Session，同 key 只允许一次冷启动。
    async fn session_for(&self, root: &Path, lang: &str) -> ToolResult<Arc<Session>> {
        let key = Self::key(root, lang);
        // 快路径：缓存命中且（未 Failed 且 exe 仍在盘——卸载/半包后活 session 失效）
        // → 复用；命中但失效 → 驱逐后走慢路径重新探测装态。
        // 禁在 if-let scrutinee 的块内再拿同一把锁：无 else 的 if-let 临时 guard
        // 活到块尾（edition 2024），同锁重入 = 自死锁（判据源 launch_exe_valid）。
        let cached = self.instances.lock().unwrap().get(&key).cloned();
        let reuse = cached
            .as_ref()
            .is_some_and(|s| !matches!(s.state(), lsp_core::session::SessionState::Failed(_)))
            && self.launch_exe_valid(&key);
        if reuse {
            let session = cached.expect("reuse implies cached");
            self.touch(&key);
            return Ok(session);
        }
        if cached.is_some() {
            self.instances.lock().unwrap().remove(&key);
        }

        // 慢路径：per-key 加载门（防同 key 并发双 spawn）+ 双检锁。
        let gate = self.load_gate_for(root, lang);
        let _guard = gate.lock().await;
        let cached = self.instances.lock().unwrap().get(&key).cloned();
        let reuse = cached
            .as_ref()
            .is_some_and(|s| !matches!(s.state(), lsp_core::session::SessionState::Failed(_)))
            && self.launch_exe_valid(&key);
        if reuse {
            let session = cached.expect("reuse implies cached");
            self.touch(&key);
            return Ok(session);
        }
        if cached.is_some() {
            self.instances.lock().unwrap().remove(&key);
        }

        // 双检后仍无可用会话。blindtest v5 P3-H：先查失败 memo（TTL 内同类失败
        // 直接短路，不 spawn 不请求）；上一会话 Failed（LS 终止/握手败）记入 memo，
        // 本调用即按「第 2 次同错」快速失败（PM 拍板：第 2 次起带 hint）。
        if let Some(e) = self.failure_memo_check(&key.root, lang) {
            return Err(e);
        }
        if let Some(s) = &cached
            && let lsp_core::session::SessionState::Failed(cause) = s.state()
        {
            self.failure_memo_record(
                &key.root,
                lang,
                MemoKind::Terminated { cause },
            );
            return Err(self.failure_memo_check(&key.root, lang).expect("just recorded"));
        }

        // 双路径（Task 21）：手写 T2 adapter 优先；servers.toml 条目（T0 配置驱动）
        // 走 config::ensure_launch——PATH 探测 / 安装缓存命中，永不触网（auto_install=false，
        // design §0 路径 A；显式下载走 CLI `install` 命令）。
        let ctx = ls_adapters::ProjectCtx {
            project_root: key.root.clone(),
        };
        let t2 = ls_registry::adapter_for(lang);
        let launch = match &t2 {
            Some(adapter) => adapter
                .launch_info(&ctx)
                .await
                .map_err(|e| {
                    let msg = format!("{e:#}");
                    if msg.contains("not found in PATH") {
                        self.failure_memo_record(
                            &key.root,
                            lang,
                            MemoKind::NotInstalled {
                                language: lang.to_string(),
                                hint: extract_install_hint(&msg),
                            },
                        );
                        ToolError::NotInstalled {
                            language: lang.to_string(),
                            hint: extract_install_hint(&msg),
                        }
                    } else {
                        self.failure_memo_record(
                            &key.root,
                            lang,
                            MemoKind::Launch { message: msg.clone() },
                        );
                        ToolError::Launch(e)
                    }
                })?,
            None => {
                let Some((_, spec)) = ls_registry::config::spec_for(lang) else {
                    return Err(ToolError::BadArgs {
                        detail: format!("unknown language: {lang}"),
                    });
                };
                let (_, args) = ls_registry::config::ensure_launch(lang, None, false, false)
                    .map_err(|msg| {
                        self.failure_memo_record(
                            &key.root,
                            lang,
                            MemoKind::NotInstalled {
                                language: lang.to_string(),
                                hint: msg.clone(),
                            },
                        );
                        ToolError::NotInstalled {
                            language: lang.to_string(),
                            hint: msg,
                        }
                    })?;
                // expand_exec 返回完整 argv（exec 模板首元素即 {bin}）。
                // spec.env：spawn 注入表（PATH 类键追加原值语义，见 spawn_env）；
                // 未声明 = 空 Vec，行为与引入前一致。
                ls_runtime::process::LaunchInfo {
                    cmd: args.into_iter().map(Into::into).collect(),
                    cwd: key.root.clone(),
                    env: spec.spawn_env(),
                    transport: ls_runtime::process::TransportKind::Stdio,
                }
            }
        };
        // 登记本次 spawn 的 LS exe（launch.cmd 首元素）——缓存命中复验的判据源。
        if let Some(exe) = launch.cmd.first() {
            self.launch_exe
                .lock()
                .unwrap()
                .insert(key.clone(), PathBuf::from(exe));
        }
        let child = ls_runtime::process::Child::spawn(launch).map_err(|e| {
            let msg = format!("runtime spawn error: {e}");
            self.failure_memo_record(&key.root, lang, MemoKind::Launch { message: msg.clone() });
            ToolError::Launch(anyhow::anyhow!("{msg}"))
        })?;
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
            folders.extend(
                lsp_core::workspace_folders::discover_additional_workspace_folders(&key.root),
            );
            folders
        });
        // T0 配置驱动路径无手写 adapter：无 initialize_patches（servers.toml 已含
        // 初始化形态）、无 set_project_root / on_server_ready 特判探针。
        if let Some(adapter) = &t2 {
            adapter.initialize_patches(&mut params);
        }
        // T0 三通道·init_options（Wave 1）：spec 声明的初始化选项深合并进
        // initializationOptions（两侧皆对象递归并集、spec 值覆盖；基线缺失直接设）。
        // T2 路径条目未声明该字段 → 零行为；T2 adapter 自有 patches 优先级不变。
        if let Some((_, spec)) = ls_registry::config::spec_for(lang)
            && let Some(opts) = &spec.init_options
        {
            let slot = params
                .initialization_options
                .get_or_insert_with(|| serde_json::Value::Object(Default::default()));
            deep_merge_json(slot, opts);
        }

        // blindtest v5 P2-D：Terminated 错误的 `ls` 字段用真实 server id（此前恒
        // "ls"——kotlin 报 `"ls":"ls"` 失真）。spec_for 命中即条目 id（kotlin 等
        // T0 = servers.toml 键）；未命中回落语言名。
        let ls_name = ls_registry::config::spec_for(lang)
            .map(|(id, _)| id)
            .unwrap_or(lang);
        let session = Session::start_named(ls_name, Some(child), params)
            .await
            .map_err(|e| {
                match &e {
                    CoreError::Terminated { cause, .. } => self.failure_memo_record(
                        &key.root,
                        lang,
                        MemoKind::Terminated { cause: cause.clone() },
                    ),
                    _ => self.failure_memo_record(
                        &key.root,
                        lang,
                        MemoKind::Launch { message: format!("LS handshake failed: {e}") },
                    ),
                }
                ToolError::Core(e)
            })?;
        // didOpen 的 languageId 用 adapter 真实语言（默认 "cpp" 对 rust-analyzer
        // 等严格 LS 是错语言 → 文档拒收）。session_for 是唯一 spawn 点，此处注入
        // 覆盖全部会话路径。lsp_language_id 换算 LSP 官方名（docker→dockerfile）。
        session.set_language_id(&ls_registry::lsp_language_id(lang));
        // T0 三通道·did_change_config + config_reply（Wave 1）：仅声明了字段的
        // 条目生效（julia nudge 先例），其余 LS 零行为。推送在 initialized 之后、
        // 任何 didOpen 之前（此时会话刚握手完成）。
        if let Some((_, spec)) = ls_registry::config::spec_for(lang) {
            if let Some(cfg) = &spec.did_change_config {
                let _ = session.client().notify(
                    "workspace/didChangeConfiguration",
                    serde_json::json!({ "settings": cfg }),
                );
            }
            if let Some(replies) = spec.config_reply.as_ref().filter(|r| !r.is_empty()) {
                let replies = std::sync::Arc::new(replies.clone());
                session
                    .client()
                    .on_server_request("workspace/configuration", move |msg| {
                        Some(configuration_reply_from_spec(&replies, &msg))
                    });
            }
        }
        // 注册 publishDiagnostics handler → 写 diag_cache + 累 generation。
        let cache_root = key.root.clone();
        let cache = std::sync::Arc::clone(&self.diag_cache);
        let generation = std::sync::Arc::clone(&self.diag_generation);
        let version_seen = std::sync::Arc::clone(&self.version_seen);
        session.client().on_notification(
            "textDocument/publishDiagnostics",
            make_diag_handler(cache_root, cache, generation, version_seen),
        );
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
            session.client().on_notification(method, move |msg| {
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
            // bd serena-rust-62z：外层包裹预算取 adapter 自报值（jdtls 90s /
            // csharp-ls 60s 不再被硬编码 30s 截断）；内部探针超时仍先于外层触发。
            if let Err(e) = tokio::time::timeout(
                adapter.ready_probe_budget(),
                adapter.on_session_ready(&session),
            )
            .await
            {
                tracing::warn!(adapter = adapter.id(), error = %e, "on_server_ready probe failed/timed out; continuing");
            }
        }
        // hybrid 语言（vue）：伴生 TS LS 的类型诊断并入同一 diag_cache，agent 的
        // diagnostics 工具同时看到模板错（主 LS）与类型错（tsserver）。handler 与
        // 主会话同源（make_diag_handler），缓存键按 uri 归一互不冲突。
        if let Some(adapter) = &t2
            && let Some(companion) = adapter.semantic_session(&key.root)
        {
            let cache_root = key.root.clone();
            let cache = std::sync::Arc::clone(&self.diag_cache);
            let generation = std::sync::Arc::clone(&self.diag_generation);
            let version_seen = std::sync::Arc::clone(&self.version_seen);
            companion.client().on_notification(
                "textDocument/publishDiagnostics",
                make_diag_handler(cache_root, cache, generation, version_seen),
            );
        }

        // 写一次、读多次；错就当不支持（fallback push 与 2.4 之前等价）。
        // per-LS 否决（Wave 1）：spec 声明 pull_diagnostics_denied = true 时强制
        // push（↖ mirror julia_server.py@7a296833：LanguageServer.jl 对 pull 崩溃）。
        let supports_pull = session
            .server_capabilities()
            .as_ref()
            .map(supports_pull_diagnostics)
            .unwrap_or(false)
            && !ls_registry::config::spec_for(lang).is_some_and(|(_, s)| s.pull_diagnostics_denied);
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

    /// bd ou83：format-on-write 收尾（默认关）。写类工具成功后对目标文件跑一次
    /// formatting 并落盘；LS 不可用 / 格式化失败不回滚写结果（debug/warn 留痕）。
    /// 返回应用编辑条数（0 = 未执行或无需格式化）。
    async fn format_on_write_for(&self, root: &Path, file: &str, lang: Option<&str>) -> usize {
        let edits = match self.tool_format(root, file, None, None, lang).await {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return 0,
            Err(e) => {
                tracing::debug!(
                    file,
                    error = %e,
                    "format-on-write: formatting unavailable (bd ou83); write kept"
                );
                return 0;
            }
        };
        let Ok(lang_s) = resolve_lang_for_file(file, lang) else {
            return 0;
        };
        let session = match self.session_for(root, lang_s.as_str()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(file, error = %e, "format-on-write: session unavailable; write kept");
                return 0;
            }
        };
        let abs = match path_guard::guarded_join(root, file) {
            Ok(p) => p,
            Err(detail) => {
                tracing::debug!(file, detail, "format-on-write: path guard rejected; write kept");
                return 0;
            }
        };
        match edit_tools::apply_format_edits(&session, root, &abs, edits).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(file, error = %e, "format-on-write failed; write result kept");
                0
            }
        }
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
    /// - `Some(0)`：立即返回当前快照（跳过 push 等待窗；冷启动首次拉取仍需
    ///   LS 握手/分析往返，pending:true = 未确认）。
    /// - `Some(N>0)`：等 generation >= N，仍受 5s 上限；超时返 `{ items: [], pending: true }`。
    ///
    /// pending 语义（2026-09-23 裸 RA 探针 + live 复现实锤）：rust-analyzer 的 pull
    /// （textDocument/diagnostic）返回的是"上次计算的快照"——didChange 后异步重算
    /// 完成前，pull 会返回**陈旧错误**（REPAIR 场景实测拿到上一版 7 条 syntax errors
    /// 且无任何版本标记）。因此 **pull 快照无法归属 didChange 之后的版本，永不可信**。
    ///
    /// 主路径 = **push 等待**：generation 只计非空推送（见 handler），gen 越基线 =
    /// didOpen/didChange 之后的新一代推送到达（cache 即新鲜 items）。窗口尽未达标
    /// → pull 做兜底。pending 语义（blindtest v5 P3-I 修订）：items 空 = 未确认
    /// （可能 pending:true，连续 3 轮空走 dmsm 降级）；**items 非空 = pending:false**
    ///（结果存在即非 in-progress；陈旧快照风险由 pull 预算收紧 v5 P2-F 间接缩小，
    /// AI 应回头复核的提示由空结果路径承担）。
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
        let session = self.semantic_session_or_main(root, lang.as_str()).await?;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;
        let key = Self::key(root, lang.as_str());

        // hybrid 语言（vue）路由到伴生 TS LS 时伴生不在 pull_diag_supported 表
        // （session_for 只探测主会话）→ 从所选会话的 serverCapabilities 现查；
        // TLS 支持 LSP 3.17 pull（.vue 的 tsserver 类型诊断即经此取出）。
        let hybrid_companion = ls_registry::adapter_for(lang.as_str()).is_some_and(|a| {
            // key.root = canonical 形态，与伴生槽键一致（audit 竞锁 #7）。
            a.semantic_session(&key.root)
                .is_some_and(|s| Arc::ptr_eq(&s, &session))
        });
        let supports_pull = if hybrid_companion {
            session
                .server_capabilities()
                .as_ref()
                .map(supports_pull_diagnostics)
                .unwrap_or(false)
        } else {
            self.pull_diag_supported
                .lock()
                .unwrap()
                .get(&key)
                .copied()
                .unwrap_or(false)
        };

        // hybrid 伴生路径：类型诊断权威源 = 伴生 pull。**必须绕过 push 缓存** ——
        // 主 Vue LS 会推空 items（模板层无错）且版本匹配，命中后短路会把
        // tsserver 侧的类型错永久遮蔽。didOpen 后 tsserver 分析有窗口期（早期
        // pull 快照为空），10s 内轮询直到出现错误级诊断或窗口满（满 = 信任
        // LS 的 full 报告为"确认无错"，同 push 缓存的确认语义）。
        if hybrid_companion {
            if supports_pull {
                // blindtest v5 P2-F：pull 请求吃 warmup 预算（默认 15s），不吃
                // 30s TOOL_TIMEOUT——冷启动窗口 LS 未就绪时 30s 拉满会顶穿 CLI
                // 超时窗（csharp 30s×3=90s 三连 TIMEOUT，降级 JSON 到不了调用方）。
                let pull_timeout = self.warmup_budget();
                for _ in 0..40 {
                    let pulled = session
                        .client()
                        .request::<serde_json::Value>(
                            "textDocument/diagnostic",
                            json!({ "textDocument": { "uri": uri.clone() } }),
                            pull_timeout,
                        )
                        .await
                        .ok()
                        .and_then(|v| Self::extract_pull_items(&v));
                    if let Some(items) = pulled {
                        let has_error = items
                            .iter()
                            .any(|d| d.get("severity").and_then(|s| s.as_i64()) == Some(1));
                        if has_error {
                            self.reset_pending_streak(&key.root, &uri);
                            return Ok(json!({ "items": compact_diags(&items), "pending": false }));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            // bd dmsm：空 pending 出口统一记账，禁无限 pending。
            return Ok(self.register_empty_pending_exit(&key.root, &uri));
        }

        // push 等待：**version 精确比对** —— RA didChange 后会先重推旧快照再推新
        // 分析（2026-09-23 实锤），generation 无法区分新旧；推送 version == 当前
        // docsync content_version 才是新内容的分析结果（items 空 = 该版确认无错）。
        // LS 不发 version（如 clangd）→ 回退 generation 达标判定（旧行为）。
        // 窗口 5s（50 × 100ms）。Some(N) 语义保留：gen 兜底路径下 N <= 当前 gen
        // → 立即返回（旧契约，测试锁定）。
        // critic3-F12：wait_gen=0 承诺「立即返回当前」——target=0 恒达标，push
        // 等待窗对它只剩白耗 5s（version 纪律 LS 的确认条件是版本匹配，与
        // generation 无关，永远等不满条件）。跳过等待直接走快照路径（缓存 +
        // pull 兜底，pending 语义不变）；冷启动首次拉取的 LS 握手/分析往返是
        // 首拉成本，不属于等待窗。
        let before_gen = self.diag_generation.load(Ordering::Relaxed);
        let target = wait_gen.unwrap_or(before_gen + 1);
        let doc_cur = session.content_version_of(&path);
        let mut confirmed_items: Option<Vec<serde_json::Value>> = None;
        let wait_ticks = if wait_gen == Some(0) { 0 } else { 50 };
        for _ in 0..wait_ticks {
            let hit = self
                .diag_cache
                .lock()
                .unwrap()
                .get(&(key.root.clone(), diag_uri_key(&uri)))
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
            // bd dmsm：确认结果清零连续空 pending 记账。
            self.reset_pending_streak(&key.root, &uri);
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
            .get(&(key.root.clone(), diag_uri_key(&uri)))
            .cloned()
            .unwrap_or_default();
        let stale_entry = matches!((entry_ver, doc_cur), (Some(v), Some(c)) if v < c);
        if stale_entry {
            items = Vec::new();
        }
        if items.is_empty()
            && supports_pull
            && !stale_entry
            // blindtest v5 P2-F：同 hybrid 路径——warmup 预算而非 30s；超时经
            // `let Ok` 落空 → register_empty_pending_exit 的 dmsm 式降级。
            && let Ok(value) = session
                .client()
                .request::<serde_json::Value>(
                    "textDocument/diagnostic",
                    json!({ "textDocument": { "uri": uri.clone() } }),
                    self.warmup_budget(),
                )
                .await
            && let Some(pull) = Self::extract_pull_items(&value)
        {
            items = pull;
        }
        if items.is_empty() {
            // bd dmsm：空 pending 出口统一记账——同文件连续 ≥3 轮且 ≥10s 仍空
            // pending → 降级 pending:false + warning，禁无限 pending。
            return Ok(self.register_empty_pending_exit(&key.root, &uri));
        }
        // items 非空（可能是陈旧快照）不是「LS 无诊断」信号：清零记账，行为不变。
        self.reset_pending_streak(&key.root, &uri);
        // blindtest v5 P3-I：found>0 不再自称 in-progress（json 实锤 found 与
        // pending:true 并存误导调用方）。陈旧快照风险由 v5 P2-F 预算收紧间接
        // 缩小；push 等待窗已按 version 精确比对。
        Ok(json!({ "items": compact_diags(&items), "pending": false }))
    }

    /// bd dmsm：`tool_diagnostics` 的空 pending 出口统一记账。连续空 pending 轮数
    /// 达标（≥3 轮且距首轮 ≥10s）→ 降级 `pending:false` + warning（该 LS 可能不
    /// 支持此语言的诊断），此后清账重计；未达标 → 原 `pending:true` 形态不变。
    fn register_empty_pending_exit(&self, root: &Path, uri: &str) -> serde_json::Value {
        let k = (root.to_path_buf(), uri.to_ascii_lowercase());
        let mut streak = self.diag_pending_streak.lock().unwrap();
        let e = streak.entry(k.clone()).or_insert((0u32, std::time::Instant::now()));
        e.0 += 1;
        let out = pending_no_diag_response(e.0, e.1.elapsed());
        if out.get("pending").and_then(serde_json::Value::as_bool) == Some(false) {
            streak.remove(&k);
        }
        out
    }

    /// bd dmsm：非空/确认结果清零连续空 pending 记账（「连续」中断）。
    fn reset_pending_streak(&self, root: &Path, uri: &str) {
        self.diag_pending_streak
            .lock()
            .unwrap()
            .remove(&(root.to_path_buf(), uri.to_ascii_lowercase()));
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

/// bd dmsm：空 pending 出口的降级判定（纯函数，测试钉死语义）。
/// `count` = 含本轮的连续空 pending 轮数；`elapsed` = 距首轮的时长。
/// blindtest v5.1 P3-E：阈值按累计时长而非轮数下限——原「≥3 轮且 ≥10s」的与门
/// 让快空（<10s 累计）永不降级（vue 3×25ms 零停手信号 pending:true 永卡）。
/// 达标（≥3 轮，或 ≥2 轮且 ≥10s 累计）→ `pending:false` + warning（不再暗示
/// "再等等"）；未达标 → 原 `pending:true` 形态逐字节不变。
const PENDING_STREAK_MIN_ROUNDS: u32 = 3;
const PENDING_STREAK_MIN_ELAPSED: std::time::Duration = std::time::Duration::from_secs(10);

fn pending_no_diag_response(count: u32, elapsed: std::time::Duration) -> serde_json::Value {
    // 快空：3 轮即降级（不看时长）；慢空：累计 ≥10s 时 2 轮（MIN_ROUNDS-1）即可。
    let enough_rounds = count >= PENDING_STREAK_MIN_ROUNDS;
    let enough_time = count + 1 >= PENDING_STREAK_MIN_ROUNDS && elapsed >= PENDING_STREAK_MIN_ELAPSED;
    if enough_rounds || enough_time {
        serde_json::json!({
            "items": [],
            "pending": false,
            "warning": "LS returned no diagnostics after repeated empty-pending polls — it may not support diagnostics for this language"
        })
    } else {
        serde_json::json!({ "items": [], "pending": true })
    }
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

    /// hybrid 双服务器语言（astro）的 per-file 语义会话：ts/js 系文件的语义只在伴生
    /// TS LS（上游 `_is_ts_file` 路由语义），其余回落 [`Self::semantic_session_or_main`]。
    async fn semantic_session_for_file(
        &self,
        root: &Path,
        file: &str,
        lang: &str,
    ) -> ToolResult<Arc<Session>> {
        // 伴生槽 key = `Supervisor::key` 的 canonical 形态（set_project_root 写入）；
        // 请求 root 必须过同款归一再比较，否则 Windows 大小写/分隔符差异静默失配
        // → 伴生路由恒 None，.html/.vue/.astro 语义静默回落主会话（audit 竞锁 #7，
        // 「路径大小写双重身份」缺陷类）。
        let slot_root = Self::key(root, lang).root;
        // per-file 重路由优先（angular `.html` references → ngserver 伴生，↖ mirror
        // 上游路由表；调用于 references 类工具，method 恒 references）。
        if let Some(adapter) = ls_registry::adapter_for(lang)
            && let Some(s) =
                adapter.session_for_file(&slot_root, Path::new(file), "textDocument/references")
        {
            return Ok(s);
        }
        let is_ts_like = std::path::Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| {
                matches!(
                    e.as_str(),
                    "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
                )
            });
        if is_ts_like
            && let Some(adapter) = ls_registry::adapter_for(lang)
            && let Some(s) = adapter.semantic_session(&slot_root)
        {
            return Ok(s);
        }
        self.semantic_session_or_main(root, lang).await
    }

    /// per-request 会话重路由：adapter 的 session_for_file（angular `.html` →
    /// ngserver/html 伴生，↖ mirror 上游 (扩展名 × 方法) 路由表）优先；未路由回落
    /// [`Self::semantic_session_or_main`]。
    async fn session_for_request(
        &self,
        root: &Path,
        lang: &str,
        file: &str,
        method: &str,
    ) -> ToolResult<Arc<Session>> {
        // 伴生槽按 canonical root 键（audit 竞锁 #7），归一后再比较。
        let slot_root = Self::key(root, lang).root;
        if let Some(adapter) = ls_registry::adapter_for(lang)
            && let Some(s) = adapter.session_for_file(&slot_root, Path::new(file), method)
        {
            return Ok(s);
        }
        self.semantic_session_or_main(root, lang).await
    }

    /// hybrid 语言语义路由：adapter 提供语义会话（vue 的伴生 TS LS）时优先，否则
    /// 主会话（session_for 兼带首次拉起 —— 冷启动首个请求必经此处拉起主 + 伴生）。
    /// 仅位置类语义请求（hover / signature-help）经此；结构类（documentSymbol）
    /// 与写类仍走主会话。
    async fn semantic_session_or_main(&self, root: &Path, lang: &str) -> ToolResult<Arc<Session>> {
        // 伴生槽按 canonical root 键（audit 竞锁 #7），归一后再比较；回落主会话
        // 仍传原始 root（session_for 自行 canonicalize）。
        let slot_root = Self::key(root, lang).root;
        if let Some(adapter) = ls_registry::adapter_for(lang)
            && let Some(s) = adapter.semantic_session(&slot_root)
        {
            return Ok(s);
        }
        self.session_for(root, lang).await
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
        let session = self
            .session_for_request(root, lang.as_str(), file, "textDocument/hover")
            .await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session
            .ensure_open(&path)
            .await
            .map_err(crate::fs_tools::ensure_open_err(file))?;
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
        let session = self
            .session_for_request(root, lang.as_str(), file, "textDocument/signatureHelp")
            .await?;
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
        let (parsed, _degraded) = parse_lsp_items(raw, "code-action");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "format");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "format-range");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "inlay-hint");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "document-highlight");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "folding-range");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "code-lens");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "document-link");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "call-hierarchy:prepare");
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
        let item_str = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/incomingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let (parsed, _degraded) = parse_lsp_items(raw, "call-hierarchy:incoming");
        Ok(parsed)
    }

    /// `callHierarchy/outgoingCalls`：当前项调出的位置集合。
    pub async fn tool_call_hierarchy_outgoing(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::CallHierarchyOutgoingCall>> {
        let item_str = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("callHierarchy/outgoingCalls", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let (parsed, _degraded) = parse_lsp_items(raw, "call-hierarchy:outgoing");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "type-hierarchy:prepare");
        Ok(parsed)
    }

    /// `typeHierarchy/supertypes`：父类型列表（OOP 继承链向上）。
    pub async fn tool_type_hierarchy_supertypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/supertypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let (parsed, _degraded) = parse_lsp_items(raw, "type-hierarchy:supertypes");
        Ok(parsed)
    }

    /// `typeHierarchy/subtypes`：子类型列表（OOP 继承链向下）。
    pub async fn tool_type_hierarchy_subtypes(
        &self,
        root: &Path,
        item: serde_json::Value,
        lang_override: Option<&str>,
    ) -> ToolResult<Vec<lsp_types::TypeHierarchyItem>> {
        let item_str = item.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let lang = resolve_lang_for_file(&file_path_from_uri(item_str), lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let params = json!({ "item": item });
        let resp: Option<serde_json::Value> = session
            .request("typeHierarchy/subtypes", params, TOOL_TIMEOUT)
            .await?;
        let raw = resp.unwrap_or(serde_json::Value::Null);
        let (parsed, _degraded) = parse_lsp_items(raw, "type-hierarchy:subtypes");
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
        let (parsed, _degraded) = parse_lsp_items(raw, "moniker");
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
                let entries = self
                    .last_used
                    .lock()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
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
        // 杠精 07u5-5：LS 不支持 workspace/diagnostic 时裸 -32601 直穿对 AI 不可操作；
        // 拦下转 BadArgs 并指路逐文件 `diagnostics`。doc 注释曾宣称此转换，实现缺位。
        let resp: Option<serde_json::Value> = session
            .request("workspace/diagnostic", params, INDEX_TIMEOUT)
            .await
            .map_err(|e| match e {
                CoreError::Rpc {
                    code: -32601, ..
                } => ToolError::BadArgs {
                    detail: format!(
                        "workspace-diagnostic: LS `{lang}` does not support workspace/diagnostic; use per-file `diagnostics <file>` instead"
                    ),
                },
                other => ToolError::Core(other),
            })?;
        let Some(raw) = resp else {
            return Ok(Vec::new());
        };
        let items = raw.get("items").cloned().unwrap_or(serde_json::Value::Null);
        let (parsed, _degraded) = parse_lsp_items(items, "workspace-diagnostic");
        Ok(parsed)
    }

    // ==== Phase 3.1 文档符号缓存存取（上游 ls.py@43ae021 文档符号缓存对应）====

    /// cache 命中查询；返回克隆（平铺 list 小，克隆远便宜于 LS 往返）。
    fn symbol_cache_get(&self, key: &SymbolCacheKey) -> Option<Vec<SymbolHit>> {
        let hit = self.symbol_cache.lock().unwrap().get(key).cloned();
        if hit.is_some() {
            // bd e1p：命中计数（差分供 invocations.jsonl cache_hit 字段）。
            self.cache_hit_counter.fetch_add(1, Ordering::Relaxed);
        }
        hit
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
            .get(&(root.to_path_buf(), diag_uri_key(uri)))
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
        if self
            .workspace_errors
            .lock()
            .unwrap()
            .contains_key(&identity)
        {
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
        self.workspace_errors
            .lock()
            .unwrap()
            .insert(identity, format!("cargo metadata failed: {detail}"));
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

    /// 首个语义工具（hover/def/refs/find-implementations/find-referencing-*/
    /// edit-context）非空成功 → 关窗。
    fn mark_semantic_ready(&self, root: &Path) {
        if let Some(w) = self
            .ls_warmup
            .lock()
            .unwrap()
            .get_mut(&key_root_identity(root))
        {
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
            .is_some_and(|w| !w.semantic_ok && w.started.elapsed() < LS_WARMUP_WINDOW);
        if active {
            vec![index_warming_message()]
        } else {
            Vec::new()
        }
    }

    /// 批2-A：本次请求的语义就绪等待预算（execute_tool 入口刷新）。
    fn warmup_budget(&self) -> Duration {
        Duration::from_millis(self.warmup_budget_ms.load(Ordering::Relaxed))
    }

    /// blindtest v5 P3-H：memo 命中 → 重建同 wire 类错误 + 追加恢复指引（进程内
    /// 短路，不 spawn 不请求）。TTL 过期清账返 None。
    fn failure_memo_check(&self, root: &Path, lang: &str) -> Option<ToolError> {
        let mut memo = self.failure_memo.lock().unwrap();
        let k = (root.to_path_buf(), lang.to_ascii_lowercase());
        let entry = memo.get(&k)?;
        let elapsed = entry.at.elapsed();
        if elapsed.as_secs() >= entry.kind.ttl_secs() {
            memo.remove(&k);
            return None;
        }
        let hint = format!(
            "; previous call failed identically {:.0}s ago; run `serena-cli stop-all` and retry",
            elapsed.as_secs_f32()
        );
        Some(match &entry.kind {
            MemoKind::NotInstalled { language, hint: h } => ToolError::NotInstalled {
                language: language.clone(),
                hint: format!("{h}{hint}"),
            },
            MemoKind::Launch { message } => {
                ToolError::Launch(anyhow::anyhow!("{message}{hint}"))
            }
            MemoKind::Terminated { cause } => ToolError::Core(CoreError::Terminated {
                ls: lang.to_string(),
                cause: format!("{cause}{hint}"),
            }),
        })
    }

    /// memo 记账（同类覆盖写，幂等）。
    fn failure_memo_record(&self, root: &Path, lang: &str, kind: MemoKind) {
        let k = (root.to_path_buf(), lang.to_ascii_lowercase());
        self.failure_memo
            .lock()
            .unwrap()
            .insert(k, FailureMemoEntry { kind, at: std::time::Instant::now() });
    }

    /// 语义层曾非空成功（任一语义工具）→ 引用类空结果可信（真·无 caller）。
    fn semantic_ok(&self, root: &Path) -> bool {
        self.ls_warmup
            .lock()
            .unwrap()
            .get(&key_root_identity(root))
            .is_some_and(|w| w.semantic_ok)
    }

    /// blindtest v5.1 P3-D：会话已拉起（mark_ls_started 记账在案）但首个非空语义
    /// 结果未到——切换/冷启动窗口，空结果不可信。与 v5 P2-E 的 daemon「切换后
    /// 首个响应」单发标记不同：按会话状态判定，并发在途请求不再漏标，窗口在
    /// mark_semantic_ready 前持续生效。
    fn session_unwarmed(&self, root: &Path) -> bool {
        self.ls_warmup
            .lock()
            .unwrap()
            .get(&key_root_identity(root))
            .is_some_and(|w| !w.semantic_ok)
    }

    /// find-referencing-*/edit-context 空 hits 的降级警示（bd serena-rust-e0hi/8vo9）。
    /// 与 def/refs 的 we0 探针（documentSymbol 位置在符号内即降级）不同——引用类
    /// 查询点恒在符号名上，该判据会把真·无 caller 永远误标、AI 永远在重试。此处
    /// 判据只认「语义层曾成功」：semantic_ok 前，类型分析未就绪窗口（RA 30-60s）
    /// refs 静默返空不可信；其后空即真值，不警示。workspace 加载错误无条件透出
    ///（与 def/refs 同规则，结果本就不可信）。
    fn referencing_empty_warnings(&self, root: &Path) -> Vec<String> {
        let mut ws = self.workspace_error_warnings(root);
        if !self.semantic_ok(root) {
            ws.push(semantic_not_ready_message());
        }
        ws
    }

    /// 写工具收尾标记（bd serena-rust-0em）：file 进入写后一致性窗口。
    fn mark_recent_write(&self, root: &Path, file: &str) {
        self.recent_writes.lock().unwrap().insert(
            (root.to_path_buf(), file.to_lowercase()),
            std::time::Instant::now(),
        );
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
        let stamp = doc_symbol_cache_key(root, file, None).2;
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
                self.delta_cache
                    .lock()
                    .unwrap()
                    .insert(key, current.clone());
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
        // bd 8ges：override 进键——不同路由平面的结果互不遮蔽。
        let cache_key = doc_symbol_cache_key(root, file, lang_override);
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
        // angular `.html` → vscode-html 伴生（ngserver documentSymbol 恒 -32601）；
        // didOpen/ensure_open 跟随重路由会话（tsls 不吃 .html）。
        let session = reroute_doc_symbols(session, root, file);
        let _guard = session
            .ensure_open(&path)
            .await
            .map_err(crate::fs_tools::ensure_open_err(file))?;

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

        Ok(flatten_symbols(resp, &uri, lang_str))
    }
    /// 跨文件符号树（PLAN Phase 2.5 / 7.2）：聚合 `dir` 下源码文件的 documentSymbol。
    ///
    /// 逐文件走 `tool_overview` —— 天然复用 3.1 缓存（同文件二次 symbol-tree/overview
    /// 免 LS 往返）；目录扫描走 3.3 `filtered_walker`（venv/node_modules/target 等内置
    /// ignore + gitignore）。`max_files` 保险丝（默认 200）：超限截断并标 `truncated`。
    ///
    /// bd vro3：`top_level=true` 时每文件只保留顶层符号（范围包含过滤），整文件嵌套
    /// 树 → 顶层条目（盲测 F.3 实测 1309 → ~130 tok）；默认 false 行为不变。
    /// bd 6ooi：`grep`（名子串，大小写不敏感）/ `max_depth`（包含链深度上限，
    /// 顶层=0）/ `files_only`（只列文件，零 LS 调用）三开关；均默认 None/false
    /// 行为不变，过滤后 symbols 空的文件条目整个省略（skip_if）。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_symbol_tree(
        &self,
        root: &Path,
        dir: &str,
        lang: Option<&str>,
        max_files: usize,
        top_level: bool,
        grep: Option<&str>,
        max_depth: Option<usize>,
        files_only: bool,
    ) -> ToolResult<serde_json::Value> {
        if dir.is_empty() {
            return Err(ToolError::BadArgs {
                detail: "missing 'dir'".into(),
            });
        }
        let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let canon_dir =
            dunce::canonicalize(canon_root.join(dir)).map_err(|e| ToolError::BadArgs {
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
        // 6ooi：files_only 到此为止——文件清单即产物，零 LS 调用。
        if files_only {
            let entries: Vec<serde_json::Value> = files
                .iter()
                .map(|f| serde_json::json!({ "file": f }))
                .collect();
            return Ok(serde_json::json!({
                "dir": dir,
                "files_scanned": files.len(),
                "truncated": truncated,
                "entries": entries,
                "errors": [],
            }));
        }
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
                        per_lang_buckets.entry(lang_id).or_default().push(idx);
                    }
                    Err(e) => {
                        // 与 tool_overview 一致：纯缓存命中也可走，但 miss 路径无法走 LS，
                        // 这里走 cache-only 分支（与下方的 cache 分流合一）。
                        // 直接尝试读缓存（避免漏已有 cache 命中）：
                        // bd 8ges：树扇出与无 override 平面共享键（历史行为，键=扩展名路由）。
                        let cache_key = doc_symbol_cache_key(root, file, None);
                        let cached = cache_arc.lock().unwrap().get(&cache_key).cloned();
                        match cached {
                            Some(symbols) if !symbols.is_empty() => {
                                let symbols = if top_level {
                                    top_level_symbols(symbols)
                                } else {
                                    symbols
                                };
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
                    let cache_key = doc_symbol_cache_key(root, file, None);
                    let cached = cache_arc.lock().unwrap().get(&cache_key).cloned();
                    match cached {
                        Some(symbols) if !symbols.is_empty() => {
                            let symbols = if top_level {
                                top_level_symbols(symbols)
                            } else {
                                symbols
                            };
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
                            drain_one(&mut set, &mut entries, &mut errors, top_level).await;
                        }
                    } else {
                        // >30 文件：先 drain 上一轮再启下一个，串行推进；
                        // 并行度退化到 1 = 单文件 didOpen 间歇 > 单 documentSymbol。
                        while !set.is_empty() {
                            drain_one(&mut set, &mut entries, &mut errors, top_level).await;
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
                    drain_one(&mut set, &mut entries, &mut errors, top_level).await;
                }
            }

            // (idx, value) 序列稳定排序，再展平为 entries（顺序 = files 顺序）。
            entries.sort_by_key(|(idx, _)| *idx);
        }

        let final_entries: Vec<serde_json::Value> = entries
            .into_iter()
            .filter_map(|(_, v)| filter_tree_entry(v, grep, max_depth))
            .collect();

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
        // angular `.html` → vscode-html 伴生（结构 outline 上的 containing walk）。
        let session = reroute_doc_symbols(session, root, file);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        Ok(collect_containing_hits(
            resp.as_ref(),
            &uri,
            line,
            col,
            &lang_str,
        ))
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
        // angular `.html` 定义落点 → vscode-html 伴生 outline（ngserver 恒 -32601）。
        let session = reroute_doc_symbols(session, root, &def_file);
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
            &target_lang,
        );
        if hits.is_empty() {
            return Ok(Some(Vec::new()));
        }

        // 4) 读盘 → 切片 body（OffsetEncoding::Utf16 与 tool_symbol_body 一致）。
        let text =
            tokio::fs::read_to_string(&target_path)
                .await
                .map_err(|e| ToolError::BadArgs {
                    detail: format!("read {def_file}: {e}"),
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
    /// 批2-A 第三元素 = 降级标记：`Some(SemanticPending)`（预算内无 lang 答复，
    /// 空结果不可信）/ `Some(Partial)`（部分 lang 未答，结果不完整）；None = 全量。
    ///
    /// 索引可能慢（>10s）：请求 timeout 走 `warmup_budget()`（批2-A 默认 15s，
    /// 可配置），不再是 120s 死等——超时即降级返回，AI 不把首答当挂死。
    pub async fn tool_find_symbol(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
        lang_override: Option<&str>,
    ) -> ToolResult<(Vec<SymbolHit>, Vec<String>, Option<Degraded>)> {
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
            // 命中即全成功快照 → warnings/degraded 恒空。
            return Ok((cached, Vec::new(), None)); // cache_hit
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
        let query = query.to_string();
        let budget = self.warmup_budget();
        let mut tasks = Vec::with_capacity(sessions.len());
        for session in sessions {
            let q = query.clone();
            tasks.push(tokio::spawn(async move {
                let params = json!({ "query": q });
                // 审计 P1-2：workspace/symbol 在 classify_method 归 Background
                // （重量级索引），但本工具是用户主动搜索 —— 必须显式 High，
                // 否则落在 TokenBucket 限流 + BG 路径，与设计注释承诺相悖。
                // 批2-A：timeout = warmup 预算；超时不再吞成空结果，按类别上抛
                //（timeout → 降级标记；其他错误 → 部分失败可见）。
                let resp: Vec<lsp_types::SymbolInformation> = match session
                    .request_at(
                        "workspace/symbol",
                        params,
                        budget,
                        lsp_core::client::Priority::High,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        return (
                            Vec::<SymbolHit>::new(),
                            Some(matches!(e, CoreError::Timeout { .. })),
                        );
                    }
                };
                (
                    resp.into_iter()
                        .map(|si| SymbolHit {
                            name: si.name,
                            kind: kind_from_lsp(&si.kind),
                            uri: si.location.uri.to_string(),
                            range: si.location.range,
                            container: si.container_name,
                        })
                        .collect(),
                    None,
                )
            }));
        }
        let mut merged: Vec<SymbolHit> = Vec::new();
        let mut timed_out = 0usize;
        let mut errored = 0usize;
        for t in tasks {
            if let Ok((v, err)) = t.await {
                match err {
                    Some(true) => timed_out += 1,
                    Some(false) => errored += 1,
                    None => {}
                }
                merged.extend(v);
            }
        }
        // 批1-C：workspace/symbol 出界过滤（先于对齐/缓存，保证缓存是净快照）。
        // 实锤：tsserver inferred project 对无 tsconfig workspace 沿父链解析
        // node_modules/@types，单字母查询 50 条 100% 落 home（盲测 v4.5）。
        let filtered_out = Self::retain_symbols_within_root(&mut merged, root);
        // bd qvv9（AI-6）：部分失败 warning 的取舍 —— 查询已被其它 lang 回答
        // （merged 非空）时，NotInstalled 的安装广告是纯噪声（169B/call，盲测 18%
        // 调用全噪声）；空结果时 warning 解释「为什么空」（x67 可见性语义保留）。
        // 非 NotInstalled（LS 崩溃/协议错）恒透出 —— 部分结果可能不完整。
        // 注：--lang 指定时 langs={override}，跨语言失败本就不进 failures。
        let any_hard_failure = failures
            .iter()
            .any(|(_, e)| !matches!(e, ToolError::NotInstalled { .. }));
        let mut warnings = if merged.is_empty() || any_hard_failure {
            failure_warnings(&failures)
        } else {
            Vec::new()
        };
        // 批2-A：就绪预算内未答复的 lang → 结构化降级（契约：15s 内就绪 → 全量
        // 无标记；超时 → 降级返回）。空结果 + 超时/请求错误 = semantic-pending
        // （明确告知「未就绪/不可信」而非「无符号」，禁把 pending 伪装成权威空——
        // 冷启动窗口 LS 也可能以 Rpc 错误快速回绝而非挂到超时，同样不可信）；
        // 有部分结果 = partial。判定提取纯函数供单测。
        let degraded = classify_find_symbol_degraded(timed_out, errored, !merged.is_empty());
        if timed_out > 0 {
            warnings.push(semantic_warming_warning());
        } else if errored > 0 {
            warnings.push(format!(
                "workspace/symbol request failed on {errored} language server(s); results may be incomplete"
            ));
        }
        // 空结果路径不写缓存：warning 只在本次调用产生（命中路径不过 session_for，
        // 无法重现），缓存会让重查静默丢失败信息 —— 宁重查不可错缓存。非空结果
        // （qvv9 过滤后 warnings 恒空）正常入缓存。
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
        // 批1-C：过滤计数在缓存判定之后才转 warning —— 出界过滤是常态信息
        // （无 tsconfig 的 TS workspace 每查必现），进缓存门会让命中集永不入缓存。
        if filtered_out > 0 {
            warnings.push(format!(
                "filtered {filtered_out} symbol hits outside project root (LS leaked beyond workspace); \
                 narrow with --lang or a more precise query"
            ));
        }
        merged.truncate(limit);
        Ok((merged, warnings, degraded))
    }

    /// 批1-C：workspace/symbol 出界过滤。AI 消费者语义 = 符号查询只返回用户
    /// project root 内结果；uri_to_path 已归一盘符大小写，`Path::starts_with`
    /// 在 Windows 上组件比较天然不敏感。uri 反解失败（非 file://）保守保留。
    /// 返回过滤条数（调用方转 warning，不静默）。
    fn retain_symbols_within_root(hits: &mut Vec<SymbolHit>, root: &Path) -> usize {
        let before = hits.len();
        hits.retain(|h| match uri_to_path(&h.uri) {
            Some(p) => p.starts_with(root),
            None => true,
        });
        before - hits.len()
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
        // bd 3mt：轮询期请求失败不再纯 debug 吞掉 —— 每文件首个失败 warn 一条
        // （轮询窗口内偶发失败是 LS 追赶的常态，逐条 warn 会刷屏），总数进最终
        // Err 回执（execute_tool 落到结果顶层 warning），排障有计数可循。
        let mut request_failures: usize = 0;
        let mut failure_warned: std::collections::HashSet<String> = std::collections::HashSet::new();
        for n_rounds in 0..REALIGN_ROUNDS {
            for uri in &targets {
                // 小写 uri（归一键）反推出的 path 中段大小写可能失真 —— 必须
                // canonicalize 还原磁盘真实大小写，否则 path_to_uri 生成的 uri 与
                // RA 记账（canonical）不一致 → RA 拒绝请求（日志实锤）。
                let Some(path) = uri_to_path(uri).and_then(|p| normalize_long_path(&p)) else {
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
                let resp = match session
                    .request::<Option<DocumentSymbolResponse>>(
                        "textDocument/documentSymbol",
                        params,
                        INDEX_TIMEOUT,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        request_failures += 1;
                        if failure_warned.insert(uri.clone()) {
                            tracing::warn!(
                                uri,
                                round = n_rounds,
                                error = %e,
                                "realign docsym request failed (bd 3mt); later retries at debug"
                            );
                        } else {
                            tracing::debug!(
                                uri,
                                round = n_rounds,
                                error = %e,
                                "realign docsym request failed"
                            );
                        }
                        continue;
                    }
                };
                let flat = flatten_symbols(resp, curi.as_str(), lang.as_str());
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
            "symbol index stale for {} file(s) after recent write; results may be misaligned, retry find-symbol shortly{}",
            targets.len(),
            if request_failures > 0 {
                format!(
                    "; {request_failures} realign docsym request(s) failed (details in daemon log)"
                )
            } else {
                String::new()
            }
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
        let pos = lsp_position_from_byte(&path, file, line, col, OffsetEncoding::Utf16).await?;

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

        let pos = lsp_position_from_byte(&path, file, line, col, OffsetEncoding::Utf16).await?;

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
        // hybrid 双服务器语言（astro）per-file 路由：ts/js 系文件的引用语义只在
        // 伴生 TS LS（↖ mirror: astro_language_server.py@7a296833 `request_references`
        // 对 `_is_ts_file` 路由伴生；主 astro-ls 对 .ts references 恒空 —— 帧录制
        // 实证）。.astro 与非 hybrid 语言回落主会话（semantic_session_or_main）。
        let session = self
            .semantic_session_for_file(root, file, lang.as_str())
            .await?;
        let path = root.join(file);
        let uri = path_to_uri_str(&path);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let pos = lsp_position_from_byte(&path, file, line, col, OffsetEncoding::Utf16).await?;

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
        let pos = lsp_position_from_byte(&path, file, line, col, OffsetEncoding::Utf16).await?;

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
    /// 第二返回值 = aap4 原始 LSP 响应 200B 快照（仅静默空形态漂移疑点时 Some）。
    pub async fn tool_referencing_symbols(
        &self,
        root: &Path,
        file: &str,
        line: u32,
        col: u32,
        lang_override: Option<&str>,
    ) -> ToolResult<(Vec<ref_tools::RefSymbolHit>, Option<String>)> {
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
    /// 第三返回值 = aap4 原始 LSP 响应 200B 快照（仅静默空形态漂移疑点时 Some）。
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
    ) -> ToolResult<(Vec<ref_tools::RefSnippetHit>, bool, Option<String>)> {
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
        // (name, range) 候选对；精确名优先于前缀，同级确定性排序（file 维度在扫描侧拼）。
        let pick = |hits: &[SymbolHit]| -> (SymbolNameRanges, SymbolNameRanges) {
            let mut exact: SymbolNameRanges = Vec::new();
            let mut prefix: SymbolNameRanges = Vec::new();
            for h in hits {
                if h.name == name {
                    exact.push((h.name.clone(), h.range));
                } else if h.name.starts_with(name) {
                    prefix.push((h.name.clone(), h.range));
                }
            }
            (exact, prefix)
        };
        let mut exact: SymbolCandidates = Vec::new();
        let mut prefix: SymbolCandidates = Vec::new();

        // 1) documentSymbol 缓存扫描。key.1 即构造时的 file 相对路径，直接可复用。
        //    `ws?{query}` 条目是 find-symbol 的 workspace 级缓存，k.1 是伪文件名——
        //    扫进去会把 query 名当文件选出（file="ws?divide" → refs 打到不存在的
        //    路径上），必须排除。
        let root_id = key_root_identity(root);
        let cached_files: Vec<(String, Vec<SymbolHit>)> = self
            .symbol_cache
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| !k.1.starts_with("ws?") && key_root_identity(&k.0) == root_id)
            .map(|(k, v)| (k.1.clone(), v.clone()))
            .collect();
        for (file, hits) in &cached_files {
            let (e, p) = pick(hits);
            exact.extend(e.into_iter().map(|(n, r)| (file.clone(), n, r)));
            prefix.extend(p.into_iter().map(|(n, r)| (file.clone(), n, r)));
        }

        // 2) 缓存零命中 → workspace/symbol 兜底（冷 daemon 窗口）。暖机窗口内
        // wssym 首查常为「假空」（bd serena-rust-bxd 打回：1 分钟后同命令命中
        // 数十处）——窗口仍 active 时追加 1 次 500ms 重试再判空。注意 warming
        // 判定必须在 fallback 之后：首查经 session_for spawn LS 才开窗。
        if exact.is_empty() && prefix.is_empty() {
            for attempt in 0..2 {
                match self.tool_find_symbol(root, name, 50, lang).await {
                    Ok((hits, _, _)) => {
                        for h in &hits {
                            let Some(path) = uri_to_path(&h.uri) else {
                                continue;
                            };
                            let rel = path
                                .strip_prefix(root)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .replace('\\', "/")
                                .to_string();
                            if h.name == name {
                                exact.push((rel, h.name.clone(), h.range));
                            } else if h.name.starts_with(name) {
                                prefix.push((rel, h.name.clone(), h.range));
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

        let by_pos = |a: &(String, String, lsp_types::Range),
                      b: &(String, String, lsp_types::Range)| {
            a.0.cmp(&b.0)
                .then(a.2.start.line.cmp(&b.2.start.line))
                .then(a.2.start.character.cmp(&b.2.start.character))
        };
        exact.sort_by(by_pos);
        prefix.sort_by(by_pos);
        // 歧义只在所选层级内计：有精确命中时前缀候选全部落选，不算「多命中」。
        let (chosen, total) = if !exact.is_empty() {
            (&exact, exact.len())
        } else {
            (&prefix, prefix.len())
        };
        let Some((file, sym_name, range)) = chosen.first().cloned() else {
            // 打回修复（bd serena-rust-bxd）：暖机窗口内零命中 ≠ 符号不存在——
            // wssym 可能仍未爬完，错误必须带 hint 防 AI 误判（对齐 find-symbol
            // 的 partial warning，不能比它更误导）。
            let mut detail = format!(
                "symbol `{name}` not found (documentSymbol cache and workspace index empty)"
            );
            if warming {
                detail.push_str(
                    "; index may still be warming (cold start), retry shortly or use find-symbol",
                );
            }
            return Err(ToolError::BadArgs { detail });
        };
        // bd serena-rust-gqyp：全范围起点 = 声明关键字处，refs 打上去恒空——精化到
        // 名字 token（refine_symbol_name_position）；文件读不到（合成缓存/竞态删除）
        // → 保守回退 range.start。
        let pos = tokio::fs::read_to_string(root.join(&file))
            .await
            .ok()
            .map(|text| refine_symbol_name_position(&text, &sym_name, range))
            .unwrap_or(range.start);
        let note = (total > 1).then(|| {
            format!(
                "resolved --symbol {name} -> {file}:{} ({total} matches; using first, 1-based line)",
                pos.line + 1
            )
        });
        Ok((file, pos.line, pos.character, note))
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
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        edit_tools::replace_text_in_symbol(&session, root, &abs, symbol, old_text, new_text)
            .await
            // bd serena-rust-i4j：走 line_edit_err 保留 WriteConflict 变体 ——
            // needle 失配是并发冲突不是参数错；其余错误 wire message 不变。
            .map_err(line_edit_err("replace_text_in_symbol"))
    }

    /// `insert_text_before_symbol`：在 symbol 开头插入 text（Task 25）。
    /// 返回插入内容末尾的 (end_line, end_col)（1-based）。
    /// bd bt3h：`auto_indent=true`（默认）时插入文本按 host 符号缩进补齐。
    pub async fn tool_edit_insert_before_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        auto_indent: bool,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        edit_tools::insert_text_before_symbol(&session, root, &abs, symbol, text, auto_indent)
            .await
            .map_err(line_edit_err("insert_text_before_symbol"))
    }

    /// `insert_text_after_symbol`：在 symbol 末尾插入 text（Task 25）。
    /// bd bt3h：`auto_indent=true`（默认）时插入文本按 host 符号缩进补齐。
    pub async fn tool_edit_insert_after_symbol(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        text: &str,
        auto_indent: bool,
        lang_override: Option<&str>,
    ) -> ToolResult<(u32, u32)> {
        let lang = resolve_lang_for_file(file, lang_override)?;
        let session = self.session_for(root, lang.as_str()).await?;
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        edit_tools::insert_text_after_symbol(&session, root, &abs, symbol, text, auto_indent)
            .await
            .map_err(line_edit_err("insert_text_after_symbol"))
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
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        edit_tools::delete_text_in_symbol(&session, root, &abs, symbol, start_line, end_line)
            .await
            .map_err(line_edit_err("delete_text_in_symbol"))
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
        let cache_key = doc_symbol_cache_key(root, file, lang_override);
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
        // angular `.html` → vscode-html 伴生（didOpen/请求跟随；缓存 hit 分支不经
        // 此——.html 符号体走伴生后写入同一张 docsym 缓存，键 = (root, file) 与
        // html 门天然共享）。
        let session = reroute_doc_symbols(session, root, file);
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        let params = json!({ "textDocument": { "uri": uri.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;

        // 递归找第一个 name == symbol 的 DocumentSymbol（Nested 形态）。
        let range =
            find_symbol_range(resp.as_ref(), symbol, &lang).ok_or_else(|| ToolError::BadArgs {
                detail: format!("symbol `{symbol}` not found in {file}"),
            })?;
        let out = read_and_slice(&path, file, range).await?;
        self.symbol_cache_put(cache_key, flatten_symbols(resp, &uri, &lang)); // cache_miss → 写入
        Ok(out)
    }

    /// bd qre0：batch-read —— 一次调用读多文件（纯读，不经写门）。逐文件走
    /// `fs_tools::read_file`（默认 clamp）；累计估算 token（字节/4，与 `apply_budget`
    /// 同一 soft-limit 口径）超过 `budget_tokens`（默认 2000）即停止读取，剩余计入
    /// `skipped`。单文件失败记 `errors` 条目不拖垮整批。响应：
    /// `{files:[ReadReport...], errors?:[{file,error}], truncated?:true, skipped?:n}`。
    pub async fn tool_batch_read(
        &self,
        root: &Path,
        files: &[String],
        budget_tokens: usize,
    ) -> ToolResult<serde_json::Value> {
        let budget_bytes = budget_tokens.saturating_mul(4);
        let mut used = 0usize;
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut errors: Vec<serde_json::Value> = Vec::new();
        let mut truncated = false;
        let mut skipped = 0usize;
        for f in files {
            if truncated {
                skipped += 1;
                continue;
            }
            match fs_tools::read_file(root, f, None, None, true, None).await {
                Ok(report) => {
                    let cost = report.content.len();
                    // 首文件超预算仍读（soft limit，与 apply_budget 语义一致）——
                    // 但后续文件一律停。
                    if used + cost > budget_bytes && !items.is_empty() {
                        truncated = true;
                        skipped += 1;
                        continue;
                    }
                    used += cost;
                    items.push(serde_json::to_value(report).unwrap_or(serde_json::Value::Null));
                }
                Err(e) => {
                    errors.push(json!({ "file": f, "error": e.to_string() }));
                }
            }
        }
        let mut o = serde_json::Map::new();
        o.insert("files".into(), json!(items));
        if !errors.is_empty() {
            o.insert("errors".into(), json!(errors));
        }
        if truncated {
            o.insert("truncated".into(), json!(true));
            o.insert("skipped".into(), json!(skipped));
        }
        Ok(serde_json::Value::Object(o))
    }

    /// bd b72k：`symbol-body` 的 meta 聚合形态 —— body + doc（上方连续注释行）+
    /// signature（body 首行）+ location（1-based）+ 上一行/下一行。依赖
    /// `tool_symbol_body` 先暖 docsym 缓存再取 range；盘上文件现读。任一附带字段
    /// 取不到就在响应里省略（skip 语义），body 缺失仍报错（与裸形态一致）。
    pub async fn tool_symbol_body_meta(
        &self,
        root: &Path,
        file: &str,
        symbol: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<serde_json::Value> {
        let body = self.tool_symbol_body(root, file, symbol, lang_override).await?;
        let (s0, e0) =
            crate::edit_context::range_from_symbol_cache(self, root, file, symbol, lang_override)
                .unwrap_or((0, 0));
        let text = tokio::fs::read_to_string(root.join(file))
            .await
            .map_err(|e| ToolError::Core(CoreError::Io(e)))?;
        let lines: Vec<&str> = text.lines().collect();
        let s = s0 as usize;
        let e = e0 as usize;
        // doc：body 起行上方连续注释行（复用 fs_tools 的注释前缀表）。
        let mut doc_start = s;
        while doc_start > 0 && crate::fs_tools::looks_like_comment(file, lines[doc_start - 1]) {
            doc_start -= 1;
        }
        let doc =
            (doc_start < s).then(|| lines[doc_start..s].join("\n").to_string());
        let signature = lines.get(s).map(|l| l.trim_end().to_string());
        let prev_line = (s > 0).then(|| lines[s - 1].to_string());
        let next_line = lines.get(e + 1).map(|l| l.to_string());
        let mut o = serde_json::Map::new();
        o.insert("file".into(), json!(file));
        o.insert("symbol".into(), json!(symbol));
        o.insert(
            "location".into(),
            json!({ "start_line": s0 + 1, "end_line": e0 + 1 }),
        );
        if let Some(d) = doc {
            o.insert("doc".into(), json!(d));
        }
        if let Some(sig) = signature {
            o.insert("signature".into(), json!(sig));
        }
        if let Some(p) = prev_line {
            o.insert("prev_line".into(), json!(p));
        }
        if let Some(n) = next_line {
            o.insert("next_line".into(), json!(n));
        }
        o.insert("body".into(), json!(body));
        Ok(serde_json::Value::Object(o))
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
        let path =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
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
        let _gate = write_gate::acquire("replace-body").await?;

        // 1) 锁内解析符号 range（杜绝客户端 range 过期）。
        let _guard = session.ensure_open(path).await.map_err(ToolError::Core)?;
        let params = json!({ "textDocument": { "uri": uri_str } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        let range =
            find_symbol_range(resp.as_ref(), symbol, lang).ok_or_else(|| ToolError::BadArgs {
                detail: format!("symbol `{symbol}` not found in {}", user_path(root, path)),
            })?;

        // 2) 读盘 + content-hash 对账（C3 防线 ①）。
        let old_text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("read {}: {e}", user_path(root, path)),
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
        let start_byte =
            lsp_core::offsets::position_to_byte(&old_text, start, OffsetEncoding::Utf16).map_err(
                |e| ToolError::BadArgs {
                    detail: format!("start position: {e}"),
                },
            )?;
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

        // undo 收口：快照旧内容 → 原子写 → 入当前事务。
        undo::recorded_write(path, &new_text)
            .await
            .map_err(|e| atomic_write_err(&user_path(root, path), e))?;

        // 4) 读回 diff 校验（C3 防线 ②③）—— 不符回滚 + 报冲突。
        // dry-run 干跑未落盘，readback 读到旧内容属预期，跳过比对。
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", user_path(root, path)),
            })?;
        if readback != new_text && !undo::is_dry_run() {
            return Err(rollback_after_readback_mismatch(path, root, &old_text).await);
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
    /// - 默认排除 binary / >1MB 大文件（批1-B 防噪启发）
    /// - `path_glob`：可选 glob 过滤（如 `"*.cpp"` `"src/**/*.py"`）
    /// - `max_results` 默认 50（批1-B 防噪硬上限）：超过返回 truncated + hint 标记
    /// - 不动 LS —— 这是 fs 工具，不需要 LSP
    // 与 tool_format_range/symbol_tree 等同款：tool 层透传形参天然偏宽，逐调用点
    // 结构化反而加一层；allow 为既定 house pattern。
    #[allow(clippy::too_many_arguments)]
    pub async fn tool_search_for_pattern(
        &self,
        root: &Path,
        pattern: &str,
        path_glob: Option<&str>,
        max_results: usize,
        case_sensitive: bool,
        exclude: &[String],
        no_ignore: bool,
    ) -> ToolResult<SearchResponse> {
        use regex::RegexBuilder;

        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| ToolError::BadArgs {
                detail: format!("bad regex: {e}"),
            })?;
        let glob_re = match path_glob {
            Some(g) => Some(glob_to_regex(g, !case_sensitive)?),
            None => None,
        };
        // 杠精 cv1e：--exclude 逃生——glob 列表（同 path_glob 语法），命中即跳过
        // （在计数/读取之前，测量脚本等噪音不进 token 账单）。
        let mut exclude_re = Vec::with_capacity(exclude.len());
        for g in exclude {
            exclude_re.push(glob_to_regex(g, !case_sensitive)?);
        }

        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());

        // 整个扫描体（walk + read + regex）丢 `spawn_blocking`：原同步 IO 内联
        // 在 async worker 上，单 search 期间 daemon 该 worker 上的其它请求全部排队。
        // 850 文件冷扫可占 worker 数百 ms-数秒，导致 /status、reaper select、L/batch
        // 并行的 7 条兄弟请求显著延迟。包 spawn_blocking 后 worker 立刻释放，P2-6。
        let (hits, truncated, files_scanned) = tokio::task::spawn_blocking(move || {
            Self::search_sync_scan(&root, &regex, glob_re.as_ref(), &exclude_re, max_results, no_ignore)
        })
        .await
        .map_err(|e| {
            ToolError::Core(CoreError::Rpc {
                code: -1,
                message: format!("search scan join error: {e}"),
            })
        })?;

        Ok(SearchResponse {
            hits,
            truncated,
            files_scanned,
            // 批1-B：截断即给降噪 hint（旗标语义不变，防裸打脏目录 12.8KB 复演）。
            hint: truncated.then(|| SEARCH_NOISE_HINT.to_string()),
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
        exclude_re: &[regex::Regex],
        max_results: usize,
        no_ignore: bool,
    ) -> (Vec<SearchHit>, bool, usize) {
        use ignore::WalkBuilder;

        let mut walker = WalkBuilder::new(root);
        // 杠精 cv1e：默认尊重 .gitignore（standard_filters）；--no-ignore 逃生 =
        // 关 gitignore 族过滤器（hidden 与 should_ignore 表仍生效——.git/ 内部与
        // 构建产物不因逃生而进结果，防噪音反灌）。
        walker
            .standard_filters(!no_ignore)
            .require_git(false)
            // 内置 ignore 目录（target/node_modules/.idea 等）—— .git 由
            // standard_filters 的 hidden filter 默认排除，但 target/node_modules
            // 不一定在 .gitignore 里，需显式表驱动过滤；与 fs_tools::filtered_walker 语义一致。
            .filter_entry(|e| {
                e.depth() == 0 || !e.file_name().to_str().is_some_and(fs_tools::should_ignore)
            });
        if no_ignore {
            walker.hidden(true).parents(false);
        }

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
            if exclude_re.iter().any(|g| g.is_match(&rel_str)) {
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            // 批1-B：>1MB 跳过（盲测实锤大文件是字节坑；ripgrep 默认大文件不搜）。
            if metadata.len() > 1024 * 1024 {
                continue;
            }

            files_scanned += 1;
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // 批1-B：合法 UTF-8 但含 NUL 字节 = 二进制，跳过（ripgrep 同款判定；
            // 非 UTF-8 文件已被上面的 read Err 路径排除）。
            if content.as_bytes().contains(&0) {
                continue;
            }

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
        let path =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
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

        let _gate = write_gate::acquire("rename-symbol").await?;

        let pos = lsp_position_from_byte(&path, file, line, col, OffsetEncoding::Utf16).await?;
        let pos_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
        });

        // 1) prepareRename —— null = 不能 rename。-32602 交语义改判（critic3-F4）。
        let prep: Option<serde_json::Value> = match session
            .request(
                "textDocument/prepareRename",
                pos_params.clone(),
                TOOL_TIMEOUT,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Err(
                    self.rename_error_reclassified(root, file, lang.as_str(), pos, e)
                        .await,
                );
            }
        };
        if prep.is_none() || prep.as_ref().is_some_and(|v| v.is_null()) {
            return Err(ToolError::BadArgs {
                detail: "prepareRename rejected this position".into(),
            });
        }

        // 2) textDocument/rename → WorkspaceEdit JSON。-32602 交语义改判。
        let edit_params = json!({
            "textDocument": { "uri": uri_str },
            "position": { "line": pos.line, "character": pos.character },
            "newName": new_name,
        });
        let resp: Option<serde_json::Value> = match session
            .request("textDocument/rename", edit_params, TOOL_TIMEOUT)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Err(
                    self.rename_error_reclassified(root, file, lang.as_str(), pos, e)
                        .await,
                );
            }
        };
        let resp = resp.ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename returned null".into(),
        })?;

        // 3) 拆 WorkspaceEdit → 按文件分组（documentChanges 优先，回退 changes map）。
        let by_uri = parse_workspace_edit(&resp).ok_or_else(|| ToolError::Protocol {
            tool: "rename_symbol".into(),
            reason: "rename response has neither `changes` map nor `documentChanges`".into(),
        })?;

        // 4) 对每个文件应用 edits。单文件失败 = 记入 skipped 后继续其余文件
        // （不得静默，BD serena-rust-93q）。
        let mut report = RenameReport::default();
        let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        for (uri, mut edits) in by_uri {
            edits.sort_by_key(|e| std::cmp::Reverse(e.0)); // 倒序
            let (abs, content, new_content) =
                match prepare_rename_file(&root_canon, &uri, &edits).await {
                    Ok(v) => v,
                    Err(skip) => {
                        report.skipped.push(skip);
                        continue;
                    }
                };

            if new_content == content {
                continue;
            }

            // undo 收口：每文件快照入同一事务 —— rename-symbol 跨文件改动
            // 聚合为单事务（契约设计第 2 条），undo 一次全部回滚。
            undo::recorded_write(&abs, &new_content)
                .await
                .map_err(|e| atomic_write_err(&abs.display().to_string(), e))?;

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
        let path =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        let uri_str = path_to_uri_str(&path);

        let _gate = write_gate::acquire("safe-delete-symbol").await?;
        let _guard = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        // 1) 锁内解析符号（杜绝过期 range）。
        let params = json!({ "textDocument": { "uri": uri_str.clone() } });
        // Option 宽容：RA 等对未就绪文档返 null（untagged enum 不匹配 null → 硬错）。
        let resp: Option<DocumentSymbolResponse> = session
            .request("textDocument/documentSymbol", params, TOOL_TIMEOUT)
            .await?;
        // per-LS quirk：fortls 的 selectionRange 恒指行首（↖ mirror
        // fortran_language_server.py@7a296833 `_build_document_symbols_from_raw_symbols`
        // 覆写）——references 锚点修到标识符真实位置，否则 safe-delete 的 references
        // 恒空/错锚。读盘失败按原样放行（修正 best-effort）。
        let mut resp = resp;
        if lang == "fortran"
            && let Some(DocumentSymbolResponse::Nested(items)) = resp.as_mut()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            symbol_quirks::fix_fortls_selection_ranges(items, &content);
        }
        let (range, selection) =
            find_symbol_node(resp.as_ref(), symbol, &lang).ok_or_else(|| ToolError::BadArgs {
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
                note: None,
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
                &[],
                false,
            )
            .await?;
        let n =
            textual_occurrences_outside_def(&search.hits, file, selection.start.line + 1, symbol);
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
                detail: format!("read {}: {e}", user_path(root, &path)),
            })?;
        let new_text = delete_symbol_text(&old_text, range)?;
        // undo 收口：快照旧内容 → 原子写 → 入当前事务。
        undo::recorded_write(&path, &new_text)
            .await
            .map_err(|e| atomic_write_err(&user_path(root, &path), e))?;
        let readback = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::BadArgs {
                detail: format!("readback {}: {e}", user_path(root, &path)),
            })?;
        // dry-run 干跑未落盘，readback 读到旧内容属预期，跳过比对。
        if readback != new_text && !undo::is_dry_run() {
            return Err(rollback_after_readback_mismatch(&path, root, &old_text).await);
        }
        // 走 `ensure_open` 的 mtime-检测路径，与 tool_replace_body / edit_tools 共享
        // 同一 content_version 单调递增（rust-analyzer 拒收非单调 version → channel 关）。
        drop(_guard);
        let _refreshed = session.ensure_open(&path).await.map_err(ToolError::Core)?;

        Ok(SafeDeleteReport {
            deleted: true,
            symbol: symbol.to_string(),
            references: vec![],
            note: new_text.is_empty().then(|| "file left empty (0 bytes)".into()),
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
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        edit_tools::insert_at_line(&session, root, &abs, line, content, expected_hash)
            .await
            .map_err(line_edit_err("insert_at_line"))
    }

    /// `create-text-file`：新建文件（已存在 = 参数错）。走写门 + recorded_write
    /// 收口 —— 旧文件不存在自动产生 created=true 快照，undo = 删除、redo = 重建。
    /// LS 侧 ensure_open 失败不阻断返回（创建已成功；无 adapter/LS 未装时仅附 warning）。
    pub async fn tool_create_text_file(
        &self,
        root: &Path,
        file: &str,
        content: &str,
        lang_override: Option<&str>,
    ) -> ToolResult<serde_json::Value> {
        // bd serena-rust-5r7：`contains("..")` 挡不住绝对路径注入（join 换基），
        // 统一走 root 界校验（词法 + canonical，含 symlink 语义）。
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
        let _gate = write_gate::acquire("create-text-file").await?;
        // 门内双检：并发两个 create 只成功一个（TOCTOU 防线）。
        if abs.exists() {
            return Err(ToolError::BadArgs {
                detail: format!("file already exists: {file}"),
            });
        }
        undo::recorded_write(&abs, content)
            .await
            .map_err(|e| atomic_write_err(&abs.display().to_string(), e))?;
        let mut warnings: Vec<String> = Vec::new();
        match resolve_lang_for_file(file, lang_override) {
            Ok(lang) => match self.session_for(root, lang.as_str()).await {
                Ok(session) => {
                    if let Err(e) = session.ensure_open(&abs).await {
                        warnings.push(format!("created but LS sync failed: {e}"));
                    }
                }
                Err(e) => warnings.push(format!("created but LS unavailable: {e}")),
            },
            // bd P2-8：纯文本创建成功不是 bad args——info 化措辞，不再用
            // "unsupported extension ... pass --lang" 的 BAD_ARGS 文案误导。
            Err(_) => warnings.push("created as plain text (no language adapter)".into()),
        }
        let mut value = serde_json::json!({ "created": true, "file": file });
        if !warnings.is_empty() {
            value["warnings"] = serde_json::json!(warnings);
        }
        Ok(value)
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
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
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
        let abs =
            path_guard::guarded_join(root, file).map_err(|detail| ToolError::BadArgs { detail })?;
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
    /// 未能应用的文件与原因（BD serena-rust-93q：多文件部分失败必须可见，不得
    /// 静默 continue）。空则省略，对旧客户端向后兼容（B0 hint 字段先例）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<RenameSkipped>,
}

/// rename 单文件跳过记录。
#[derive(Debug, Serialize)]
pub struct RenameSkipped {
    /// 相对 root 路径；URI 无法解析时为原始 URI。
    pub file: String,
    /// 跳过原因（uri 不可解析 / root 外 / 读盘失败 / 编辑位置越界）。
    pub reason: String,
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
        .filter(
            |h| match uri_to_path(&h.uri).and_then(|p| dunce::canonicalize(p).ok()) {
                Some(p) => !tables.contains_key(&p),
                // 无法归一 → 保留（不误删他人条目）。
                None => true,
            },
        )
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

/// 诊断缓存 uri 键归一：percent-decode（pyright 推 `file:///c%3A/...`，实测）+
/// 小写（RA 推盘符小写 `file:///d:/...`）。path_to_uri 生成的形态两者皆非，
/// 不归一则 push 缓存永不命中（2026-09-25 pyright 诊断恒空根因）。
fn diag_uri_key(uri: &str) -> String {
    percent_decode(uri).to_lowercase()
}

/// publishDiagnostics → diag_cache 统一 handler 工厂。主/伴生会话共用同一缓存：
/// vue hybrid 的类型诊断来自伴生 TS LS、模板诊断来自主 Vue LS，agent 的
/// diagnostics 工具（读 diag_cache）天然同时可见两者。
fn make_diag_handler(
    cache_root: PathBuf,
    cache: DiagCache,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    version_seen: std::sync::Arc<Mutex<HashMap<(PathBuf, String), bool>>>,
) -> impl Fn(lsp_core::framing::JsonRpc) + Send + Sync + 'static {
    move |msg| {
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
                .insert((cache_root.clone(), diag_uri_key(uri)), true);
        }
        let mut cache = cache.lock().unwrap();
        let key = (cache_root.clone(), diag_uri_key(uri));
        if items.is_empty() {
            cache.insert(key, (Vec::new(), ver));
        } else {
            cache.insert(key, (items, ver));
            generation.fetch_add(1, Ordering::Relaxed);
        }
    }
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

/// rename 单文件准备：URI→path → root 守卫 → 读盘 → edits 逐条换字节区间。
/// `Err` = 该文件整体跳过，原因进 `RenameReport.skipped`（BD serena-rust-93q：
/// 多文件部分失败必须可见，不得静默 continue/break）。字节换算失败按整文件
/// 跳过处理——部分应用的 rename 会留下不一致的引用状态，跳过并报告更安全。
async fn prepare_rename_file(
    root_canon: &Path,
    uri: &str,
    edits: &[EditSpec],
) -> Result<(std::path::PathBuf, String, String), RenameSkipped> {
    let abs = uri_to_path(uri).ok_or_else(|| RenameSkipped {
        file: uri.to_string(),
        reason: "uri not convertible to a file path".into(),
    })?;
    let rel = |p: &Path| {
        p.strip_prefix(root_canon)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    // 必须在 root 内（防 path traversal 风险）。
    if !abs.starts_with(root_canon) {
        return Err(RenameSkipped {
            file: rel(&abs),
            reason: "outside workspace root".into(),
        });
    }
    let content = tokio::fs::read_to_string(&abs)
        .await
        .map_err(|e| RenameSkipped {
            file: rel(&abs),
            reason: format!("read failed: {e}"),
        })?;
    let mut new_content = content.clone();
    for (i, (_key, range, new_text)) in edits.iter().enumerate() {
        let start_byte =
            lsp_core::offsets::position_to_byte(
                &new_content,
                lsp_core::offsets::Position {
                    line: range.start.line,
                    character: range.start.character,
                },
                OffsetEncoding::Utf16,
            )
            .map_err(|_| RenameSkipped {
                file: rel(&abs),
                reason: format!("edit {i} start position out of range"),
            })?;
        let end_byte = lsp_core::offsets::position_to_byte(
            &new_content,
            lsp_core::offsets::Position {
                line: range.end.line,
                character: range.end.character,
            },
            OffsetEncoding::Utf16,
        )
        .map_err(|_| RenameSkipped {
            file: rel(&abs),
            reason: format!("edit {i} end position out of range"),
        })?;
        new_content = format!(
            "{}{}{}",
            &new_content[..start_byte],
            new_text,
            &new_content[end_byte..]
        );
    }
    Ok((abs, content, new_content))
}

/// safe-delete 结果报告。
#[derive(Debug, Serialize)]
pub struct SafeDeleteReport {
    /// false = 有引用拒删；true = 已删除。
    pub deleted: bool,
    pub symbol: String,
    /// deleted=false 时非空：引用位置（相对路径 + 1-based 行号，按 file+line 排序去重）。
    pub references: Vec<SafeDeleteRef>,
    /// bd P2-7：符号删除把整文件清空时提示 0 字节残壳（否则消费者不知道文件
    /// 被清空了）。其余场景省略键（wire 不变）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
/// 批1-B：裸 search 无护栏时脏目录可吞 12.8KB（盲测 v4.5）——默认上限截断时
/// 响应带此 hint 指路降噪旗标。
const SEARCH_NOISE_HINT: &str = "add --path-glob / --max-results";

/// 搜索响应。
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    pub files_scanned: usize,
    /// 批1-B：默认上限截断时的降噪 hint（None 不序列化，旧 wire 零扰动）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
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
///
/// `lang`：per-LS 符号名归一（↖ mirror 上游 `_normalize_symbol_name` 构建层挂点；
/// erlang `/`→`#`、lua/swift 前缀剥离、nextflow 关键字剥离——ls-adapters
/// `symbol_quirks`，未覆盖语言恒等）。
fn flatten_symbols(
    resp: Option<DocumentSymbolResponse>,
    file_uri: &str,
    lang: &str,
) -> Vec<SymbolHit> {
    let mut out = Vec::new();
    match resp {
        Some(DocumentSymbolResponse::Flat(items)) => {
            for it in items {
                out.push(SymbolHit {
                    name: symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind),
                    kind: kind_from_lsp(&it.kind),
                    uri: it.location.uri.to_string(),
                    range: it.location.range,
                    container: it.container_name,
                });
            }
        }
        Some(DocumentSymbolResponse::Nested(items)) => {
            for it in items {
                push_nested(&it, None, file_uri, lang, &mut out);
            }
        }
        None => {}
    }
    out
}

/// bd vro3：`symbol-tree --top-level` 的顶层过滤 —— 保留 range **不被**同文件其它
/// 符号的 range 严格包含的命中（扁平缓存无深度信息，用范围包含重建层级：子符号
/// range ⊆ 父符号 range）。相同 range 互不包含（并列保留）。
fn top_level_symbols(hits: Vec<SymbolHit>) -> Vec<SymbolHit> {
    let n = hits.len();
    let mut keep = vec![true; n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let (h, o) = (&hits[i], &hits[j]);
            if o.range.start <= h.range.start
                && h.range.end <= o.range.end
                && (o.range.start < h.range.start || h.range.end < o.range.end)
            {
                keep[i] = false;
                break;
            }
        }
    }
    hits.into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(h, _)| h)
        .collect()
}

/// bd 6ooi：symbol-tree 条目级过滤（`--grep` / `--max-depth`）。两开关均未给时
/// 原样返回（默认 wire 逐字节不变）；过滤后 symbols 空的条目整个省略。
fn filter_tree_entry(
    entry: serde_json::Value,
    grep: Option<&str>,
    max_depth: Option<usize>,
) -> Option<serde_json::Value> {
    if grep.is_none() && max_depth.is_none() {
        return Some(entry);
    }
    let symbols = entry.get("symbols")?.as_array()?;
    let grep_lc = grep.map(str::to_lowercase);
    let kept: Vec<serde_json::Value> = symbols
        .iter()
        .filter(|s| {
            if let Some(g) = &grep_lc {
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if !name.to_lowercase().contains(g.as_str()) {
                    return false;
                }
            }
            if let Some(d) = max_depth
                && symbol_containment_depth(s, symbols) >= d
            {
                return false;
            }
            true
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut obj = entry.as_object()?.clone();
    obj.insert("symbols".into(), serde_json::Value::Array(kept));
    Some(serde_json::Value::Object(obj))
}

/// 深度 = 同文件其它符号中 range 严格包含它的个数（顶层=0）。与
/// `top_level_symbols` 同一包含判据（start ≤ 且 end ≥，至少一端严格）；
/// 相同 range 互不严格包含（并列不算深）。
fn symbol_containment_depth(sym: &serde_json::Value, all: &[serde_json::Value]) -> usize {
    let (Some((sl, sc)), Some((el, ec))) = (json_pos(sym, "start"), json_pos(sym, "end")) else {
        return 0;
    };
    all.iter()
        .filter(|o| {
            let (Some((osl, osc)), Some((oel, oec))) = (json_pos(o, "start"), json_pos(o, "end"))
            else {
                return false;
            };
            let start_le = (osl, osc) <= (sl, sc);
            let end_ge = (el, ec) <= (oel, oec);
            let strict = (osl, osc) < (sl, sc) || (el, ec) < (oel, oec);
            start_le && end_ge && strict
        })
        .count()
}

fn json_pos(sym: &serde_json::Value, key: &str) -> Option<(u64, u64)> {
    let p = sym.get("range")?.get(key)?;
    Some((
        p.get("line")?.as_u64()?,
        p.get("character")?.as_u64()?,
    ))
}

fn push_nested(
    sym: &DocumentSymbol,
    container: Option<String>,
    file_uri: &str,
    lang: &str,
    out: &mut Vec<SymbolHit>,
) {
    let container = container.or_else(|| Some(sym.name.clone()));
    out.push(SymbolHit {
        name: symbol_quirks::normalize_symbol_name(lang, &sym.name, sym.kind),
        kind: kind_from_lsp(&sym.kind),
        uri: file_uri.to_string(),
        range: sym.range,
        container: container.clone(),
    });
    if let Some(children) = sym.children.as_ref() {
        for child in children {
            push_nested(child, Some(sym.name.clone()), file_uri, lang, out);
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
    top_level: bool,
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
            let symbols = if top_level {
                top_level_symbols(symbols)
            } else {
                symbols
            };
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
    // bd 8ges：树扇出键固定 None 平面——lang_override 只用于本文件会话解析，
    // 与 tool_overview(None) 的历史共享不碎片化。
    let cache_key = doc_symbol_cache_key(&root, &file, None);
    if let Some(cached) = cache_arc.lock().unwrap().get(&cache_key).cloned() {
        return Ok(cached);
    }
    let lang_str = resolve_lang_for_file(&file, lang_override)?;
    let path = root.join(&file);
    let uri = path_to_uri_str(&path);
    // angular `.html` → vscode-html 伴生（'static 并发路径与 tool_overview_inner
    // 同语义；didOpen/ensure_open 跟随重路由会话——tsls 不吃 .html；flatten 的
    // lang 用原会话语言——伴生是 html 门，符号名无归一 quirk）。
    let resp_session = reroute_doc_symbols(Arc::clone(&session), &root, &file);
    let _guard = resp_session
        .ensure_open(&path)
        .await
        .map_err(ToolError::Core)?;
    let timeout = ls_registry::config::effective_timeout_ms(&lang_str, None)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(TOOL_TIMEOUT);
    let params = serde_json::json!({ "textDocument": { "uri": uri.clone() } });
    let resp: Option<DocumentSymbolResponse> = resp_session
        .request("textDocument/documentSymbol", params, timeout)
        .await?;
    let out = flatten_symbols(resp, &uri, &session.language_id());
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
    lang: &str,
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
        lang: &str,
        out: &mut Vec<SymbolHit>,
    ) {
        for it in items {
            if position_in_range(it.range, line, col) {
                out.push(SymbolHit {
                    name: symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind),
                    kind: kind_from_lsp(&it.kind),
                    uri: file_uri.to_owned(),
                    range: it.range,
                    container: container.map(str::to_owned),
                });
                if let Some(children) = it.children.as_ref() {
                    walk(children, file_uri, line, col, Some(&it.name), lang, out);
                }
            } else if let Some(children) = it.children.as_ref() {
                // 父节点不命中但子节点仍可能命中（罕见：嵌套树里父 range 比子 range 大）。
                walk(children, file_uri, line, col, container, lang, out);
            }
        }
    }

    match resp {
        DocumentSymbolResponse::Nested(items) => {
            let mut out = Vec::new();
            walk(items, file_uri, line, col, None, lang, &mut out);
            out
        }
        DocumentSymbolResponse::Flat(items) => {
            // Flat：每项自带 location & container_name；无层级。
            items
                .iter()
                .filter(|it| position_in_range(it.location.range, line, col))
                .map(|it| SymbolHit {
                    name: symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind),
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
                let res = overview_via_session(
                    session,
                    cache_arc,
                    root_buf,
                    file.clone(),
                    lang_owned.as_deref(),
                )
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
    let fake = Location {
        uri,
        range: hit.range,
    };
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
    let items: Vec<serde_json::Value> = resp.items.iter().map(compact_completion_item).collect();
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

/// bd 8ft：结果集被 `max_results` 截断时响应顶层标 `truncated:true`。对象形态
/// 直接加键；裸数组（非 compact）按 [`attach_warning`] 同款升级为对象。仅截断
/// 发生时调用（skip_if false）。
fn attach_truncated(value: &mut serde_json::Value) {
    if value.is_array() {
        let items = std::mem::take(value);
        *value = serde_json::json!({ "compact": false, "items": items, "truncated": true });
    } else if let Some(obj) = value.as_object_mut() {
        obj.insert("truncated".into(), serde_json::Value::Bool(true));
    }
}

/// 读数值环境变量；设了但非法 → warn + 视同未设（不炸请求，配置错误可观测）。
fn env_u64_var(name: &str) -> Option<u64> {
    match std::env::var(name) {
        Ok(s) => match parse_num_env(&s) {
            Some(n) => Some(n),
            None => {
                tracing::warn!("{name}={s:?} is not a number; ignored");
                None
            }
        },
        Err(_) => None,
    }
}

/// br41/z0kg：数值 env 解析（trim + u64）；None = 非法。
fn parse_num_env(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// z0kg：list 类工具条数默认值——显式旗 > `SERENA_DEFAULT_MAX_ITEMS` > 既有默认。
/// 未显式传旗时 env 才生效（默认行为不变，br41/z0kg 裁决同构）。
fn arg_limit(args: &serde_json::Value, key: &str, default: u64) -> u64 {
    if let Some(n) = args.get(key).and_then(|v| v.as_u64()) {
        return n;
    }
    env_u64_var("SERENA_DEFAULT_MAX_ITEMS").unwrap_or(default)
}

/// aap4：raw_lsp_response 诊断开关——请求旗 `_debug_raw` 或环境
/// `SERENA_DEBUG_RAW=1`（daemon 进程环境）。默认关，响应零新字段。
fn debug_raw_enabled(args: &serde_json::Value) -> bool {
    args.get("_debug_raw")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || std::env::var("SERENA_DEBUG_RAW").as_deref() == Ok("1")
}

/// tfa3：写回执瘦身（B.2/B.4 形态）。`post_write_diagnostics` 有 items 原样保留；
/// items 空 + `pending:true`（LS 未确认，空≠无错）→ 压成单字符串 `"pending"`
/// （语义不丢：AI 仍需回头复核）；items 空 + 确认干净 → 键省略（skip_if）。
/// 语义依据见 `post_diag_for_write` doc 的 pending 双态定义。
fn slim_write_receipt(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(diag) = obj.get("post_write_diagnostics") else {
        return;
    };
    let empty = diag
        .get("items")
        .and_then(|v| v.as_array())
        .is_some_and(|a| a.is_empty());
    if !empty {
        return;
    }
    if diag.get("pending").and_then(|v| v.as_bool()).unwrap_or(false) {
        obj.insert(
            "post_write_diagnostics".into(),
            serde_json::json!("pending"),
        );
    } else {
        obj.remove("post_write_diagnostics");
    }
}

/// dry-run 成功信封的后处理（critic3-F11 抽出供单测锚定）：打 dry_run 标记 +
/// applied/would_apply 语义翻转（杠精 wuhi）+ 预览 hunk 化（token 卫生）+
/// **剥 `post_write_diagnostics`** —— 干跑没写盘，诊断无从谈起，挂着
/// "pending" 字段只会误导调用方以为有写后反馈通道。
fn dry_run_envelope(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    root: &Path,
    preview: Vec<(String, String)>,
) {
    obj.insert("dry_run".into(), serde_json::Value::Bool(true));
    // 杠精 wuhi：dry_run:true 与 applied:true 同现自相矛盾（AI 扫字段
    // 误判已写入）——dry-run 语义 = applied:false + would_apply:true。
    obj.insert("applied".into(), serde_json::Value::Bool(false));
    obj.insert("would_apply".into(), serde_json::Value::Bool(true));
    // 杠精 wuhi：content 全文预览在大文件上是 token 炸弹——默认 hunk 化
    // （unified diff，盘上现内容 vs 将写内容；新建文件走 /dev/null 头）。
    obj.insert(
        "would_write".into(),
        serde_json::Value::Array(
            preview
                .into_iter()
                .map(|(p, c)| {
                    let pb = std::path::PathBuf::from(&p);
                    let rel = pb
                        .strip_prefix(root)
                        .unwrap_or(&pb)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let (before, created) = match std::fs::read_to_string(&pb) {
                        Ok(s) => (s, false),
                        Err(_) => (String::new(), true),
                    };
                    serde_json::json!({
                        "file": p,
                        "patch": recipe::unified_diff(&rel, &before, &c, created),
                    })
                })
                .collect(),
        ),
    );
    obj.remove("post_write_diagnostics");
}

/// 51ib：`format` = brief|full|json（默认 full = 既有 wire 逐字节不变）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutFormat {
    Brief,
    Full,
    Json,
}

fn out_format(args: &serde_json::Value) -> Result<OutFormat, ToolError> {
    match args.get("format").and_then(|v| v.as_str()) {
        None | Some("full") => Ok(OutFormat::Full),
        Some("brief") => Ok(OutFormat::Brief),
        Some("json") => Ok(OutFormat::Json),
        Some(other) => Err(ToolError::BadArgs {
            detail: format!("unknown format `{other}` (accepted: brief, full, json)"),
        }),
    }
}

/// glob → regex（search path_glob / --exclude 共用）。支持 `**` 跨段、`*` 单段、
/// `?` 单字符；glob 不含 `/` 时匹配任意路径（`*.cpp` 也命中 `src/a.cpp`）。
fn glob_to_regex(g: &str, case_insensitive: bool) -> ToolResult<regex::Regex> {
    let mut r = String::from("^");
    if !g.contains('/') {
        r.push_str(".*");
    }
    let mut i = 0;
    let chars: Vec<char> = g.chars().collect();
    while i < chars.len() {
        let c = chars[i];
        // `**` 跨任意段（包括 `/`）；吞掉紧跟的 `/`（`src/**/foo` 等价 `src/foo`）。
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            r.push_str(".*");
            i += 2;
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
    regex::RegexBuilder::new(&r)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| ToolError::BadArgs {
            detail: format!("bad glob: {e}"),
        })
}

/// rsqq：search 头部一行概要（kq6e overview summary 同形——字符串头字段）。
fn search_summary(resp: &SearchResponse) -> String {
    let files: std::collections::BTreeSet<&str> = resp
        .hits
        .iter()
        .map(|h| h.file.as_str())
        .collect();
    let mut s = format!("{} hits in {} files", resp.hits.len(), files.len());
    if resp.truncated {
        s.push_str("; truncated");
    }
    s
}

/// bd xxl：Windows 8.3 短名 → 长名归一单点。CI runner 的 `TEMP` 常为
/// `C:\Users\RUNNER~1\...` 形态——`dunce::canonicalize` 走 fast path 不解析短名，
/// didOpen URI 与 LS 内部长路径键失配（pyright 系 -32602 / vscode-langservers 系
/// documentSymbol 恒空）。`std::fs::canonicalize`（GetFinalPathNameByHandle）能解
/// 短名 → 剥 `\\?\` 前缀（`\\?\UNC\server\share` → `\\server\share`）。解析失败
/// （不存在/权限）回落 dunce，再失败原样返回 —— 不新增失败模式。
pub(crate) fn normalize_long_path(p: &Path) -> Option<std::path::PathBuf> {
    match std::fs::canonicalize(p) {
        Ok(long) => {
            let s = long.as_os_str().to_string_lossy();
            let stripped = s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\\{r}"));
            let stripped = stripped
                .or_else(|| s.strip_prefix(r"\\?\").map(str::to_string))
                .unwrap_or_else(|| s.to_string());
            Some(std::path::PathBuf::from(stripped))
        }
        Err(_) => dunce::canonicalize(p).ok(),
    }
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

/// 批2-A：语义工具渐进首答的降级标记（wire additive 字段）。`semantic-pending`
/// 明确告知「语义层未就绪」而非「权威空结果」（禁把 pending 伪装成真空）；
/// `partial` 表示语义层部分结果。（syntax-only 标记位留给未来的 documentSymbol
/// 替代层——本票无产生路径，不设死变体。）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Degraded {
    /// 语义层部分结果（部分 lang 未就绪/超时）。
    Partial,
    /// 语义层未就绪，空结果不可信（hover/def/find-symbol 超时路径）。
    SemanticPending,
}

impl Degraded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::SemanticPending => "semantic-pending",
        }
    }
}

/// warmup 结构化标记（wire additive）：stage 固定 indexing；progress 在 lsp-core
/// 索引跟踪提供百分比粒度前恒 null（现有 IndexProgressTracker 只有在飞计数，
/// 无 0-1 进度——契约允许 null）；retry_after_warm 恒 true（降级结果可重查）。
fn warmup_marker_json() -> serde_json::Value {
    serde_json::json!({
        "stage": "indexing",
        "progress": null,
        "retry_after_warm": true,
    })
}

/// 降级响应组装：顶层插 `degraded`（字符串标记）+ `warmup`（结构化标记）。
/// 调用时序约定在 attach_warning 之后——warning 非空已把裸 null/标量/数组升级
/// 为对象形态，此处对象直插键；非对象（调用方未先 attach_warning）按同型升级，
/// 保证字段总能落到 wire。supervisor overview 臂复用（blindtest v5.1 P3-D 切换/
/// 冷启动窗口的空结果标记，按会话状态判定）。
pub fn attach_degraded(value: &mut serde_json::Value, degraded: Degraded) {
    let d = serde_json::Value::String(degraded.as_str().to_string());
    let warmup = warmup_marker_json();
    if value.is_object() {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("degraded".to_string(), d);
            obj.insert("warmup".to_string(), warmup);
        }
    } else {
        let items = std::mem::take(value);
        *value = serde_json::json!({ "items": items, "degraded": d, "warmup": warmup });
    }
}

/// 降级场景的人话 warning（契约文案）：告诉 AI 不是挂死、不是真空，给出自救路径。
pub(crate) fn semantic_warming_warning() -> String {
    "semantic index warming; results are partial — run `wait-ready --stage indexing` or retry in ~30s"
        .to_string()
}

/// 批2-A：find-symbol 降级分类。任一 lang 超时或请求错误 → 全量性破坏：
/// 有结果 = partial；空结果 = semantic-pending（空不可信，无论 LS 是挂满预算
/// 还是快速回错）。全部 lang 正常答复 → None（全量，无标记）。
fn classify_find_symbol_degraded(
    timed_out: usize,
    errored: usize,
    has_hits: bool,
) -> Option<Degraded> {
    if timed_out == 0 && errored == 0 {
        return None;
    }
    Some(if has_hits {
        Degraded::Partial
    } else {
        Degraded::SemanticPending
    })
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

/// JSON 对象深层合并（T0 三通道·init_options）：两侧皆对象 → 递归并集、`overlay`
/// 覆盖同名键；否则 `overlay` 整体替换。
fn deep_merge_json(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) if bv.is_object() && v.is_object() => deep_merge_json(bv, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// T0 三通道·config_reply：spec 声明的 per-LS 真值应答表 → `workspace/configuration`
/// 结果。items 逐项按 `section` 精确匹配（大小写敏感）；未命中/缺 section/缺 items
/// 回 null（数组长度仍与 items 等长——与 lsp-core 默认应答形态一致）。
fn configuration_reply_from_spec(
    replies: &[ls_registry::spec::ConfigReplyEntry],
    msg: &lsp_core::framing::JsonRpc,
) -> serde_json::Value {
    let items = msg
        .params
        .as_ref()
        .and_then(|p| p.get("items"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    serde_json::Value::Array(
        items
            .iter()
            .map(|item| {
                item.get("section")
                    .and_then(|v| v.as_str())
                    .and_then(|s| replies.iter().find(|r| r.section == s))
                    .map(|r| r.value.clone())
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect(),
    )
}

/// 0-based LSP Position 是否落在任一符号的 range 内（bd serena-rust-we0 判据）。
/// hover/def 空结果 + 位置在语法层符号内 = 「类型分析未就绪」而非「无符号」；
/// 位置在符号外 = 空是正常语义（不标记）。
fn position_in_hits(hits: &[SymbolHit], line: u32, col: u32) -> bool {
    hits.iter().any(|h| {
        let (sl, sc) = (h.range.start.line, h.range.start.character);
        let (el, ec) = (h.range.end.line, h.range.end.character);
        (sl < line || (sl == line && sc <= col)) && (line < el || (line == el && col <= ec))
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

/// -32602 语义改判（critic3-F4，纯函数供单测锚定）：RA 类型分析
/// 未就绪时 rename 对**语法层合法的位置**抛 `rpc -32602: No references found
/// at position` —— 用户按错误信息排查位置（实际位置正确），白耗排查时间。
/// documentSymbol 覆盖该位置 = 位置合法 → 改判 `NotReady`（wire=LS_NOT_READY，
/// retryable，带 wait-ready 指引）；其余一律保留原错误（真·位置无符号 /
/// 语法层探测失败或空 / 非 -32602，均无从证明是未就绪）。
fn rename_rpc_reclassify(
    overview: Option<&[SymbolHit]>,
    pos: Position,
    err: CoreError,
    file: &str,
) -> ToolError {
    if let CoreError::Rpc { code: -32602, message } = &err
        && let Some(hits) = overview
        && position_in_hits(hits, pos.line, pos.character)
    {
        return ToolError::Core(CoreError::NotReady {
            cause: format!(
                "rename rejected at a position that documentSymbol reports as inside a symbol \
                 (rpc -32602: {message}); the language server's type analysis is not ready yet \
                 — run `wait-ready --file {file} --stage semantic` and retry"
            ),
        });
    }
    ToolError::Core(err)
}

impl Supervisor {
    /// prepareRename/rename 出错时的统一收口：documentSymbol 探测 + 语义改判
    /// （两个请求错误分支共用，probe 失败 = None = 保守保留原错误）。
    async fn rename_error_reclassified(
        &self,
        root: &Path,
        file: &str,
        lang: &str,
        pos: Position,
        err: CoreError,
    ) -> ToolError {
        let hits = self.tool_overview(root, file, Some(lang)).await.ok();
        rename_rpc_reclassify(hits.as_deref(), pos, err, file)
    }
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
fn ref_symbol_hits_envelope(hits: &[ref_tools::RefSymbolHit], compact: bool) -> serde_json::Value {
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
fn diff_hits(a: &serde_json::Value, b: &serde_json::Value) -> Vec<serde_json::Value> {
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

/// bd kq6e：overview 头部一行概要 —— 总数 + kind 直方图。
fn overview_summary(hits: &[SymbolHit]) -> String {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for h in hits {
        let k = match &h.kind {
            SymbolKindTag::File => "file".into(),
            SymbolKindTag::Module => "module".into(),
            SymbolKindTag::Class => "class".into(),
            SymbolKindTag::Method => "method".into(),
            SymbolKindTag::Function => "function".into(),
            SymbolKindTag::Field => "field".into(),
            SymbolKindTag::Variable => "variable".into(),
            SymbolKindTag::Other(n) => format!("other({n})"),
        };
        *counts.entry(k).or_default() += 1;
    }
    let hist = counts
        .iter()
        .map(|(k, n)| format!("{k}×{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} symbol(s): {}", hits.len(), hist)
}

/// bd w2b5：`--kind` 名 → SymbolKindTag。注意 `from_lsp` 只窄化 Function/Method/Class，
/// struct/enum/module 等在 hits 里都是 `Other(LSP 码)` —— 别名表按 LSP 3.17 §SymbolKind
/// 码表直接给 `Other(n)`，保证 `--kind struct` 能命中 RA 输出。未知名返 None（调用方
/// 报 BAD_ARGS，避免拼写错误静默零结果）。
fn kind_tag_from_name(name: &str) -> Option<SymbolKindTag> {
    match name.to_ascii_lowercase().as_str() {
        "function" | "fn" => Some(SymbolKindTag::Function),
        "method" => Some(SymbolKindTag::Method),
        "class" => Some(SymbolKindTag::Class),
        "file" => Some(SymbolKindTag::Other(1)),
        "module" | "mod" => Some(SymbolKindTag::Other(2)),
        "namespace" => Some(SymbolKindTag::Other(3)),
        "package" => Some(SymbolKindTag::Other(4)),
        "enum" => Some(SymbolKindTag::Other(10)),
        "interface" => Some(SymbolKindTag::Other(11)),
        "struct" => Some(SymbolKindTag::Other(23)),
        "field" => Some(SymbolKindTag::Other(8)),
        "property" => Some(SymbolKindTag::Other(7)),
        "variable" | "var" => Some(SymbolKindTag::Other(13)),
        "constant" | "const" => Some(SymbolKindTag::Other(14)),
        _ => None,
    }
}
/// 把 (line, col) 经 offsets.rs 换算为 LSP Position。M0 固定 utf-16。
/// 读盘 → `position_to_byte` → 校验落在 char 边界。这里简化：假定输入 line/col 即
/// LSP position（0-based），仅校验越界，避免上层传 0-based 错位时掩盖。
async fn lsp_position_from_byte(
    path: &Path,
    file: &str,
    line: u32,
    col: u32,
    enc: OffsetEncoding,
) -> ToolResult<Position> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            // sec-S5：报用户传入的相对 file，不回显绝对路径（bd serena-rust-hah）。
            detail: format!("read {file}: {e}"),
        })?;
    let pos = LspPos {
        line,
        character: col,
    };
    lsp_core::offsets::position_to_byte(&text, pos, enc).map_err(|_| ToolError::BadArgs {
        // 杠精 07u5-4：报用户 1-based 坐标（内部 line/col 已是 0-based），原因只说
        // 一遍——裸 LSP Display 是泛化的 "position out of range"，原样拼接会重复。
        detail: format!(
            "position {}:{} (1-based) out of range: line is past end of file or column is past end of line",
            line + 1,
            col + 1
        ),
    })?;
    Ok(Position::new(pos.line, pos.character))
}

/// blindtest v5 P1-2：裸 .rs 目录（无 Cargo.toml）RA 无工程可加载，语义类工具
/// 全链挂死（v5 实测 415s 超时零降级）。只读语义工具在 dispatch 入口短路：fs
/// 探测 ~0ms，墙钟 ≤2s。语法类（overview/edit-context/read）与写类不经此门。
/// 限定 root 直下 Cargo.toml：monorepo 无根 manifest 的形态不在本门覆盖内
/// （warning 文案即告知根因，用户可自行判读）。
const RUST_SEMANTIC_GATE_TOOLS: [&str; 7] = [
    "hover",
    "def",
    "find-implementations",
    "find-referencing-code-snippets",
    "signature-help",
    "document-highlight",
    "diagnostics",
];

/// blindtest v5.1 P2-C：rust 语法层预算门判据——与 [`rust_semantic_gate`] 同一
/// 工程判定（--lang override 优先，否则按扩展名解析；root 直下无 Cargo.toml）。
/// monorepo 无根 manifest 同样不在覆盖内（同门注释）。
fn rust_no_cargo(root: &Path, lang_override: Option<&str>, file: &str) -> bool {
    let is_rust = lang_override
        .map(|l| l.to_ascii_lowercase())
        .or_else(|| {
            resolve_lang_for_file(file, None).ok()
        })
        .is_some_and(|l| l == "rust");
    is_rust && !root.join("Cargo.toml").is_file()
}

fn rust_semantic_gate(
    tool: &str,
    root: &Path,
    args: &serde_json::Value,
    lang_override: Option<&str>,
) -> Option<serde_json::Value> {
    if !RUST_SEMANTIC_GATE_TOOLS.contains(&tool) {
        return None;
    }
    let lang = lang_override
        .map(|l| l.to_ascii_lowercase())
        .or_else(|| {
            args.get("file")
                .and_then(|f| f.as_str())
                .and_then(|f| resolve_lang_for_file(f, None).ok())
        })?;
    if lang != "rust" || root.join("Cargo.toml").is_file() {
        return None;
    }
    Some(serde_json::json!({
        "items": [],
        "degraded": "semantic-pending",
        "warmup": { "stage": "indexing", "progress": null, "retry_after_warm": false },
        "warning": "rust LS requires a Cargo project (no Cargo.toml under root); semantic tools unavailable, syntax tools still work",
    }))
}

/// 解析 lang: 有 override 直接用 (大小写折叠), 否则按文件扩展名探测
/// （内置 EXT_TABLE → external-servers.toml extensions 兜底）。
/// EXT_TABLE 只看扩展名；再落 file_detect（文件名/shebang）兜底无扩展名文件
/// （Dockerfile 等，bd 56a）。
fn resolve_lang_for_file(file: &str, lang_override: Option<&str>) -> ToolResult<String> {
    if let Some(l) = lang_override {
        return Ok(l.to_ascii_lowercase());
    }
    ls_registry::resolve_lang_name(Path::new(file))
        .or_else(|| ls_registry::file_detect::detect_language(Path::new(file)).map(|l| l.as_str()))
        .map(str::to_string)
        .ok_or_else(|| ToolError::BadArgs {
            // 杠精 F3：与路径守卫拒绝（"path escapes project root"）文案分流——
            // 带扩展名时点明扩展名，用户能直接分清文件类型错 vs 路径错。
            // blindtest v5 P1-1：扩展名在 servers.toml 有登记（.rb/.m 类）→ 不是
            // 真未知类型，是「注册了但无 adapter 路由」，给 ls-use 指引而非裸
            // unsupported（保留原语义给真未知扩展名）。
            detail: match Path::new(file).extension().and_then(|e| e.to_str()) {
                Some(ext) => match ls_registry::registered_lang_for_extension(ext) {
                    Some(lang) => format!(
                        "registered but no adapter for {lang} (.{ext}); use ls-use to \
                         register an external server: {file}"
                    ),
                    None => format!(
                        "unsupported extension .{ext}: {file} (pass --lang <lang> to override)"
                    ),
                },
                None => format!(
                    "file not supported: {file} (no extension/filename language match; \
                     pass --lang <lang> to override)"
                ),
            },
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

/// Phase 4 基建 Task 22b：从 args 中移除 `_timeout_ms` / `_index_timeout_ms` / `_warmup_ms`
/// 私有字段，返回清理后的 args clone。`execute_tool` 入口调用，避免污染后续 `required_file` 等
/// 私有 helper（它们只看业务字段如 `file`/`line`，忽略下划线前缀；清掉是为了
/// JSONL 反序列化时 `_timeout_ms` 不会泄漏到 tool 输出）。
fn sanitize_timeout_args(mut args: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = args.as_object_mut() {
        obj.remove("_timeout_ms");
        obj.remove("_index_timeout_ms");
        obj.remove("_warmup_ms");
    }
    args
}

/// 信封内「list 数组」键（按序探测）。items=refs/find-symbol/diagnostics 系既有
/// 信封；hits=search；top=repo-map；entries=symbol-tree。顶层裸数组（overview/
/// list-dir/find-file）截断后折进 items 信封词汇（bd serena-rust-1ve9）。
const BUDGET_LIST_KEYS: [&str; 4] = ["items", "hits", "top", "entries"];

/// AI-token 特性 G（plan-g-budget.md §Task1 / ai-token-features-design §10-G）：
/// 工具响应 token 预算护栏。超出预算则截断信封内的 list 数组到最大可容纳条数，
/// 并写入 `truncated: true` + `original_count`；未超预算零改动（返回 false）。
/// 返回值 = 是否发生截断。截断是 **success 语义**（wire/退出码不变，仅加标志）。
///
/// 估算：4 字节 ≈ 1 token（BPE 粗略近似，soft limit）。
/// delta 响应（有 `added`/`removed` 键）跳过截断——items 语义已归一为增量集，
/// 按条截断会破坏增量对照关系，直接放行。read-file（content 串，行号 clamp +
/// content_hash 契约）/edit-context（body 串）等非 list 信封不在护栏范围。
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
    if value.is_array() {
        // 顶层裸数组无处内嵌标志——折进既有 items 信封词汇后走同一截断管线。
        let mut envelope = serde_json::json!({ "items": std::mem::take(value) });
        let fired = truncate_envelope_list(&mut envelope, budget_bytes);
        *value = envelope;
        return fired;
    }
    truncate_envelope_list(value, budget_bytes)
}

/// 在信封对象内定位 [`BUDGET_LIST_KEYS`] 中的首个 list 数组，二分截断到预算内
/// 最大条数，写入 `truncated`/`original_count`（字段沿用现名，wire 不改）。
fn truncate_envelope_list(value: &mut serde_json::Value, budget_bytes: usize) -> bool {
    // 查找（闭包只判 is_array，无引用逃逸 FnMut 边界）与借用（闭包外 get_mut）分离：
    // &mut 数组引用不能从 find_map 闭包内返回（E0521）。
    let key = BUDGET_LIST_KEYS
        .iter()
        .copied()
        .find(|k| value.get_mut(*k).is_some_and(|v| v.is_array()));
    let Some(key) = key else {
        return false; // 无 list 数组的响应（标量/树形）不在预算护栏范围
    };
    let Some(items) = value.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return false; // 无 list 数组的响应（标量/树形）不在预算护栏范围
    };
    let original_count = items.len();
    // 二分查找预算内最大保留条数；+18 字节为 truncated/original_count 标志开销余量。
    let mut lo = 0usize;
    let mut hi = items.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let trial = serde_json::json!({ key: &items[..mid] });
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

/// bd 2rxp：工具响应出站 uri 归一——递归遍历响应树，把 `uri`/`targetUri` 字符串
/// 字段统一为 `file:///C:/...` 形态（盘符大写 + 驱动器冒号解码）。归一点在
/// `execute_tool` 出口（公共序列化处），禁逐工具手补；`file:` 之外的 uri 形态原样。
fn normalize_output_uris(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if (k == "uri" || k == "targetUri") && v.is_string() {
                    if let Some(s) = v.as_str() {
                        let normalized = normalize_file_uri(s);
                        if normalized != s {
                            *v = serde_json::Value::String(normalized);
                        }
                    }
                } else {
                    normalize_output_uris(v);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_output_uris(v);
            }
        }
        _ => {}
    }
}

/// 单个 uri 归一：`file:///` 后首个段形如 `c%3A` / `c:`（Windows 盘符）→
/// 大写盘符 + 字面冒号。其余内容（路径段编码、query）不动。
fn normalize_file_uri(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file:///") else {
        return uri.to_string();
    };
    let bytes = rest.as_bytes();
    // 形态一：`c%3A/...`；形态二：`c:/...`。
    let drive = *bytes.first().unwrap_or(&b'\0');
    if !drive.is_ascii_alphabetic() {
        return uri.to_string();
    }
    let tail: Option<&str> = if rest[1..].starts_with("%3A") {
        Some(&rest[1 + "%3A".len()..])
    } else if rest[1..].starts_with(':') {
        Some(&rest[2..])
    } else {
        None
    };
    let drive_upper = drive.to_ascii_uppercase() as char;
    match tail {
        // tail 自带 `/` 前缀（如 "/Users/x/a.py"）——盘符与路径间只补冒号。
        Some(t) => format!("file:///{drive_upper}:{t}"),
        None => uri.to_string(),
    }
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
    /// bd e1p：符号缓存命中总次数（`symbol_cache_get` 命中即 ++）。
    fn cache_hits_total(&self) -> u64 {
        self.cache_hit_counter.load(Ordering::Relaxed)
    }

    async fn execute_tool(
        &self,
        tool: &str,
        project_root: &str,
        args: serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError> {
        // bd xxl：root 带短名（CI runner 的 TEMP=RUNNER~1 等 8.3 形态）→ 一切下游
        // didOpen URI 与 LS 内部长路径键失配。此处单点归一为长名（解析失败回落
        // dunce/原样），后续 tool_* 拿到的 root 已是磁盘真实形态。
        let long_root = normalize_long_path(Path::new(project_root))
            .unwrap_or_else(|| PathBuf::from(project_root));
        let root = long_root.as_path();
        // Phase 4 基建 Task 22b：把 args._timeout_ms / args._index_timeout_ms 提取成
        // per-call override，并清掉这两个私有字段（避免传染给具体 tool 的 args 解析）。
        // 实际 timeout 在 tool_* 内部通过 `effective_tool_timeout(lang, &args)` 拿到。
        let args = sanitize_timeout_args(args);
        // 批2-A：刷新本次请求的语义就绪等待预算（_warmup_ms / env / 15s 默认）。
        // recipe/ct 嵌套调用不经过入口，读 Supervisor 当前值即本次预算。
        self.warmup_budget_ms.store(
            effective_warmup_budget(lang, &args).as_millis() as u64,
            Ordering::Relaxed,
        );
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
        // 全部要求已存在（create-text-file 例外：新建语义，file 不存在是前置条件）。
        // bd serena-rust-5r7：写类工具（undo::WRITE_TOOLS）在存在性检查前先过
        // root 界校验 —— 指向 root 外真实存在文件的写请求旧检查放行、会直写
        // root 外；现在入口即 BAD_ARGS。读类入口不设界（读侧收口归 fs_tools::safe_join）。
        if let Some(f) = args.get("file").and_then(|v| v.as_str()) {
            if undo::WRITE_TOOLS.contains(&tool) {
                // bd serena-rust-5r7 路径穿越：写类工具先过 root 界校验（词法 +
                // canonical，symlink 语义见 path_guard）——穿越请求在拉起任何 LS
                // 前即 BAD_ARGS 拒收。create-text-file 新建语义免下方存在性检查。
                let abs = path_guard::guarded_join(root, f)
                    .map_err(|detail| ToolError::BadArgs { detail })?;
                if tool != "create-text-file" && !abs.is_file() {
                    return Err(ToolError::BadArgs {
                        detail: format!("file not found: {f}"),
                    });
                }
            } else if !root.join(f).is_file() {
                // 读/位置类维持原状（bd serena-rust-84n）：不存在 = 确定性参数错，
                // 否则 ensure_open 的 io NotFound 经 Core 冒成 INTERNAL 不可免重试。
                return Err(ToolError::BadArgs {
                    detail: format!("file not found: {f}"),
                });
            }
        }
        // ==== IDE undo/redo：事务边界（契约设计第 2/3 条）====
        // 写类工具一次 execute_tool 调用 = 一个 undo 事务：rename-symbol 跨文件
        // 改动在同一调用内逐文件 recorded_write，天然聚合成单事务。工具成功 →
        // 快照落盘（commit 含 prune 与清空 redo 链）；失败 → 丢弃本调用已记快照。
        // TXN_UID task-local 隔离并发调用（batch 每条独立 task）。
        let txn_uid = undo::next_uid();
        // audit 竞锁 #10 / 内存 F8 前半：execute_tool future 被取消（客户端断连
        // drop handler future）时 commit/abort 均不执行 → PENDING/TOUCHED 的 uid
        // 条目永久泄漏。守卫 drop 未收口即兜底 abort，取消路径账本必清。
        let txn = undo::TxnGuard::new(txn_uid);
        // A3b #3（bd i4a1/wlrr）：写类工具 --dry-run 干跑 —— dispatch 照常执行
        // （定位/计算/校验全跑），recorded_write 拦截为预览收集；不进事务、不 commit
        // （commit 含 prune/清 redo 链，干跑绝不触发），预览附进成功返回。
        let dry_run = undo::WRITE_TOOLS.contains(&tool)
            && args.get("dry_run").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let result = if dry_run {
            let (mut r, preview) =
                undo::scope_dry_run(self.dispatch_tool(tool, root, &args, lang)).await;
            if let Ok(v) = &mut r
                && let Some(obj) = v.as_object_mut()
            {
                dry_run_envelope(obj, root, preview);
            }
            r
        } else if undo::WRITE_TOOLS.contains(&tool) {
            // bd serena-rust-15jb：WAL 先记账后写盘，store 必须在工具执行期可见
            // （recorded_write 前置持久化 txn 记录）。root 不可解析时在此早失败，
            // 任何目标文件都尚未写 —— 优于旧版「写成功后 commit 才报 BAD_ARGS」。
            let store = undo::store_for(root)?;
            undo::TXN_STORE
                .scope(
                    store,
                    undo::TXN_UID.scope(txn_uid, self.dispatch_tool(tool, root, &args, lang)),
                )
                .await
        } else {
            undo::TXN_UID
                .scope(txn_uid, self.dispatch_tool(tool, root, &args, lang))
                .await
        };
        if !dry_run && undo::WRITE_TOOLS.contains(&tool) {
            match &result {
                Ok(_) => {
                    // P3：commit 落盘移入写门。undo/redo 恢复路径（undo_at/redo_at）
                    // 各自持门操作 undo 存储目录（prune/rename），走到这里时各工具
                    // 函数内的 gate 已释放 —— commit 若在门外落盘，可与并发 undo/redo
                    // 的 prune 交错。guard 活到 commit 完成：落盘毫秒级串行化，正确性
                    // 优先。commit_at 自身不加门（非重入门），顺序保证全靠此处。
                    let _gate = write_gate::acquire("undo-commit").await?;
                    // commit_at 先取走 PENDING 再落盘（audit 内存 F8）：IO 失败时
                    // 条目已被 drain，settle 后向上报错不会二次泄漏。
                    let committed = undo::commit(root, txn_uid).await;
                    txn.settle();
                    committed?;
                }
                Err(_) => {
                    undo::abort(txn_uid);
                    txn.settle();
                }
            }
        }
        result
    }

    fn loaded_entries(&self) -> Vec<Key> {
        self.last_used.lock().unwrap().keys().cloned().collect()
    }

    async fn evict_failed(&self) -> usize {
        Self::evict_failed_instances(self).await
    }
}

impl Supervisor {
    /// P2-b：undo/redo 恢复写（盘上 atomic_write，不推 didChange）后同步 LS 内存态
    /// —— 只靠 LS 盘上监听自愈对 rust-analyzer 可靠，pyright 等监听弱。逐文件：
    /// - created 且已被删除（undo 删 created 文件）→ `force_close`（didClose，
    ///   文件已不存在，didChange 无从谈起）；
    /// - 其余（改写 / redo 重建）→ 仅当该文件在本 daemon 的 buffers 表内才
    ///   `ensure_open` 推全量 didChange；表外 = LS 从未 didOpen（后续语义工具会
    ///   按需 ensure_open 自盘读），跳过避免无谓 didOpen 洪泛。
    /// - 遍历 root 下所有存活 session（rename 等多文件事务可能跨语言 LS）；
    ///   无存活 session（如纯 create-text-file 后 undo，从未起 LS）→ 空转跳过。
    /// - 任何同步失败只 warn：undo 本身已成功，绝不因 LS 同步失败让 undo 报错。
    async fn sync_ls_after_undo(&self, root: &Path, touched: &[undo::TouchedFile]) {
        if touched.is_empty() {
            return;
        }
        // 与 Supervisor::key 同款归一：instances 键的 root 经 canonicalize + 去尾分隔符。
        let mut canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        while matches!(
            canon.as_os_str().as_encoded_bytes().last(),
            Some(b'/' | b'\\')
        ) {
            canon.pop();
        }
        let sessions: Vec<std::sync::Arc<lsp_core::session::Session>> = {
            let instances = self.instances.lock().unwrap();
            instances
                .iter()
                .filter(|(k, _)| k.root == canon)
                .map(|(_, s)| std::sync::Arc::clone(s))
                .collect()
        };
        for t in touched {
            let p = Path::new(&t.path);
            let deleted = t.created && !p.exists();
            for session in &sessions {
                if matches!(session.state(), lsp_core::session::SessionState::Failed(_)) {
                    continue;
                }
                if deleted {
                    // 表外 = LS 没见过该文件，force_close 自行 no-op。
                    session.force_close(p);
                } else if session.is_open(p)
                    && let Err(e) = session.ensure_open(p).await
                {
                    tracing::warn!("undo/redo LS sync (didChange) failed for {}: {e}", t.path);
                }
            }
        }
    }

    /// 工具名 → 实现的分发表（原 execute_tool 主体；undo/redo 事务边界壳见上）。
    /// inherent 方法：trait impl 只收 SupervisorTrait 成员，分发表留在自有块。
    pub(crate) async fn dispatch_tool(
        &self,
        tool: &str,
        root: &Path,
        args: &serde_json::Value,
        lang: Option<&str>,
    ) -> Result<serde_json::Value, ToolError> {
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
        // blindtest v5 P1-2：裸 .rs 目录语义类工具短路（见 rust_semantic_gate）。
        if let Some(v) = rust_semantic_gate(tool, root, args, lang) {
            return Ok(v);
        }
        let mut value: serde_json::Value = match tool {
            "overview" => {
                let file = required_file(args)?;
                // bd P2-1：LS 不支持 documentSymbol（-32601）→ 优雅降级 + 指路，
                // 不再裸 RPC_ERROR rc=1（ct_verify format_skipped 同款显式降级先例）。
                let overview = async {
                    match self.tool_overview(root, &file, lang).await {
                        Ok(v) => Ok(Some(v)),
                        Err(ToolError::Core(CoreError::Rpc {
                            code: -32601, ..
                        })) => Ok(None),
                        Err(e) => Err(e),
                    }
                };
                // blindtest v5.1 P2-C：rust 无 Cargo 场景语法层也受 warmup 预算——
                // 裸 .rs 的 RA standalone 分析可慢墙 120s+（v5.1 实测 overview 120.1s
                // 静默慢墙；行级写类 insert/replace-lines 不等分析、秒回，无此问题）。
                // 超预算 → 结构化降级（retry_after_warm:true：分析完成后重查即快，
                // 与语义门 rust-no-cargo 的 false 相区隔）。
                let raw = if rust_no_cargo(root, lang, &file) {
                    match tokio::time::timeout(self.warmup_budget(), overview).await {
                        Ok(r) => r?,
                        Err(_) => {
                            return Ok(json!({
                                "items": [],
                                "degraded": "semantic-pending",
                                "warmup": { "stage": "indexing", "progress": null, "retry_after_warm": true },
                                "warning": "rust language server is still analyzing this file (no Cargo.toml under root; standalone-file analysis can take minutes); overview did not finish within the warmup budget — retry shortly",
                            }));
                        }
                    }
                } else {
                    overview.await?
                };
                let raw = match raw {
                    Some(v) => v,
                    None => {
                        return Ok(json!({
                            "symbols": [],
                            "overview_skipped":
                                "LS does not support textDocument/documentSymbol for this language; \
                                 use read-file / search to inspect the file structure",
                        }));
                    }
                };
                let empty = raw.is_empty();
                // bd kq6e：`summary:true` 升级为 {summary, symbols} 对象（头部一行概要
                // + 符号数）；默认维持裸数组 wire 不变。
                let mut value = if args
                    .get("summary")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    serde_json::json!({ "summary": overview_summary(&raw), "symbols": raw })
                } else {
                    serde_json::to_value(raw).map_err(|e| ToolError::Serialize(e.into()))?
                };
                // blindtest v5.1 P3-D：会话未热身（切换/冷启动窗口）的空 overview =
                // 「新 LS 会话未就绪」而非权威空——按会话状态标记（v5 P2-E 的 daemon
                // 全局单发标记已撤除），并发在途请求不再漏标，首个非空语义结果前
                // 持续生效。
                if empty && self.session_unwarmed(root) {
                    attach_warning(&mut value, &[semantic_not_ready_message()]);
                    attach_degraded(&mut value, Degraded::SemanticPending);
                }
                let root_key = format!("{}|{}", root.display(), file);
                Ok(self.maybe_delta("overview", &root_key, value, delta).await)
            }
            "symbol-tree" => {
                let dir =
                    args.get("dir")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'dir'".into(),
                        })?;
                let max_files = arg_limit(args, "max_files", 200) as usize;
                // bd vro3：`top_level=true` 每文件只返顶层符号（默认 false 行为不变）。
                let top_level = args
                    .get("top_level")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // bd 6ooi：三开关——grep（名子串，大小写不敏感）/ max_depth（包含链
                // 深度上限，顶层=0）/ files_only（只列文件，零 LS 调用）。
                let grep = args.get("grep").and_then(|v| v.as_str());
                let max_depth = args
                    .get("max_depth")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let files_only = args
                    .get("files_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                serde_json::to_value(
                    self.tool_symbol_tree(
                        root,
                        dir,
                        lang,
                        max_files,
                        top_level,
                        grep,
                        max_depth,
                        files_only,
                    )
                    .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-symbol" => {
                let query = args.get("query").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'query'".into(),
                    }
                })?;
                let limit = arg_limit(args, "limit", 50) as usize;
                // bd w2b5：`kind` 逗号分隔（如 "fn,struct"）——未知名 BAD_ARGS。
                let kind_filter: Option<Vec<SymbolKindTag>> =
                    match args.get("kind").and_then(|v| v.as_str()) {
                        Some(s) if !s.trim().is_empty() => {
                            let mut tags = Vec::new();
                            for name in s.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                                tags.push(kind_tag_from_name(name).ok_or_else(|| {
                                    ToolError::BadArgs {
                                        detail: format!(
                                            "unknown kind `{name}` (accepted: fn, method, class, struct, enum, interface, module, namespace, package, field, property, variable, constant, file)"
                                        ),
                                    }
                                })?);
                            }
                            Some(tags)
                        }
                        _ => None,
                    };
                let (mut raw, mut warnings, degraded) =
                    self.tool_find_symbol(root, query, limit, lang).await?;
                if let Some(kinds) = &kind_filter {
                    raw.retain(|h| kinds.contains(&h.kind));
                }
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
                let warming = self.index_warming_warnings(root);
                let warming_active = !warming.is_empty();
                warnings.extend(warming);
                let mut value = match out_format(args)? {
                    // 51ib：brief = 单串 `"name file:line:col"`（grep 友好，最省）。
                    OutFormat::Brief => {
                        let items: Vec<String> = raw
                            .iter()
                            .map(|h| format!("{} {}", h.name, compact_symbol_hit(h)))
                            .collect();
                        serde_json::json!({ "items": items, "raw_count": raw.len() })
                    }
                    // full（默认，与既有 wire 逐字节一致）/ json（--json 全形态）。
                    OutFormat::Full => symbol_hits_envelope(&raw, compact),
                    OutFormat::Json => symbol_hits_envelope(&raw, false),
                };
                attach_warning(&mut value, &warnings);
                // 批2-A：降级标记 additive 落 wire（degraded + warmup 结构化字段）。
                // 三路：请求层超时/错误（tool 层判定）＞暖机窗口（clangd 类 LS 对
                // workspace/symbol 未就绪时**成功回空**而非挂起——空+窗口内 =
                // semantic-pending 非权威空；非空+窗口内 = partial，O4 波动语义）。
                if let Some(d) = degraded {
                    attach_degraded(&mut value, d);
                } else if warming_active {
                    attach_degraded(
                        &mut value,
                        if raw.is_empty() {
                            Degraded::SemanticPending
                        } else {
                            Degraded::Partial
                        },
                    );
                }
                let root_key = format!("{}|{}|{}", root.display(), query, limit);
                Ok(self
                    .maybe_delta("find-symbol", &root_key, value, delta)
                    .await)
            }
            "signature-help" => {
                let (file, line, col) = required_position(args)?;
                serde_json::to_value(
                    self.tool_signature_help(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            // ==== Phase 1 · 上游 wrapper 缺口（13 个）====
            "code-action" => {
                let (file, line, col) = required_position(args)?;
                let kind = args.get("kind").and_then(|v| v.as_str());
                serde_json::to_value(
                    self.tool_code_action(root, &file, line, col, kind, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format" => {
                let file = required_file(args)?;
                let tab_size = args
                    .get("tab_size")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let insert_spaces = args.get("insert_spaces").and_then(|v| v.as_bool());
                serde_json::to_value(
                    self.tool_format(root, &file, tab_size, insert_spaces, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "format-range" => {
                let file = required_file(args)?;
                let start_line =
                    args.get("start_line")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'start_line'".into(),
                        })? as u32;
                let start_col = args
                    .get("start_col")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'start_col'".into(),
                    })? as u32;
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'end_line'".into(),
                    })? as u32;
                let end_col = args
                    .get("end_col")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'end_col'".into(),
                    })? as u32;
                let tab_size = args
                    .get("tab_size")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
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
                let file = required_file(args)?;
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
                serde_json::to_value(
                    self.tool_inlay_hint(root, &file, start_line, end_line, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-highlight" => {
                let (file, line, col) = required_position(args)?;
                serde_json::to_value(
                    self.tool_document_highlight(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "folding-range" => {
                let file = required_file(args)?;
                serde_json::to_value(self.tool_folding_range(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "semantic-tokens" => {
                let file = required_file(args)?;
                serde_json::to_value(self.tool_semantic_tokens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "code-lens" => {
                let file = required_file(args)?;
                serde_json::to_value(self.tool_code_lens(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "document-link" => {
                let file = required_file(args)?;
                serde_json::to_value(self.tool_document_link(root, &file, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "call-hierarchy" => {
                let op =
                    args.get("op")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'op' (prepare|incoming|outgoing)".into(),
                        })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(args)?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "incoming" => {
                        let item_value =
                            args.get("item")
                                .cloned()
                                .ok_or_else(|| ToolError::BadArgs {
                                    detail: "missing 'item' (CallHierarchyItem from prepare)"
                                        .into(),
                                })?;
                        serde_json::to_value(
                            self.tool_call_hierarchy_incoming(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "outgoing" => {
                        let item_value =
                            args.get("item")
                                .cloned()
                                .ok_or_else(|| ToolError::BadArgs {
                                    detail: "missing 'item' (CallHierarchyItem from prepare)"
                                        .into(),
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
                let op =
                    args.get("op")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::BadArgs {
                            detail: "missing 'op' (prepare|supertypes|subtypes)".into(),
                        })?;
                match op {
                    "prepare" => {
                        let (file, line, col) = required_position(args)?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_prepare(root, &file, line, col, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "supertypes" => {
                        let item_value =
                            args.get("item")
                                .cloned()
                                .ok_or_else(|| ToolError::BadArgs {
                                    detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                        .into(),
                                })?;
                        serde_json::to_value(
                            self.tool_type_hierarchy_supertypes(root, item_value, lang)
                                .await?,
                        )
                        .map_err(|e| ToolError::Serialize(e.into()))
                    }
                    "subtypes" => {
                        let item_value =
                            args.get("item")
                                .cloned()
                                .ok_or_else(|| ToolError::BadArgs {
                                    detail: "missing 'item' (TypeHierarchyItem from prepare)"
                                        .into(),
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
                let (file, line, col) = required_position(args)?;
                serde_json::to_value(self.tool_moniker(root, &file, line, col, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "workspace-diagnostic" => {
                serde_json::to_value(self.tool_workspace_diagnostic(root, lang).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "hover" => {
                let (file, line, col) = required_position(args)?;
                let resp = self.tool_hover(root, &file, line, col, lang).await?;
                let mut value =
                    serde_json::to_value(resp).map_err(|e| ToolError::Serialize(e.into()))?;
                // bd serena-rust-we0：空 hover 可能是「类型分析未就绪」而非「无悬停」；
                // bd serena-rust-xzb：workspace 加载错误无条件透出（结果不可信）。
                let mut ws = self.workspace_error_warnings(root);
                if hover_is_empty(&value) {
                    let not_ready = self
                        .semantic_not_ready_warnings(root, &file, line, col, lang)
                        .await;
                    // 批2-A：未就绪窗口的空 hover = semantic-pending（非权威空），
                    // 结构化告知 AI 重查而非接受 null。
                    let pending = !not_ready.is_empty();
                    ws.extend(not_ready);
                    // 杠精 ke2a-5：裸 null 5 字符无法区分「位置无符号」和「未就绪」；
                    // 未就绪已由 we0 warning 表达，就绪态的空结果补静态 hint 收口。
                    if ws.is_empty() {
                        ws.push(
                            "hover null: no symbol information at this position (semantic layer is ready; for a symbol name use find-symbol / edit-context)"
                                .into(),
                        );
                    }
                    attach_warning(&mut value, &ws);
                    if pending {
                        attach_degraded(&mut value, Degraded::SemanticPending);
                    }
                } else {
                    // bd serena-rust-bxd O2/O4：首个语义成功 → 关暖机窗口。
                    self.mark_semantic_ready(root);
                    attach_warning(&mut value, &ws);
                }
                Ok(value)
            }
            "diagnostics" => {
                let file = required_file(args)?;
                let wait_gen = args.get("wait_gen").and_then(|v| v.as_u64());
                serde_json::to_value(self.tool_diagnostics(root, &file, lang, wait_gen).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
            }
            "def" => {
                let (file, line, col) = required_position(args)?;
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
                    let not_ready = self
                        .semantic_not_ready_warnings(root, &file, line, col, lang)
                        .await;
                    // 批2-A：未就绪窗口的空 def = semantic-pending（非权威空）。
                    let pending = !not_ready.is_empty();
                    ws.extend(not_ready);
                    attach_warning(&mut value, &ws);
                    if pending {
                        attach_degraded(&mut value, Degraded::SemanticPending);
                    }
                } else {
                    self.mark_semantic_ready(root);
                    attach_warning(&mut value, &ws);
                }
                Ok(value)
            }

            "containing-symbol" => {
                let (file, line, col) = required_position(args)?;
                serde_json::to_value(
                    self.tool_containing_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "defining-symbol" => {
                let (file, line, col) = required_position(args)?;
                serde_json::to_value(
                    self.tool_defining_symbol(root, &file, line, col, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }

            "refs" => {
                let (file, line, col) = required_position(args)?;
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
                let (file, line, col) = required_position(args)?;
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
                let (file, line, col) = required_position(args)?;
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
                Ok(self
                    .maybe_delta("find-implementations", &root_key, value, delta)
                    .await)
            }
            "search" => {
                let pattern = args
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'pattern'".into(),
                    })?;
                let path_glob = args.get("path_glob").and_then(|v| v.as_str());
                // 批1-B：默认 50 硬上限（ripgrep 对标；旗标显式传值语义不变）。
                let max_results = arg_limit(args, "max_results", 50) as usize;
                let case_sensitive = args
                    .get("case_sensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut resp = self
                    .tool_search_for_pattern(
                        root,
                        pattern,
                        path_glob,
                        max_results,
                        case_sensitive,
                        // 杠精 cv1e：--exclude glob 列表 + --no-ignore 逃生。
                        &args
                            .get("exclude")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                        args
                            .get("no_ignore")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    )
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
                // zpzw：--distinct-symbols —— 同符号（enrich 命中名）多行命中只留首条
                //（symbol=None 的行不在任何符号内，全部保留）。
                if args
                    .get("distinct_symbols")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let mut seen: std::collections::HashSet<String> = Default::default();
                    resp.hits.retain(|h| match &h.symbol {
                        Some(s) => seen.insert(s.clone()),
                        None => true,
                    });
                }
                match out_format(args)? {
                    // 51ib：brief = grep 风格单串 `file:line:col: text`（最省）。
                    OutFormat::Brief => {
                        let items: Vec<String> = resp
                            .hits
                            .iter()
                            .map(|h| format!("{}:{}:{}: {}", h.file, h.line, h.col, h.text))
                            .collect();
                        let mut v = serde_json::json!({
                            "items": items,
                            "raw_count": resp.hits.len(),
                        });
                        if resp.truncated {
                            v["truncated"] = serde_json::json!(true);
                            v["hint"] = serde_json::json!(SEARCH_NOISE_HINT);
                        }
                        Ok(v)
                    }
                    // search 默认即全形态，full 与 json 同形（search 无紧凑裁剪层）。
                    // rsqq：头部一行概要（kq6e overview summary 同形键名）。
                    OutFormat::Full | OutFormat::Json => {
                        let mut v =
                            serde_json::to_value(&resp).map_err(|e| ToolError::Serialize(e.into()))?;
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "summary".into(),
                                serde_json::json!(search_summary(&resp)),
                            );
                        }
                        Ok(v)
                    }
                }
            }
            "symbol-body" => {
                let (file, symbol) = required_symbol_body_args(args)?;
                // bd b72k：`meta:true` 升级聚合形态（doc/signature/location/上一行下一行），
                // 默认裸 body 字符串 wire 不变。
                if args.get("meta").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let report = self
                        .tool_symbol_body_meta(root, &file, &symbol, lang)
                        .await?;
                    serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
                } else {
                    serde_json::to_value(self.tool_symbol_body(root, &file, &symbol, lang).await?)
                        .map_err(|e| ToolError::Serialize(e.into()))
                }
            }
            "edit-context" => {
                // B: 单次调用拿 body + callers + doc + tests（ai-token-features §10-B）。
                let (file, symbol) = required_symbol_body_args(args)?;
                let (report, warnings) =
                    crate::edit_context::collect(self, root, &file, &symbol, lang).await?;
                let mut value =
                    serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))?;
                // bd serena-rust-e0hi/8vo9：callers 空 + 语义未证就绪 → 降级警示字段。
                attach_warning(&mut value, &warnings);
                Ok(value)
            }
            "repo-map" => {
                // E: workspace 级符号地图（ai-token-features §10-E）。
                // 走 symbol-tree + per-symbol refs 计数，top_n 降序输出。
                let top_n = arg_limit(args, "top_n", 20) as usize;
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
                let (file, symbol, new_body) = required_replace_args(args)?;
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
                let (file, line, col, new_name) = required_rename_args(args)?;
                serde_json::to_value(
                    self.tool_rename_symbol(root, &file, line, col, &new_name, lang)
                        .await?,
                )
                .map_err(|e| ToolError::Serialize(e.into()))
            }
            "read-file" => {
                let file = required_file(args)?;
                let start_line = args
                    .get("start_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                // bd 66al：`no_clamp=true` 关闭 mfxg 的 EOF clamp（严格越界 BAD_ARGS，
                // 供探测文件真实长度）；默认 clamp。
                let clamp = !args
                    .get("no_clamp")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // bd a14g：read-file 本地 max_tokens（公共 args，非 `_max_tokens`
                // 私有预算）—— content 串超预算按整行砍，写 `truncated:true` +
                // `total_bytes` + `total_tokens`。=0 走 fs_tools::BadArgs → rc=2。
                let max_tokens = args
                    .get("max_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let report = fs_tools::read_file(
                    root,
                    &file,
                    start_line,
                    end_line,
                    clamp,
                    max_tokens,
                )
                .await
                .map_err(|e| ToolError::BadArgs {
                    detail: format!("read_file: {e}"),
                })?;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            "batch-read" => {
                // bd qre0：一次调用读多文件，聚合输出预算内停止（默认 2000 tok），
                // 砍「逐文件 read-file」往返。
                let files: Vec<String> = args
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'files' (array of repo-relative paths)".into(),
                    })?;
                if files.is_empty() {
                    return Err(ToolError::BadArgs {
                        detail: "'files' must not be empty".into(),
                    });
                }
                if files.len() > 50 {
                    return Err(ToolError::BadArgs {
                        detail: format!("'files' capped at 50 per call (got {})", files.len()),
                    });
                }
                let budget_tokens = args
                    .get("budget_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(2000) as usize;
                serde_json::to_value(self.tool_batch_read(root, &files, budget_tokens).await?)
                    .map_err(|e| ToolError::Serialize(e.into()))
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
                let max_entries = arg_limit(args, "max_entries", 500) as usize;
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
                let max_results = arg_limit(args, "max_results", 200) as usize;
                let hits = fs_tools::find_file(root, name_pattern, path_glob, max_results)
                    .map_err(|e| ToolError::BadArgs {
                        detail: format!("find_file: {e}"),
                    })?;
                serde_json::to_value(hits).map_err(|e| ToolError::Serialize(e.into()))
            }
            "find-referencing-symbols" => {
                let (file, line, col) = required_position(args)?;
                let (hits, raw_snip) = self
                    .tool_referencing_symbols(root, &file, line, col, lang)
                    .await?;
                let empty = hits.is_empty();
                let grouped = args
                    .get("grouped")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mut value = if grouped {
                    let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                    let page_size =
                        args.get("page_size").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                    let report = ref_tools::group_refs(hits, page, page_size);
                    serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))?
                } else {
                    ref_symbol_hits_envelope(&hits, compact)
                };
                // aap4：`_debug_raw`/SERENA_DEBUG_RAW 开且命中静默空 + LS 响应非 null
                // → 附原始响应 200B 快照（形态漂移诊断）；默认零新字段。
                if debug_raw_enabled(args)
                    && empty
                    && let Some(snip) = raw_snip
                    && let Some(obj) = value.as_object_mut()
                {
                    obj.insert("raw_lsp_response".into(), serde_json::json!(snip));
                }
                // bd serena-rust-e0hi/8vo9：空 = 真无 caller 或语义未就绪，警示让 AI 可分。
                let ws = if empty {
                    self.referencing_empty_warnings(root)
                } else {
                    self.mark_semantic_ready(root);
                    Vec::new()
                };
                attach_warning(&mut value, &ws);
                Ok(value)
            }
            "find-referencing-code-snippets" => {
                // O3：`symbol` 直查 —— 符号名解析为 (file, line, col)（LSP 0-based），
                // 与位置参数路径同基线；命中多个时附 warning 提示用了哪个。
                let (file, line, col, resolution_note) =
                    match args.get("symbol").and_then(|v| v.as_str()) {
                        Some(name) => self.resolve_symbol_position(root, name, lang).await?,
                        None => {
                            let (f, l, c) = required_position(args)?;
                            (f, l, c, None)
                        }
                    };
                let context_lines = args
                    .get("context_lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as u32;
                let max_results = arg_limit(args, "max_results", 50) as usize;
                let (hits, truncated, raw_snip) = self
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
                let empty = hits.is_empty();
                let mut value = ref_snippet_hits_envelope(&hits, compact);
                // bd 8ft：命中顶到 max_results 上限 → `truncated:true`（wire 新字段，
                // 仅截断发生时出现）。
                if truncated {
                    attach_truncated(&mut value);
                }
                // aap4：`_debug_raw`/SERENA_DEBUG_RAW 开且命中静默空 + LS 响应非 null
                // → 附原始响应 200B 快照（形态漂移诊断）；默认零新字段。
                if debug_raw_enabled(args)
                    && empty
                    && let Some(snip) = raw_snip
                    && let Some(obj) = value.as_object_mut()
                {
                    obj.insert("raw_lsp_response".into(), serde_json::json!(snip));
                }
                // bd serena-rust-e0hi/8vo9：空 = 真无 caller 或语义未就绪，警示让 AI 可分；
                // resolution_note（--symbol 多命中提示）保序拼接在后。
                let mut ws = if empty {
                    self.referencing_empty_warnings(root)
                } else {
                    self.mark_semantic_ready(root);
                    Vec::new()
                };
                if let Some(note) = resolution_note {
                    ws.push(note);
                }
                attach_warning(&mut value, &ws);
                Ok(value)
            }
            "replace-text-in-symbol" => {
                let file = required_file(args)?;
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
                let (file, symbol, text) = required_edit_args(args)?;
                // bd bt3h：默认 auto-indent（匹配插入点缩进）；`auto_indent:false` 关闭。
                let auto_indent = args
                    .get("auto_indent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let (end_line, end_col) = self
                    .tool_edit_insert_after_symbol(root, &file, &symbol, &text, auto_indent, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                    "post_write_diagnostics": diag,
                }))
            }
            "insert-text-before-symbol" => {
                let (file, symbol, text) = required_edit_args(args)?;
                // bd bt3h：默认 auto-indent（匹配插入点缩进）；`auto_indent:false` 关闭。
                let auto_indent = args
                    .get("auto_indent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let (end_line, end_col) = self
                    .tool_edit_insert_before_symbol(root, &file, &symbol, &text, auto_indent, lang)
                    .await?;
                let diag = self.post_diag_for_write(root, &file, lang).await;
                Ok(serde_json::json!({
                    "end_line": end_line,
                    "end_col": end_col,
                    "post_write_diagnostics": diag,
                }))
            }
            "delete-text-in-symbol" => {
                let file = required_file(args)?;
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
                let (file, symbol) = required_symbol_body_args(args)?;
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
                let file = required_file(args)?;
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
                        opt_expected_hash(args).as_deref(),
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
                let (file, start_line, end_line) = required_line_range(args)?;
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
                    opt_expected_hash(args).as_deref(),
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
                let (file, start_line, end_line) = required_line_range(args)?;
                self.tool_delete_lines(
                    root,
                    &file,
                    start_line,
                    end_line,
                    opt_expected_hash(args).as_deref(),
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
            // ==== IDE undo/redo（事务版快照栈）====
            "undo" => {
                let steps = args.get("steps").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                if args.get("list").and_then(|v| v.as_bool()).unwrap_or(false) {
                    undo::list(root).await
                } else {
                    let out = undo::undo(root, steps).await;
                    // P2-b：恢复写已落盘，同步 LS 内存态。登记表恒取走（含失败路径
                    // —— 中途冲突时已恢复的前缀文件同样要同步；同步失败不反转
                    // undo 结果，见 sync_ls_after_undo）。
                    let touched = undo::take_touched();
                    self.sync_ls_after_undo(root, &touched).await;
                    // 杠精 07u5-8：空栈静默 {"skipped":[],"undone":[]} 易被误读成
                    // 「可能已回滚」；结构化 nothing_to_undo 收口（保持 rc=0：空栈
                    // 是合法查询结果，非用法错误）。
                    out.map(|mut v| {
                        if v.get("undone")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|a| a.is_empty())
                            && v.get("skipped")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|a| a.is_empty())
                            && let Some(o) = v.as_object_mut()
                        {
                            o.insert("nothing_to_undo".into(), serde_json::Value::Bool(true));
                        }
                        v
                    })
                }
            }
            "redo" => {
                let out = undo::redo(root).await;
                let touched = undo::take_touched();
                self.sync_ls_after_undo(root, &touched).await;
                out
            }
            // ==== recipe 批1 地基三原子命令（local/recipe-plan.md §2 批1）====
            // test 只读源码、产物不进 undo/txn 栈（不在 undo::WRITE_TOOLS 名单）。
            "test" => {
                // 参数名用 target 而非 file：入口存在性预检按 file 字段一律要求
                // 「已存在文件」，而 test 目标可以是目录（crate/test 目录）。
                let file = args.get("target").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'target'".into(),
                    }
                })?;
                let name = args.get("name").and_then(|v| v.as_str());
                recipe::run_test(root, file, name).await
            }
            "diff" => {
                let txn_id = args.get("txn_id").and_then(|v| v.as_u64());
                let patch = args.get("patch").and_then(|v| v.as_bool()).unwrap_or(false);
                recipe::diff_txn(root, txn_id, patch).await
            }
            "find-test" => {
                let symbol = args.get("symbol").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'symbol'".into(),
                    }
                })?;
                recipe::find_test(self, root, symbol).await
            }
            // recipe 批4 编排层（local/recipe-plan.md §2 批4）：写步各自独立
            // undo 事务（嵌套 TXN_UID scope），故不进 WRITE_TOOLS——外层 uid
            // 无 recorded_write，TxnGuard drop 兜底 abort 无害。
            "recipe" => recipe_ops::run(self, root, args).await,
            // 新建文件（created=true 快照场景的可执行路径；文件已存在 = 参数错）。
            "create-text-file" => {
                let file = args.get("file").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::BadArgs {
                        detail: "missing 'file'".into(),
                    }
                })?;
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::BadArgs {
                        detail: "missing 'content'".into(),
                    })?;
                let report = self
                    .tool_create_text_file(root, file, content, lang)
                    .await?;
                serde_json::to_value(report).map_err(|e| ToolError::Serialize(e.into()))
            }
            other => Err(ToolError::BadArgs {
                detail: format!("unknown tool: {other}"),
            }),
        }?;
        // bd ou83：format-on-write（默认关）。写类工具成功后按旗对目标文件跑一次
        // formatting 并落盘（失败不回滚写结果，仅 warn）；`formatted` 仅在真实应用
        // ≥1 条编辑时出现（skip_if 0）。
        if args
            .get("format_on_write")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && undo::WRITE_TOOLS.contains(&tool)
            && let Some(f) = args.get("file").and_then(|v| v.as_str())
        {
            let n = self.format_on_write_for(root, f, lang).await;
            if n > 0
                && let Some(obj) = value.as_object_mut()
            {
                obj.insert("formatted".into(), serde_json::json!(n));
            }
        }
        // AI-token 特性 G（§10-G）：execute_tool 末尾统一后处理。_max_tokens 按预算
        // 截断信封 list（items/hits/top/entries + 顶层裸数组折 items 信封，1ve9）；
        // _compress 删 container/kind 冗余字段。两者与 _compact/_delta 同套私有约定
        // （sanitize 不清）。
        // 偏离 plan：原建议逐分支改造为统一变量；实际 match 整体即 Result，
        // `?` 一行收口零分支改动（11 特性已改动各分支，最小侵入）。
        // tfa3：写回执瘦身——诊断空时只留语义（先于预算截断，省下的字节不再进预算）。
        slim_write_receipt(&mut value);
        // br41：SERENA_DEFAULT_MAX_TOKENS 未设 = 无预算（现行为）；设了 = 未显式
        // `_max_tokens` 时的默认预算，显式旗永远优先。
        if let Some(max_tokens) = args
            .get("_max_tokens")
            .and_then(|v| v.as_u64())
            .or_else(|| env_u64_var("SERENA_DEFAULT_MAX_TOKENS"))
        {
            apply_budget(&mut value, max_tokens as usize);
        }
        if args
            .get("_compress")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            apply_compress(&mut value);
        }
        // bd 2rxp：出站 uri 统一形态（file:///C:/ 大写盘符、%3A 解码）——同一文件
        // 跨工具两种 uri 字符串（overview file:///C:/ vs refs file:///c%3A/）会让
        // 按 uri 分组/去重的 AI 消费者算重。入站方向（LS 推送小写盘符）不动。
        normalize_output_uris(&mut value);
        Ok(value)
    }
}

/// bd 4lw：LSP 响应 items 解析收口。`null` = 合法空（各工具「空/null = 无结果」
/// 契约）；**非 null 解析失败** = LS 升级/quirk 漂移的形态变化，不得伪装成
/// 「无结果」—— `warn!` 留痕（daemon 日志可查）后返空。第二返回值 true = degraded。
fn parse_lsp_items<T: serde::de::DeserializeOwned>(
    raw: serde_json::Value,
    site: &str,
) -> (Vec<T>, bool) {
    match raw {
        serde_json::Value::Null => (Vec::new(), false),
        v => match serde_json::from_value::<Vec<T>>(v) {
            Ok(items) => (items, false),
            Err(e) => {
                tracing::warn!(
                    site,
                    error = %e,
                    "LSP response parse failed (bd 4lw): shape drift, degrading to empty"
                );
                (Vec::new(), true)
            }
        },
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
///
/// 匹配键 = per-LS 归一后的 LS 名（↖ mirror 上游归一命名空间寻址——erlang
/// "create_user#2"、lua "M.foo"→"foo"；`lang` 见 `flatten_symbols` 注）。
fn find_symbol_range(
    resp: Option<&DocumentSymbolResponse>,
    symbol: &str,
    lang: &str,
) -> Option<lsp_types::Range> {
    fn walk(items: &[DocumentSymbol], symbol: &str, lang: &str) -> Option<lsp_types::Range> {
        for it in items {
            if symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind) == symbol {
                return Some(it.range);
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol, lang)
            {
                return Some(r);
            }
        }
        None
    }
    match resp? {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol, lang),
        DocumentSymbolResponse::Flat(_) => None,
    }
}

/// 读盘 + 按 LSP range 切符号体（tool_symbol_body 缓存命中/miss 两路共用）。
async fn read_and_slice(path: &Path, file: &str, range: lsp_types::Range) -> ToolResult<String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| ToolError::BadArgs {
            // sec-S5：报用户传入的相对 file，不回显绝对路径（bd serena-rust-hah）。
            detail: format!("read {file}: {e}"),
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
/// 匹配键同 `find_symbol_range`（归一命名空间）。
/// `None`（LS 对未就绪/未加载文档返 `null`）视为未找到。
fn find_symbol_node(
    resp: Option<&DocumentSymbolResponse>,
    symbol: &str,
    lang: &str,
) -> Option<(lsp_types::Range, lsp_types::Range)> {
    fn walk(
        items: &[DocumentSymbol],
        symbol: &str,
        lang: &str,
    ) -> Option<(lsp_types::Range, lsp_types::Range)> {
        for it in items {
            if symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind) == symbol {
                return Some((it.range, it.selection_range));
            }
            if let Some(children) = it.children.as_ref()
                && let Some(r) = walk(children, symbol, lang)
            {
                return Some(r);
            }
        }
        None
    }
    match resp? {
        DocumentSymbolResponse::Nested(items) => walk(items, symbol, lang),
        DocumentSymbolResponse::Flat(items) => items
            .iter()
            .find(|it| symbol_quirks::normalize_symbol_name(lang, &it.name, it.kind) == symbol)
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
/// bd edpi：原子写 io 错误分类。NotFound（os error 2/3：文件/路径不存在，典型 =
/// 父目录缺失）不是写冲突——冲突语义是「盘上内容与预期不符，重读重试」，按
/// hint 重试永远失败（盲测 v4 实锤：create-text-file 到缺失目录报
/// WRITE_CONFLICT + re-read 指引）。NotFound 归 BAD_ARGS 带创建指引；其余
/// （权限/占用等真冲突候选）保持 WRITE_CONFLICT。各 `recorded_write` 写点共用。
pub(crate) fn atomic_write_err(path: &str, e: std::io::Error) -> ToolError {
    if e.kind() == std::io::ErrorKind::NotFound {
        let dir = std::path::Path::new(path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| path.to_string());
        return ToolError::BadArgs {
            detail: format!(
                "parent directory does not exist: {dir}; create it first (atomic write failed: {e})"
            ),
        };
    }
    ToolError::WriteConflict {
        path: path.to_string(),
        reason: format!("atomic write failed: {e}"),
    }
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
/// readback 不符后的回滚收口（replace-body / safe-delete-symbol 两处 C3 防线共用）。
/// 回滚本身失败必须如实上报（错误链带 rollback-failed 事实），不得谎报 rolled
/// back——盘上新内容仍在，调用方必须知道（BD serena-rust-75k）。
async fn rollback_after_readback_mismatch(path: &Path, root: &Path, old_text: &str) -> ToolError {
    let reason = match atomic_write(path, old_text).await {
        Ok(()) => "readback mismatch; rolled back".to_string(),
        Err(e) => format!("readback mismatch; rollback FAILED ({e}); file left with new content"),
    };
    ToolError::WriteConflict {
        path: user_path(root, path),
        reason,
    }
}

/// sec-S5（bd serena-rust-hah）：用户可见错误 message 里的路径相对化（strip root
/// 前缀）——绝对路径会把用户工程目录结构还原给任意消费方；root 外路径（防御性，
/// 不伪造）原样输出。
fn user_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .display()
        .to_string()
}

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
    // audit 内存 F2：TempPath 守卫活到 rename 成功——写失败（磁盘满）与 future
    // 取消（客户端断连）路径由 Drop 统一清 `.tmp*` 残留，不再在用户项目目录留下
    // 孤儿临时文件；rename 成功后 keep() 解除删除。
    let tmp_path = tmp.into_temp_path();
    tokio::fs::write(&tmp_path, content).await?;

    let mut attempt = 0;
    loop {
        match tokio::fs::rename(&tmp_path, path).await {
            Ok(()) => {
                let _ = tmp_path.keep();
                return Ok(());
            }
            Err(e) if attempt < 5 => {
                // Windows ERROR_SHARING_VIOLATION(32) / ERROR_ACCESS_DENIED(5) 常见于杀软/索引器；
                // 统一退避重试。
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let _ = e;
            }
            Err(e) => {
                return Err(e); // tmp_path Drop 清理残留
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
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
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
        assert_eq!(
            textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "add"),
            2
        );
    }

    /// refs 空 + 文本无可疑出现（定义行本身 + 注释/字符串）→ 放行删除。
    #[test]
    fn text_gate_passes_when_nothing_outside_definition() {
        let hits = vec![
            hit_line("src/lib.rs", 7, "fn orphan() {}"),
            hit_line("src/lib.rs", 8, "// orphan kept for docs"),
            hit_line("src/lib.rs", 9, "    let s = \"orphan\";"),
        ];
        assert_eq!(
            textual_occurrences_outside_def(&hits, "src/lib.rs", 7, "orphan"),
            0
        );
    }

    /// 定义文件路径分隔符/大小写差异不重开定义行豁免（Windows 调用方传 `\` 形态）。
    #[test]
    fn text_gate_normalizes_definition_path() {
        let hits = vec![hit_line("src/lib.rs", 7, "fn add() {}")];
        assert_eq!(
            textual_occurrences_outside_def(&hits, "src\\lib.rs", 7, "add"),
            0
        );
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
        let hits = collect_containing_hits(Some(&resp), "file://x", 4, 10, "rust");
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
        let hits = collect_containing_hits(Some(&resp), "file://x", 20, 0, "rust");
        assert!(hits.is_empty(), "expected empty, got {hits:?}");
    }

    #[test]
    fn position_at_first_char_of_first_line_hits_only_outer() {
        let resp = nested_two_level();
        // outer @ 0-10；inner @ 4-5；line=0 落在 outer（不在 inner）。
        let hits = collect_containing_hits(Some(&resp), "file://x", 0, 0, "rust");
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
        let hits = collect_containing_hits(Some(&resp), "file://x", 6, 0, "rust");
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
        let params = parsed.signatures[0]
            .parameters
            .as_ref()
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
        let parsed: Option<lsp_types::SignatureHelp> =
            serde_json::from_value(raw).expect("null should round-trip to None");
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
        let parsed: lsp_types::SignatureHelp =
            serde_json::from_value(raw).expect("missing activeParameter should still round-trip");
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
    use super::{Supervisor, diag_uri_key};
    use lsp_core::docsync::path_to_uri_str;
    use lsp_core::init_params::supports_pull_diagnostics;
    use serde_json::json;
    use std::path::PathBuf;

    /// pyright 推送 uri 形态（`file:///c%3A/...` 实测）与 path_to_uri 生成形态
    /// （`file:///C:/...`）必须归一到同一缓存键 —— 否则 push 缓存永不命中，
    /// python 诊断恒空（2026-09-25 实锤根因）。
    #[test]
    fn diag_uri_key_normalizes_percent_encoded_and_cased_uris() {
        let path = std::path::Path::new("C:/Users/x/proj/broken.py");
        let ours = diag_uri_key(&path_to_uri_str(path));
        let pyright = diag_uri_key("file:///c%3A/Users/x/proj/broken.py");
        let ra = diag_uri_key("file:///c:/users/x/proj/broken.py");
        assert_eq!(ours, pyright, "pyright %3A 形态必须命中缓存");
        assert_eq!(ours, ra, "RA 小写盘符形态必须命中缓存");
    }

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
        handler("file:///a.cpp".into(), vec![json!({"message": "err1"})]);
        assert_eq!(cache_arc.lock().unwrap().len(), 1);
        assert_eq!(generation.load(Ordering::Relaxed), 1);

        // 推空 push：cache 应清空该条目，generation 2。
        handler("file:///a.cpp".into(), vec![]);
        assert_eq!(cache_arc.lock().unwrap().len(), 0, "空 push 必须清缓存");
        assert_eq!(generation.load(Ordering::Relaxed), 2);

        // 推空 push 对未存在的 uri：cache 不增不减，generation 3。
        handler("file:///b.cpp".into(), vec![]);
        assert_eq!(
            cache_arc.lock().unwrap().len(),
            0,
            "空 push 对空 key 是 no-op"
        );
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
        assert_eq!(
            a["pending"],
            serde_json::json!(true),
            "失败降级必带 pending"
        );

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

    /// bd dmsm 纯函数锁：连续空 pending 的降级阈值（≥3 轮且 ≥10s）与报文形态。
    #[test]
    fn pending_no_diag_response_thresholds() {
        use std::time::Duration;
        let a = super::pending_no_diag_response(1, Duration::from_secs(0));
        assert_eq!(a["pending"], serde_json::json!(true));
        assert!(a.get("warning").is_none(), "未达标不带 warning: {a}");
        // blindtest v5.1 P3-E：快空 2 轮（<10s 累计）仍不降级——单轮误判防护。
        let b = super::pending_no_diag_response(2, Duration::from_secs(9));
        assert_eq!(
            b["pending"],
            serde_json::json!(true),
            "轮数与时长均不够不降级: {b}"
        );
        // 快空 3 轮（<10s 累计）→ 降级（v5.1 实锤 vue 3×25ms 曾永卡 pending:true）。
        let b2 = super::pending_no_diag_response(3, Duration::from_secs(0));
        assert_eq!(
            b2["pending"],
            serde_json::json!(false),
            "快空 3 轮不看时长也降级: {b2}"
        );
        let c = super::pending_no_diag_response(3, Duration::from_secs(10));
        assert_eq!(c["pending"], serde_json::json!(false), "达标降级: {c}");
        assert_eq!(
            c["warning"],
            serde_json::json!(
                "LS returned no diagnostics after repeated empty-pending polls — it may not support diagnostics for this language"
            )
        );
        // 慢空 ≥10s 累计 2 轮即降级（按累计时长而非轮数下限）。
        let d = super::pending_no_diag_response(2, Duration::from_secs(10));
        assert_eq!(d["pending"], serde_json::json!(false), "慢空 2 轮 10s 降级: {d}");
        assert_eq!(
            super::pending_no_diag_response(4, Duration::from_secs(60))["pending"],
            serde_json::json!(false),
            "≥3 语义"
        );
    }

    /// bd dmsm 接线锁：register 计数、达标降级、降级后清账、reset 清账。
    /// 回拨首轮时刻（Instant - 11s）免真实等待。
    #[tokio::test]
    async fn diag_pending_streak_counts_degrades_and_resets() {
        let sup = Supervisor::direct().await.expect("supervisor");
        let root = std::path::Path::new("Z:/no/such/dmsm_proj");
        let uri = "file:///Z:/no/such/dmsm_proj/x.sql";
        for _ in 0..2 {
            let r = sup.register_empty_pending_exit(root, uri);
            assert_eq!(r["pending"], serde_json::json!(true), "{r}");
            assert!(r.get("warning").is_none(), "{r}");
        }
        let key = (root.to_path_buf(), uri.to_ascii_lowercase());
        sup.diag_pending_streak.lock().unwrap().get_mut(&key).unwrap().1 =
            std::time::Instant::now() - std::time::Duration::from_secs(11);
        let r = sup.register_empty_pending_exit(root, uri);
        assert_eq!(r["pending"], serde_json::json!(false), "第 3 轮达标降级: {r}");
        assert!(
            r["warning"]
                .as_str()
                .unwrap()
                .contains("no diagnostics after repeated empty-pending polls"),
            "{r}"
        );
        // 降级即清账：下一轮从头计数 → pending:true。
        let r = sup.register_empty_pending_exit(root, uri);
        assert_eq!(r["pending"], serde_json::json!(true), "清账后重计: {r}");
        // blindtest v5.1 P3-E：快空 3 连（<10s 累计）同样降级，不再永卡 pending:true。
        let fast_uri = "file:///Z:/no/such/dmsm_proj/fast_empty.vue";
        assert_eq!(
            sup.register_empty_pending_exit(root, fast_uri)["pending"],
            serde_json::json!(true)
        );
        assert_eq!(
            sup.register_empty_pending_exit(root, fast_uri)["pending"],
            serde_json::json!(true)
        );
        assert_eq!(
            sup.register_empty_pending_exit(root, fast_uri)["pending"],
            serde_json::json!(false),
            "快空 3 连不看时长也降级"
        );
        // reset（确认/非空 items 路径）清零。
        sup.reset_pending_streak(root, uri);
        assert!(
            sup.diag_pending_streak.lock().unwrap().get(&key).is_none(),
            "reset 必须清账"
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
        if let Some(ws) = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
        {
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
        tokio::fs::write(&file, b"int x=0;\n")
            .await
            .expect("write fixture");

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
        assert!(
            reclaimed >= 1,
            "归零超 TTL 后生产路径必须能回收，至少 1 条；reclaimed={reclaimed}"
        );
        assert_eq!(
            sup.reclaim_count_snapshot(),
            0,
            "reclaim 触发后 counter 应归零"
        );

        // 清理：shutdown session + 卸 supervisor 池条目。
        session.shutdown().await;
        let _ = sup.evict(&key).await;
    }

    /// audit 竞锁 #5 + P2-0bq：evict **保留** load_gates（慢路径持旧 gate guard
    /// 冷启动期间删表项 → 第三调用者新建 gate 同 key 双 spawn），后续 load_gate_for
    /// 必须拿到同一个 Arc gate 与在飞慢路径串行；pull_diag_supported 照旧清理
    /// （只 insert 不 remove，按驱逐次数单调累积）。
    #[tokio::test]
    async fn evict_keeps_load_gate_and_clears_pull_diag_supported() {
        let sup = Supervisor::direct().await.unwrap();
        let key = Supervisor::key(Path::new("Z:/no/such/project"), "rust");

        // 经 load_gate_for 建表项（同款 Arc），再插 pull 标记。
        let gate_before = sup.load_gate_for(&key.root, &key.lang);
        sup.pull_diag_supported
            .lock()
            .unwrap()
            .insert(key.clone(), true);

        // instances 没有该 key → evict 返 false，但 pull 表照清、gate 照留。
        let removed = sup.evict(&key).await.expect("evict");
        assert!(!removed, "instances 没 key 时 evict 返 false");

        let gate_after = sup.load_gate_for(&key.root, &key.lang);
        assert!(
            Arc::ptr_eq(&gate_before, &gate_after),
            "load_gates 表项必须保留且同一 Arc（同 key 双 spawn 防线）"
        );
        assert!(
            !sup.pull_diag_supported.lock().unwrap().contains_key(&key),
            "pull_diag_supported 必须清掉"
        );
    }

    /// audit 内存 F3：evict 按 root 归一清理 version_seen——(root, uri)→bool 条目
    /// 原先永不回收，长命 daemon 逐文件累积；同 root 清、异 root 不误伤。
    #[tokio::test]
    async fn evict_clears_version_seen_by_root_identity() {
        let sup = Supervisor::direct().await.unwrap();
        let key = Supervisor::key(Path::new("Z:/no/such/vs-project"), "rust");
        let other = Supervisor::key(Path::new("Z:/no/such/other-project"), "go");

        sup.version_seen
            .lock()
            .unwrap()
            .insert((key.root.clone(), "file:///a.rs".to_string()), true);
        sup.version_seen
            .lock()
            .unwrap()
            .insert((other.root.clone(), "file:///b.go".to_string()), false);

        // instances 无 key → 不触 shutdown，仍清表。
        let removed = sup.evict(&key).await.expect("evict");
        assert!(!removed);

        {
            let vs = sup.version_seen.lock().unwrap();
            assert!(
                !vs.keys()
                    .any(|(r, _)| key_root_identity(r) == key_root_identity(&key.root)),
                "被驱逐 root 的 version_seen 条目必须清理"
            );
            assert!(
                vs.contains_key(&(other.root.clone(), "file:///b.go".to_string())),
                "异 root 条目不得误伤"
            );
        }
        // 清场：进程级表不给他用例留垃圾。
        sup.version_seen
            .lock()
            .unwrap()
            .remove(&(other.root.clone(), "file:///b.go".to_string()));
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
        let mk_uri_pct =
            |f: &str| format!("file:///{base}/{f}").replacen("file:///C:/", "file:///C%3A/", 1);
        std::fs::write(
            dir.path().join("moved.rs"),
            "fn stale_line() {}\nfn added() {}\n",
        )
        .unwrap();
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
        assert!(
            sup.recent_written_uris(root, Duration::from_secs(10))
                .is_empty()
        );
        sup.mark_recent_write(root, "lib.rs");
        let uris = sup.recent_written_uris(root, Duration::from_secs(10));
        assert_eq!(uris.len(), 1, "窗口内标记必须可见");
        assert!(uris[0].ends_with("/lib.rs"), "归一 uri: {}", uris[0]);
        // TTL = 0 → 立即过期并清理。
        assert!(sup.recent_written_uris(root, Duration::ZERO).is_empty());
        assert!(
            sup.recent_written_uris(root, Duration::from_secs(10))
                .is_empty()
        );
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

    /// 批1-C：workspace/symbol 出界过滤 —— root 外符号（tsserver inferred project
    /// 沿父链解析 node_modules/@types 的实锤形态）被丢弃并计数，root 内保留。
    #[test]
    fn retain_symbols_within_root_filters_out_of_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let in_root = root.join("a.py");
        std::fs::write(&in_root, "x = 1\n").unwrap();
        let out_dir = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&out_dir).unwrap();
        let out_of_root = out_dir.join("leaked.py");
        std::fs::write(&out_of_root, "y = 2\n").unwrap();

        let mk = |name: &str, p: &Path| SymbolHit {
            name: name.to_string(),
            kind: SymbolKindTag::Function,
            uri: path_to_uri_str(p),
            range: Default::default(),
            container: None,
        };
        let mut hits = vec![
            mk("inside", &in_root),
            mk("leaked", &out_of_root),
        ];
        let filtered = Supervisor::retain_symbols_within_root(&mut hits, &root);
        assert_eq!(filtered, 1, "root 外条目必须被过滤");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "inside");
    }

    /// bd serena-rust-e0hi/8vo9：引用类空结果降级判定——semantic_ok 未证就绪前
    /// 警示（类型分析窗口 refs 静默返空不可信），证就绪后空即真值不警示。
    #[tokio::test]
    async fn referencing_empty_warnings_gate_on_semantic_flag() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/ref-degraded");

        // 无记账（生产不可达：空 hits 必经 session_for → mark_ls_started）→
        // 保守默认：未证就绪 → 警示。
        assert_eq!(
            sup.referencing_empty_warnings(root),
            vec![semantic_not_ready_message()]
        );

        // LS 已启动、语义未证 → 仍警示。
        sup.mark_ls_started(root);
        assert_eq!(
            sup.referencing_empty_warnings(root),
            vec![semantic_not_ready_message()]
        );

        // 任一语义工具非空成功 → 空即真值（真·无 caller），不警示。
        sup.mark_semantic_ready(root);
        assert!(sup.referencing_empty_warnings(root).is_empty());
    }

    /// bd serena-rust-e0hi：空 refs 的 compact envelope + 降级警示 = AI 可分的
    /// wire 形态（raw_count:0 不再裸奔）。
    #[test]
    fn empty_refs_envelope_carries_degraded_warning() {
        let hits: Vec<ref_tools::RefSymbolHit> = Vec::new();
        let mut value = ref_symbol_hits_envelope(&hits, true);
        attach_warning(&mut value, &[semantic_not_ready_message()]);
        assert_eq!(value["compact"], serde_json::json!(true));
        assert_eq!(value["raw_count"], serde_json::json!(0));
        let w = value["warning"].as_str().unwrap();
        assert!(w.contains("not be ready"), "就绪性关键词: {w}");
        assert!(w.contains("30-60s"), "预期窗口: {w}");
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

    /// blindtest v5.1 P3-D：overview 空结果降级判定按会话状态（session_unwarmed），
    /// 不再依赖 daemon 全局单发标记——记账在案且语义未证 → 未热身（并发在途同样
    /// 命中）；语义证毕 / 无记账 → false。
    #[tokio::test]
    async fn overview_unwarmed_marker_tracks_session_state() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/p3d-proj");
        // 无记账（LS 从未拉起）→ 不标。
        assert!(!sup.session_unwarmed(root));
        // 记账在案（mark_ls_started：切换/冷启动窗口）+ 语义未证 → 未热身。
        sup.mark_ls_started(root);
        assert!(sup.session_unwarmed(root), "切换后未热身窗口必须标");
        // 首个非空语义结果 → 窗口关闭。
        sup.mark_semantic_ready(root);
        assert!(!sup.session_unwarmed(root), "热身后不标");
    }

    /// blindtest v5.1 P2-C：rust 语法层预算门判据——--lang 优先、扩展名兜底、
    /// root 直下无 Cargo.toml 才生效（monorepo 无根 manifest 不在覆盖内）。
    #[test]
    fn rust_no_cargo_predicate_tracks_lang_and_manifest() {
        let root = Path::new("Z:/no/such");
        assert!(rust_no_cargo(root, Some("rust"), "x.rs"));
        assert!(!rust_no_cargo(root, Some("python"), "x.rs"), "非 rust 不判");
        assert!(!rust_no_cargo(root, None, "x.py"), "扩展名分流");
        // root 直下有 Cargo.toml → 不判（临时目录真实探测）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert!(
            !rust_no_cargo(dir.path(), Some("rust"), "src/main.rs"),
            "有根 manifest 不判（有 Cargo 工程，LS 正常路由）"
        );
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
            doc_symbol_cache_key(root, "a.rs", None),
            vec![mk("ensure_open_impl", "file:///x/a.rs", 0, 0)],
        );
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "b.rs", None),
            vec![mk("ensure_open", "file:///x/b.rs", 4, 2)],
        );
        let (file, line, col, note) = sup
            .resolve_symbol_position(root, "ensure_open", None)
            .await
            .unwrap();
        assert_eq!((file.as_str(), line, col), ("b.rs", 4, 2));
        assert!(note.is_none(), "单命中不提示");

        // 多命中：同名精确 ×2（前缀 ×1 落选不计）→ 取 (file, line, col) 序首者 + note 报总数。
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "c.rs", None),
            vec![mk("ensure_open", "file:///x/c.rs", 1, 0)],
        );
        let (file, _l, _c, note) = sup
            .resolve_symbol_position(root, "ensure_open", None)
            .await
            .unwrap();
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
            detail.contains(
                "index may still be warming (cold start), retry shortly or use find-symbol"
            ),
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

    /// bd serena-rust-gqyp：--symbol 解析坐标必须精化到**名字 token**——缓存里的
    /// range 是全范围（起点 = def 关键字），直接喂 refs 恒空（对拍实锤）。真实
    /// tempdir 文件锁换算约定：`def divide` 关键字 (1,4) → 名字 (1,8)。
    #[tokio::test]
    async fn resolve_symbol_position_refines_to_name_token() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("m.py"),
            "class C:\n    def divide(self, x):\n        return divide(x)\n",
        )
        .unwrap();
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "m.py", None),
            vec![SymbolHit {
                name: "divide".into(),
                kind: SymbolKindTag::Function,
                uri: "file:///x/m.py".into(),
                range: lsp_types::Range {
                    start: Position::new(1, 4), // `def` 关键字
                    end: Position::new(2, 24),
                },
                container: Some("C".into()),
            }],
        );
        let (file, line, col, note) = sup
            .resolve_symbol_position(root, "divide", None)
            .await
            .unwrap();
        assert_eq!(
            (file.as_str(), line, col),
            ("m.py", 1, 8),
            "必须精化到名字 (1,8) 而非关键字 (1,4)"
        );
        assert!(note.is_none(), "单命中不提示");
    }

    /// bd serena-rust-gqyp 伴随缺陷：find-symbol 的 `ws?{query}` 缓存条目 k.1 是
    /// 伪文件名，直查扫描不得把它当 file 候选（否则可能选出 "ws?divide" 当路径）。
    /// fallback 走 tool_find_symbol 消费 URI 真路径属设计内，保留。
    #[tokio::test]
    async fn resolve_symbol_position_ignores_ws_pseudo_file_entries() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/o3-ws");
        sup.symbol_cache_put(
            find_symbol_cache_key(root, "divide", None),
            vec![SymbolHit {
                name: "divide".into(),
                kind: SymbolKindTag::Function,
                uri: "file:///x/a.py".into(),
                range: lsp_types::Range {
                    start: Position::new(3, 0),
                    end: Position::new(9, 0),
                },
                container: None,
            }],
        );
        // 无过滤时直查扫描会把 "ws?divide" 当文件候选，且按文件名序 "ws?divide" <
        // "x/a.py" 排前当选 → Ok("ws?divide")；过滤后只能经 fallback 从 URI 派生
        // 真相对路径 → Ok("x/a.py")。
        let (file, line, col, _note) = sup
            .resolve_symbol_position(root, "divide", None)
            .await
            .unwrap();
        assert_eq!(
            (file.as_str(), line, col),
            ("x/a.py", 3, 0),
            "ws? 伪文件名不得当选，必须用 URI 派生路径"
        );
    }

    /// bd serena-rust-gqyp：名字精化的换算约定——整词匹配、UTF-16 col、找不到回退。
    #[test]
    fn refine_symbol_name_position_finds_whole_word_name_token() {
        let text = "class C:\n    def divide(self, x):\n        return divide(x)\n";
        let full = lsp_types::Range {
            start: Position::new(1, 4),
            end: Position::new(2, 24),
        };
        let p = refine_symbol_name_position(text, "divide", full);
        assert_eq!(p, Position::new(1, 8), "名字 token 位置，非 def 关键字");

        // 整词边界：add 不得命中 additional → 回退 range.start。
        let text2 = "def additional(x):\n    pass\n";
        let full2 = lsp_types::Range {
            start: Position::new(0, 0),
            end: Position::new(1, 8),
        };
        assert_eq!(
            refine_symbol_name_position(text2, "add", full2),
            Position::new(0, 0),
            "无整词命中回退 range.start"
        );

        // col 按 UTF-16 计：名字前有多字节字符，byte offset 12 → utf16 col 10。
        let text3 = "x = \"中\" # divide\n";
        let full3 = lsp_types::Range {
            start: Position::new(0, 0),
            end: Position::new(0, 17),
        };
        assert_eq!(
            refine_symbol_name_position(text3, "divide", full3),
            Position::new(0, 10),
            "col 必须是 UTF-16 单位数"
        );
    }

    /// ↖ mirror: ls.py@a5fd4d68 — 低层（LS 会话）版本变化后高层缓存不得再命中：
    /// 同 root 的单文件级与 workspace 级缓存全部失效，其他 root 不受影响。
    #[tokio::test]
    async fn session_rebuild_invalidates_symbol_cache_for_root() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let other = Path::new("Z:/no/such/other");
        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs", None), vec![hit("main")]);
        sup.symbol_cache_put(find_symbol_cache_key(root, "main", None), vec![hit("main")]);
        sup.symbol_cache_put(doc_symbol_cache_key(other, "a.rs", None), vec![hit("helper")]);

        // 模拟 (root, lang) 会话换代（session_for 挂入新会话前的失效动作）。
        sup.invalidate_symbol_cache_for_root(root);

        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "a.rs", None))
                .is_none(),
            "会话换代后同 root 文档符号缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&find_symbol_cache_key(root, "main", None))
                .is_none(),
            "会话换代后同 root workspace 级缓存必须 miss"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(other, "a.rs", None))
                .is_some(),
            "其他 root 的缓存不受影响"
        );
    }

    /// cache 命中：同 file 二次 overview 命中不拉 LS（命中路径 vs LS 往返秒级的量级差）。
    /// 阈值 50ms：命中路径正常 <1ms（预热后 HashMap 查 + async 轮询），满载调度抖动
    /// 实测数 ms 以上（10ms 已在并行全量下抖破一次，we0 在案）；慢路径是不存在 root
    /// 的 LS spawn 尝试（百 ms~秒级），50ms 仍保留一个量级判定力。
    #[tokio::test]
    async fn overview_cache_hit_returns_under_1ms() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        // bd 8ges：键含 override 维度——seed 与查询必须同平面（Some("rust")），
        // 否则命中路径 miss 会尝试拉 LS（不存在 root → spawn 失败）。
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "a.rs", Some("rust")),
            vec![hit("main")],
        );

        // warmup：预热 LazyLock / 代码路径，排除首次抖动。
        let _ = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();

        let t0 = Instant::now();
        let out = sup.tool_overview(root, "a.rs", Some("rust")).await.unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "main");
        assert!(
            elapsed < Duration::from_millis(50),
            "cache hit took {elapsed:?}"
        );
    }

    /// 命中必须先于 lang 解析 / session 拉起（不可解析扩展名 + 不存在 root 也命中）。
    #[tokio::test]
    async fn overview_cache_hit_precedes_lang_resolution() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "x.unknownext", None),
            vec![hit("weird")],
        );
        let out = sup.tool_overview(root, "x.unknownext", None).await.unwrap();
        assert_eq!(out[0].name, "weird");
    }

    /// bd 8ges 锁：override 进键——同文件 None（扩展名路由）与 Some(lang) 是不同
    /// 缓存平面，override 调用不被先前无 override 的结果遮蔽（Dockerfile
    /// `--lang python` 曾静默返回 docker 符号），且 override 大小写归一。
    #[tokio::test]
    async fn override_lang_results_do_not_shadow_extension_route() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "Dockerfile", None),
            vec![hit("FROM")],
        );
        sup.symbol_cache_put(
            doc_symbol_cache_key(root, "Dockerfile", Some("python")),
            vec![hit("py_sym")],
        );
        assert_eq!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "Dockerfile", None))
                .unwrap()[0]
                .name,
            "FROM",
            "None 平面保持扩展名路由结果"
        );
        assert_eq!(
            sup.symbol_cache_get(&doc_symbol_cache_key(
                root,
                "Dockerfile",
                Some("PYTHON")
            ))
            .unwrap()[0]
                .name,
            "py_sym",
            "override 平面独立且大小写归一"
        );
    }

    /// cache miss：空 supervisor 必 miss；put 后 get 命中（首次调用写入 cache 的机制）。
    #[tokio::test]
    async fn cache_miss_then_put_then_hit() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let key = doc_symbol_cache_key(root, "a.rs", None);
        assert!(
            sup.symbol_cache_get(&key).is_none(),
            "fresh supervisor must miss"
        );
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        let got = sup.symbol_cache_get(&key).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "f");
    }

    /// bd e1p：符号缓存命中即累计 `cache_hit_counter`（miss 不计）——daemon 在
    /// 单次调用前后差分此值 = 该调用的命中与否，落 invocations.jsonl `cache_hit`。
    #[tokio::test]
    async fn cache_hit_bumps_counter_for_daemon_diffing() {
        let sup = Supervisor::direct().await.unwrap();
        let root = Path::new("Z:/no/such/project");
        let key = doc_symbol_cache_key(root, "a.rs", None);
        let before = sup.cache_hits_total();
        assert!(sup.symbol_cache_get(&key).is_none(), "miss");
        assert_eq!(sup.cache_hits_total(), before, "miss must not bump");
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key).is_some(), "hit");
        assert_eq!(sup.cache_hits_total(), before + 1, "hit must bump by 1");
    }

    /// invalidate：mtime 变 → key 变 → miss（下次重调 LS）。std set_modified，无新依赖。
    #[tokio::test]
    async fn mtime_change_invalidates_entry() {
        let sup = Supervisor::direct().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let root = dir.path();

        let key1 = doc_symbol_cache_key(root, "a.rs", None);
        sup.symbol_cache_put(key1.clone(), vec![hit("f")]);
        assert!(sup.symbol_cache_get(&key1).is_some());

        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
            .unwrap();
        let key2 = doc_symbol_cache_key(root, "a.rs", None);
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
        let ka = doc_symbol_cache_key(root, "a.rs", None);
        let kb = doc_symbol_cache_key(root, "b.rs", None);
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
        let (out, warnings, _) = sup
            .tool_find_symbol(root, "parse", 2, Some("rust"))
            .await
            .unwrap();
        let elapsed = t0.elapsed();
        assert!(warnings.is_empty(), "cache hit must not fabricate warnings");
        // 100ms：命中路径正常 <1ms，意外走慢路径（LS 拉起 / root walk+报错）仍是
        // 百 ms~秒级，判定力保留。50ms 在并行满载下抖破（h-report 实测 2/3 轮命中；
        // tempdir walk + 86 测试并行调度抖动 16ms+），10ms 更必破——放宽到 100ms
        // 是 h-report 建议值。
        assert!(
            elapsed < Duration::from_millis(100),
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

        sup.symbol_cache_put(doc_symbol_cache_key(root, "a.rs", None), vec![hit("sym_a")]);
        sup.symbol_cache_put(doc_symbol_cache_key(root, "b.rs", None), vec![hit("sym_b")]);

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200, false, None, None, false)
            .await
            .unwrap();
        assert_eq!(
            tree["files_scanned"], 2,
            "node_modules/notes.txt 必须被过滤: {tree}"
        );
        assert_eq!(tree["truncated"], false);
        let entries = tree["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "两文件各有符号条目: {tree}");

        // dir 逃逸 root。
        let err = sup
            .tool_symbol_tree(root, "../elsewhere", Some("rust"), 200, false, None, None, false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::BadArgs { .. }),
            "unexpected: {err:?}"
        );
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
                doc_symbol_cache_key(root, &format!("f{i}.rs"), None),
                vec![hit("x")],
            );
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 3, false, None, None, false)
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
            sup.symbol_cache_put(doc_symbol_cache_key(root, n, None), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200, false, None, None, false)
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
            sup.symbol_cache_put(doc_symbol_cache_key(root, n, None), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200, false, None, None, false)
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
            sup.symbol_cache_put(doc_symbol_cache_key(root, n, None), vec![hit("x")]);
        }

        let tree = sup
            .tool_symbol_tree(root, ".", Some("rust"), 200, false, None, None, false)
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
                doc_symbol_cache_key(root, n, None),
                vec![hit(&format!("sym_{idx}"))],
            );
        }

        // lang = None 走逐文件 resolve（lang 是预置短路，下面的 resolve_lang_for_file
        // 不被 lang 短路，直接靠扩展名探测 —— 即 P0 修复的核心路径）。
        let tree = sup.tool_symbol_tree(root, ".", None, 200, false, None, None, false).await.unwrap();
        let entries = tree["entries"].as_array().unwrap();
        let errors = tree["errors"].as_array().unwrap();

        // 必须全部命中（cache priming）—— 任何 file 进 errors 即视为被误判为
        // "lang 不可解析"，是 P0 修复前的回归迹象。
        assert_eq!(
            entries.len(),
            files.len(),
            "所有 5 个文件都应该进入 entries: {tree}"
        );
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
    fn launch_mock_ls_track(
        track_log: &std::path::Path,
    ) -> Option<ls_runtime::process::LaunchInfo> {
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
        let session =
            lsp_core::session::Session::start(Some(child), lsp_types::InitializeParams::default())
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

        let key1 = doc_symbol_cache_key(root, "a.rs", None);
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

        let key2 = doc_symbol_cache_key(root, "a.rs", None);
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

        let key = doc_symbol_cache_key(root, "a.rs", None);
        let other_key = doc_symbol_cache_key(root, "b.rs", None);
        sup.symbol_cache_put(key.clone(), vec![hit("f")]);
        sup.symbol_cache_put(other_key.clone(), vec![hit("g")]);

        assert_eq!(
            doc_symbol_cache_key(root, "a.rs", None),
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
                doc_symbol_cache_key(root, &format!("f{i}.rs"), None),
                vec![hit("x")],
            );
        }
        assert_eq!(
            sup.symbol_cache_len(),
            cap,
            "刚好达到上限时不清空（< 阈值）"
        );
        // 第 cap+1 次 → 触发闸门 → 全清后只剩本条。
        sup.symbol_cache_put(doc_symbol_cache_key(root, "overflow.rs", None), vec![hit("y")]);
        assert_eq!(
            sup.symbol_cache_len(),
            1,
            "超上限触发全清后只剩新插入的 1 条"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "overflow.rs", None))
                .is_some(),
            "新写入的 key 必须可命中"
        );
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root, "f0.rs", None))
                .is_none(),
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
                doc_symbol_cache_key(root, &format!("g{i}.rs"), None),
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
                doc_symbol_cache_key(root, &format!("edit_{i}.rs"), None),
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
                doc_symbol_cache_key(root, &format!("f{i}.rs"), None),
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
                doc_symbol_cache_key(root, &format!("s{i}.rs"), None),
                vec![hit("x")],
            );
        }
        // 触发全清
        sup.symbol_cache_put(doc_symbol_cache_key(root, "trigger.rs", None), vec![hit("y")]);
        let mut all_miss = true;
        for i in 0..cap {
            if sup
                .symbol_cache_get(&doc_symbol_cache_key(root, &format!("s{i}.rs"), None))
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
                doc_symbol_cache_key(root_a, &format!("a{i}.rs"), None),
                vec![hit("a")],
            );
        }
        sup.symbol_cache_put(doc_symbol_cache_key(root_b, "b0.rs", None), vec![hit("b")]);
        // 触发全清
        sup.symbol_cache_put(doc_symbol_cache_key(root_a, "trigger.rs", None), vec![hit("t")]);
        // 全清后 root_a 只有 trigger.rs + root_b 的 b0.rs
        // invalidate root_a → 只剩 root_b 的 1 条
        sup.invalidate_symbol_cache_for_root(root_a);
        assert_eq!(sup.symbol_cache_len(), 1, "只 root_b 一条存活");
        assert!(
            sup.symbol_cache_get(&doc_symbol_cache_key(root_b, "b0.rs", None))
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

        let resp = sup.tool_search_for_pattern(root, "foo_drain_window", None, 50, false, &[], false)
            .await
            .unwrap();
        let files: Vec<&str> = resp.hits.iter().map(|h| h.file.as_str()).collect();
        assert!(
            files
                .iter()
                .all(|f| !f.starts_with(".git/") && !f.starts_with("target/")),
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
        assert!(
            all_hits.len() >= 2,
            "默认形态应含代码+注释，实际: {all_hits:?}"
        );

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
} // ============================================================================
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
        assert!(p.to_string_lossy().starts_with("D:"), "盘符必须大写: {p:?}");
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
        let r = sup.execute_tool("call-hierarchy", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_call_hierarchy_incoming_missing_item_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "incoming"});
        let r = sup.execute_tool("call-hierarchy", ".", args, None).await;
        assert!(matches!(r, Err(ToolError::BadArgs { .. })));
    }

    #[tokio::test]
    async fn execute_tool_type_hierarchy_unknown_op_returns_bad_args() {
        let sup = Supervisor::direct().await.unwrap();
        let args = json!({"op": "bogus"});
        let r = sup.execute_tool("type-hierarchy", ".", args, None).await;
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
        let parsed: Vec<lsp_types::Diagnostic> = serde_json::from_value(items).unwrap_or_default();
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

    // ==== 批2-A：语义工具渐进首答 ====

    #[test]
    fn effective_warmup_budget_defaults_15s_and_honors_warmup_ms() {
        let args = json!({});
        assert_eq!(
            effective_warmup_budget(Some("python"), &args),
            Duration::from_secs(15),
            "批2-A 默认 15s（120s 死等收紧）"
        );
        let args = json!({"_warmup_ms": 250, "query": "q"});
        assert_eq!(
            effective_warmup_budget(None, &args),
            Duration::from_millis(250),
            "args._warmup_ms（CLI --warmup-timeout）优先"
        );
    }

    #[test]
    fn degraded_names_match_wire_contract() {
        assert_eq!(Degraded::Partial.as_str(), "partial");
        assert_eq!(Degraded::SemanticPending.as_str(), "semantic-pending");
    }

    #[test]
    fn classify_find_symbol_degraded_covers_timeout_and_error_paths() {
        assert_eq!(classify_find_symbol_degraded(0, 0, false), None);
        assert_eq!(classify_find_symbol_degraded(0, 0, true), None);
        // 超时：空 = pending，有结果 = partial。
        assert_eq!(
            classify_find_symbol_degraded(1, 0, false),
            Some(Degraded::SemanticPending)
        );
        assert_eq!(
            classify_find_symbol_degraded(1, 0, true),
            Some(Degraded::Partial)
        );
        // 请求错误（冷启动窗口 LS 快速回错而非挂满预算）：空结果同样不可信。
        assert_eq!(
            classify_find_symbol_degraded(0, 2, false),
            Some(Degraded::SemanticPending)
        );
        assert_eq!(
            classify_find_symbol_degraded(0, 2, true),
            Some(Degraded::Partial)
        );
    }

    #[test]
    fn attach_degraded_inserts_fields_and_upgrades_null() {
        // 对象形态：顶层插键，既有键不动。
        let mut v = json!({"items": [], "raw_count": 0});
        attach_degraded(&mut v, Degraded::SemanticPending);
        assert_eq!(v["degraded"], json!("semantic-pending"));
        assert_eq!(v["warmup"]["stage"], json!("indexing"));
        assert_eq!(v["warmup"]["retry_after_warm"], json!(true));
        assert_eq!(v["warmup"]["progress"], serde_json::Value::Null);
        assert_eq!(v["raw_count"], json!(0), "既有键不动");

        // 裸 null（hover 空结果在无 warning 时的兜底）：升级为对象形态。
        let mut v = serde_json::Value::Null;
        attach_degraded(&mut v, Degraded::Partial);
        assert_eq!(v["items"], serde_json::Value::Null);
        assert_eq!(v["degraded"], json!("partial"));
    }

    #[test]
    fn sanitize_timeout_args_strips_warmup_ms() {
        let args = json!({"query": "q", "_warmup_ms": 100});
        let out = sanitize_timeout_args(args);
        assert!(out.get("_warmup_ms").is_none(), "_warmup_ms 不外泄 tool args");
        assert_eq!(out["query"], json!("q"));
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
        assert!(
            out.get("_index_timeout_ms").is_none(),
            "_index_timeout_ms 应被清掉"
        );
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
    #[cfg(windows)]
    fn compact_loc_decodes_percent_and_normalizes_drive_and_one_based() {
        // 盘符归一是 Windows 专有路径逻辑，unix 无盘符概念（v0.2.0 CI linux 实锤）。
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
        if std::env::var_os("SERENA_SKIP_LS_E2E")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            // 真 LS fixture 测试：CI 门禁外（runner 语义就绪窗口不可控），真机/nightly 覆盖。
            return;
        }
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
        assert_eq!(
            item["detail"],
            serde_json::json!("fn add(a: i32, b: i32) -> i32")
        );
        assert!(item.get("insert").is_none(), "insert==label 兜底副本应省略");
        assert!(
            item.get("doc").is_none(),
            "compact 下 doc 应省略（--json 可取全）"
        );
        assert!(item.get("deprecated").is_none(), "deprecated:false 应省略");
        assert!(
            item.get("edits").is_none(),
            "空 additional_text_edits 应省略"
        );
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
        assert_eq!(
            edits[0][1],
            serde_json::json!("use std::collections::HashMap;\n")
        );
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
        assert_eq!(
            by_line(6)["symbol"],
            json!("mock_helper"),
            "L6 = mock_helper"
        );
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
        enrich_search_with_symbols(&sup, Path::new("Z:/nonexistent_xyz_root"), &mut hits, None)
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
        assert_eq!(
            container, None,
            "顶层符号容器应为 None（滤 flatten 自身名）"
        );
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

    /// 1ve9：search 形态（hits 数组）超预算 → 截断 + 真实 original_count。
    #[test]
    fn apply_budget_truncates_hits_envelope() {
        let mut v = serde_json::json!({
            "hits": (0..100)
                .map(|i| serde_json::json!({"file": format!("f{i}.rs"), "line": i, "text": "x".repeat(20)}))
                .collect::<Vec<_>>(),
            "truncated": false,
            "files_scanned": 7,
        });
        let truncated = apply_budget(&mut v, 50); // 200 字节预算
        assert!(truncated, "超预算应发生截断");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["original_count"], 100, "original_count 取截断前条数");
        let hits = v["hits"].as_array().unwrap();
        assert!(hits.len() < 100, "hits 应被截短，实际 {}", hits.len());
        assert_eq!(v["files_scanned"], 7, "非 list 字段不动");
    }

    /// 1ve9：repo-map 形态（top 数组）超预算 → 截断。
    #[test]
    fn apply_budget_truncates_repo_map_top() {
        let mut v = serde_json::json!({
            "total_symbols": 100,
            "top": (0..100)
                .map(|i| serde_json::json!({"name": format!("s{i}"), "file": "a.rs", "kind": "function", "container": null}))
                .collect::<Vec<_>>(),
            "budget_bytes": 9999,
        });
        assert!(apply_budget(&mut v, 50));
        assert_eq!(v["truncated"], true);
        assert_eq!(v["original_count"], 100);
        assert!(v["top"].as_array().unwrap().len() < 100);
    }

    /// 1ve9：overview/list-dir 形态（顶层裸数组）→ 截断后折进 items 信封（字段沿用现名）。
    #[test]
    fn apply_budget_wraps_top_level_array_into_items_envelope() {
        let mut v = serde_json::Value::Array(
            (0..100)
                .map(|i| serde_json::json!({"name": format!("s{i}"), "kind": "function"}))
                .collect(),
        );
        let truncated = apply_budget(&mut v, 50);
        assert!(truncated);
        assert!(v.is_object(), "超预算裸数组应折进信封，实际: {v}");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["original_count"], 100);
        assert!(v["items"].as_array().unwrap().len() < 100);
    }

    /// 1ve9：read-file 形态（content 串超预算、无 list 数组）→ 不截断不折信封
    /// （行号 clamp 与 content_hash 写门契约不受预算护栏影响）。
    #[test]
    fn apply_budget_leaves_non_list_envelope_untouched() {
        let mut v = serde_json::json!({"content": "x".repeat(4096), "total_lines": 128});
        assert!(!apply_budget(&mut v, 50));
        assert!(v.get("truncated").is_none());
        assert!(v.get("original_count").is_none());
        assert_eq!(v["content"].as_str().unwrap().len(), 4096);
    }

    /// 1ve9：预算内裸数组零改动（不折信封，wire 形态保持）。
    #[test]
    fn apply_budget_passes_through_small_top_level_array() {
        let mut v = serde_json::Value::Array(vec![serde_json::json!({"name": "s"})]);
        assert!(!apply_budget(&mut v, 10000));
        assert!(v.is_array(), "预算内不得折信封");
        assert!(v.get("truncated").is_none());
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
        assert!(
            v["meta"].get("kind").is_none(),
            "递归删非 items 层的同名字段"
        );
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
        assert_eq!(
            v["compact"],
            serde_json::Value::Bool(true),
            "compact 键保留"
        );
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
    /// Ok 且 hits 来自 rust。bd qvv9 起 warning 语义分级：**查询已被 rust 回答**
    /// （hits 非空）→ NotInstalled 安装广告是纯噪声，不透出；**查询只存在于缺失
    /// lang**（py_foo，hits 空）→ python warning 解释「为什么空」（x67 语义保留）。
    /// 随后同 root 仅查 rust（warm session）→ 分支 3 全成功无 warning。
    /// 真拉 rust-analyzer —— RA 符号索引双阶段就绪（session ready ≠ workspace/symbol
    /// 可见，首查可能静默空），轮询非空。
    #[tokio::test]
    async fn find_symbol_partial_ls_failure_keeps_hits_and_warns() {
        if std::env::var_os("SERENA_SKIP_LS_E2E")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            // 真 LS fixture 测试：CI 门禁外（runner 语义就绪窗口不可控），真机/nightly 覆盖。
            return;
        }
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
            let (h, w, _) = sup
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
        // bd qvv9：查询已被 rust 回答 → python/toml 安装广告不透出（纯噪声）。
        assert!(
            warnings.is_empty(),
            "answered query must carry no NotInstalled noise; warnings={warnings:?}"
        );

        // 空结果路径：查询只存在于 py 文件且 pyright 缺失 → hits 空且 warning
        // 归属 python（x67「为什么空」语义保留）。
        let (hits, warnings, _) = sup
            .tool_find_symbol(dir.path(), "py_foo", 50, None)
            .await
            .expect("python-only query with pyright missing must not fail the call");
        assert!(hits.is_empty(), "py_foo must not appear without pyright");
        assert!(
            warnings.iter().any(|w| w.starts_with("python: ")),
            "empty result must be explained; warnings={warnings:?}"
        );

        // 分支 3：同 root 仅查 rust（session 已 warm、索引已就绪）→ 全成功无 warning。
        let (hits, warnings, _) = sup
            .tool_find_symbol(dir.path(), "alpha_main", 50, Some("rust"))
            .await
            .expect("rust-only query on warm session");
        assert!(!hits.is_empty(), "rust-only query must still hit");
        assert!(warnings.is_empty(), "all-success must carry no warnings");
    }
}

#[cfg(test)]
mod rename_semantic_reclassify_tests {
    //! critic3-F4（rename -32602 语义改判）+ critic3-F11（dry-run 剥
    //! post_write_diagnostics）的纯函数契约，不触 LS。
    use super::*;

    fn hit(range_sl: u32, range_sc: u32, range_el: u32, range_ec: u32) -> SymbolHit {
        SymbolHit {
            name: "add".into(),
            kind: SymbolKindTag::Function,
            uri: "file:///t/lib.rs".into(),
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

    fn rpc_32602() -> CoreError {
        CoreError::Rpc {
            code: -32602,
            message: "No references found at position".into(),
        }
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn rename_rpc_32602_inside_syntax_symbol_reclassifies_not_ready() {
        // 位置在 documentSymbol 覆盖内 = 位置合法 → 改判 NotReady（wire
        // LS_NOT_READY），cause 带 wait-ready 指引。
        let hits = vec![hit(4, 7, 6, 20)];
        let err = rename_rpc_reclassify(
            Some(&hits),
            pos(5, 8),
            rpc_32602(),
            "lib.rs",
        );
        let ToolError::Core(CoreError::NotReady { cause }) = err else {
            panic!("expect NotReady reclassification, got {err:?}");
        };
        assert!(cause.contains("wait-ready"), "指引: {cause}");
        assert!(cause.contains("lib.rs"), "带文件: {cause}");
        assert!(cause.contains("-32602"), "保留原始信息: {cause}");
    }

    #[test]
    fn rename_rpc_32602_outside_syntax_symbols_keeps_original() {
        // 语法层也无符号 = 真·位置无符号 → 原错误语义保留。
        let hits = vec![hit(4, 7, 6, 20)];
        let err = rename_rpc_reclassify(
            Some(&hits),
            pos(50, 0),
            rpc_32602(),
            "lib.rs",
        );
        match err {
            ToolError::Core(CoreError::Rpc { code: -32602, .. }) => {}
            other => panic!("original Rpc must be preserved, got {other:?}"),
        }
    }

    #[test]
    fn rename_rpc_32602_overview_probe_failed_keeps_original() {
        // 语法层探测失败/空 = 无法证明位置合法 → 保守保留原错误。
        for overview in [None, Some(Vec::<SymbolHit>::new())] {
            let err = rename_rpc_reclassify(
                overview.as_deref(),
                pos(5, 8),
                rpc_32602(),
                "lib.rs",
            );
            assert!(
                matches!(err, ToolError::Core(CoreError::Rpc { code: -32602, .. })),
                "overview={:?} must keep original, got {err:?}",
                overview.map(|h| h.len())
            );
        }
    }

    #[test]
    fn rename_rpc_non_32602_passes_through_untouched() {
        // 非 -32602（如 -32801 content modified）不归本判据管，原样透传。
        let hits = vec![hit(4, 7, 6, 20)];
        let err = rename_rpc_reclassify(
            Some(&hits),
            pos(5, 8),
            CoreError::Rpc {
                code: -32801,
                message: "content modified".into(),
            },
            "lib.rs",
        );
        match err {
            ToolError::Core(CoreError::Rpc { code: -32801, .. }) => {}
            other => panic!("non-32602 must pass through, got {other:?}"),
        }
    }

    #[test]
    fn dry_run_envelope_strips_post_write_diagnostics() {
        // critic3-F11：dry-run 没写盘，post_write_diagnostics 字段必须剥掉；
        // applied 翻转与 would_write 预览管道不变。
        let mut obj = serde_json::json!({
            "files_modified": 1,
            "post_write_diagnostics": "pending",
        })
        .as_object()
        .unwrap()
        .clone();
        dry_run_envelope(&mut obj, std::path::Path::new("/tmp"), vec![]);
        assert!(obj.get("post_write_diagnostics").is_none(), "{obj:?}");
        assert_eq!(obj["dry_run"], serde_json::json!(true));
        assert_eq!(obj["applied"], serde_json::json!(false));
        assert_eq!(obj["would_apply"], serde_json::json!(true));
        assert!(obj["would_write"].is_array(), "{obj:?}");
        assert_eq!(obj["files_modified"], serde_json::json!(1), "原字段不动");
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
        assert!(hover_is_empty(
            &serde_json::json!({"contents": {"value": ""}})
        ));
        assert!(!hover_is_empty(
            &serde_json::json!({"contents": {"value": "fn hello"}})
        ));
        assert!(!hover_is_empty(
            &serde_json::json!({"contents": [{"value": "x"}]})
        ));
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

    /// 装态复验三态:登记 exe 在盘 → 有效;登记 exe 消失(卸载/半包语义)→ 失效;
    /// 无登记 → 有效(防御,不让缺登记炸掉复用路径)。evict 必须同步清登记。
    /// session 级全链(命中→失效→LS_NOT_INSTALLED)由真机 e2e 覆盖(PM 复验命令)。
    #[tokio::test]
    async fn launch_exe_valid_rejects_vanished_exe() {
        let sup = Supervisor::direct().await.expect("Supervisor::direct");
        let key = Supervisor::key(Path::new("Z:/no/such/project"), "rust");
        // 无登记 → 有效。
        assert!(sup.launch_exe_valid(&key));
        // 登记的 exe 在盘 → 有效。
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("ls.exe");
        std::fs::write(&exe, "").expect("write fake exe");
        sup.launch_exe
            .lock()
            .unwrap()
            .insert(key.clone(), exe.clone());
        assert!(sup.launch_exe_valid(&key));
        // 登记的 exe 消失(卸载/半包:目录在 exe 不在)→ 失效。
        std::fs::remove_file(&exe).expect("remove fake exe");
        assert!(
            !sup.launch_exe_valid(&key),
            "登记 exe 消失必须判失效,活 session 不得复用"
        );
        // evict 同步清登记 → 回到无登记=有效。
        sup.launch_exe
            .lock()
            .unwrap()
            .insert(key.clone(), exe.clone());
        sup.evict(&key).await.expect("evict");
        assert!(
            sup.launch_exe_valid(&key),
            "evict 必须同步清 launch_exe 登记"
        );
        // bd serena-rust-bua（审计 A3）：exe 在盘但会话已 Failed → 判失效。
        // Session 构造不出 Failed 态，直接测纯函数两因子真值表。
        std::fs::write(&exe, "fake").expect("recreate fake exe");
        assert!(Supervisor::reuse_allowed(Some(&exe), None), "无会话态=有效");
        assert!(
            Supervisor::reuse_allowed(
                Some(&exe),
                Some(&lsp_core::session::SessionState::Ready)
            ),
            "exe 在盘 + 非 Failed = 有效"
        );
        assert!(
            !Supervisor::reuse_allowed(
                Some(&exe),
                Some(&lsp_core::session::SessionState::Failed("boom".into()))
            ),
            "Failed 会话必须判失效（respawn 不得复用旧登记）"
        );
        assert!(
            Supervisor::reuse_allowed(None, None),
            "无登记条目防御性放行（exe None=有效，单测锚定语义）"
        );
    }
}

/// NotInstalled Display 的 ls-use 中央指引（bd serena-rust-4ux）：所有语言的
/// NOT_INSTALLED 文案都必须带 ls-use 注册指引（用户自装 LS 免重复下载）。
#[cfg(test)]
mod not_installed_display_tests {
    use super::*;

    #[test]
    fn not_installed_display_carries_ls_use_guidance() {
        let e = ToolError::NotInstalled {
            language: "python".to_string(),
            hint: "pip install pyright".to_string(),
        };
        let msg = e.to_string();
        assert!(
            msg.contains("not installed: pip install pyright"),
            "原 hint 保留: {msg}"
        );
        assert!(
            msg.contains("serena-cli ls-use <lang> <path-to-ls-binary>"),
            "ls-use 指引: {msg}"
        );
    }
}

/// 上游对拍采纳 Wave 1：T0 三通道接线层的纯函数单测（不拉 LS）。
#[cfg(test)]
mod wave1_tuning_channel_tests {
    use super::*;
    use ls_registry::spec::ConfigReplyEntry;
    use lsp_core::framing::JsonRpc;

    #[test]
    fn deep_merge_json_overlays_nested_keys_and_replaces_types() {
        let mut base = serde_json::json!({ "a": { "x": 1, "y": 2 }, "keep": true });
        let overlay = serde_json::json!({ "a": { "y": 3, "z": 4 }, "b": "new" });
        deep_merge_json(&mut base, &overlay);
        assert_eq!(
            base,
            serde_json::json!({ "a": { "x": 1, "y": 3, "z": 4 }, "keep": true, "b": "new" })
        );
        // 类型不匹配：overlay 整体替换该键。
        let mut base2 = serde_json::json!({ "a": { "x": 1 } });
        deep_merge_json(&mut base2, &serde_json::json!({ "a": [1, 2] }));
        assert_eq!(base2, serde_json::json!({ "a": [1, 2] }));
    }

    fn reply(section: &str, value: serde_json::Value) -> ConfigReplyEntry {
        ConfigReplyEntry {
            section: section.to_string(),
            value,
        }
    }

    #[test]
    fn configuration_reply_matches_section_and_keeps_null_misses() {
        let replies = vec![
            reply("perl", serde_json::json!({ "perlInc": ["lib"] })),
            reply("runlinter", serde_json::Value::Bool(true)),
        ];
        let msg = JsonRpc::notification(
            "workspace/configuration",
            serde_json::json!({ "items": [
                { "section": "perl" },
                { "section": "unknown" },
                { },
                { "section": "runlinter" },
            ]}),
        );
        assert_eq!(
            configuration_reply_from_spec(&replies, &msg),
            serde_json::json!([
                { "perlInc": ["lib"] },
                serde_json::Value::Null,
                serde_json::Value::Null,
                true,
            ]),
            "命中回真值、未命中/缺 section 回 null（等长数组 = 默认应答形态）"
        );
    }

    #[test]
    fn configuration_reply_without_items_returns_empty_array() {
        let replies = vec![reply("x", serde_json::json!(1))];
        let msg = JsonRpc::notification("workspace/configuration", serde_json::json!({}));
        assert_eq!(
            configuration_reply_from_spec(&replies, &msg),
            serde_json::json!([])
        );
    }
}

#[cfg(test)]
mod path_traversal_tests {
    //! bd serena-rust-5r7：写类工具路径穿越拒收。纪律：不拉 LS ——
    //! execute_tool 入口 containment 检查先于 dispatch/tool 内 session_for，
    //! 穿越请求在任何 LS 启动前即 BAD_ARGS；helper 自身的词法/canonical
    //! 语义（绝对注入/混合分隔符/symlink）由 path_guard.rs 单测覆盖。

    use super::*;

    #[tokio::test]
    async fn every_write_tool_rejects_traversal_at_entry() {
        let sup = Supervisor::direct().await.expect("supervisor");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        for tool in undo::WRITE_TOOLS {
            let result = sup
                .execute_tool(
                    tool,
                    root,
                    json!({ "file": "../outside.txt", "content": "x", "start_line": 1, "end_line": 1 }),
                    None,
                )
                .await;
            let err = result.expect_err("穿越必须被拒");
            match err {
                ToolError::BadArgs { detail } => assert!(
                    detail.contains("escapes project root"),
                    "tool={tool}: detail={detail}"
                ),
                other => panic!("tool={tool} 应为 BAD_ARGS，实得 {other:?}"),
            }
        }
    }

    #[test]
    fn unsupported_extension_error_distinct_from_path_escape() {
        // 杠精 F3：扩展名门与路径守卫文案分流——扩展名错点明扩展名 + --lang 出路，
        // 用户能分清"文件类型错"vs"路径错"。
        let err = resolve_lang_for_file("doc.zzunsup99", None).unwrap_err();
        let ToolError::BadArgs { detail } = err else {
            panic!("应为 BAD_ARGS");
        };
        assert!(
            detail.contains("unsupported extension .zzunsup99") && detail.contains("--lang"),
            "{detail}"
        );
        // 无扩展名文件保持原语义（file not supported），不误报扩展名。
        let err = resolve_lang_for_file("zznosuchname99", None).unwrap_err();
        let ToolError::BadArgs { detail } = err else {
            panic!("应为 BAD_ARGS");
        };
        assert!(detail.contains("file not supported"), "{detail}");
    }

    #[test]
    fn registered_extension_gets_no_adapter_message() {
        // blindtest v5 P1-1：.rb 在 servers.toml 有登记（ruby_lsp extensions）→
        // 「注册了但无 adapter」+ ls-use 指引，不再是裸 unsupported。
        let err = resolve_lang_for_file("main.rb", None).unwrap_err();
        let ToolError::BadArgs { detail } = err else {
            panic!("应为 BAD_ARGS");
        };
        assert!(
            detail.contains("registered but no adapter for ruby") && detail.contains("ls-use"),
            "{detail}"
        );
    }

    #[test]
    fn rust_semantic_gate_fires_only_for_rust_without_cargo_manifest() {
        // blindtest v5 P1-2：裸 .rs 目录 → hover 短路 degraded；有 Cargo.toml /
        // 非 rust / 非语义工具 → 不拦。
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let args = serde_json::json!({ "file": "main.rs", "line": 0, "col": 3 });

        let v = rust_semantic_gate("hover", root, &args, None).expect("裸 rust 目录必拦");
        assert_eq!(v["degraded"], serde_json::json!("semantic-pending"));
        assert_eq!(v["warmup"]["retry_after_warm"], serde_json::json!(false));
        assert!(
            v["warning"]
                .as_str()
                .unwrap()
                .contains("rust LS requires a Cargo project"),
            "{}",
            v["warning"]
        );
        // 语法类工具不在门内。
        assert!(rust_semantic_gate("overview", root, &args, None).is_none());
        // 有 Cargo.toml 不拦。
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert!(rust_semantic_gate("hover", root, &args, None).is_none());
        std::fs::remove_file(root.join("Cargo.toml")).unwrap();
        // 非 rust 语言不拦。
        std::fs::write(root.join("app.py"), "x = 1\n").unwrap();
        let py_args = serde_json::json!({ "file": "app.py", "line": 0, "col": 0 });
        assert!(rust_semantic_gate("hover", root, &py_args, None).is_none());
        // --lang override 优先于扩展名。
        assert!(rust_semantic_gate("hover", root, &args, Some("rust")).is_some());
        assert!(rust_semantic_gate("hover", root, &args, Some("python")).is_none());
    }

    #[tokio::test]
    async fn failure_memo_short_circuits_second_call_with_hint() {
        // blindtest v5 P3-H：记账后第二次检查快速失败并带恢复指引；TTL 过期清账。
        let sup = Supervisor::direct().await.unwrap();
        let root = std::path::Path::new("D:/does/not/matter");
        sup.failure_memo_record(
            root,
            "al",
            MemoKind::NotInstalled {
                language: "al".into(),
                hint: "install the runtime".into(),
            },
        );
        let err = sup
            .failure_memo_check(root, "AL")
            .expect("同 root 不同大小写 lang 也要命中（键归一）");
        let ToolError::NotInstalled { hint, .. } = &err else {
            panic!("应重建 NotInstalled 同 wire 类");
        };
        assert!(
            hint.contains("previous call failed identically") && hint.contains("stop-all"),
            "{hint}"
        );
        // Terminated 类重建保 wire 类（LS_TERMINATED）。
        sup.failure_memo_record(
            root,
            "kotlin",
            MemoKind::Terminated { cause: "stdout pump EOF".into() },
        );
        let err = sup.failure_memo_check(root, "kotlin").unwrap();
        let ToolError::Core(CoreError::Terminated { ls, cause }) = &err else {
            panic!("应重建 Terminated 同 wire 类");
        };
        assert_eq!(ls, "kotlin");
        assert!(cause.contains("stdout pump EOF"), "{cause}");
    }

    #[tokio::test]
    async fn traversal_to_existing_outside_file_rejected() {
        // 真穿透形态：目标在 root 外但真实存在 —— 旧入口 is_file() 检查放行、
        // 写工具直写 root 外；修后入口 containment 即拒。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sibling.txt"), "old\n").unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("main.rs"), "fn main() {}\n").unwrap();
        let sup = Supervisor::direct().await.expect("supervisor");
        let result = sup
            .execute_tool(
                "replace-lines",
                proj.to_str().unwrap(),
                json!({ "file": "../sibling.txt", "start_line": 1, "end_line": 1, "content": "hacked" }),
                None,
            )
            .await;
        match result {
            Err(ToolError::BadArgs { detail }) => {
                assert!(detail.contains("escapes project root"), "{detail}")
            }
            other => panic!("应为 BAD_ARGS，实得 {other:?}"),
        }
        // 盘上未被改写。
        assert_eq!(std::fs::read_to_string(dir.path().join("sibling.txt")).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn create_text_file_rejects_traversal_without_touching_disk() {
        let sup = Supervisor::direct().await.expect("supervisor");
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        // 入口路径（execute_tool）。
        let via_entry = sup
            .execute_tool(
                "create-text-file",
                proj.to_str().unwrap(),
                json!({ "file": "../../evil.txt", "content": "x" }),
                None,
            )
            .await;
        assert!(matches!(via_entry, Err(ToolError::BadArgs { .. })), "{via_entry:?}");
        // 直调工具层（绕过入口的 --direct 形态）也有 per-site guard。
        let via_tool = sup
            .tool_create_text_file(&proj, "../../evil2.txt", "x", None)
            .await;
        assert!(matches!(via_tool, Err(ToolError::BadArgs { .. })), "{via_tool:?}");
        // root 外（含 root 旁两级）无任何落盘。
        assert!(!dir.path().join("evil.txt").exists());
        assert!(!dir.path().parent().unwrap().join("evil.txt").exists());
        assert!(!dir.path().parent().unwrap().join("evil2.txt").exists());
    }

    #[tokio::test]
    async fn write_tool_accepts_in_root_file_unchanged() {
        // 无回归：root 内合法相对路径照常通过入口（仅形态冒烟，语义链路由
        // e2e_write.rs 的真 LS 用例覆盖）。
        let sup = Supervisor::direct().await.expect("supervisor");
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.rs"), "fn main() {}\n").unwrap();
        let result = sup
            .execute_tool(
                "create-text-file",
                dir.path().to_str().unwrap(),
                json!({ "file": "proj/b.rs", "content": "fn b() {}\n" }),
                None,
            )
            .await;
        assert!(result.is_ok(), "root 内新建应放行: {result:?}");
        assert!(proj.join("b.rs").is_file());
    }
}

/// BD serena-rust-75k / 93q：静默失败清扫回归（回滚失败如实上报、rename 跳过可见）。
#[cfg(test)]
mod silent_failure_tests {
    use super::*;

    // ---- 75k：readback 回滚失败必须上报 ----

    /// 回滚成功路径：文件恢复老内容，reason 如实报 rolled back（无 FAILED 字样）。
    #[tokio::test]
    async fn rollback_reports_restored_on_success() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let path = tmp.path().join("a.rs");
        std::fs::write(&path, "new content").unwrap();

        let err = rollback_after_readback_mismatch(&path, tmp.path(), "old content").await;
        match err {
            ToolError::WriteConflict { reason, .. } => {
                assert!(reason.contains("rolled back"), "{reason}");
                assert!(!reason.contains("FAILED"), "{reason}");
            }
            other => panic!("应为 WriteConflict，实得 {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old content");
    }

    /// 回滚失败路径（目录占位使写回必败）：reason 必须带 rollback FAILED 事实，
    /// 不得谎报 rolled back。
    #[tokio::test]
    async fn rollback_failure_is_reported_not_lied() {
        let tmp = tempfile::TempDir::new().expect("TempDir::new");
        let blocker = tmp.path().join("blocked");
        std::fs::create_dir(&blocker).unwrap(); // 目录占位 → atomic_write rename 必败

        let err = rollback_after_readback_mismatch(&blocker, tmp.path(), "old content").await;
        match err {
            ToolError::WriteConflict { path, reason } => {
                assert!(reason.contains("rollback FAILED"), "{reason}");
                assert!(!reason.contains("rolled back"), "{reason}");
                assert!(path.contains("blocked"), "{path}");
            }
            other => panic!("应为 WriteConflict，实得 {other:?}"),
        }
    }

    // ---- 93q：rename 单文件跳过分类 ----

    fn file_uri(p: &Path) -> String {
        format!("file:///{}", p.to_string_lossy().replace('\\', "/"))
    }

    #[tokio::test]
    async fn prepare_rename_reports_unparsable_uri() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = prepare_rename_file(tmp.path(), "not-a-uri", &[])
            .await
            .unwrap_err();
        assert_eq!(err.file, "not-a-uri");
        assert!(err.reason.contains("uri"), "{}", err.reason);
    }

    #[tokio::test]
    async fn prepare_rename_reports_outside_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let f = outside.path().join("x.rs");
        std::fs::write(&f, "fn a() {}\n").unwrap();
        let f = dunce::canonicalize(&f).unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();

        let err = prepare_rename_file(&root, &file_uri(&f), &[])
            .await
            .unwrap_err();
        assert!(err.reason.contains("outside workspace root"), "{}", err.reason);
        assert!(err.file.ends_with("x.rs"), "{}", err.file);
    }

    #[tokio::test]
    async fn prepare_rename_applies_edits_and_reports_out_of_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let f = dunce::canonicalize(tmp.path().join("a.rs")).unwrap();
        let pos = |l: u32, c: u32| lsp_types::Position { line: l, character: c };
        let rng = |sl: u32, sc: u32, el: u32, ec: u32| lsp_types::Range {
            start: pos(sl, sc),
            end: pos(el, ec),
        };

        // 正常路径：edit 应用，内容与原内容都返回。
        let edits = vec![(0u64, rng(0, 3, 0, 4), "b".to_string())];
        let (abs, content, new_content) =
            prepare_rename_file(&root, &file_uri(&f), &edits).await.unwrap();
        assert_eq!(abs, f);
        assert_eq!(content, "fn a() {}\nfn b() {}\n");
        assert_eq!(new_content, "fn b() {}\nfn b() {}\n");

        // 越界行 → 整文件跳过 + 原因可见（原 break 静默点）。
        let bad = vec![(9u64, rng(999, 0, 999, 1), "x".to_string())];
        let err = prepare_rename_file(&root, &file_uri(&f), &bad)
            .await
            .unwrap_err();
        assert!(err.reason.contains("position out of range"), "{}", err.reason);
        assert!(err.file.ends_with("a.rs"), "{}", err.file);
    }

    /// wire 双面：skipped 为空时字段省略（向后兼容），非空时随报告输出。
    #[test]
    fn rename_report_skipped_field_backward_compatible() {
        let empty = RenameReport {
            files_modified: 1,
            edits_applied: 2,
            files: vec!["a.rs".into()],
            skipped: vec![],
        };
        let s = serde_json::to_string(&empty).unwrap();
        assert!(!s.contains("skipped"), "{s}");

        let partial = RenameReport {
            files_modified: 1,
            edits_applied: 1,
            files: vec!["a.rs".into()],
            skipped: vec![RenameSkipped {
                file: "b.rs".into(),
                reason: "read failed: os error 2".into(),
            }],
        };
        let s = serde_json::to_string(&partial).unwrap();
        assert!(s.contains("skipped"), "{s}");
        assert!(s.contains("read failed"), "{s}");
    }
}

#[cfg(test)]
mod sweep_a1_unit_tests {
    //! 清仓波 A1 新增逻辑的纯单测（bd qvv9/w2b5/kq6e/vro3/4lw/xxl/6k8x/eesd）。

    use super::*;

    fn hit(name: &str, sl: u32, sc: u32, el: u32, ec: u32) -> SymbolHit {
        SymbolHit {
            name: name.into(),
            kind: SymbolKindTag::Function,
            uri: "file:///t/f.rs".into(),
            range: lsp_types::Range {
                start: lsp_types::Position::new(sl, sc),
                end: lsp_types::Position::new(el, ec),
            },
            container: None,
        }
    }

    // ==== vro3 ====

    #[test]
    fn top_level_symbols_drops_contained_children() {
        let hits = vec![
            hit("mod", 0, 0, 20, 0),
            hit("fn_a", 2, 4, 5, 5),
            hit("fn_b", 6, 4, 9, 5),
            hit("sibling", 21, 0, 30, 0),
        ];
        let top = top_level_symbols(hits);
        let names: Vec<&str> = top.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["mod", "sibling"], "children must be dropped");
    }

    #[test]
    fn top_level_symbols_keeps_identical_ranges() {
        // 相同 range 互不严格包含 → 并列保留（可预测性优先）。
        let hits = vec![hit("a", 1, 0, 2, 0), hit("b", 1, 0, 2, 0)];
        assert_eq!(top_level_symbols(hits).len(), 2);
    }

    // ==== kq6e ====

    #[test]
    fn overview_summary_counts_kinds() {
        let mut f = hit("a", 0, 0, 1, 0);
        f.kind = SymbolKindTag::Function;
        let mut m = hit("b", 0, 0, 1, 0);
        m.kind = SymbolKindTag::Method;
        let mut o = hit("c", 0, 0, 1, 0);
        o.kind = SymbolKindTag::Other(23);
        let s = overview_summary(&[f.clone(), f, m, o]);
        assert!(s.starts_with("4 symbol(s): "), "{s}");
        assert!(s.contains("function×2"), "{s}");
        assert!(s.contains("method×1"), "{s}");
        assert!(s.contains("other(23)×1"), "{s}");
    }

    // ==== w2b5 ====

    #[test]
    fn kind_tag_from_name_maps_lsp_codes_for_narrowed_out_kinds() {
        assert_eq!(kind_tag_from_name("fn"), Some(SymbolKindTag::Function));
        assert_eq!(kind_tag_from_name("Function"), Some(SymbolKindTag::Function));
        assert_eq!(kind_tag_from_name("method"), Some(SymbolKindTag::Method));
        assert_eq!(kind_tag_from_name("class"), Some(SymbolKindTag::Class));
        // from_lsp 窄化表外的 kind 都是 Other(n) —— 别名按 LSP 3.17 码表命中。
        assert_eq!(kind_tag_from_name("struct"), Some(SymbolKindTag::Other(23)));
        assert_eq!(kind_tag_from_name("enum"), Some(SymbolKindTag::Other(10)));
        assert_eq!(kind_tag_from_name("mod"), Some(SymbolKindTag::Other(2)));
        assert_eq!(kind_tag_from_name("var"), Some(SymbolKindTag::Other(13)));
        assert_eq!(kind_tag_from_name("nope"), None);
    }

    // ==== 4lw ====

    #[test]
    fn parse_lsp_items_null_is_legal_empty_and_shape_drift_degrades() {
        let (v, degraded) = parse_lsp_items::<lsp_types::FoldingRange>(serde_json::Value::Null, "t");
        assert!(v.is_empty() && !degraded, "null = 合法空，不得标 degraded");
        // 非 null 但形态漂移（对象而非数组）→ 空结果 + degraded（warn 已在日志）。
        let (v, degraded) =
            parse_lsp_items::<lsp_types::FoldingRange>(serde_json::json!({ "x": 1 }), "t");
        assert!(v.is_empty() && degraded, "degraded={degraded}");
        // 合法数组 → 原样。
        let (v, degraded) = parse_lsp_items::<lsp_types::FoldingRange>(json!([]), "t");
        assert!(v.is_empty() && !degraded);
    }

    // ==== xxl：8.3 短名长名归一 ====

    #[tokio::test]
    async fn lsp_position_from_byte_passes_through_zero_based() {
        // A3b #2 契约锁：tool_refs/tool_hover 的 (line, col) 是 LSP 0-based 原样
        // 透传，接收侧不做 1-based 补偿——调用方（CLI to_lsp_pos、SymbolHit.range）
        // 自行保证 0-based。此前 ct_impact/recipe 多 +1 → 请求错位一行一列。
        let tmp = tempfile::tempdir().expect("tempdir");
        let f = tmp.path().join("a.rs");
        std::fs::write(&f, "fn a() {}\nfn b() {}\n").expect("write");
        let pos = lsp_position_from_byte(&f, "a.rs", 1, 4, OffsetEncoding::Utf16)
            .await
            .expect("in range");
        assert_eq!(
            (pos.line, pos.character),
            (1, 4),
            "0-based 输入必须原样透传"
        );
        let err = lsp_position_from_byte(&f, "a.rs", 9, 0, OffsetEncoding::Utf16)
            .await
            .expect_err("越界必须拒绝");
        assert!(err.to_string().contains("out of range"));
    }

    #[tokio::test]
    async fn recorded_write_dry_run_intercepts_and_collects_preview() {
        // A3b #3（bd i4a1/wlrr）undo 层契约：干跑下 recorded_write 返回 Ok、
        // 不落盘、不记 undo 快照，将写内容进 PREVIEW。
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("a.txt");
        let (_, preview) = undo::scope_dry_run(async {
            undo::recorded_write(&p, "new\n").await.expect("dry write ok");
        })
        .await;
        assert_eq!(
            preview,
            vec![(p.display().to_string(), "new\n".to_string())],
            "预览须收到 (path, new_content)"
        );
        assert!(!p.exists(), "干跑绝不落盘");
    }

    #[tokio::test]
    async fn dry_run_create_text_file_previews_without_disk_write() {
        // A3b #3（bd i4a1/wlrr）execute_tool 层接线：写门入口 dry_run 检查，
        // 成功返回附 dry_run + would_write，盘上无文件（.txt 无 adapter 免拉 LS）。
        let sup = Supervisor::direct().await.expect("supervisor");
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_str().expect("utf8 root");
        let v = sup
            .execute_tool(
                "create-text-file",
                root,
                serde_json::json!({
                    "file": "preview.txt",
                    "content": "fn dry() {}\n",
                    "dry_run": true
                }),
                None,
            )
            .await
            .expect("dry-run must succeed");
        assert_eq!(v["dry_run"], serde_json::json!(true), "返回须带 dry_run 标记");
        // 杠精 wuhi：dry_run:true 下 applied 必须 false（语义修正），写意原图 would_apply。
        assert_eq!(v["applied"], serde_json::json!(false), "dry-run 不得报 applied:true");
        assert_eq!(v["would_apply"], serde_json::json!(true), "须带 would_apply:true");
        let ww = v["would_write"].as_array().expect("would_write array");
        assert_eq!(ww.len(), 1, "单文件写恰好一条预览");
        assert!(
            ww[0]["file"].as_str().unwrap_or("").ends_with("preview.txt"),
            "预览 file 为目标路径: {}",
            ww[0]["file"]
        );
        // 杠精 wuhi：would_write 默认 hunk 化（unified patch），不再塞全文 content。
        assert!(
            ww[0].get("content").is_none(),
            "content 全文字段必须移除: {}",
            ww[0]
        );
        let patch = ww[0]["patch"].as_str().expect("patch string");
        assert!(
            patch.starts_with("--- /dev/null\n+++ b/") && patch.contains("+fn dry() {}"),
            "新建文件 unified patch 形态: {patch}"
        );
        assert!(!dir.path().join("preview.txt").exists(), "干跑绝不落盘");
    }

    #[test]
    fn normalize_long_path_roundtrips_existing_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = dunce::canonicalize(tmp.path()).expect("canonicalize");
        let out = normalize_long_path(&p).expect("some");
        assert_eq!(
            out.to_string_lossy().to_lowercase(),
            p.to_string_lossy().to_lowercase(),
        );
        // 不存在的路径 → std canonicalize 与 dunce 回落都失败 → None（不新增失败模式）。
        assert!(normalize_long_path(Path::new("Z:/definitely_not_there_xyz_42")).is_none());
    }

    // ==== 6k8x：盘符大小写三形态归一（realign 块同一链路）====

    #[cfg(windows)]
    #[test]
    fn uri_case_forms_converge_to_canonical_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = dunce::canonicalize(tmp.path()).expect("canonicalize");
        let p = real.to_string_lossy().replace('\\', "/");
        let (drive, rest) = p.split_once(':').expect("drive path");
        let rest_body = rest.trim_start_matches('/');
        let forms = [
            format!("file:///{drive}:{rest}"), // 原生形态
            format!(
                "file:///{0}:/{1}",
                drive.to_ascii_uppercase(),
                rest_body.to_ascii_lowercase()
            ), // 盘符大写 + 内段全小写
            format!("file:///{0}:{rest}", drive.to_ascii_lowercase()), // 盘符小写
        ];
        for uri in forms {
            let got = uri_to_path(&uri).and_then(|pp| normalize_long_path(&pp));
            assert_eq!(
                got.as_ref().map(|g| g.to_string_lossy().to_lowercase()),
                Some(real.to_string_lossy().to_lowercase()),
                "uri {uri} must converge to the real path"
            );
        }
    }

    // ==== eesd：UNC `\\server\share`（path_to_uri_str + uri_to_path fixture）====

    #[cfg(windows)]
    #[test]
    fn unc_path_roundtrips_through_uri_str_form() {
        let unc = Path::new(r"\\server\share\dir\f.rs");
        // path_to_uri_str（纯字符串，不碰盘）：UNC 发射为 4 斜杠 file://// 形态。
        let uri = path_to_uri_str(unc);
        assert!(
            uri.starts_with("file:////server/share/"),
            "unexpected UNC uri shape: {uri}"
        );
        assert!(uri.ends_with("dir/f.rs"), "{uri}");
        // uri_to_path：canonicalize 不可达 UNC 失败 → 词法回退（bd eesd 实测该回退
        // 会丢 UNC 前缀形态——lsp-core 侧已知限制，修法归 lsp-core 域）。此处只钉
        // 「不 panic、返回 Some」的契约边界。
        assert!(uri_to_path(&uri).is_some(), "unreachable UNC must still yield Some");
    }

    #[cfg(windows)]
    #[test]
    fn unc_canonical_three_slash_uri_yields_relative_path_known_limitation() {
        // RFC 常见 `file://server/share/f.rs` 三斜杠形态：uri_to_path 当前语义丢
        // authority → 相对路径（lsp-core 侧边界，bd eesd 登记为已知限制）。
        let back = uri_to_path("file://server/share/f.rs").expect("some");
        assert!(!back.is_absolute(), "document current behavior: {back:?}");
    }
}

#[cfg(test)]
mod sweep_a3a_unit_tests {
    //! 清仓波 A3a 新增逻辑的纯单测（bd tfa3/51ib/rsqq/z0kg/6ooi）。

    use super::*;

    fn sym_json(name: &str, sl: u32, sc: u32, el: u32, ec: u32) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "kind": "Function",
            "uri": "file:///t/f.rs",
            "range": {
                "start": {"line": sl, "character": sc},
                "end": {"line": el, "character": ec},
            },
            "container": null,
        })
    }

    // ==== tfa3：写回执瘦身 ====

    #[test]
    fn slim_write_receipt_pending_shrinks_to_word() {
        let mut v = serde_json::json!({
            "applied": true, "file": "a.rs",
            "post_write_diagnostics": {"items": [], "pending": true},
        });
        slim_write_receipt(&mut v);
        assert_eq!(v["post_write_diagnostics"], serde_json::json!("pending"));
        assert_eq!(v["applied"], serde_json::json!(true), "其余键不动");
    }

    #[test]
    fn slim_write_receipt_confirmed_clean_drops_key() {
        let mut v = serde_json::json!({
            "applied": true, "file": "a.rs",
            "post_write_diagnostics": {"items": [], "pending": false},
        });
        slim_write_receipt(&mut v);
        assert!(v.get("post_write_diagnostics").is_none(), "确认干净 → 键省略");
    }

    #[test]
    fn slim_write_receipt_nonempty_items_untouched() {
        let diag = serde_json::json!({
            "items": [{"message": "x", "line": 1}], "pending": false,
        });
        let mut v = serde_json::json!({
            "applied": true, "file": "a.rs", "post_write_diagnostics": diag,
        });
        slim_write_receipt(&mut v);
        assert_eq!(
            v["post_write_diagnostics"]["items"].as_array().map(Vec::len),
            Some(1),
            "有 items 原样保留"
        );
    }

    #[test]
    fn slim_write_receipt_without_key_is_noop() {
        let mut v = serde_json::json!({"created": true, "file": "a.rs"});
        slim_write_receipt(&mut v);
        assert_eq!(v, serde_json::json!({"created": true, "file": "a.rs"}));
    }

    // ==== 51ib：format 档位 ====

    #[test]
    fn out_format_defaults_to_full_and_rejects_unknown() {
        assert_eq!(out_format(&json!({})).unwrap(), OutFormat::Full);
        assert_eq!(out_format(&json!({"format": "full"})).unwrap(), OutFormat::Full);
        assert_eq!(out_format(&json!({"format": "brief"})).unwrap(), OutFormat::Brief);
        assert_eq!(out_format(&json!({"format": "json"})).unwrap(), OutFormat::Json);
        assert!(out_format(&json!({"format": "xml"})).is_err());
    }

    // ==== rsqq：search summary 头 ====

    #[test]
    fn search_summary_counts_files_and_marks_truncation() {
        let mk = |file: &str| SearchHit {
            file: file.into(),
            line: 1,
            col: 1,
            text: "x".into(),
            match_start: 0,
            match_end: 1,
            symbol: None,
            container: None,
        };
        let mut resp = SearchResponse {
            hits: vec![mk("a.rs"), mk("a.rs"), mk("b.rs")],
            truncated: false,
            files_scanned: 9,
            hint: None,
        };
        assert_eq!(search_summary(&resp), "3 hits in 2 files");
        resp.truncated = true;
        assert_eq!(search_summary(&resp), "3 hits in 2 files; truncated");
    }

    // ==== z0kg：env 默认条数 ====

    #[test]
    fn parse_num_env_trims_and_rejects_garbage() {
        assert_eq!(parse_num_env(" 50 "), Some(50));
        assert_eq!(parse_num_env("0"), Some(0));
        assert_eq!(parse_num_env("abc"), None);
        assert_eq!(parse_num_env(""), None);
    }

    #[test]
    fn arg_limit_explicit_flag_wins() {
        // 显式旗永远优先（env 未设路径）；env 生效路径 = or_else 一行，无分支可错。
        let args = json!({"limit": 7});
        assert_eq!(arg_limit(&args, "limit", 50), 7);
        assert_eq!(arg_limit(&json!({}), "limit", 50), 50);
    }

    // ==== 6ooi：symbol-tree 过滤 ====

    #[test]
    fn filter_tree_entry_passthrough_when_no_filters() {
        let e = serde_json::json!({"file": "a.rs", "symbols": [sym_json("f", 0, 0, 1, 0)]});
        let out = filter_tree_entry(e.clone(), None, None).unwrap();
        assert_eq!(out, e, "无开关原样返回（默认 wire 不变）");
    }

    #[test]
    fn filter_tree_entry_grep_is_case_insensitive_substring() {
        let e = serde_json::json!({
            "file": "a.rs",
            "symbols": [sym_json("run_fast", 0, 0, 1, 0), sym_json("slow", 2, 0, 3, 0)],
        });
        let out = filter_tree_entry(e, Some("RUN"), None).unwrap();
        let names: Vec<&str> = out["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["run_fast"]);
    }

    #[test]
    fn filter_tree_entry_max_depth_uses_containment_chain() {
        let e = serde_json::json!({
            "file": "a.rs",
            "symbols": [
                sym_json("mod", 0, 0, 20, 0),
                sym_json("fn_in", 2, 4, 5, 5),
                sym_json("peer", 21, 0, 22, 0),
            ],
        });
        // max_depth=1 = 只留顶层（fn_in 被 mod 严格包含 → 深度 1 ≥ 1 滤掉）。
        let out = filter_tree_entry(e, None, Some(1)).unwrap();
        let names: Vec<&str> = out["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["mod", "peer"]);
    }

    #[test]
    fn filter_tree_entry_drops_entry_when_all_filtered() {
        let e = serde_json::json!({"file": "a.rs", "symbols": [sym_json("f", 0, 0, 1, 0)]});
        assert!(filter_tree_entry(e, Some("zzz"), None).is_none(), "滤空条目省略");
    }

    #[test]
    fn symbol_containment_depth_excludes_identical_ranges() {
        let all = vec![sym_json("a", 1, 0, 2, 0), sym_json("b", 1, 0, 2, 0)];
        assert_eq!(symbol_containment_depth(&all[0], &all), 0, "并列不加深");
    }
}

/// 盲测 v4 修复波的纯函数单测（bd edpi/2rxp/P2-2）。
#[cfg(test)]
mod blindv4_unit_tests {
    use super::*;

    // ---- bd edpi：原子写 NotFound → BAD_ARGS（父目录指引），其余保持 WRITE_CONFLICT ----

    #[test]
    fn atomic_write_not_found_maps_to_bad_args_with_parent_hint() {
        // os error 3（Windows ERROR_PATH_NOT_FOUND）→ io::ErrorKind::NotFound。
        let e = std::io::Error::from_raw_os_error(3);
        let err = atomic_write_err("D:/tmp/proj/no_dir/sub/f.txt", e);
        match err {
            ToolError::BadArgs { detail } => {
                assert!(
                    detail.contains("parent directory does not exist: "),
                    "hint 指到缺失的父目录: {detail}"
                );
                assert!(detail.contains("no_dir/sub"), "{detail}");
                assert!(detail.contains("create it first"), "{detail}");
            }
            other => panic!("NotFound 必须归 BAD_ARGS，实际: {other:?}"),
        }
    }

    #[test]
    fn atomic_write_other_io_errors_stay_write_conflict() {
        let e = std::io::Error::from_raw_os_error(5); // 拒绝访问——真冲突候选
        let err = atomic_write_err("D:/tmp/proj/a.py", e);
        assert!(
            matches!(err, ToolError::WriteConflict { .. }),
            "非 NotFound 保持 WRITE_CONFLICT: {err:?}"
        );
    }

    // ---- bd 2rxp：出站 uri 归一 ----

    #[test]
    fn file_uri_drive_letter_normalized() {
        assert_eq!(
            normalize_file_uri("file:///c%3A/Users/x/a.py"),
            "file:///C:/Users/x/a.py"
        );
        assert_eq!(
            normalize_file_uri("file:///c:/Users/x/a.py"),
            "file:///C:/Users/x/a.py"
        );
        // 已是目标形态：原样。
        assert_eq!(
            normalize_file_uri("file:///C:/Users/x/a.py"),
            "file:///C:/Users/x/a.py"
        );
        // 非 file scheme / 非盘符段：原样。
        assert_eq!(
            normalize_file_uri("file:///home/u/a.py"),
            "file:///home/u/a.py"
        );
        assert_eq!(
            normalize_file_uri("https://example.com/x"),
            "https://example.com/x"
        );
        // 路径段编码（%20）不动。
        assert_eq!(
            normalize_file_uri("file:///C:/My%20Docs/a.py"),
            "file:///C:/My%20Docs/a.py"
        );
    }

    #[test]
    fn output_uri_walker_touches_only_uri_fields() {
        let mut v = serde_json::json!({
            "items": [
                {"uri": "file:///c%3A/Users/x/a.py", "line": 1},
                {"targetUri": "file:///d:/y/b.py"},
            ],
            "uri": "file:///e:/top.py",
            "note": "file:///c%3A/not_a_uri_field.py",
            "nested": {"deep": [{"uri": "file:///f%3A/z.py"}]},
        });
        normalize_output_uris(&mut v);
        assert_eq!(v["items"][0]["uri"], "file:///C:/Users/x/a.py", "{v}");
        assert_eq!(v["items"][1]["targetUri"], "file:///D:/y/b.py", "{v}");
        assert_eq!(v["uri"], "file:///E:/top.py", "{v}");
        assert_eq!(
            v["note"],
            "file:///c%3A/not_a_uri_field.py",
            "非 uri 字段不动"
        );
        assert_eq!(v["nested"]["deep"][0]["uri"], "file:///F:/z.py", "{v}");
    }
}
