//! IDE 级 undo/redo（事务版快照栈）。
//!
//! 设计（PM 拍板 + 两轮补充契约）：
//! - 存储：`default_cache_root()/undo/{project_hash}/txn-{N}/`。project_hash =
//!   `dunce::canonicalize(project_root)` 路径串的 sha256 前 16 hex —— **禁用
//!   std DefaultHasher/RandomState**（每进程随机种子，daemon 重启即换目录名 =
//!   数据丢失）；canonicalize 失败（目录被删）直接报错不落盘。路径中无版本号/
//!   构建号段，跨软件更新/重启命中同一目录。
//! - 每事务目录含 `manifest.json`：`{txn_id, timestamp, files:[FileRec]}`。
//!   FileRec 契约四字段 `path/created/before/after_sha256` 之外追加
//!   `after/before_file/after_file`：redo 要把文件写回 after 内容（契约设计第 3
//!   条「redo 可重放」），只有 after_sha256 物理上无法重放；>100KB 的快照旁路存
//!   `before/{i}`、`after/{i}`（i = 同类旁路文件序号，manifest 持有相对路径引用）
//!   防 manifest 爆炸。
//! - 事务边界 = execute_tool 的一次调用（rename-symbol 多文件改动在同一调用内
//!   逐个 `recorded_write`，天然聚合成一个事务）。写点统一收口 `recorded_write`。
//! - 崩溃序（bd serena-rust-15jb）：`recorded_write` 先把该文件的 txn 记录持久化
//!   （side 文件 + manifest 原子重写，manifest 是记账提交点），**再**写目标文件
//!   —— kill 打中写提交窗只会留下「有记录无改动」（undo 侧幂等收口为 no-op），
//!   绝不出现「有改动无记录」的永久脱账写入。
//! - undo 冲突语义（bd serena-rust-3ux6 拍板：**自动跳过 + warning**，不做
//!   `--force`；bd e1f4 收紧：冲突即停）：盘面与事务后状态不符（外部编辑）的事务
//!   已不可干净回滚，undo 把它整事务改名 `discarded-{N}`（留档、不参与栈/prune
//!   照常回收），附 warning 指明原因，**并立即停止**——冲突证明时间线已被外部
//!   编辑打乱，更老事务的 pre-image 不再可信，fall-through 会静默回滚无关改动
//!   （盲测 v4 实锤）。响应带 `stopped_early`（CLI 据此置 rc=2）。torn（无
//!   manifest 的崩溃窗残骸）仍跳过继续——那不是外部编辑，时间线未乱。剩余
//!   `WRITE_CONFLICT`（IO 占用/存储损坏）仍整步报错。redo 侧对称（外部冲突
//!   链式 discarded 清栈）。
//! - redo 重放序（bd serena-rust-b5od）：按**事务时间序（N 升序）**重放，即最
//!   后 undo 的先 redo —— undo 是 LIFO 弹栈，正放必须还原原始写入顺序，否则
//!   深度 ≥2 时 pre-image 对账必然失配（F1 根因：曾取 N 最大 undone 项）。每次
//!   redo 调用重放一个有效事务（IDE 单步语义不变）；盘面已处于目标态（崩溃窗）
//!   → no-op 收口。
//! - undo 冲突门（保留）：恢复写前逐文件校验盘面，外部编辑冲突整事务跳过（见
//!   上），IO/存储错误整步报错。错误映射复用 `ToolError::WriteConflict` → wire
//!   `WRITE_CONFLICT`（零新错误码）。
//! - created=true 的文件 undo = 删除文件（用户拍板）；redo = 按 after 内容重建。
//! - LS 态同步（P2-b）：整事务恢复成功后把涉及文件登记进 `TOUCHED`（uid 键侧信道），
//!   undo/redo 收口取走并逐文件 didChange / didClose（lib.rs `sync_ls_after_undo`）。
//! - 栈序：N 大 = 新。undo 取 N 最大的 `txn-{N}`；undo 后 rename 为
//!   `undone-{N}`；redo 取 N **最小**的 `undone-{N}` 重放后 rename 回 `txn-{N}`
//!   （时间序正放，见上）；新事务落盘后删除全部 `undone-*`（IDE 语义：新写入
//!   清空 redo 链）。
//! - 保留策略（新事务追加前 + `undo --list` 时 prune，无后台定时器）：
//!   ① 事务时间戳 > 30 天 ② 总事务数 > 20 ③ 总大小 > 200 MB —— 从最旧
//!   （N 最小）开始整事务淘汰。

use std::cmp::Reverse;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ToolError;

/// 单项目 undo 栈深度上限（契约设计第 3 条）。
pub(crate) const MAX_TXNS: usize = 20;
/// 单项目 undo 目录总大小上限（补充契约 7）。
pub(crate) const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;
/// 事务保留时长（补充契约 7）。
pub(crate) const MAX_AGE_SECS: u64 = 30 * 24 * 3600;
/// 快照内嵌 manifest 的阈值；超过旁路存文件，防 manifest 爆炸（契约设计第 1 条授权自定）。
const INLINE_LIMIT: usize = 100 * 1024;

/// prune 上限参数化：生产走 [`Limits::default`]，单测注入小上限验证淘汰顺序。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_txns: usize,
    pub max_total_bytes: u64,
    pub max_age_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_txns: MAX_TXNS,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_age_secs: MAX_AGE_SECS,
        }
    }
}

/// manifest.json（serde_json 落盘形态）。
#[derive(Serialize, Deserialize)]
struct Manifest {
    txn_id: u64,
    /// unix epoch 秒。
    timestamp: u64,
    files: Vec<FileRec>,
}

/// 单文件快照记录。`before/after` Some = ≤100KB 内嵌；None + 对应 `*_file` =
/// 旁路文件（相对事务目录）；`before=None` 且 `created=true` = 新建无旧内容。
#[derive(Serialize, Deserialize)]
struct FileRec {
    /// 绝对路径（契约）。
    path: String,
    created: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    after_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_file: Option<String>,
}

/// 打开的 WAL 事务：uid → txn 目录绝对路径（bd serena-rust-15jb：记账先行，
/// 事务目录在 `recorded_write` 首笔写入时即落盘，commit 只做收口、abort 负责
/// 删除）。abort 在同步上下文（TxnGuard::drop）调用，故存绝对路径而非 store，
/// 免去收口点再解析项目根。
static OPEN_TXNS: StdMutex<Vec<(u64, PathBuf)>> = StdMutex::new(Vec::new());
static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// undo/redo 恢复写涉及的文件登记（P2-b LS 态同步桥）。键 = 承载本次 undo/redo
/// 工具调用的事务 uid —— 恢复路径在 execute_tool 的 TXN_UID 作用域内执行，并发
/// undo/redo 各记各账（与 PENDING 同款 std 锁 + 短临界区）。选侧信道而非改
/// undo_at/redo_at 返回类型：wire 输出与既有单测零改动，且中途冲突时已恢复的
/// 前缀文件也能带出（返回值版会被 Err 吞掉）。
static TOUCHED: StdMutex<Vec<(u64, TouchedFile)>> = StdMutex::new(Vec::new());

/// undo/redo 恢复写涉及的单个文件（LS 态同步用）：路径 + 是否 created 文件。
/// created 且已被删除 → didClose；其余（改写 / redo 重建）→ didChange。
#[derive(Clone, Debug)]
pub(crate) struct TouchedFile {
    pub path: String,
    pub created: bool,
}

tokio::task_local! {
    /// 当前事务 uid。execute_tool 用 `scope` 包裹工具执行；scope 外（--direct
    /// 调试路径）读到 0 = 不记账。
    pub(crate) static TXN_UID: u64;

    /// 当前事务的 undo 存储目录根（bd serena-rust-15jb）。WAL 要求
    /// `recorded_write` 在写目标文件前持久化 txn 记录，而存储目录按项目根哈希
    /// 键控、工具层只有文件路径 —— 由 execute_tool / ct_txn 在进入 TXN_UID
    /// scope 时一并注入。uid≠0 而本值缺失 = 接线缺陷，recorded_write 显式报错。
    pub(crate) static TXN_STORE: PathBuf;

    /// A3b #3（bd i4a1/wlrr）：--dry-run 干跑开关。execute_tool 对写类工具以
    /// `scope_dry_run` 包裹 dispatch；recorded_write 命中时不落盘、不记 undo 快照，
    /// 把 (path → new_content) 收进 [`PREVIEW`]，由 execute_tool 收尾附进返回。
    static DRY_RUN: bool;

    /// dry-run 期间 collected 将写内容（随 DRY_RUN scope 同生共死）。
    static PREVIEW: std::cell::RefCell<Vec<(String, String)>>;
}

/// dry-run scope：不进 undo 事务（无 TxnGuard commit/abort），recorded_write 全部
/// 转预览收集。返回 (工具输出, 预览) —— 预览必须在 scope 内取走（TaskLocalFuture
/// drop 即销毁），调用方不得再自行 take_preview。
///
/// fut 必须 Box::pin：dispatch_tool 的 future 巨大（全工具 match 单体），再叠两层
/// TaskLocalFuture 直接内嵌会在全量测试并行下把 poll 栈压过临界（实测
/// STATUS_STACK_OVERFLOW，bd A3b 票4 收口时修）。
pub(crate) async fn scope_dry_run<F: Future>(fut: F) -> (F::Output, Vec<(String, String)>) {
    DRY_RUN
        .scope(
            true,
            PREVIEW.scope(std::cell::RefCell::new(Vec::new()), Box::pin(async move {
                let out = fut.await;
                let preview = PREVIEW.with(|p| p.borrow_mut().drain(..).collect());
                (out, preview)
            })),
        )
        .await
}

/// 当前是否处于 --dry-run 干跑（scope 外恒 false）。写工具内的盘面自证步骤
/// （readback 比对）在干跑下没有前提，调用方据此跳过。
pub(crate) fn is_dry_run() -> bool {
    DRY_RUN.try_with(|v| *v).unwrap_or(false)
}

/// 分配本调用的事务 uid（execute_tool 开局调用）。
pub(crate) fn next_uid() -> u64 {
    NEXT_UID.fetch_add(1, Ordering::Relaxed)
}

/// sha256(bytes) 完整 64 hex 小写。
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 项目根 → undo 存储目录：`default_cache_root()/undo/{project_hash}`。
/// bd serena-rust-ej5：canonicalize 失败（挪盘瞬窗/目录权限变化）不再 BAD_ARGS
/// 整栈孤儿——退回对原始路径串做 sha256（无 canonicalize 也有确定性键）；彻底
/// 删除的目录仍 BAD_ARGS 不落盘（补充契约 9：连原路径串都拿不到才算不存在）。
pub(crate) fn store_for(root: &Path) -> Result<PathBuf, ToolError> {
    let hash_source = match dunce::canonicalize(root) {
        Ok(canon) => canon.to_string_lossy().into_owned(),
        Err(e) if root.exists() => {
            tracing::warn!(root = %root.display(), error = %e, "canonicalize failed; hashing raw path for undo store");
            root.to_string_lossy().into_owned()
        }
        Err(e) => {
            return Err(ToolError::BadArgs {
                detail: format!("project root not found ({}): {e}", root.display()),
            });
        }
    };
    let hash = sha256_hex(hash_source.as_bytes());
    Ok(ls_runtime::install::default_cache_root()
        .join("undo")
        .join(&hash[..16]))
}

/// 写类工具名单：这些工具的 execute_tool 调用按「一次调用 = 一个事务」记账。
/// format/format-range 不在此列 —— 它们只返回 TextEdit 列表，不写盘。
pub(crate) const WRITE_TOOLS: &[&str] = &[
    "replace-body",
    "rename-symbol",
    "replace-text-in-symbol",
    "insert-text-before-symbol",
    "insert-text-after-symbol",
    "delete-text-in-symbol",
    "safe-delete-symbol",
    "insert-at-line",
    "replace-lines",
    "delete-lines",
    "create-text-file",
];

/// 写类工具判据（bd wy1：daemon 断连 detach 用）。名单本体保持 crate 内，
/// 跨 crate（daemon http 层）只暴露这个谓词。
pub fn is_write_tool(tool: &str) -> bool {
    WRITE_TOOLS.contains(&tool)
}

/// 写点统一收口：快照旧内容 → **先持久化 txn 记录（WAL）** → 原子写目标文件。
///
/// 崩溃序不变量（bd serena-rust-15jb）：manifest（记账提交点）先于目标文件落
/// 盘 —— kill 打中写提交窗只会留下「有记录无改动」，undo 侧按 no-op 幂等收口；
/// 「有改动无记录」的脱账写入在结构上不可能出现。与 [`crate::atomic_write`] 同
/// 签名同错误面（io::Error），调用点仅换函数名。
/// 无事务上下文（uid=0，--direct 路径）退化为裸 atomic_write。
pub(crate) async fn recorded_write(path: &Path, new_content: &str) -> std::io::Result<()> {
    // A3b #3：--dry-run 干跑 —— 不落盘、不记 undo 快照，将写内容转预览收集
    // （execute_tool 收尾取走附进返回）。
    if is_dry_run() {
        PREVIEW.with(|p| {
            p.borrow_mut()
                .push((path.display().to_string(), new_content.to_owned()))
        });
        return Ok(());
    }
    let uid = TXN_UID.try_with(|v| *v).unwrap_or(0);
    if uid == 0 {
        return crate::atomic_write(path, new_content).await;
    }
    let store = TXN_STORE
        .try_with(|s| s.clone())
        .map_err(|_| std::io::Error::other("undo txn store not in scope (missing TXN_STORE wiring)"))?;
    // 快照必须在写盘前取（契约设计第 2 条：成功写盘前把旧状态快照入栈）。
    let (before, created) = match tokio::fs::read_to_string(path).await {
        Ok(c) => (Some(c), false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, true),
        Err(e) => return Err(e),
    };
    // WAL：先记账（txn 目录 + manifest 原子重写），再动目标文件。
    wal_append(&store, uid, path, before, created, new_content).await?;
    crate::atomic_write(path, new_content).await
}

/// WAL 初始化：uid 首笔写入时 prune → 分配事务号 → 建 `txn-{N}` 目录 → 写空
/// manifest（原子写保证目录自创建起始终可读）。uid → 目录登记进 [`OPEN_TXNS`]。
/// WAL_INIT 串行化分配临界区：不同 uid 并发 wal_open 时防止 alloc 撞号（写门只
/// 保证单工具内有序，不假设所有 recorded_write 调用点都持门）。
static WAL_INIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn wal_open(store: &Path, uid: u64) -> std::io::Result<PathBuf> {
    {
        let open = OPEN_TXNS.lock().expect("undo OPEN_TXNS lock poisoned");
        if let Some((_, dir)) = open.iter().find(|(u, _)| *u == uid) {
            return Ok(dir.clone());
        }
    }
    let _init = WAL_INIT.lock().await;
    // 双检：等锁期间同 uid 可能已开账。
    {
        let open = OPEN_TXNS.lock().expect("undo OPEN_TXNS lock poisoned");
        if let Some((_, dir)) = open.iter().find(|(u, _)| *u == uid) {
            return Ok(dir.clone());
        }
    }
    bump_evicted_count(store, prune_at(store, &Limits::default()).await).await;
    let n = alloc_txn_num(store);
    let dir = store.join(format!("txn-{n}"));
    tokio::fs::create_dir_all(dir.join("before")).await?;
    tokio::fs::create_dir_all(dir.join("after")).await?;
    write_manifest(
        &dir,
        &Manifest {
            txn_id: n,
            timestamp: epoch_secs(),
            files: Vec::new(),
        },
    )
    .await?;
    OPEN_TXNS
        .lock()
        .expect("undo OPEN_TXNS lock poisoned")
        .push((uid, dir.clone()));
    Ok(dir)
}

/// 追加一条文件记录进 uid 的事务：先写 side 文件，再原子重写 manifest（提交点）。
async fn wal_append(
    store: &Path,
    uid: u64,
    path: &Path,
    before: Option<String>,
    created: bool,
    after: &str,
) -> std::io::Result<()> {
    let dir = wal_open(store, uid).await?;
    // 绝对路径归一（Windows 反斜杠/盘符大小写由 dunce 处理；失败用原路径）。
    let path_str = dunce::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let (before, before_file) = match &before {
        Some(c) if c.len() <= INLINE_LIMIT => (Some(c.clone()), None),
        Some(c) => write_side_file(&dir, "before", c).await?,
        None if created => (None, None),
        None => {
            return Err(std::io::Error::other(
                "txn entry: modified file without before snapshot",
            ));
        }
    };
    let (after_inline, after_file) = if after.len() <= INLINE_LIMIT {
        (Some(after.to_string()), None)
    } else {
        write_side_file(&dir, "after", after).await?
    };
    let mut manifest = read_manifest(&dir)
        .await
        .map_err(|e| std::io::Error::other(format!("wal manifest unreadable: {e}")))?;
    manifest.files.push(FileRec {
        path: path_str,
        created,
        before,
        after: after_inline,
        after_sha256: sha256_hex(after.as_bytes()),
        before_file,
        after_file,
    });
    write_manifest(&dir, &manifest).await
}

/// 写 side 快照文件，返回 (None, 相对路径) 的 FileRec 字段对。side 文件先于
/// manifest 落盘：manifest 引用的旁路文件必然已完整存在。
async fn write_side_file(
    dir: &Path,
    kind: &str,
    content: &str,
) -> std::io::Result<(Option<String>, Option<String>)> {
    // 同类序号自增（WAL 逐笔追加，内嵌快照不占位）：相对路径由 manifest 引用，
    // 命名对外不透明。
    let i = {
        let side = dir.join(kind);
        let mut n = 0u32;
        if let Ok(mut rd) = tokio::fs::read_dir(&side).await {
            while let Some(ent) = rd.next_entry().await? {
                if ent.file_name().to_string_lossy().parse::<u32>().is_ok() {
                    n += 1;
                }
            }
        }
        n
    };
    let rel = format!("{kind}/{i}");
    tokio::fs::write(dir.join(&rel), content).await?;
    Ok((None, Some(rel)))
}

/// manifest 原子写（temp+rename）：manifest 是记账提交点，撕裂写 = 假账。
async fn write_manifest(dir: &Path, manifest: &Manifest) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::other(format!("manifest serialize: {e}")))?;
    crate::atomic_write(&dir.join("manifest.json"), &body).await
}

/// 丢弃某事务（工具失败/取消路径调用）：删除已开账的 WAL 事务目录 + 清旁表。
/// 同步 fs 删除 —— 调用点在错误/Drop 路径，目录只含本事务快照，短暂阻塞可接受。
pub(crate) fn abort(uid: u64) {
    let dir = OPEN_TXNS
        .lock()
        .expect("undo OPEN_TXNS lock poisoned")
        .iter()
        .find(|(u, _)| *u == uid)
        .map(|(_, d)| d.clone());
    if let Some(dir) = dir {
        let _ = std::fs::remove_dir_all(&dir);
        OPEN_TXNS
            .lock()
            .expect("undo OPEN_TXNS lock poisoned")
            .retain(|(u, _)| *u != uid);
    }
    // audit 内存 F8：TOUCHED 同款回收——恢复登记只属 undo/redo 路径，但 abort 语义
    // 是"本事务账目全清"，两条旁表一起 retain 才对得上。
    TOUCHED
        .lock()
        .expect("undo TOUCHED lock poisoned")
        .retain(|(u, _)| *u != uid);
}

/// execute_tool 事务守卫（audit 竞锁 #10 / 内存 F8）：drop 时未 `settle()`（即
/// commit/abort 均未走到）→ 兜底 [`abort`] 清掉本 uid 的 PENDING/TOUCHED 快照。
/// 覆盖 future 取消路径——客户端断连时 hyper drop handler future，原实现
/// commit/abort 双双不执行，快照滞留至进程退出。
pub(crate) struct TxnGuard {
    uid: u64,
    settled: std::sync::atomic::AtomicBool,
}

impl TxnGuard {
    pub(crate) fn new(uid: u64) -> Self {
        Self {
            uid,
            settled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 收口：commit（或显式 abort）已完成，豁免 drop 兜底。
    pub(crate) fn settle(&self) {
        self.settled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for TxnGuard {
    fn drop(&mut self) {
        if !self.settled.load(std::sync::atomic::Ordering::Relaxed) {
            abort(self.uid);
        }
    }
}

/// 登记一次恢复写涉及的文件（undo_one/redo_one 整事务恢复成功后调用；
/// TXN_UID 作用域外 = --direct 路径，无 daemon LS 态可同步，丢弃）。
fn register_touched(files: &[FileRec]) {
    let uid = TXN_UID.try_with(|v| *v).unwrap_or(0);
    if uid == 0 {
        return;
    }
    TOUCHED
        .lock()
        .expect("undo TOUCHED lock poisoned")
        .extend(files.iter().map(|f| {
            (
                uid,
                TouchedFile {
                    path: f.path.clone(),
                    created: f.created,
                },
            )
        }));
}

/// 取走并清空当前事务（TXN_UID 作用域内）登记的涉及文件清单。
/// undo/redo 收口恒调用（含失败路径）——条目随取随清，不泄漏。
pub(crate) fn take_touched() -> Vec<TouchedFile> {
    let uid = TXN_UID.try_with(|v| *v).unwrap_or(0);
    if uid == 0 {
        return Vec::new();
    }
    let mut reg = TOUCHED.lock().expect("undo TOUCHED lock poisoned");
    let taken: Vec<TouchedFile> = reg
        .iter()
        .filter(|(u, _)| *u == uid)
        .map(|(_, f)| f.clone())
        .collect();
    reg.retain(|(u, _)| *u != uid);
    taken
}

/// 提交某事务：WAL 收口（记账已在 `recorded_write` 前置落盘）——清空 redo 链。
/// uid 未开账（无任何 recorded_write）= no-op。
///
/// 收口近似无败：记账先行的全部意义即「写成功 ⇒ 账必已落盘」，收口只剩 redo 链
/// 清理。旧版（收口时才落账）在此处 IO 失败会产生「写成功但无 undo 保险」的不
/// 可修复态，已随 WAL 化消除。
pub(crate) async fn commit(root: &Path, uid: u64) -> Result<(), ToolError> {
    let store = store_for(root)?;
    commit_at(&store, uid)
        .await
        .map_err(|e| ToolError::Core(lsp_core::error::CoreError::Io(e)))
}

/// [`commit`] 的存储路径注入版（单测用）。
pub(crate) async fn commit_at(store: &Path, uid: u64) -> std::io::Result<()> {
    let Some((_, dir)) = OPEN_TXNS
        .lock()
        .expect("undo OPEN_TXNS lock poisoned")
        .iter()
        .find(|(u, _)| *u == uid)
        .map(|(u, d)| (*u, d.clone()))
    else {
        return Ok(());
    };
    OPEN_TXNS
        .lock()
        .expect("undo OPEN_TXNS lock poisoned")
        .retain(|(u, _)| *u != uid);
    // IDE 语义：新写入清空 redo 链。空事务（0 文件，理论不可达）不动 redo 链，
    // 与旧版 entries.is_empty() 早退对齐。
    let has_files = read_manifest(&dir)
        .await
        .map(|m| !m.files.is_empty())
        .unwrap_or(false);
    if has_files {
        let store = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| store.to_path_buf());
        remove_all_undone(&store).await;
    }
    Ok(())
}

/// 读 before/after 快照内容（内嵌或旁路文件）。旁路丢失 = 存储损坏。
async fn side_content(
    dir: &Path,
    inline: &Option<String>,
    side_file: &Option<String>,
    kind: &str,
) -> Result<String, ToolError> {
    if let Some(c) = inline {
        return Ok(c.clone());
    }
    let rel = side_file
        .as_deref()
        .ok_or_else(|| ToolError::WriteConflict {
            path: dir.display().to_string(),
            reason: format!(
                "undo store corrupt: {kind} content missing (neither inline nor side file)"
            ),
        })?;
    tokio::fs::read_to_string(dir.join(rel))
        .await
        .map_err(|e| ToolError::WriteConflict {
            path: dir.join(rel).display().to_string(),
            reason: format!("undo store corrupt: cannot read {kind} side file: {e}"),
        })
}

/// 栈顶活跃事务（N 最大的 txn-*），空栈 → None。
async fn top_active(store: &Path) -> std::io::Result<Option<u64>> {
    let mut best: Option<u64> = None;
    let mut rd = tokio::fs::read_dir(store).await?;
    while let Some(ent) = rd.next_entry().await? {
        if let Some(n) = parse_dir_n(ent.file_name().to_string_lossy().as_ref(), "txn-") {
            best = Some(best.map_or(n, |b: u64| b.max(n)));
        }
    }
    Ok(best)
}

/// 解析 `prefix{N}` 目录名的 N。
fn parse_dir_n(name: &str, prefix: &str) -> Option<u64> {
    name.strip_prefix(prefix)?.parse().ok()
}

/// 分配下一个事务号：现有 txn-*/undone-*/*discarded-* 的 max N + 1（跨进程重启
/// 天然续号）。discarded 计入：否则 N 复用会让 `txn-{N}` → `discarded-{N}` 的
/// 退栈 rename 覆盖同号旧留档（Windows rename 语义 = 静默覆盖）。
fn alloc_txn_num(store: &Path) -> u64 {
    let mut max = 0u64;
    if let Ok(rd) = std::fs::read_dir(store) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let n = parse_dir_n(&name, "txn-")
                .or_else(|| parse_dir_n(&name, "undone-"))
                .or_else(|| parse_dir_n(&name, "discarded-"));
            if let Some(n) = n {
                max = max.max(n);
            }
        }
    }
    max + 1
}

async fn remove_all_undone(store: &Path) {
    let Ok(mut rd) = tokio::fs::read_dir(store).await else {
        return;
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with("undone-") {
            let _ = tokio::fs::remove_dir_all(ent.path()).await;
        }
    }
}

/// undo 单步结果。
enum UndoOne {
    /// 正常回滚（或幂等补完此前半途的恢复），值为涉及文件数。
    Reverted(usize),
    /// 盘面已处于事务前状态（崩溃窗「有记录无改动」/外部已回退）——无恢复写，
    /// 仅收口记账。
    NothingToRevert,
}

/// undo/redo 单步失败：外部编辑冲突（可安全丢弃整事务）vs 瞬态错误（原样上抛）。
enum UndoFail {
    External { path: String, reason: String },
    /// WAL 开账撕裂（bd serena-rust-15jb）：manifest.json 缺失 = 崩溃打中
    /// mkdir 与首次 manifest 原子写之间的窗口 —— 零记录零影响，安全丢弃。
    Torn,
    Transient(ToolError),
}

/// 盘面分类：事务后状态（待恢复）/ 事务前状态（已恢复或未落盘）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileState {
    AfterState,
    BeforeState,
}

/// undo 单事务：冲突门（盘 sha == after_sha256，整事务拒绝）→ 恢复 before /
/// 删除 created 文件 → rename 为 undone-{N}。
///
/// 兼容包装（recipe 回滚路径）：外部编辑冲突仍按 `WRITE_CONFLICT` 报错，由调用
/// 方逐事务处置；undo 工具路径走 [`undo_one_classified`] 的自动跳过语义。
pub(crate) async fn undo_one(store: &Path, n: u64) -> Result<usize, ToolError> {
    match undo_one_classified(store, n).await {
        Ok(UndoOne::Reverted(files)) => Ok(files),
        Ok(UndoOne::NothingToRevert) => Ok(0),
        // 撕裂账（无 manifest）零记录零影响，回滚计 0 文件。
        Err(UndoFail::Torn) => Ok(0),
        Err(UndoFail::External { path, reason }) => Err(conflict(&path, &reason)),
        Err(UndoFail::Transient(e)) => Err(e),
    }
}

/// undo 单事务（盘面分类版）：全事务后态 → 恢复；全事务前态 → no-op 收口（崩
/// 溃窗「有记录无改动」/恢复半途崩溃的幂等补完）；混态无外部冲突 → 幂等补完；
/// 任一外部冲突 → 整事务拒绝（不产生部分恢复写）。
async fn undo_one_classified(store: &Path, n: u64) -> Result<UndoOne, UndoFail> {
    let dir = store.join(format!("txn-{n}"));
    let manifest = match read_manifest(&dir).await {
        Ok(m) => m,
        Err(e) => {
            // WAL 开账撕裂窗口（见 UndoFail::Torn）：manifest 不在盘 = 安全丢弃；
            // 在盘但损坏 = 真存储损坏，保持报错（取证优先，不静默吞）。
            if tokio::fs::metadata(dir.join("manifest.json"))
                .await
                .is_err()
            {
                return Err(UndoFail::Torn);
            }
            return Err(UndoFail::Transient(e));
        }
    };
    // 先全量分类，后写盘：门不过绝不产生部分恢复（契约设计第 4 条）。
    let mut states = Vec::with_capacity(manifest.files.len());
    for f in &manifest.files {
        states.push(classify_for_undo(&dir, f).await?);
    }
    if states.iter().all(|s| *s == FileState::BeforeState) {
        mark_undone(store, n, &dir)
            .await
            .map_err(|e| UndoFail::Transient(io_conflict(&dir, "undo: mark txn undone failed", e)))?;
        return Ok(UndoOne::NothingToRevert);
    }
    // 恢复写（幂等：BeforeState 文件重写同内容；AfterState 正常恢复）。
    for f in &manifest.files {
        restore_before(&dir, f).await.map_err(UndoFail::Transient)?;
    }
    mark_undone(store, n, &dir)
        .await
        .map_err(|e| UndoFail::Transient(io_conflict(&dir, "undo: mark txn undone failed", e)))?;
    // 整事务恢复成功后才登记（P2-b）：LS 同步只对真正落盘恢复的文件。
    register_touched(&manifest.files);
    Ok(UndoOne::Reverted(manifest.files.len()))
}

/// undo 单文件盘面分类；外部编辑（既非事务前也非事务后状态）→ External。
async fn classify_for_undo(dir: &Path, f: &FileRec) -> Result<FileState, UndoFail> {
    let current = tokio::fs::read_to_string(&f.path).await;
    match (f.created, current) {
        // created 文件已被外部删除 = undo 目标状态一致，放行（幂等跳过删除）。
        (true, Err(_)) => Ok(FileState::BeforeState),
        (true, Ok(c)) if sha256_hex(c.as_bytes()) == f.after_sha256 => Ok(FileState::AfterState),
        (true, Ok(_)) => Err(UndoFail::External {
            path: f.path.clone(),
            reason: "undo conflict: created file was modified after the transaction".into(),
        }),
        (false, Ok(c)) if sha256_hex(c.as_bytes()) == f.after_sha256 => Ok(FileState::AfterState),
        (false, Ok(c)) => {
            let before = side_content(dir, &f.before, &f.before_file, "before")
                .await
                .map_err(UndoFail::Transient)?;
            if sha256_hex(before.as_bytes()) == sha256_hex(c.as_bytes()) {
                Ok(FileState::BeforeState)
            } else {
                Err(UndoFail::External {
                    path: f.path.clone(),
                    reason: "undo conflict: file changed after the transaction (sha mismatch)"
                        .into(),
                })
            }
        }
        (false, Err(_)) => Err(UndoFail::External {
            path: f.path.clone(),
            reason:
                "undo conflict: expected the file to exist (post-transaction state), but it is missing"
                    .into(),
        }),
    }
}

/// 恢复单文件到事务前状态（created → 删除；modified → before 内容原子写回）。
async fn restore_before(dir: &Path, f: &FileRec) -> Result<(), ToolError> {
    let p = Path::new(&f.path);
    if f.created {
        // created 文件已被外部删除 = 结果一致，跳过（幂等）。
        if p.exists() {
            tokio::fs::remove_file(p)
                .await
                .map_err(|e| conflict(&f.path, &format!("undo remove created file failed: {e}")))?;
        }
        return Ok(());
    }
    let before = side_content(dir, &f.before, &f.before_file, "before").await?;
    // 原子写（temp+rename）：undo 是数据恢复路径，截断写半途崩溃 = 文件损坏。
    // Windows 上目标被编辑器占用时 rename 可能失败——返回冲突门错误，用户关掉
    // 占用后重试 undo（幂等补完）即可。
    crate::atomic_write(p, &before)
        .await
        .map_err(|e| conflict(&f.path, &format!("undo restore failed: {e}")))
}

/// undo 收口：事务目录翻转 `txn-{N}` → `undone-{N}`。
async fn mark_undone(store: &Path, n: u64, dir: &Path) -> std::io::Result<()> {
    tokio::fs::rename(dir, store.join(format!("undone-{n}"))).await
}

/// redo 单步结果。
enum RedoOne {
    Replayed(usize),
    /// 盘面已处于事务后状态（redo 写后、收口前崩溃；或外部已应用同内容）。
    NothingToReplay,
}

/// redo 单事务（盘面分类版）：全事务前态 → 正放 after；混态无外部冲突 → 幂等
/// 补完（redo 半途崩溃）；全事务后态 → no-op 收口；任一外部冲突 → 整事务拒绝。
async fn redo_one_classified(store: &Path, n: u64) -> Result<RedoOne, UndoFail> {
    let dir = store.join(format!("undone-{n}"));
    let manifest = match read_manifest(&dir).await {
        Ok(m) => m,
        // 同 undo_one_classified：无 manifest = 撕裂账安全丢弃（防御对称）。
        Err(_) if tokio::fs::metadata(dir.join("manifest.json")).await.is_err() => {
            return Err(UndoFail::Torn);
        }
        Err(e) => return Err(UndoFail::Transient(e)),
    };
    let mut states = Vec::with_capacity(manifest.files.len());
    for f in &manifest.files {
        states.push(classify_for_redo(&dir, f).await?);
    }
    if states.iter().all(|s| *s == FileState::AfterState) {
        mark_active(store, n, &dir)
            .await
            .map_err(|e| UndoFail::Transient(io_conflict(&dir, "redo: mark txn active failed", e)))?;
        return Ok(RedoOne::NothingToReplay);
    }
    // 正放/补完：逐文件写回 after（幂等，AfterState 文件重写同内容）。
    for f in &manifest.files {
        let after = side_content(&dir, &f.after, &f.after_file, "after")
            .await
            .map_err(UndoFail::Transient)?;
        let p = Path::new(&f.path);
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // 原子写，同 undo_one：恢复路径禁截断写（防半途损坏）。
        crate::atomic_write(p, &after).await.map_err(|e| {
            UndoFail::Transient(conflict(&f.path, &format!("redo reapply failed: {e}")))
        })?;
    }
    mark_active(store, n, &dir)
        .await
        .map_err(|e| UndoFail::Transient(io_conflict(&dir, "redo: mark txn active failed", e)))?;
    // 整事务重放成功后才登记（P2-b），同 undo_one。
    register_touched(&manifest.files);
    Ok(RedoOne::Replayed(manifest.files.len()))
}

/// redo 单文件盘面分类；外部编辑 → External。
async fn classify_for_redo(dir: &Path, f: &FileRec) -> Result<FileState, UndoFail> {
    let current = tokio::fs::read_to_string(&f.path).await;
    match (f.created, current) {
        // created 文件 undo 后应不存在；缺失 = 可重建。
        (true, Err(_)) => Ok(FileState::BeforeState),
        (true, Ok(c)) if sha256_hex(c.as_bytes()) == f.after_sha256 => Ok(FileState::AfterState),
        (true, Ok(_)) => Err(UndoFail::External {
            path: f.path.clone(),
            reason: "redo: created file exists with foreign content".into(),
        }),
        (false, Ok(c)) => {
            let disk = sha256_hex(c.as_bytes());
            if disk == f.after_sha256 {
                return Ok(FileState::AfterState);
            }
            let before = side_content(dir, &f.before, &f.before_file, "before")
                .await
                .map_err(UndoFail::Transient)?;
            if disk == sha256_hex(before.as_bytes()) {
                Ok(FileState::BeforeState)
            } else {
                Err(UndoFail::External {
                    path: f.path.clone(),
                    reason: "redo: file on disk is not in the expected pre-redo (post-undo) state"
                        .into(),
                })
            }
        }
        (false, Err(_)) => Err(UndoFail::External {
            path: f.path.clone(),
            reason: "redo: expected the file to exist (post-undo state), but it is missing".into(),
        }),
    }
}

/// redo 收口：事务目录翻转 `undone-{N}` → `txn-{N}`。
async fn mark_active(store: &Path, n: u64, dir: &Path) -> std::io::Result<()> {
    tokio::fs::rename(dir, store.join(format!("txn-{n}"))).await
}

/// 冲突事务退栈留档：`{prefix}-{N}` → `discarded-{N}`（bd serena-rust-3ux6）。
/// 留档不参与栈（top_active/bottom_undone 不扫），prune 照常按上限回收。
async fn discard_dir(store: &Path, prefix: &str, n: u64) -> std::io::Result<()> {
    tokio::fs::rename(
        store.join(format!("{prefix}-{n}")),
        store.join(format!("discarded-{n}")),
    )
    .await
}

fn conflict(path: &str, reason: &str) -> ToolError {
    ToolError::WriteConflict {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

/// io::Error → 旧版同形 `WriteConflict`（收口 rename 失败等，错误面保持不变）。
fn io_conflict(dir: &Path, what: &str, e: std::io::Error) -> ToolError {
    conflict(&dir.display().to_string(), &format!("{what}: {e}"))
}

/// 读 `manifest.json`；不可读/损坏 = 存储损坏级 `WRITE_CONFLICT`（留档可查）。
async fn read_manifest(dir: &Path) -> Result<Manifest, ToolError> {
    let body = tokio::fs::read_to_string(dir.join("manifest.json"))
        .await
        .map_err(|e| {
            conflict(
                &dir.display().to_string(),
                &format!("manifest unreadable: {e}"),
            )
        })?;
    serde_json::from_str(&body).map_err(|e| {
        conflict(
            &dir.display().to_string(),
            &format!("manifest corrupt: {e}"),
        )
    })
}

// ==== recipe `diff` 数据源（只读：不进写门、不产生事务） ====

/// 单事务快照（[`read_txn`] 返回形态；字段对 recipe 层公开）。
#[derive(Debug)]
pub(crate) struct TxnSnapshot {
    pub txn_id: u64,
    pub timestamp: u64,
    pub files: Vec<TxnFileSnapshot>,
}

/// 单文件写前写后内容（内嵌直读，旁路文件读回；created 的 before=None）。
#[derive(Debug)]
pub(crate) struct TxnFileSnapshot {
    pub path: String,
    pub created: bool,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// 读事务快照：`None` = 最近活跃事务；显式 id 双形态（txn-N / undone-N）都可读。
/// 活跃栈空 / 指定 id 缺失 → BAD_ARGS（确定性用法错，非存储损坏）。
pub(crate) async fn read_txn(root: &Path, txn_id: Option<u64>) -> Result<TxnSnapshot, ToolError> {
    read_txn_at(&store_for(root)?, txn_id).await
}

/// [`read_txn`] 的存储路径注入版（单测用）。
pub(crate) async fn read_txn_at(
    store: &Path,
    txn_id: Option<u64>,
) -> Result<TxnSnapshot, ToolError> {
    let (dir, resolved_n) = match txn_id {
        Some(n) => {
            let active = store.join(format!("txn-{n}"));
            if active.is_dir() {
                (active, n)
            } else {
                let undone = store.join(format!("undone-{n}"));
                if undone.is_dir() {
                    (undone, n)
                } else {
                    return Err(ToolError::BadArgs {
                        detail: format!("txn-{n} not found in undo store {}", store.display()),
                    });
                }
            }
        }
        None => {
            let n = top_active(store)
                .await
                .map_err(|e| ToolError::Core(lsp_core::error::CoreError::Io(e)))?
                .ok_or_else(|| ToolError::BadArgs {
                    detail: "no undo transactions: run a write tool first".into(),
                })?;
            (store.join(format!("txn-{n}")), n)
        }
    };
    let m = read_manifest(&dir).await?;
    let mut files = Vec::with_capacity(m.files.len());
    for f in m.files {
        // created 文件 before/before_file 双 None → before=None；旁路文件走
        // side_content 读回（丢失 = 存储损坏，沿用 WriteConflict 语义）。
        let before = if f.before.is_none() && f.before_file.is_none() {
            None
        } else {
            Some(side_content(&dir, &f.before, &f.before_file, "before").await?)
        };
        let after = if f.after.is_none() && f.after_file.is_none() {
            None
        } else {
            Some(side_content(&dir, &f.after, &f.after_file, "after").await?)
        };
        files.push(TxnFileSnapshot {
            path: f.path,
            created: f.created,
            before,
            after,
        });
    }
    Ok(TxnSnapshot {
        txn_id: resolved_n,
        timestamp: m.timestamp,
        files,
    })
}

/// `undo`：回滚最近 `steps` 个事务；外部编辑冲突的事务自动 discarded 跳过并附
/// warning（bd serena-rust-3ux6），no-op 崩溃窗事务幂等收口（bd serena-rust-15jb）。
pub(crate) async fn undo(root: &Path, steps: usize) -> Result<serde_json::Value, ToolError> {
    undo_at(&store_for(root)?, steps).await
}

/// [`undo`] 的存储路径注入版（单测用）。
pub(crate) async fn undo_at(store: &Path, steps: usize) -> Result<serde_json::Value, ToolError> {
    // 与写工具串行化：恢复写期间不得有并发写改盘。
    let _gate = crate::write_gate::acquire("undo").await?;
    let mut undone = Vec::new();
    let mut skipped = Vec::new();
    let mut done = 0usize;
    // bd e1f4：sha 冲突 discard 后必须**立即停止**——冲突证明时间线已被外部编辑
    // 打乱，更老事务的 pre-image 不再可信（fall-through 会静默回滚无关改动，
    // 盲测 v4 实锤：txn3 冲突却撤掉了 txn2）。torn（无 manifest 的崩溃窗残骸）
    // 仍按 3ux6 语义跳过继续——那不是外部编辑，时间线未乱。
    let mut stopped_early: Option<serde_json::Value> = None;
    // steps 只数真实回滚；no-op 收口与外部冲突 discarded 跳过不占名额（栈单调
    // 收缩保证终止）。空栈 = no-op（IDE undo 语义）。
    while done < steps && stopped_early.is_none() {
        let n = match top_active(store).await {
            Ok(Some(n)) => n,
            _ => break,
        };
        match undo_one_classified(store, n).await {
            Ok(UndoOne::Reverted(files)) => {
                undone.push(serde_json::json!({"txn_id": n, "files": files}));
                done += 1;
            }
            Ok(UndoOne::NothingToRevert) => {
                undone.push(serde_json::json!({
                    "txn_id": n,
                    "files": 0,
                    "note": "no-op: change not present on disk (crash window or already reverted)"
                }));
            }
            Err(UndoFail::External { path, reason }) => {
                discard_dir(store, "txn", n).await.map_err(|e| {
                    conflict(
                        &format!("txn-{n}"),
                        &format!("undo: discard conflicted txn failed: {e}"),
                    )
                })?;
                let next = top_active(store).await.ok().flatten();
                stopped_early = Some(serde_json::json!({
                    "txn_id": n,
                    "reason": reason,
                    "file": path,
                    "note": "older transactions not rolled back: timeline broken by external edit",
                }));
                skipped.push(serde_json::json!({
                    "txn_id": n,
                    "state": "discarded",
                    "reason": reason,
                    "file": path,
                    "next_active_txn": next,
                }));
            }
            Err(UndoFail::Torn) => {
                discard_dir(store, "txn", n).await.map_err(|e| {
                    conflict(
                        &format!("txn-{n}"),
                        &format!("undo: discard torn txn failed: {e}"),
                    )
                })?;
                let next = top_active(store).await.ok().flatten();
                skipped.push(serde_json::json!({
                    "txn_id": n,
                    "state": "discarded",
                    "reason": "torn transaction (no manifest) — discarded",
                    "next_active_txn": next,
                }));
            }
            Err(UndoFail::Transient(e)) => return Err(e),
        }
    }
    let mut resp = serde_json::json!({"undone": undone, "skipped": skipped});
    if let Some(stop) = stopped_early {
        // CLI 据此置 rc=2：部分完成非完全成功（载荷已打印，agent 可解析）。
        resp["stopped_early"] = stop;
    }
    Ok(resp)
}

/// `redo`：重放最近被 undo 的事务。
pub(crate) async fn redo(root: &Path) -> Result<serde_json::Value, ToolError> {
    redo_at(&store_for(root)?).await
}

/// [`redo`] 的存储路径注入版（单测用）。
pub(crate) async fn redo_at(store: &Path) -> Result<serde_json::Value, ToolError> {
    let _gate = crate::write_gate::acquire("redo").await?;
    let mut redone = Vec::new();
    let mut skipped = Vec::new();
    // 每次调用重放一个**有效**事务（IDE 单步语义；--steps 未列入契约）。重放序 =
    // 事务时间序（N 升序：最后 undo 的先 redo；bd serena-rust-b5od）。外部冲突
    // 链式 discarded 清栈（因果链已被外部编辑打破，后续多半同弃，但栈收缩保证
    // 终止，报文一次交代完整）；no-op 收口占当次名额。
    loop {
        let Some(n) = bottom_undone(store).await else {
            break;
        };
        match redo_one_classified(store, n).await {
            Ok(RedoOne::Replayed(files)) => {
                redone.push(serde_json::json!({"txn_id": n, "files": files}));
                break;
            }
            Ok(RedoOne::NothingToReplay) => {
                redone.push(serde_json::json!({
                    "txn_id": n,
                    "files": 0,
                    "note": "no-op: change already on disk (crash window)"
                }));
                break;
            }
            Err(UndoFail::External { path, reason }) => {
                discard_dir(store, "undone", n).await.map_err(|e| {
                    conflict(
                        &format!("undone-{n}"),
                        &format!("redo: discard conflicted txn failed: {e}"),
                    )
                })?;
                let next = bottom_undone(store).await;
                skipped.push(serde_json::json!({
                    "txn_id": n,
                    "state": "discarded",
                    "reason": reason,
                    "file": path,
                    "next_undone_txn": next,
                }));
            }
            Err(UndoFail::Torn) => {
                discard_dir(store, "undone", n).await.map_err(|e| {
                    conflict(
                        &format!("undone-{n}"),
                        &format!("redo: discard torn txn failed: {e}"),
                    )
                })?;
                let next = bottom_undone(store).await;
                skipped.push(serde_json::json!({
                    "txn_id": n,
                    "state": "discarded",
                    "reason": "torn transaction (no manifest) — discarded",
                    "next_undone_txn": next,
                }));
            }
            Err(UndoFail::Transient(e)) => return Err(e),
        }
    }
    Ok(serde_json::json!({"redone": redone, "skipped": skipped}))
}

/// 重放队首：N **最小**的 undone 事务（时间序正放，bd serena-rust-b5od）。
/// 旧版取 N 最大（LIFO）——深度 ≥2 时 pre-image 对账必然失配且失败不弹栈 =
/// 永久 WRITE_CONFLICT 楔死，即 F1 根因。
async fn bottom_undone(store: &Path) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut rd = tokio::fs::read_dir(store).await.ok()?;
    while let Ok(Some(ent)) = rd.next_entry().await {
        if let Some(n) = parse_dir_n(ent.file_name().to_string_lossy().as_ref(), "undone-") {
            best = Some(best.map_or(n, |b: u64| b.min(n)));
        }
    }
    best
}

/// `undo --list`：栈概览（txn id/时间/文件数/摘要），顺带 prune（补充契约 7）。
pub(crate) async fn list(root: &Path) -> Result<serde_json::Value, ToolError> {
    list_at(&store_for(root)?).await
}

/// bd serena-rust-mfht F8：list 摘要升级——文件名 + 操作类型（created/modify）+
/// 行数 +/-；多文件聚合 first 文件 + 余文件 +/- 总量。仅内嵌快照算行数（不读
/// 旁路文件——列表高频访问，IO 风暴风险）；旁路走纯字节 +/- 形态。`text.lines()`
/// 不识别裸 `\r`（fs_tools `split_lines_mixed` 形态），但 undo 写入源统一 `\n`
/// 近似够用。
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let nl = s.bytes().filter(|b| *b == b'\n').count();
    if s.as_bytes().last() == Some(&b'\n') {
        nl
    } else {
        nl + 1
    }
}

/// 单文件 diff 形态（行数）。created → 只算 after；before 为 None；纯文本替换
/// after == before 时退化为 "no change"。
fn file_diff_text(f: &FileRec) -> String {
    if f.created {
        let after_lines = f.after.as_deref().map(count_lines).unwrap_or(0);
        return format!("created (+{after_lines} lines)");
    }
    let before = f.before.as_deref();
    let after = f.after.as_deref();
    if before == after {
        return "no change".to_string();
    }
    let before_lines = before.map(count_lines).unwrap_or(0);
    let after_lines = after.map(count_lines).unwrap_or(0);
    let added = after_lines.saturating_sub(before_lines);
    let removed = before_lines.saturating_sub(after_lines);
    format!("+{added}/-{removed} lines")
}

/// [`list`] 的存储路径注入版（单测用）。
pub(crate) async fn list_at(store: &Path) -> Result<serde_json::Value, ToolError> {
    let _gate = crate::write_gate::acquire("undo-list").await?;
    let limits = Limits::default();
    bump_evicted_count(store, prune_at(store, &limits).await).await;
    let mut txns = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&store).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            let (prefix, n) = parse_dir_n(&name, "txn-")
                .map(|n| ("active", n))
                .or_else(|| parse_dir_n(&name, "undone-").map(|n| ("undone", n)))
                .or_else(|| parse_dir_n(&name, "discarded-").map(|n| ("discarded", n)))
                .unwrap_or(("", 0));
            if n == 0 {
                continue;
            }
            let dir = ent.path();
            let (files, summary, ts) = match read_manifest(&dir).await {
                Ok(m) => {
                    let first_name = m.files.first().map(|f| {
                        Path::new(&f.path)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| f.path.clone())
                    });
                    let summary = match (&first_name, m.files.first(), m.files.len()) {
                        (Some(name), Some(first), 1) => {
                            format!("{name} {}", file_diff_text(first))
                        }
                        (Some(name), Some(first), n) => {
                            let mut added = 0usize;
                            let mut removed = 0usize;
                            for f in &m.files {
                                if f.created {
                                    added = added.saturating_add(
                                        f.after.as_deref().map(count_lines).unwrap_or(0),
                                    );
                                } else {
                                    let bl = f.before.as_deref().map(count_lines).unwrap_or(0);
                                    let al = f.after.as_deref().map(count_lines).unwrap_or(0);
                                    added = added.saturating_add(al.saturating_sub(bl));
                                    removed = removed.saturating_add(bl.saturating_sub(al));
                                }
                            }
                            format!(
                                "{name} {} (+{} files, +{added}/-{removed} total)",
                                file_diff_text(first),
                                n - 1
                            )
                        }
                        _ => String::new(),
                    };
                    (m.files.len(), summary, m.timestamp)
                }
                // 损坏事务也要可见（用户可手工清理），不静默吞。
                Err(e) => (0, format!("<unreadable: {e}>"), 0),
            };
            txns.push(serde_json::json!({
                "txn_id": n,
                "state": prefix,
                "timestamp": ts,
                "files": files,
                "summary": summary,
            }));
        }
    }
    txns.sort_by_key(|t| Reverse(t["txn_id"].as_i64().unwrap_or(0)));
    // 杠精 F13：栈上限显式化——淘汰（条数/字节/时长）是静默发生的，用户看栈
    // "少了"要能就地找到原因；累计逐出数持久化在 store，跨 daemon 重启可查。
    let mut out = serde_json::json!({
        "project_hash": store.file_name().map(|s| s.to_string_lossy().to_string()),
        "txns": txns,
        "stack_limits": {
            "max_entries": limits.max_txns,
            "max_total_bytes": limits.max_total_bytes,
            "max_age_secs": limits.max_age_secs,
        },
    });
    let evicted = evicted_count(store).await;
    if evicted > 0 {
        out["evicted_count"] = serde_json::json!(evicted);
    }
    Ok(out)
}

/// 保留策略 prune：超龄 → 超数 → 超量，均从最旧（N 最小）整事务淘汰。
/// 返回本轮逐出的事务数（供累计计数持久化，`undo --list` 的 evicted_count 用）。
async fn prune_at(store: &Path, limits: &Limits) -> usize {
    let now = epoch_secs();
    // 收集 (n, state, timestamp, size)。
    let mut items: Vec<(u64, bool, u64, u64)> = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(store).await else {
        return 0;
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        let (is_active, n) = parse_dir_n(&name, "txn-")
            .map(|n| (true, n))
            .or_else(|| parse_dir_n(&name, "undone-").map(|n| (false, n)))
            .or_else(|| parse_dir_n(&name, "discarded-").map(|n| (false, n)))
            .unwrap_or((false, 0));
        if n == 0 {
            continue;
        }
        let dir = ent.path();
        let ts = read_manifest(&dir)
            .await
            .map(|m| m.timestamp)
            .unwrap_or(now);
        let size = dir_size(&dir);
        items.push((n, is_active, ts, size));
    }
    // 淘汰集：超龄无条件；数量/大小超限从 N 最小开始。
    // 注意元组序 (n, is_active, ts, size)：超龄过滤必须显式取第 3 位。
    let mut expired: Vec<u64> = items
        .iter()
        .filter(|(_, _, ts, _)| now.saturating_sub(*ts) > limits.max_age_secs)
        .map(|(n, ..)| *n)
        .collect();
    let mut alive: Vec<(u64, bool, u64, u64)> = items
        .into_iter()
        .filter(|(n, ..)| !expired.contains(n))
        .collect();
    alive.sort_by_key(|(n, ..)| *n);
    while alive.len() > limits.max_txns {
        let (n, ..) = alive.remove(0);
        expired.push(n);
    }
    let mut total: u64 = alive.iter().map(|(.., size)| *size).sum();
    while total > limits.max_total_bytes && !alive.is_empty() {
        let (n, _, _, size) = alive.remove(0);
        total = total.saturating_sub(size);
        expired.push(n);
    }
    let evicted = expired.len();
    for n in expired {
        // active/undone/discarded 同号不并存（状态机互斥），按三种名式尝试删除。
        let _ = tokio::fs::remove_dir_all(store.join(format!("txn-{n}"))).await;
        let _ = tokio::fs::remove_dir_all(store.join(format!("undone-{n}"))).await;
        let _ = tokio::fs::remove_dir_all(store.join(format!("discarded-{n}"))).await;
    }
    evicted
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(m) = ent.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// 累计逐出计数持久化（杠精 F13：静默淘汰必须留账，否则 `undo --list` 看栈"少了"
/// 不知原因）。文件名不落 txn-/undone-/discarded- 前缀，列表/编号/扫描按前缀解析
/// 天然跳过。写点仅 prune 路径（wal_open 持 WAL_INIT 锁、list 持写门），读改写
/// 竞窗最坏少计一次——提示字段，不参与决策，可接受。
async fn bump_evicted_count(store: &Path, evicted: usize) {
    if evicted == 0 {
        return;
    }
    let prev = evicted_count(store).await;
    let _ = tokio::fs::write(store.join("evicted-count"), (prev + evicted as u64).to_string())
        .await;
}

async fn evicted_count(store: &Path) -> u64 {
    tokio::fs::read_to_string(store.join("evicted-count"))
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
