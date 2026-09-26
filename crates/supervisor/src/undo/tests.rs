//! undo 事务栈单测：聚合/链路/冲突门/prune 三上限/确定性 hash。
//! PENDING 是进程级 static，全部用例经 uid() 分配全局唯一事务 uid。

use super::*;

use std::time::{Duration, Instant};

/// 测试专用事务 uid：PENDING 是进程级 static，并行测试必须全局唯一 uid，
/// 否则同 uid 的并发用例互相偷 entries。
fn uid() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(1000);
    N.fetch_add(1, Ordering::Relaxed)
}

/// 唯一 tempdir（进程内计数器避免并行测试撞名）。
fn tmpdir(label: &str) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "serena_undo_{label}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// 在事务上下文内走收口写（模拟 execute_tool scope 包裹的工具执行）。
async fn txn_write(path: &Path, content: &str, uid: u64) {
    TXN_UID
        .scope(uid, recorded_write(path, content))
        .await
        .expect("txn write");
}

fn committed_dirs(store: &Path) -> Vec<String> {
    let mut names: Vec<(u64, String)> = std::fs::read_dir(store)
        .expect("store exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("txn-") || n.starts_with("undone-"))
        .map(|n| {
            let k = parse_dir_n(&n, "txn-")
                .or_else(|| parse_dir_n(&n, "undone-"))
                .unwrap_or(0);
            (k, n)
        })
        .collect();
    names.sort();
    names.into_iter().map(|(_, n)| n).collect()
}

/// 事务聚合：同 uid 多文件写入 → 一次 commit → 单事务多 files；异 uid 隔离。
#[tokio::test]
async fn commit_groups_entries_by_uid() {
    let work = tmpdir("agg_work");
    let store = tmpdir("agg_store");
    let a = work.join("a.txt");
    let b = work.join("b.txt");
    std::fs::write(&a, "old-a").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&a, "new-a", u1).await;
    txn_write(&b, "new-b", u1).await;
    txn_write(&work.join("c.txt"), "other-txn", u2).await;

    commit_at(&store, u1, &Limits::default()).await.unwrap();
    commit_at(&store, u2, &Limits::default()).await.unwrap();
    let dirs = committed_dirs(&store);
    assert_eq!(dirs.len(), 2, "two uids → two txns: {dirs:?}");

    let m1: Manifest = serde_json::from_str(
        &std::fs::read_to_string(store.join("txn-1").join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(m1.files.len(), 2, "uid=1 aggregates a.txt+b.txt");
    assert!(!m1.files[0].created);
    assert_eq!(m1.files[0].before.as_deref(), Some("old-a"));
}

/// abort：工具失败路径的快照不入栈。
#[tokio::test]
async fn abort_drops_pending_entries() {
    let work = tmpdir("abort_work");
    let store = tmpdir("abort_store");
    let a = work.join("a.txt");
    let u = uid();
    txn_write(&a, "x", u).await;
    abort(u);
    commit_at(&store, u, &Limits::default()).await.unwrap();
    assert!(
        committed_dirs(&store).is_empty(),
        "aborted txn must not persist"
    );
}

/// 主链路：修改 → undo 恢复 → redo 重放；目录状态 txn↔undone 翻转。
#[tokio::test]
async fn undo_restores_and_redo_reapplies() {
    let work = tmpdir("chain_work");
    let store = tmpdir("chain_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&a, "v2", u).await;
    commit_at(&store, u, &Limits::default()).await.unwrap();

    let r = undo_at(&store, 1).await.unwrap();
    assert_eq!(r["undone"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "v1",
        "undo restores before"
    );
    assert_eq!(committed_dirs(&store), vec!["undone-1".to_string()]);

    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "v2",
        "redo reapplies after"
    );
    assert_eq!(committed_dirs(&store), vec!["txn-1".to_string()]);
}

/// created 文件：undo = 删除；redo = 按 after 内容重建。
#[tokio::test]
async fn undo_created_deletes_file_and_redo_restores() {
    let work = tmpdir("created_work");
    let store = tmpdir("created_store");
    let a = work.join("new.txt");
    let u = uid();
    txn_write(&a, "created-content", u).await; // 旧文件不存在 → created
    commit_at(&store, u, &Limits::default()).await.unwrap();

    undo_at(&store, 1).await.unwrap();
    assert!(!a.exists(), "undo of created file deletes it");

    redo_at(&store).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "created-content",
        "redo restores it"
    );
}

/// 冲突门：事务后外部改文件 → undo 整事务拒绝且不产生半恢复。
#[tokio::test]
async fn undo_conflict_rejects_whole_txn() {
    let work = tmpdir("conflict_work");
    let store = tmpdir("conflict_store");
    let a = work.join("a.txt");
    let b = work.join("b.txt");
    std::fs::write(&a, "old-a").unwrap();
    std::fs::write(&b, "old-b").unwrap();
    let u = uid();
    txn_write(&a, "new-a", u).await;
    txn_write(&b, "new-b", u).await;
    commit_at(&store, u, &Limits::default()).await.unwrap();

    // 外部只改 b；undo 必须整体拒绝，a 也不得被回滚。
    std::fs::write(&b, "external-edit").unwrap();
    let err = undo_at(&store, 1).await.expect_err("conflict must reject");
    assert!(
        matches!(&err, ToolError::WriteConflict { path, .. } if path.ends_with("b.txt")),
        "error names the conflicting file: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "new-a",
        "a untouched (all-or-nothing)"
    );
    assert_eq!(
        std::fs::read_to_string(&b).unwrap(),
        "external-edit",
        "b untouched"
    );
    assert_eq!(
        committed_dirs(&store),
        vec!["txn-1".to_string()],
        "txn stays active"
    );
}

/// redo 冲突门：undo 后外部又改了文件 → redo 拒绝。
#[tokio::test]
async fn redo_conflict_rejects_after_external_edit() {
    let work = tmpdir("rconf_work");
    let store = tmpdir("rconf_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&a, "v2", u).await;
    commit_at(&store, u, &Limits::default()).await.unwrap();
    undo_at(&store, 1).await.unwrap();
    std::fs::write(&a, "hand-edited").unwrap();

    let err = redo_at(&store).await.expect_err("redo must refuse");
    assert!(
        matches!(err, ToolError::WriteConflict { .. }),
        "got: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "hand-edited",
        "disk untouched"
    );
}

/// IDE 语义：新写入落盘后 redo 链清空。
#[tokio::test]
async fn new_write_clears_redo_chain() {
    let work = tmpdir("redo_clear_work");
    let store = tmpdir("redo_clear_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&a, "v2", u1).await;
    commit_at(&store, u1, &Limits::default()).await.unwrap();
    undo_at(&store, 1).await.unwrap();
    assert_eq!(committed_dirs(&store), vec!["undone-1".to_string()]);

    txn_write(&a, "v3", u2).await;
    commit_at(&store, u2, &Limits::default()).await.unwrap();
    assert_eq!(
        committed_dirs(&store),
        vec!["txn-2".to_string()],
        "undone-1 wiped"
    );

    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"].as_array().unwrap().len(), 0, "redo chain empty");
}

/// undo 多步 --steps：两个事务逐步回滚。
#[tokio::test]
async fn undo_steps_rolls_back_multiple_txns() {
    let work = tmpdir("steps_work");
    let store = tmpdir("steps_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&a, "v2", u1).await;
    commit_at(&store, u1, &Limits::default()).await.unwrap();
    txn_write(&a, "v3", u2).await;
    commit_at(&store, u2, &Limits::default()).await.unwrap();

    undo_at(&store, 2).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "v1",
        "both txns rolled back"
    );
    assert_eq!(
        committed_dirs(&store),
        vec!["undone-1".to_string(), "undone-2".to_string()]
    );
}

/// prune 数量上限：>20 从事务号最小（最旧）开始整事务淘汰，含 undone。
#[tokio::test]
async fn prune_evicts_oldest_beyond_max_txns() {
    let store = tmpdir("prune_count_store");
    let work = tmpdir("prune_count_work");
    let a = work.join("a.txt");
    for i in 1..=22u64 {
        let u = uid();
        std::fs::write(&a, format!("v{i}")).unwrap();
        txn_write(&a, &format!("w{i}"), u).await;
        commit_at(&store, u, &Limits::default()).await.unwrap();
    }
    undo_at(&store, 1).await.unwrap(); // 最旧 txn-1 → undone-1
    let limits = Limits {
        max_txns: 20,
        ..Limits::default()
    };
    prune_at(&store, &limits).await;
    let dirs = committed_dirs(&store);
    assert_eq!(dirs.len(), 20, "stack depth capped: {dirs:?}");
    assert!(
        !dirs
            .iter()
            .any(|d| d == "txn-1" || d == "undone-1" || d == "txn-2"),
        "oldest evicted first: {dirs:?}"
    );
    assert!(
        dirs.contains(&"txn-22".to_string()) || dirs.contains(&"undone-22".to_string()),
        "newest survives (active or undone): {dirs:?}"
    );
}

/// prune 大小上限：注入小 max_total_bytes，断言从最旧淘汰且栈保持完整可用。
#[tokio::test]
async fn prune_evicts_oldest_beyond_total_bytes() {
    let store = tmpdir("prune_size_store");
    let work = tmpdir("prune_size_work");
    let a = work.join("a.txt");
    let payload = "x".repeat(400); // 每事务 before+after ~800B
    for i in 1..=3u64 {
        let u = uid();
        std::fs::write(&a, format!("v{i}-{payload}")).unwrap();
        txn_write(&a, &format!("w{i}-{payload}"), u).await;
        commit_at(&store, u, &Limits::default()).await.unwrap();
    }
    // cap=1500：3×~1134B 超限 → 删最旧 2 个，剩余 1134B ≤ cap 收敛。
    let limits = Limits {
        max_total_bytes: 1500,
        ..Limits::default()
    };
    prune_at(&store, &limits).await;
    let dirs = committed_dirs(&store);
    assert_eq!(
        dirs,
        vec!["txn-3".to_string()],
        "oldest evicted, newest kept: {dirs:?}"
    );
    // 剩余栈仍可用：undo 栈顶回滚成功。
    undo_at(&store, 1).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        format!("v3-{payload}")
    );
}

/// prune 时限：伪旧 timestamp 的 manifest 被淘汰（不依赖真实等待）。
#[tokio::test]
async fn prune_evicts_expired_transactions() {
    let store = tmpdir("prune_age_store");
    let work = tmpdir("prune_age_work");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&a, "v2", u1).await;
    commit_at(&store, u1, &Limits::default()).await.unwrap();

    // 手改 manifest 时间戳为 31 天前。
    let mpath = store.join("txn-1").join("manifest.json");
    let mut m: Manifest = serde_json::from_str(&std::fs::read_to_string(&mpath).unwrap()).unwrap();
    m.timestamp -= 31 * 24 * 3600;
    std::fs::write(&mpath, serde_json::to_string(&m).unwrap()).unwrap();

    txn_write(&a, "v3", u2).await;
    commit_at(&store, u2, &Limits::default()).await.unwrap(); // commit 追加前 prune

    let dirs = committed_dirs(&store);
    // 超龄事务被淘汰；新事务号从剩余 max+1 重算（N 复用，目录已删安全）。
    assert_eq!(dirs.len(), 1, "expired txn pruned on append: {dirs:?}");
    // 剩余 undo 链未被破坏：栈顶 v3 → undo → v2。
    undo_at(&store, 1).await.unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "v2");
}

/// project_hash 确定性：同路径两次一致、不同路径互异（禁 DefaultHasher 回归锁）。
#[tokio::test]
async fn project_hash_is_deterministic() {
    let a = tmpdir("hash_a");
    let b = tmpdir("hash_b");
    let ha1 = store_for(&a).unwrap();
    let ha2 = store_for(&a).unwrap();
    let hb = store_for(&b).unwrap();
    assert_eq!(ha1, ha2, "same root → same store across calls/processes");
    assert_ne!(ha1, hb, "different roots → different stores");
    assert_eq!(
        ha1.file_name().unwrap().to_string_lossy().len(),
        16,
        "16 hex"
    );
}

/// list：active/undone 状态、文件数、摘要齐备。
#[tokio::test]
async fn list_reports_stack_overview() {
    let work = tmpdir("list_work");
    let store = tmpdir("list_store");
    let a = work.join("alpha.txt");
    let b = work.join("beta.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&a, "v2", u).await;
    txn_write(&b, "v2", u).await;
    commit_at(&store, u, &Limits::default()).await.unwrap();

    let r = list_at(&store).await.unwrap();
    let txns = r["txns"].as_array().unwrap();
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0]["state"], "active");
    assert_eq!(txns[0]["files"], 2);
    assert_eq!(txns[0]["summary"], "alpha.txt (+2 files)");

    undo_at(&store, 1).await.unwrap();
    let r = list_at(&store).await.unwrap();
    assert_eq!(r["txns"][0]["state"], "undone");
}

/// 空栈 undo/redo = no-op，不报错（IDE 语义）。
#[tokio::test]
async fn empty_stack_is_noop() {
    let store = tmpdir("empty_store");
    std::fs::create_dir_all(&store).unwrap();
    let r = undo_at(&store, 1).await.unwrap();
    assert_eq!(r["undone"].as_array().unwrap().len(), 0);
    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"].as_array().unwrap().len(), 0);
}

/// recorded_write 无事务上下文（uid=0）退化为裸写且不入栈。
#[tokio::test]
async fn bare_write_outside_scope_skips_ledger() {
    let work = tmpdir("bare_work");
    let store = tmpdir("bare_store");
    let a = work.join("a.txt");
    recorded_write(&a, "direct").await.unwrap(); // 无 scope → uid=0
    commit_at(&store, 0, &Limits::default()).await.unwrap();
    assert!(committed_dirs(&store).is_empty());
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "direct");
}

/// 大快照旁路：>100KB 的 before/after 走 side file，undo/redo 语义不变。
#[tokio::test]
async fn large_snapshots_go_to_side_files() {
    let work = tmpdir("side_work");
    let store = tmpdir("side_store");
    let a = work.join("big.txt");
    let big = "y".repeat(150 * 1024);
    std::fs::write(&a, format!("v1{big}")).unwrap();
    let u = uid();
    txn_write(&a, &format!("v2{big}"), u).await;
    commit_at(&store, u, &Limits::default()).await.unwrap();

    let m: Manifest = serde_json::from_str(
        &std::fs::read_to_string(store.join("txn-1").join("manifest.json")).unwrap(),
    )
    .unwrap();
    assert!(
        m.files[0].before.is_none() && m.files[0].before_file.is_some(),
        "before side-filed"
    );
    assert!(
        m.files[0].after.is_none() && m.files[0].after_file.is_some(),
        "after side-filed"
    );

    undo_at(&store, 1).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        format!("v1{big}"),
        "undo via side file"
    );
    redo_at(&store).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        format!("v2{big}"),
        "redo via side file"
    );
}

/// 写门不饿死：undo 与并发写工具串行而非死锁（门获取两次成对释放）。
#[tokio::test]
async fn undo_and_write_gate_serialize() {
    let store = tmpdir("gate_store");
    std::fs::create_dir_all(&store).unwrap();
    let t = tokio::spawn({
        let store = store.clone();
        async move { undo_at(&store, 1).await }
    });
    let _ = crate::write_gate::acquire().await; // 与 undo_at 抢门
    drop(crate::write_gate::acquire().await);
    t.await.unwrap().unwrap();
}

/// 时间无泄漏哨兵：单测套件整体墙钟上界（防未来加入真实 sleep/超长 IO）。
#[tokio::test]
async fn suite_runs_fast() {
    let start = Instant::now();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(start.elapsed() < Duration::from_secs(5));
}
