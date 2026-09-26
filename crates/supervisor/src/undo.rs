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
//!   `before/{i}`、`after/{i}`（i = files 数组下标）防 manifest 爆炸。
//! - 事务边界 = execute_tool 的一次调用（rename-symbol 多文件改动在同一调用内
//!   逐个 `recorded_write`，天然聚合成一个事务）。写点统一收口 `recorded_write`。
//! - undo 冲突门：恢复前逐文件校验盘上 sha256 == after_sha256；任一不匹配整事务
//!   拒绝。错误映射复用 `ToolError::WriteConflict` → wire `WRITE_CONFLICT`（语义
//!   同族：盘上内容与预期状态不符，C3 防线；exit 1、不可重试），零新错误码。
//! - created=true 的文件 undo = 删除文件（用户拍板）；redo = 按 after 内容重建。
//! - LS 态同步（P2-b）：整事务恢复成功后把涉及文件登记进 `TOUCHED`（uid 键侧信道），
//!   undo/redo 收口取走并逐文件 didChange / didClose（lib.rs `sync_ls_after_undo`）。
//! - 栈序：N 大 = 新。undo 取 N 最大的 `txn-{N}`；undo 后 rename 为
//!   `undone-{N}`；redo 取 N 最大的 `undone-{N}` 重放后 rename 回 `txn-{N}`；
//!   新事务落盘后删除全部 `undone-*`（IDE 语义：新写入清空 redo 链）。
//! - 保留策略（新事务追加前 + `undo --list` 时 prune，无后台定时器）：
//!   ① 事务时间戳 > 30 天 ② 总事务数 > 20 ③ 总大小 > 200 MB —— 从最旧
//!   （N 最小）开始整事务淘汰。

use std::cmp::Reverse;
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

/// 内存态快照条目（commit 时转 FileRec 落盘）。
struct Entry {
    path: PathBuf,
    before: Option<String>,
    created: bool,
    after: String,
    after_sha256: String,
}

/// 待落盘快照。键 = 事务 uid（execute_tool 每次调用分配，进程内唯一）。
static PENDING: StdMutex<Vec<(u64, Entry)>> = StdMutex::new(Vec::new());
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
/// canonicalize 失败（项目目录被删）→ BAD_ARGS，不落盘（补充契约 9）。
pub(crate) fn store_for(root: &Path) -> Result<PathBuf, ToolError> {
    let canon = dunce::canonicalize(root).map_err(|e| ToolError::BadArgs {
        detail: format!("project root not found ({}): {e}", root.display()),
    })?;
    let hash = sha256_hex(canon.to_string_lossy().as_bytes());
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

/// 写点统一收口：快照旧内容 → 原子写 → 成功后入待落盘栈。
///
/// 与 [`crate::atomic_write`] 同签名同错误面（io::Error），调用点仅换函数名。
/// 无事务上下文（uid=0，--direct 路径）退化为裸 atomic_write。
pub(crate) async fn recorded_write(path: &Path, new_content: &str) -> std::io::Result<()> {
    let uid = TXN_UID.try_with(|v| *v).unwrap_or(0);
    if uid == 0 {
        return crate::atomic_write(path, new_content).await;
    }
    // 快照必须在写盘前取（契约设计第 2 条：成功写盘前把旧状态快照入栈）。
    let (before, created) = match tokio::fs::read_to_string(path).await {
        Ok(c) => (Some(c), false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, true),
        Err(e) => return Err(e),
    };
    crate::atomic_write(path, new_content).await?;
    let entry = Entry {
        path: path.to_path_buf(),
        before,
        created,
        after: new_content.to_string(),
        after_sha256: sha256_hex(new_content.as_bytes()),
    };
    PENDING
        .lock()
        .expect("undo PENDING lock poisoned")
        .push((uid, entry));
    Ok(())
}

/// 丢弃某事务的待落盘快照（工具失败时调用）。
pub(crate) fn abort(uid: u64) {
    PENDING
        .lock()
        .expect("undo PENDING lock poisoned")
        .retain(|(u, _)| *u != uid);
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

/// 提交某事务：无快照 = no-op；否则 prune → 落盘 `txn-{N}` → 清空 redo 链。
///
/// 落盘 IO 失败映射 `CoreError::Io` → wire INTERNAL：写本身已成功但 undo 记账
/// 失败，绝不能静默吞（用户会误以为仍有 undo 保险）。
pub(crate) async fn commit(root: &Path, uid: u64) -> Result<(), ToolError> {
    let store = store_for(root)?;
    commit_at(&store, uid, &Limits::default())
        .await
        .map_err(|e| ToolError::Core(lsp_core::error::CoreError::Io(e)))
}

/// [`commit`] 的存储路径注入版（单测用）。
pub(crate) async fn commit_at(store: &Path, uid: u64, limits: &Limits) -> std::io::Result<()> {
    let entries: Vec<Entry> = {
        let mut pending = PENDING.lock().expect("undo PENDING lock poisoned");
        let taken: Vec<Entry> = pending
            .iter()
            .filter(|(u, _)| *u == uid)
            .map(|(_, e)| Entry {
                path: e.path.clone(),
                before: e.before.clone(),
                created: e.created,
                after: e.after.clone(),
                after_sha256: e.after_sha256.clone(),
            })
            .collect();
        pending.retain(|(u, _)| *u != uid);
        taken
    };
    if entries.is_empty() {
        return Ok(());
    }
    prune_at(store, limits).await;
    let n = alloc_txn_num(store);
    let dir = store.join(format!("txn-{n}"));
    tokio::fs::create_dir_all(dir.join("before")).await?;
    tokio::fs::create_dir_all(dir.join("after")).await?;

    let mut files = Vec::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        // 绝对路径归一（Windows 反斜杠/盘符大小写由 dunce 处理；失败用原路径）。
        let path_str = dunce::canonicalize(&e.path)
            .unwrap_or_else(|_| e.path.clone())
            .to_string_lossy()
            .to_string();
        let (before, before_file) = match &e.before {
            Some(c) if c.len() <= INLINE_LIMIT => (Some(c.clone()), None),
            Some(c) => {
                let rel = format!("before/{i}");
                tokio::fs::write(dir.join(&rel), c).await?;
                (None, Some(rel))
            }
            None if e.created => (None, None),
            None => {
                return Err(std::io::Error::other(format!(
                    "txn entry {i}: modified file without before snapshot"
                )));
            }
        };
        let (after, after_file) = if e.after.len() <= INLINE_LIMIT {
            (Some(e.after.clone()), None)
        } else {
            let rel = format!("after/{i}");
            tokio::fs::write(dir.join(&rel), &e.after).await?;
            (None, Some(rel))
        };
        files.push(FileRec {
            path: path_str,
            created: e.created,
            before,
            after,
            after_sha256: e.after_sha256.clone(),
            before_file,
            after_file,
        });
    }
    let manifest = Manifest {
        txn_id: n,
        timestamp: epoch_secs(),
        files,
    };
    let body = serde_json::to_string_pretty(&manifest)
        .map_err(|e| std::io::Error::other(format!("manifest serialize: {e}")))?;
    tokio::fs::write(dir.join("manifest.json"), body).await?;

    // IDE 语义：新写入清空 redo 链。
    remove_all_undone(store).await;
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

/// 分配下一个事务号：现有 txn-*/undone-* 的 max N + 1（跨进程重启天然续号）。
fn alloc_txn_num(store: &Path) -> u64 {
    let mut max = 0u64;
    if let Ok(rd) = std::fs::read_dir(store) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let n = parse_dir_n(&name, "txn-").or_else(|| parse_dir_n(&name, "undone-"));
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

/// undo 单事务：冲突门（盘 sha == after_sha256，整事务拒绝）→ 恢复 before /
/// 删除 created 文件 → rename 为 undone-{N}。
async fn undo_one(store: &Path, n: u64) -> Result<usize, ToolError> {
    let dir = store.join(format!("txn-{n}"));
    let manifest = read_manifest(&dir).await?;
    // 冲突门：先全量校验，任一文件不匹配则整事务拒绝（契约设计第 4 条）。
    // 注意：校验全过后的恢复写入阶段若 IO 失败（磁盘满/权限/占用），仍可能留下
    // 半恢复状态——此时事务保持 txn-{n}，重试 undo 即幂等补完（created 已删跳过、
    // before 内容确定性写回）。
    for f in &manifest.files {
        check_conflict_undo(f).await?;
    }
    for f in &manifest.files {
        let p = Path::new(&f.path);
        if f.created {
            // created 文件已被外部删除 = 结果一致，跳过（幂等）。
            if p.exists() {
                tokio::fs::remove_file(p)
                    .await
                    .map_err(|e| ToolError::WriteConflict {
                        path: f.path.clone(),
                        reason: format!("undo remove created file failed: {e}"),
                    })?;
            }
        } else {
            let before = side_content(&dir, &f.before, &f.before_file, "before").await?;
            // 原子写（temp+rename）：undo 是数据恢复路径，截断写半途崩溃 = 文件损坏。
            // Windows 上目标被编辑器占用时 rename 可能失败——此时返回冲突门错误，
            // 用户关掉占用后重试 undo（幂等）即可。
            crate::atomic_write(p, &before)
                .await
                .map_err(|e| ToolError::WriteConflict {
                    path: f.path.clone(),
                    reason: format!("undo restore failed: {e}"),
                })?;
        }
    }
    tokio::fs::rename(&dir, store.join(format!("undone-{n}")))
        .await
        .map_err(|e| ToolError::WriteConflict {
            path: dir.display().to_string(),
            reason: format!("undo: mark txn undone failed: {e}"),
        })?;
    // 整事务恢复成功后才登记（P2-b）：LS 同步只对真正落盘恢复的文件。
    register_touched(&manifest.files);
    Ok(manifest.files.len())
}

/// redo 单事务：冲突门（盘 sha == before_sha256 = undo 后状态）→ 写回 after。
async fn redo_one(store: &Path, n: u64) -> Result<usize, ToolError> {
    let dir = store.join(format!("undone-{n}"));
    let manifest = read_manifest(&dir).await?;
    for f in &manifest.files {
        // redo 冲突门：预期盘上处于 undo 后状态。
        if f.created {
            // created 文件 undo 后应不存在；已存在且内容 == after = 幂等重放，放行。
            if let Ok(c) = tokio::fs::read_to_string(&f.path).await
                && sha256_hex(c.as_bytes()) != f.after_sha256
            {
                return Err(conflict(
                    &f.path,
                    "redo: created file exists with foreign content",
                ));
            }
        } else {
            let before = side_content(&dir, &f.before, &f.before_file, "before").await?;
            let unchanged = tokio::fs::read_to_string(&f.path)
                .await
                .map(|c| sha256_hex(c.as_bytes()) == sha256_hex(before.as_bytes()))
                .unwrap_or(false);
            if !unchanged {
                return Err(conflict(
                    &f.path,
                    "redo: file on disk is not in the expected pre-redo (post-undo) state",
                ));
            }
        }
    }
    for f in &manifest.files {
        let after = side_content(&dir, &f.after, &f.after_file, "after").await?;
        let p = Path::new(&f.path);
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        // 原子写，同 undo_one：恢复路径禁截断写（防半途损坏）。
        crate::atomic_write(p, &after)
            .await
            .map_err(|e| ToolError::WriteConflict {
                path: f.path.clone(),
                reason: format!("redo reapply failed: {e}"),
            })?;
    }
    tokio::fs::rename(&dir, store.join(format!("txn-{n}")))
        .await
        .map_err(|e| ToolError::WriteConflict {
            path: dir.display().to_string(),
            reason: format!("redo: mark txn active failed: {e}"),
        })?;
    // 整事务重放成功后才登记（P2-b），同 undo_one。
    register_touched(&manifest.files);
    Ok(manifest.files.len())
}

fn conflict(path: &str, reason: &str) -> ToolError {
    ToolError::WriteConflict {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

/// undo 冲突门：单文件校验盘上状态 == 事务后状态。
async fn check_conflict_undo(f: &FileRec) -> Result<(), ToolError> {
    let current = tokio::fs::read_to_string(&f.path).await;
    match (f.created, current) {
        // created 文件已被外部删除 = undo 目标状态一致，放行（幂等跳过删除）。
        (true, Err(_)) => Ok(()),
        (true, Ok(c)) if sha256_hex(c.as_bytes()) == f.after_sha256 => Ok(()),
        (true, Ok(_)) => Err(conflict(
            &f.path,
            "undo conflict: created file was modified after the transaction",
        )),
        (false, Ok(c)) if sha256_hex(c.as_bytes()) == f.after_sha256 => Ok(()),
        (false, Ok(_)) => Err(conflict(
            &f.path,
            "undo conflict: file changed after the transaction (sha mismatch)",
        )),
        (false, Err(_)) => Err(conflict(
            &f.path,
            "undo conflict: expected the file to exist (post-transaction state), but it is missing",
        )),
    }
}

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

/// `undo`：回滚最近 `steps` 个事务；中途冲突即停（已完成的事务保留 undone 状态）。
pub(crate) async fn undo(root: &Path, steps: usize) -> Result<serde_json::Value, ToolError> {
    undo_at(&store_for(root)?, steps).await
}

/// [`undo`] 的存储路径注入版（单测用）。
pub(crate) async fn undo_at(store: &Path, steps: usize) -> Result<serde_json::Value, ToolError> {
    // 与写工具串行化：恢复写期间不得有并发写改盘。
    let _gate = crate::write_gate::acquire().await;
    let mut undone = Vec::new();
    for _ in 0..steps {
        match top_active(store).await {
            Ok(Some(n)) => {
                let files = undo_one(store, n).await?;
                undone.push(serde_json::json!({"txn_id": n, "files": files}));
            }
            // 空栈 = no-op（IDE undo 语义）。
            _ => break,
        }
    }
    Ok(serde_json::json!({"undone": undone}))
}

/// `redo`：重放最近被 undo 的事务。
pub(crate) async fn redo(root: &Path) -> Result<serde_json::Value, ToolError> {
    redo_at(&store_for(root)?).await
}

/// [`redo`] 的存储路径注入版（单测用）。
pub(crate) async fn redo_at(store: &Path) -> Result<serde_json::Value, ToolError> {
    let _gate = crate::write_gate::acquire().await;
    let mut redone = Vec::new();
    // redo 一次重放一个（IDE redo 单步语义；--steps 未列入契约）。
    if let Some(n) = top_undone(store).await {
        let files = redo_one(store, n).await?;
        redone.push(serde_json::json!({"txn_id": n, "files": files}));
    }
    Ok(serde_json::json!({"redone": redone}))
}

/// 栈顶 undone 事务（N 最大的 undone-*）。
async fn top_undone(store: &Path) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut rd = tokio::fs::read_dir(store).await.ok()?;
    while let Ok(Some(ent)) = rd.next_entry().await {
        if let Some(n) = parse_dir_n(ent.file_name().to_string_lossy().as_ref(), "undone-") {
            best = Some(best.map_or(n, |b: u64| b.max(n)));
        }
    }
    best
}

/// `undo --list`：栈概览（txn id/时间/文件数/摘要），顺带 prune（补充契约 7）。
pub(crate) async fn list(root: &Path) -> Result<serde_json::Value, ToolError> {
    list_at(&store_for(root)?).await
}

/// [`list`] 的存储路径注入版（单测用）。
pub(crate) async fn list_at(store: &Path) -> Result<serde_json::Value, ToolError> {
    let _gate = crate::write_gate::acquire().await;
    prune_at(store, &Limits::default()).await;
    let mut txns = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&store).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            let (prefix, n) = parse_dir_n(&name, "txn-")
                .map(|n| ("active", n))
                .or_else(|| parse_dir_n(&name, "undone-").map(|n| ("undone", n)))
                .unwrap_or(("", 0));
            if n == 0 {
                continue;
            }
            let dir = ent.path();
            let (files, summary, ts) = match read_manifest(&dir).await {
                Ok(m) => {
                    let first = m.files.first().map(|f| {
                        Path::new(&f.path)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| f.path.clone())
                    });
                    let summary = match (&first, m.files.len()) {
                        (Some(f), 1) => f.clone(),
                        (Some(f), n) => format!("{f} (+{n} files)"),
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
    Ok(
        serde_json::json!({"project_hash": store.file_name().map(|s| s.to_string_lossy().to_string()), "txns": txns}),
    )
}

/// 保留策略 prune：超龄 → 超数 → 超量，均从最旧（N 最小）整事务淘汰。
async fn prune_at(store: &Path, limits: &Limits) {
    let now = epoch_secs();
    // 收集 (n, state, timestamp, size)。
    let mut items: Vec<(u64, bool, u64, u64)> = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(store).await else {
        return;
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        let (is_active, n) = parse_dir_n(&name, "txn-")
            .map(|n| (true, n))
            .or_else(|| parse_dir_n(&name, "undone-").map(|n| (false, n)))
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
    for n in expired {
        // active 与 undone 同号不并存（状态机互斥），按两种名式尝试删除即可。
        let _ = tokio::fs::remove_dir_all(store.join(format!("txn-{n}"))).await;
        let _ = tokio::fs::remove_dir_all(store.join(format!("undone-{n}"))).await;
    }
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

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
