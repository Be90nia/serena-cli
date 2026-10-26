//! AI 原生协同：content_version 版本账本 + others_active 活动推导（bd serena-rust-1282）。
//!
//! 两个 agent 先后对同一符号写，后写基于旧读静默覆盖先写（applied:true 无告警）
//! —— 写路径缺 read-write lineage。本模块提供两条护栏：
//!
//! 1. **版本账本**：按 canonical file 维护 daemon 生命周期内单调递增
//!    `content_version`（初始 0，每次成功写盘 +1）。写类工具可带
//!    `expected_version`：与账本不符 → `ToolError::StaleRead`（wire STALE_READ）
//!    拒绝；未带 → 照旧应用（零破坏），响应附当前版本。
//! 2. **活动账本**：近期（5min 窗）写活动环，推导 `others_active` 附在写成功 /
//!    edit-context / overview 响应——AI 能看见"别人刚动过哪些文件"。
//!
//! 接线（唯一咽喉，不逐工具加）：
//! - 校验 + bump 都在 [`crate::undo::recorded_write`]（全部写类路径的统一写盘点，
//!   且恒在写门内执行）——校验在 WAL 记账前、bump 在原子写成功后。
//! - `expected_version` / 目标文件经 task-local [`WRITE_CTX`] 由 execute_tool
//!   写类分支注入（TXN_STORE 同款模式）。
//! - 响应字段统一在 dispatch_tool 尾部 [`attach_collab_fields`] 塞入。
//!
//! 已知边界（wire 字段 doc 同步注明）：daemon 重启归零；外部编辑不 bump（版本
//! 语义 = "本 daemon 见过几次成功写"，单调性防 ABA——undo 回滚内容也不回退版本）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::undo;

/// others_active 窗口：近期写活动的判定窗（契约 5 分钟）。
const OTHERS_WINDOW: Duration = Duration::from_secs(5 * 60);
/// others_active 条目上限（契约）。
const OTHERS_MAX: usize = 5;
/// 活动环容量：超限裁最老。5min 窗内单 daemon 写事务远达不到 256。
const ACTIVITY_CAP: usize = 256;

/// 单条写活动（others_active / changed_since 数据源）。
#[derive(Clone, Debug)]
struct WriteRec {
    file: PathBuf,
    tool: String,
    uid: u64,
    at: Instant,
}

/// 版本账本：canonical file → 单调版本。daemon 生命周期作用域。
/// BTreeMap 而非 HashMap：`::new()` 是 const（static 初始化无需 LazyLock）。
static VERSIONS: StdMutex<BTreeMap<PathBuf, u64>> = StdMutex::new(BTreeMap::new());

/// 活动环（append-only，超限裁最老）。同步锁短临界区，无 await。
static ACTIVITY: StdMutex<Vec<WriteRec>> = StdMutex::new(Vec::new());
/// 写类分支注入的版本戳上下文：目标文件（canonical）+ 可选 expected_version。
#[derive(Clone, Debug)]
pub(crate) struct WriteCtx {
    target: PathBuf,
    /// 用户视角的 file 参数（错误消息展示用）。
    user_path: String,
    expected: Option<u64>,
    tool: String,
}

tokio::task_local! {
    /// execute_tool 写类分支 scope 注入；scope 外（--direct / 单测直调 recorded_write）
    /// 读不到 = 无校验（向后兼容）。
    pub(crate) static WRITE_CTX: Option<WriteCtx>;
}

/// STALE_READ 侧信道：recorded_write 门内校验失败时 stash（io::Error 面无法保真
/// 结构化详情），execute_tool 收尾按 uid 取走替换为干净的 `ToolError::StaleRead`。
/// 全局 map + uid 打标（task-local scope 在 take 之前已销毁，不能落 task-local）。
static STALE_HITS: StdMutex<BTreeMap<u64, StaleHit>> = StdMutex::new(BTreeMap::new());

/// STALE_READ 拒绝详情（wire 面逐字对应：expected_version / current_version /
/// changed_since）。
#[derive(Clone, Debug)]
pub(crate) struct StaleHit {
    pub user_path: String,
    pub expected: u64,
    pub current: u64,
    pub changed_since: String,
}

impl StaleHit {
    pub(crate) fn into_tool_error(self) -> crate::ToolError {
        crate::ToolError::StaleRead {
            path: self.user_path,
            expected: self.expected,
            current: self.current,
            changed_since: self.changed_since,
        }
    }
}

/// 从写类工具 args 提取版本戳上下文。`rename-symbol` / `safe-delete-symbol` 不接
/// expected_version（rename 已有 partial 对账）——显式传即 BAD_ARGS：静默忽略 =
/// AI 误以为有保护（假安全）。
pub(crate) fn write_ctx_from_args(
    root: &Path,
    tool: &str,
    args: &serde_json::Value,
) -> crate::ToolResult<Option<WriteCtx>> {
    let expected = match args.get("expected_version") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| {
            crate::ToolError::BadArgs {
                detail: "expected_version must be a non-negative integer (content_version from a \
                     prior read/write response)"
                    .to_string(),
            }
        })?),
    };
    if expected.is_some() && matches!(tool, "rename-symbol" | "safe-delete-symbol") {
        return Err(crate::ToolError::BadArgs {
            detail: format!(
                "{tool} does not accept expected_version (its own reconciliation covers partial \
                 application); re-read the file and retry instead"
            ),
        });
    }
    let Some(file) = args.get("file").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let abs = crate::path_guard::guarded_join(root, file)
        .map_err(|detail| crate::ToolError::BadArgs { detail })?;
    let canon = dunce::canonicalize(&abs).unwrap_or(abs);
    Ok(Some(WriteCtx {
        target: canon,
        user_path: file.to_string(),
        expected,
        tool: tool.to_string(),
    }))
}

/// 当前账本版本（canonical file；无条目 = 0）。
pub(crate) fn current_version(canon: &Path) -> u64 {
    VERSIONS
        .lock()
        .expect("version_ledger VERSIONS lock poisoned")
        .get(canon)
        .copied()
        .unwrap_or(0)
}

/// 写盘成功后的记账：版本 +1 + 活动登记。仅 [`crate::undo::recorded_write`] 调用
/// （写盘成功后一行）。dry-run / 直调（uid=0）也 bump？——uid=0 的直调路径
/// （--direct 调试）同样真实改盘，照记；tool 取不到时记 "unknown"。
pub(crate) fn record_write(write_path: &Path) {
    let canon = dunce::canonicalize(write_path).unwrap_or_else(|_| write_path.to_path_buf());
    let (tool, uid) = WRITE_CTX
        .try_with(|ctx| {
            (
                ctx.as_ref()
                    .map(|c| c.tool.clone())
                    .unwrap_or_else(|| "unknown".into()),
                undo::current_txn_uid(),
            )
        })
        .unwrap_or((String::from("unknown"), 0));
    VERSIONS
        .lock()
        .expect("version_ledger VERSIONS lock poisoned")
        .entry(canon.clone())
        .and_modify(|v| *v += 1)
        .or_insert(1);
    let mut act = ACTIVITY
        .lock()
        .expect("version_ledger ACTIVITY lock poisoned");
    act.push(WriteRec {
        file: canon,
        tool,
        uid,
        at: Instant::now(),
    });
    let excess = act.len().saturating_sub(ACTIVITY_CAP);
    if excess > 0 {
        act.drain(..excess);
    }
}

/// 门内、写盘前的权威校验（expected_version）。写入路径 ≠ 注入目标（rename 多文件
/// 事务）不拦；无 expected 不拦。失败 → StaleHit 进侧信道 + Err（中断写）。
pub(crate) fn stale_guard_check(write_path: &Path) -> Result<(), StaleHit> {
    let ctx = WRITE_CTX.try_with(|c| c.clone()).unwrap_or(None);
    let Some(ctx) = ctx else {
        return Ok(());
    };
    let Some(expected) = ctx.expected else {
        return Ok(());
    };
    let canon = dunce::canonicalize(write_path).unwrap_or_else(|_| write_path.to_path_buf());
    if canon != ctx.target {
        return Ok(());
    }
    let current = current_version(&canon);
    if current == expected {
        return Ok(());
    }
    Err(StaleHit {
        user_path: ctx.user_path,
        expected,
        current,
        changed_since: last_write_desc(&canon),
    })
}

/// changed_since 描述：该文件最近一条活动 `<tool> by tx-<uid>, <age>s ago`；
/// 无记录（daemon 重启 / 外部编辑）→ 显式说明。
fn last_write_desc(canon: &Path) -> String {
    let act = ACTIVITY
        .lock()
        .expect("version_ledger ACTIVITY lock poisoned");
    match act.iter().rev().find(|r| r.file == *canon) {
        Some(r) => {
            format!(
                "{} by tx-{}, {}s ago",
                r.tool,
                r.uid,
                r.at.elapsed().as_secs()
            )
        }
        None => "unknown writer (daemon restarted, external edit, or version scope exhausted)"
            .to_string(),
    }
}

/// execute_tool 收尾取走本事务的 STALE_READ 信号（uid 不匹配不动他人条目）。
pub(crate) fn take_stale_hit(uid: u64) -> Option<StaleHit> {
    STALE_HITS
        .lock()
        .expect("version_ledger STALE_HITS lock poisoned")
        .remove(&uid)
}

/// recorded_write 校验失败路径：stash 供 execute_tool 按当前事务 uid 取走。
pub(crate) fn stash_stale_hit(uid: u64, hit: StaleHit) {
    STALE_HITS
        .lock()
        .expect("version_ledger STALE_HITS lock poisoned")
        .insert(uid, hit);
}

/// 当前文件最近一条写活动（edit-context / overview 的 `last_changed` 数据源）。
/// 与 [`others_active`]（排除本文件）互补：同文件保护归版本戳，本文件视角归这里
/// （bd serena-rust-1282 PM 复验——"打开我下一步要改的文件"时，同文件近期写活动
/// 恰是最关键决策信息，裸 content_version 无基线可比）。无记录 = None（省略）。
pub(crate) fn last_changed_for(canon: &Path) -> Option<serde_json::Value> {
    let act = ACTIVITY
        .lock()
        .expect("version_ledger ACTIVITY lock poisoned");
    act.iter().rev().find(|r| r.file == *canon).map(|r| {
        json!({
            "tool": r.tool,
            "invocation": format!("tx-{}", r.uid),
            "age_s": r.at.elapsed().as_secs(),
        })
    })
}

/// others_active 推导：5min 窗、同 root、非本文件，按新→旧取 5。空 = 省略
/// （调用方判 empty）。file 展示为 root 相对路径。
pub(crate) fn others_active(root: &Path, exclude: &Path) -> Vec<serde_json::Value> {
    let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let act = ACTIVITY
        .lock()
        .expect("version_ledger ACTIVITY lock poisoned");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in act.iter().rev() {
        if r.file == *exclude || !r.file.starts_with(&root_canon) {
            continue;
        }
        let age = r.at.elapsed();
        if age > OTHERS_WINDOW {
            break; // 环按时间有序（append 递增），倒序首条超窗即全超窗
        }
        if !seen.insert((r.uid, r.file.clone())) {
            continue;
        }
        // file 展示 = user_path 同款惯例（strip_prefix + display，原生分隔符）
        let file = r
            .file
            .strip_prefix(&root_canon)
            .unwrap_or(&r.file)
            .display()
            .to_string();
        out.push(json!({
            "invocation": format!("tx-{}", r.uid),
            "tool": r.tool,
            "file": file,
            "age_s": age.as_secs(),
        }));
        if out.len() >= OTHERS_MAX {
            break;
        }
    }
    out
}

/// dispatch_tool 尾部统一塞协同字段：读类 4 工具（symbol-body / read-file /
/// edit-context / overview）塞 `content_version`；写类 + edit-context + overview
/// 另塞 `others_active`（空省略）。裸数组形态（overview 默认 wire）无处可塞即跳过
/// ——字段齐全让位于 wire 兼容（bd kq6e 裸数组承诺）。
pub(crate) fn attach_collab_fields(
    tool: &str,
    root: &Path,
    args: &serde_json::Value,
    value: &mut serde_json::Value,
) {
    const READ_TOOLS: &[&str] = &["symbol-body", "read-file", "edit-context", "overview"];
    let is_write = undo::WRITE_TOOLS.contains(&tool);
    if !is_write && !READ_TOOLS.contains(&tool) {
        return;
    }
    let Some(file) = args.get("file").and_then(serde_json::Value::as_str) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let abs = root.join(file);
    let canon = dunce::canonicalize(&abs).unwrap_or(abs);
    obj.insert("content_version".into(), json!(current_version(&canon)));
    if is_write || matches!(tool, "edit-context" | "overview") {
        let others = others_active(root, &canon);
        if !others.is_empty() {
            obj.insert("others_active".into(), serde_json::Value::Array(others));
        }
    }
    // bd serena-rust-1282 PM 复验：edit-context / overview 附当前文件最近写活动
    // （others_active 排除本文件的语义不动——写响应/他文件保护归版本戳）。
    if matches!(tool, "edit-context" | "overview")
        && let Some(rec) = last_changed_for(&canon)
    {
        obj.insert("last_changed".into(), rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_UID: AtomicU64 = AtomicU64::new(9000);

    fn tmp_file(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join(tag);
        std::fs::write(&p, "x\n").expect("write fixture");
        (dir, p)
    }

    #[test]
    fn version_starts_at_zero_and_bumps_per_write() {
        let (_d, p) = tmp_file("a.rs");
        let canon = dunce::canonicalize(&p).expect("canon");
        assert_eq!(current_version(&canon), 0, "账本无条目 = 0");
        record_write(&p);
        record_write(&p);
        assert_eq!(current_version(&canon), 2, "每次成功写 +1");
    }

    #[test]
    fn multi_file_versions_track_independently() {
        let (_d, a) = tmp_file("a.rs");
        let (_d2, b) = tmp_file("b.rs");
        record_write(&a);
        record_write(&a);
        record_write(&b);
        let ca = dunce::canonicalize(&a).unwrap();
        let cb = dunce::canonicalize(&b).unwrap();
        assert_eq!(current_version(&ca), 2);
        assert_eq!(current_version(&cb), 1, "各归各账");
    }

    #[tokio::test]
    async fn stale_check_rejects_mismatched_expected_and_passes_current() {
        let (_d, p) = tmp_file("c.rs");
        let canon = dunce::canonicalize(&p).unwrap();
        record_write(&p); // version = 1
        let ctx = |expected| WriteCtx {
            target: canon.clone(),
            user_path: "c.rs".into(),
            expected,
            tool: "replace-body".into(),
        };
        let hit = WRITE_CTX
            .scope(Some(ctx(Some(0))), async { stale_guard_check(&p) })
            .await
            .unwrap_err();
        assert_eq!(hit.expected, 0);
        assert_eq!(hit.current, 1, "旧 expected → STALE_READ");
        assert!(
            hit.changed_since.contains("by tx-"),
            "{}",
            hit.changed_since
        );
        // 提供"正确 version"→ 放行
        WRITE_CTX
            .scope(Some(ctx(Some(1))), async { stale_guard_check(&p) })
            .await
            .expect("current version passes");
        // 未提供 → 放行
        WRITE_CTX
            .scope(Some(ctx(None)), async { stale_guard_check(&p) })
            .await
            .expect("no expected_version passes");
        // 非目标文件（rename 多文件）→ 不拦
        let (_d3, q) = tmp_file("other.rs");
        WRITE_CTX
            .scope(Some(ctx(Some(0))), async { stale_guard_check(&q) })
            .await
            .expect("non-target file passes");
    }

    #[tokio::test]
    async fn stash_take_roundtrip_scoped_by_uid() {
        let (_d, p) = tmp_file("d.rs");
        let uid = TEST_UID.fetch_add(1, Ordering::Relaxed);
        let canon = dunce::canonicalize(&p).expect("canon");
        record_write(&p); // 账本 = 1，使 expected=0 构成 stale
        let ctx = WriteCtx {
            target: canon,
            user_path: "d.rs".into(),
            expected: Some(0),
            tool: "t".into(),
        };
        WRITE_CTX
            .scope(Some(ctx), async {
                if let Err(hit) = stale_guard_check(&p) {
                    stash_stale_hit(uid, hit);
                }
            })
            .await;
        assert!(take_stale_hit(uid + 1).is_none(), "他人 uid 取不走");
        let hit = take_stale_hit(uid).expect("own uid takes");
        assert_eq!(hit.expected, 0);
        assert!(take_stale_hit(uid).is_none(), "取走即清");
    }

    #[tokio::test]
    async fn others_active_filters_window_root_and_excludes_self() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.rs");
        let b = dir.path().join("sub");
        std::fs::create_dir_all(&b).unwrap();
        let b = b.join("b.rs");
        std::fs::write(&a, "x\n").unwrap();
        std::fs::write(&b, "x\n").unwrap();
        record_write(&a);
        record_write(&b);
        let ca = dunce::canonicalize(&a).unwrap();
        let root = dir.path();
        let others = others_active(root, &ca);
        assert_eq!(others.len(), 1, "排除本文件，只剩 sub/b.rs: {others:?}");
        assert_eq!(
            others[0]["file"],
            json!(format!("sub{0}b.rs", std::path::MAIN_SEPARATOR)),
            "root 相对路径展示（原生分隔符，user_path 惯例）"
        );
        assert!(others[0]["age_s"].is_u64());
        assert!(others[0]["tool"].is_string());
        // 本文件排除后，a 的活动仍在（a 是"别人"）→ 1 条
        let others_b = others_active(root, &dunce::canonicalize(&b).unwrap());
        assert_eq!(others_b.len(), 1, "{others_b:?}");
        assert_eq!(others_b[0]["file"], json!("a.rs"));
        // root 外文件不可见
        let (outside, op) = tmp_file("outside.rs");
        record_write(&op);
        assert!(
            !others_active(root, &ca)
                .iter()
                .any(|o| o["file"].as_str().is_some_and(|f| f.contains("outside"))),
            "root 外活动不进 others_active"
        );
        drop(outside);
    }

    #[test]
    fn write_ctx_rejects_expected_version_on_rename_and_safe_delete() {
        let args = json!({"file": "a.rs", "expected_version": 1});
        for tool in ["rename-symbol", "safe-delete-symbol"] {
            let err = write_ctx_from_args(Path::new("."), tool, &args).unwrap_err();
            assert!(
                err.to_string().contains("does not accept expected_version"),
                "{tool}: {err}"
            );
        }
    }

    #[test]
    fn write_ctx_rejects_non_integer_expected_version() {
        let args = json!({"file": "a.rs", "expected_version": "5"});
        let err = write_ctx_from_args(Path::new("."), "replace-body", &args).unwrap_err();
        assert!(err.to_string().contains("non-negative integer"), "{err}");
    }

    #[test]
    fn attach_adds_content_version_and_others_active() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "x\n").unwrap();
        std::fs::write(&b, "x\n").unwrap();
        record_write(&a); // 别人先写了 a.rs
        // edit-context（读类）：object 响应塞 content_version + others_active
        let mut v = json!({"body": "...", "callers": []});
        attach_collab_fields(
            "edit-context",
            dir.path(),
            &json!({"file": "b.rs", "symbol": "f"}),
            &mut v,
        );
        assert_eq!(v["content_version"], json!(0), "b.rs 未写过 = 0");
        assert_eq!(v["others_active"][0]["file"], json!("a.rs"));
        // 写类：全新 root（无任何活动）→ others_active 省略
        let dir2 = tempfile::tempdir().expect("tempdir2");
        let c = dir2.path().join("c.rs");
        std::fs::write(&c, "x\n").unwrap();
        let mut v2 = json!({"applied": true, "file": "c.rs"});
        attach_collab_fields(
            "replace-body",
            dir2.path(),
            &json!({"file": "c.rs"}),
            &mut v2,
        );
        assert_eq!(v2["content_version"], json!(0));
        assert!(v2.get("others_active").is_none(), "空活动省略字段: {v2}");
    }

    #[test]
    fn attach_skips_non_object_and_non_collab_tools() {
        // overview 默认裸数组 wire（bd kq6e 承诺）→ 无处塞即跳过
        let mut arr = json!([{"name": "f"}]);
        attach_collab_fields(
            "overview",
            Path::new("."),
            &json!({"file": "a.rs"}),
            &mut arr,
        );
        assert!(arr.is_array(), "裸数组形态不得被改写: {arr}");
        // 非协同工具不动
        let mut v = json!({"hits": []});
        attach_collab_fields(
            "find-symbol",
            Path::new("."),
            &json!({"query": "x"}),
            &mut v,
        );
        assert_eq!(v, json!({"hits": []}));
    }

    /// bd serena-rust-1282 PM 复验：edit-context 对**当前文件**附 last_changed
    /// （同文件近期写 = 打开"下一步要改的文件"的关键决策信息）；无活动省略；
    /// others_active 排除本文件的语义不动。
    #[tokio::test]
    async fn edit_context_carries_last_changed_for_current_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = dir.path().join("b.rs");
        std::fs::write(&b, "x\n").unwrap();
        // 无活动 → 省略
        let mut v0 = json!({"body": "...", "callers": []});
        attach_collab_fields(
            "edit-context",
            dir.path(),
            &json!({"file": "b.rs", "symbol": "f"}),
            &mut v0,
        );
        assert!(v0.get("last_changed").is_none(), "无活动省略: {v0}");
        // 同文件近期写 → last_changed 指认该写
        let canon = dunce::canonicalize(&b).unwrap();
        let ctx = WriteCtx {
            target: canon,
            user_path: "b.rs".into(),
            expected: None,
            tool: "replace-body".into(),
        };
        WRITE_CTX
            .scope(Some(ctx), async {
                record_write(&b);
            })
            .await;
        let mut v = json!({"body": "...", "callers": []});
        attach_collab_fields(
            "edit-context",
            dir.path(),
            &json!({"file": "b.rs", "symbol": "f"}),
            &mut v,
        );
        let lc = v.get("last_changed").expect("last_changed in {v}");
        assert_eq!(lc["tool"], json!("replace-body"), "{lc}");
        assert!(lc["invocation"].as_str().unwrap().starts_with("tx-"));
        assert!(lc["age_s"].is_u64());
        // others_active 仍排除本文件（b 是唯一活动 → 字段省略，语义不变）
        assert!(v.get("others_active").is_none(), "{v}");
    }
}
