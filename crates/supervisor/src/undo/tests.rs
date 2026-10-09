//! undo 事务栈单测：WAL 记账序（bd 15jb）/ redo FIFO 重放（bd b5od）/ 冲突自动
//! 跳过（bd 3ux6）/ 聚合 / 链路 / prune 三上限 / 确定性 hash。
//! OPEN_TXNS 是进程级 static，全部用例经 uid() 分配全局唯一事务 uid。

use super::*;

use std::time::{Duration, Instant};

/// 测试专用事务 uid：OPEN_TXNS 是进程级 static，并行测试必须全局唯一 uid，
/// 否则同 uid 的并发用例互相偷账。
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

/// 在事务上下文内走 WAL 收口写（模拟 execute_tool 的 TXN_STORE+TXN_UID 双 scope
/// 包裹的工具执行）。
async fn txn_write(store: &Path, path: &Path, content: &str, uid: u64) {
    TXN_STORE
        .scope(
            store.to_path_buf(),
            TXN_UID.scope(uid, recorded_write(path, content)),
        )
        .await
        .expect("txn write");
}

/// 栈内目录（txn-/undone-），按号排序。discarded 不在此列（已退栈）。
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

/// 事务聚合：同 uid 多文件写入 → 单事务多 files；异 uid 隔离。
#[tokio::test]
async fn commit_groups_entries_by_uid() {
    let work = tmpdir("agg_work");
    let store = tmpdir("agg_store");
    let a = work.join("a.txt");
    let b = work.join("b.txt");
    std::fs::write(&a, "old-a").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&store, &a, "new-a", u1).await;
    txn_write(&store, &b, "new-b", u1).await;
    txn_write(&store, &work.join("c.txt"), "other-txn", u2).await;

    commit_at(&store, u1).await.unwrap();
    commit_at(&store, u2).await.unwrap();
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

/// WAL 记账先于目标写盘（bd serena-rust-15jb）：txn_write 返回后 manifest 必已
/// 在盘（kill 打中写提交窗的后半段也有账）。
#[tokio::test]
async fn wal_manifest_persisted_before_target_write() {
    let work = tmpdir("wal_order_work");
    let store = tmpdir("wal_order_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "old").unwrap();
    let u = uid();
    // 在 recorded_write 的 WAL 段与目标写之间注入"崩溃"：手动走 WAL 后不写目标。
    let dir = TXN_STORE
        .scope(store.clone(), wal_open(&store, u))
        .await
        .unwrap();
    TXN_STORE
        .scope(
            store.clone(),
            wal_append(&store, u, &a, Some("old".into()), false, "new"),
        )
        .await
        .unwrap();
    // 记账提交点已在盘，目标文件未动 = 「有记录无改动」崩溃态。
    let m: Manifest =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(m.files.len(), 1, "manifest has the record");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "old", "target untouched");
}

/// 「有记录无改动」崩溃态可恢复不楔死（bd serena-rust-15jb 验收）：undo 幂等
/// 收口为 no-op，文件不动，栈照常翻转。
#[tokio::test]
async fn undo_recovering_record_without_change_is_noop() {
    let work = tmpdir("wal_recover_work");
    let store = tmpdir("wal_recover_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "old").unwrap();
    let u = uid();
    let dir = TXN_STORE
        .scope(store.clone(), wal_open(&store, u))
        .await
        .unwrap();
    TXN_STORE
        .scope(
            store.clone(),
            wal_append(&store, u, &a, Some("old".into()), false, "new"),
        )
        .await
        .unwrap();
    // 崩溃点：目标写从未发生。undo 必须干净收口（绝不 WRITE_CONFLICT 楔死）。
    let r = undo_at(&store, 1).await.unwrap();
    let entries = r["undone"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["files"], 0);
    assert!(
        entries[0]["note"].as_str().unwrap().contains("no-op"),
        "no-op note present: {}",
        entries[0]
    );
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "old", "file untouched");
    assert_eq!(committed_dirs(&store), vec!["undone-1".to_string()]);
    // 收口后栈仍健康：redo 可重放（盘面 == before，正放通过）。
    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"][0]["txn_id"], 1);
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "new", "redo reapplies");
    let _ = std::fs::remove_dir_all(dir);
}

/// abort：工具失败路径删除已开账的 WAL 目录，不入栈。
#[tokio::test]
async fn abort_drops_pending_entries() {
    let work = tmpdir("abort_work");
    let store = tmpdir("abort_store");
    let a = work.join("a.txt");
    let u = uid();
    txn_write(&store, &a, "x", u).await;
    abort(u);
    commit_at(&store, u).await.unwrap();
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
    txn_write(&store, &a, "v2", u).await;
    commit_at(&store, u).await.unwrap();

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

/// bd serena-rust-b5od 核心锁：深度 3 全链重放 —— undo ×3 后 redo ×3 全部成功
/// （旧版 LIFO 取 N 最大 undone 项，深度 ≥2 即永久 WRITE_CONFLICT）。
#[tokio::test]
async fn redo_replays_full_chain_in_time_order() {
    let work = tmpdir("fifo_work");
    let store = tmpdir("fifo_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    for (u, v) in [(uid(), "v2"), (uid(), "v3"), (uid(), "v4")] {
        txn_write(&store, &a, v, u).await;
        commit_at(&store, u).await.unwrap();
    }
    undo_at(&store, 3).await.unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "v1", "back to baseline");
    assert_eq!(
        committed_dirs(&store),
        vec![
            "undone-1".to_string(),
            "undone-2".to_string(),
            "undone-3".to_string()
        ]
    );

    // 重放序 = 事务时间序：txn-1 → txn-2 → txn-3；已重放的翻回 txn-*。
    for (expect_dirs, expect_content) in [
        (
            vec!["txn-1", "undone-2", "undone-3"],
            "v2",
        ),
        (
            vec!["txn-1", "txn-2", "undone-3"],
            "v3",
        ),
        (
            vec!["txn-1", "txn-2", "txn-3"],
            "v4",
        ),
    ] {
        let r = redo_at(&store).await.unwrap();
        assert_eq!(r["redone"].as_array().unwrap().len(), 1, "replay {expect_content}");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            expect_content,
            "time-order replay"
        );
        assert_eq!(
            committed_dirs(&store),
            expect_dirs,
            "stack flips one per call"
        );
    }
}

/// created 文件：undo = 删除；redo = 按 after 内容重建。
#[tokio::test]
async fn undo_created_deletes_file_and_redo_restores() {
    let work = tmpdir("created_work");
    let store = tmpdir("created_store");
    let a = work.join("new.txt");
    let u = uid();
    txn_write(&store, &a, "created-content", u).await; // 旧文件不存在 → created
    commit_at(&store, u).await.unwrap();

    undo_at(&store, 1).await.unwrap();
    assert!(!a.exists(), "undo of created file deletes it");

    redo_at(&store).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "created-content",
        "redo restores it"
    );
}

/// bd serena-rust-3ux6 核心锁：事务后外部编辑 → undo 不再永久楔死，而是整事务
/// discarded 退栈 + warning（含原因与下一可用事务），其余文件零恢复写。
#[tokio::test]
async fn undo_external_conflict_discards_txn_with_warning() {
    let work = tmpdir("conflict_work");
    let store = tmpdir("conflict_store");
    let a = work.join("a.txt");
    let b = work.join("b.txt");
    std::fs::write(&a, "old-a").unwrap();
    std::fs::write(&b, "old-b").unwrap();
    let u = uid();
    txn_write(&store, &a, "new-a", u).await;
    txn_write(&store, &b, "new-b", u).await;
    commit_at(&store, u).await.unwrap();

    // 外部只改 b：整事务不可干净回滚 → discarded，a 也不得被回滚。
    std::fs::write(&b, "external-edit").unwrap();
    let r = undo_at(&store, 1).await.unwrap();
    assert!(r["undone"].as_array().unwrap().is_empty(), "nothing reverted");
    let skipped = r["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "one discard warning: {r}");
    assert_eq!(skipped[0]["txn_id"], 1);
    assert_eq!(skipped[0]["state"], "discarded");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("sha mismatch"),
        "reason names the mismatch: {}",
        skipped[0]
    );
    assert!(skipped[0]["file"].as_str().unwrap().ends_with("b.txt"));
    assert!(skipped[0]["next_active_txn"].is_null(), "stack now empty");

    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "new-a",
        "a untouched (all-or-nothing)"
    );
    assert_eq!(
        committed_dirs(&store),
        Vec::<String>::new(),
        "txn left the stack"
    );
    assert!(store.join("discarded-1").is_dir(), "forensic copy kept");
    // 退栈后栈可用：新写入正常入栈（且不复用 discarded 的 N）。
    let u2 = uid();
    txn_write(&store, &a, "newer-a", u2).await;
    commit_at(&store, u2).await.unwrap();
    assert_eq!(committed_dirs(&store), vec!["txn-2".to_string()], "N not reused");
}

/// bd serena-rust-3ux6：跳过不占 steps 名额 —— 冲突顶事务 discarded 后，本次
/// 调用继续回滚下一个可用事务，warning 指向它。
#[tokio::test]
async fn undo_skip_does_not_consume_step_budget() {
    let work = tmpdir("skip_work");
    let store = tmpdir("skip_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&store, &a, "v2", u1).await;
    commit_at(&store, u1).await.unwrap();
    txn_write(&store, &a, "v3", u2).await;
    commit_at(&store, u2).await.unwrap();

    // 外部把顶事务（txn-2）的产物改成别的；txn-1 的预期盘面（v2）也被破坏 ——
    // 两个都冲突 → 链式 discarded，栈清空。
    std::fs::write(&a, "hand-edit").unwrap();
    let r = undo_at(&store, 1).await.unwrap();
    assert_eq!(r["skipped"].as_array().unwrap().len(), 2, "chained discard: {r}");
    assert_eq!(
        r["skipped"][0]["next_active_txn"], 1,
        "warning points to next candidate"
    );
    assert_eq!(
        committed_dirs(&store),
        Vec::<String>::new(),
        "stack fully unwound"
    );
}

/// 幂等补完：恢复写半途崩溃（一文件已恢复、一文件未动）→ 重试 undo 补完整事务。
#[tokio::test]
async fn undo_completes_partially_restored_txn() {
    let work = tmpdir("partial_work");
    let store = tmpdir("partial_store");
    let a = work.join("a.txt");
    let b = work.join("b.txt");
    std::fs::write(&a, "old-a").unwrap();
    std::fs::write(&b, "old-b").unwrap();
    let u = uid();
    txn_write(&store, &a, "new-a", u).await;
    txn_write(&store, &b, "new-b", u).await;
    commit_at(&store, u).await.unwrap();

    // 模拟崩溃点：a 已恢复到 before，b 未动，目录仍在 txn-1。
    std::fs::write(&a, "old-a").unwrap();
    let r = undo_at(&store, 1).await.unwrap();
    assert_eq!(r["undone"][0]["files"], 2, "idempotent completion");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "old-b", "b restored");
    assert_eq!(committed_dirs(&store), vec!["undone-1".to_string()]);
}

/// redo 幂等收口：redo 写后、收口 rename 前崩溃（盘面已 == after 但目录仍
/// undone）→ 重试 redo no-op 收口，不楔死。
#[tokio::test]
async fn redo_noop_when_change_already_on_disk() {
    let work = tmpdir("redo_noop_work");
    let store = tmpdir("redo_noop_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&store, &a, "v2", u).await;
    commit_at(&store, u).await.unwrap();
    undo_at(&store, 1).await.unwrap();

    // 崩溃点：after 已落盘，目录仍是 undone-1。
    std::fs::write(&a, "v2").unwrap();
    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"][0]["files"], 0, "no rewrite needed");
    assert!(
        r["redone"][0]["note"].as_str().unwrap().contains("no-op"),
        "no-op note: {r}"
    );
    assert_eq!(committed_dirs(&store), vec!["txn-1".to_string()], "flip done");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "v2");
}

/// bd serena-rust-b5od：redo 外部冲突 → undone 事务 discarded 退栈 + warning，
/// 盘面不动（与 undo 对称的出路）。
#[tokio::test]
async fn redo_external_conflict_discards_txn() {
    let work = tmpdir("rconf_work");
    let store = tmpdir("rconf_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&store, &a, "v2", u).await;
    commit_at(&store, u).await.unwrap();
    undo_at(&store, 1).await.unwrap();
    std::fs::write(&a, "hand-edited").unwrap();

    let r = redo_at(&store).await.unwrap();
    assert!(r["redone"].as_array().unwrap().is_empty(), "nothing replayed");
    let skipped = r["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "discard warning: {r}");
    assert_eq!(skipped[0]["txn_id"], 1);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("pre-redo (post-undo) state"),
        "reason names the broken pre-image: {}",
        skipped[0]
    );
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "hand-edited",
        "disk untouched"
    );
    assert_eq!(committed_dirs(&store), Vec::<String>::new());
    assert!(store.join("discarded-1").is_dir());
}

/// IDE 语义：新写入落盘后 redo 链清空。
#[tokio::test]
async fn new_write_clears_redo_chain() {
    let work = tmpdir("redo_clear_work");
    let store = tmpdir("redo_clear_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let (u1, u2) = (uid(), uid());
    txn_write(&store, &a, "v2", u1).await;
    commit_at(&store, u1).await.unwrap();
    undo_at(&store, 1).await.unwrap();
    assert_eq!(committed_dirs(&store), vec!["undone-1".to_string()]);

    txn_write(&store, &a, "v3", u2).await;
    commit_at(&store, u2).await.unwrap();
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
    txn_write(&store, &a, "v2", u1).await;
    commit_at(&store, u1).await.unwrap();
    txn_write(&store, &a, "v3", u2).await;
    commit_at(&store, u2).await.unwrap();

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
        txn_write(&store, &a, &format!("w{i}"), u).await;
        commit_at(&store, u).await.unwrap();
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
        txn_write(&store, &a, &format!("w{i}-{payload}"), u).await;
        commit_at(&store, u).await.unwrap();
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
    txn_write(&store, &a, "v2", u1).await;
    commit_at(&store, u1).await.unwrap();

    // 手改 manifest 时间戳为 31 天前。
    let mpath = store.join("txn-1").join("manifest.json");
    let mut m: Manifest = serde_json::from_str(&std::fs::read_to_string(&mpath).unwrap()).unwrap();
    m.timestamp -= 31 * 24 * 3600;
    std::fs::write(&mpath, serde_json::to_string(&m).unwrap()).unwrap();

    txn_write(&store, &a, "v3", u2).await;
    commit_at(&store, u2).await.unwrap(); // wal_open 开账前 prune

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

/// bd serena-rust-ej5：canonicalize 失败但目录仍在（挪盘瞬窗/权限变化）→
/// 退回原始路径串哈希，老事务栈不孤儿；彻底删除仍 BAD_ARGS。
#[tokio::test]
async fn store_for_falls_back_to_raw_path_when_canonicalize_fails() {
    // 目录在盘：fallback 分支与 canonicalize 分支都给 16-hex 确定性键。
    let a = tmpdir("hash_fallback_a");
    let h1 = store_for(&a).unwrap();
    let h2 = store_for(&a).unwrap();
    assert_eq!(h1, h2);
    assert_eq!(h1.file_name().unwrap().to_string_lossy().len(), 16);

    // 彻底删除：BAD_ARGS 契约保持。
    let gone = std::env::temp_dir().join(format!("serena-undo-gone-{}", std::process::id()));
    let err = store_for(&gone).unwrap_err();
    assert!(
        matches!(err, crate::ToolError::BadArgs { .. }),
        "deleted root must stay BAD_ARGS, got {err:?}"
    );
}

/// list：active/undone/discarded 状态、文件数、摘要齐备。
#[tokio::test]
async fn list_reports_stack_overview() {
    let work = tmpdir("list_work");
    let store = tmpdir("list_store");
    let a = work.join("alpha.txt");
    let b = work.join("beta.txt");
    std::fs::write(&a, "v1").unwrap();
    let u = uid();
    txn_write(&store, &a, "v2", u).await;
    txn_write(&store, &b, "v2", u).await;
    commit_at(&store, u).await.unwrap();

    let r = list_at(&store).await.unwrap();
    let txns = r["txns"].as_array().unwrap();
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0]["state"], "active");
    assert_eq!(txns[0]["files"], 2);
    // bd serena-rust-mfht F8：list 摘要带行数 diff；多文件聚合 first 文件名 +
    // 余文件 +/- 总量。alpha.txt "v1"→"v2" 行数 1→1 但内容变了（行数 +/-0
    // 仍算 modify，非 no change），beta.txt 新建 1 行，总聚合 = alpha first
    // + 1 余文件 +1/-0 总量。
    assert_eq!(
        txns[0]["summary"],
        "alpha.txt +0/-0 lines (+1 files, +1/-0 total)"
    );

    undo_at(&store, 1).await.unwrap();
    let r = list_at(&store).await.unwrap();
    assert_eq!(r["txns"][0]["state"], "undone");

    // discarded 留档可见（外部冲突退栈后仍可盘点，不静默消失）。
    std::fs::write(&a, "hand-edit").unwrap();
    let _ = redo_at(&store).await.unwrap();
    let r = list_at(&store).await.unwrap();
    assert_eq!(r["txns"][0]["state"], "discarded", "{r}");
}

/// bd serena-rust-mfht F8：list 摘要带行数 diff——单文件 modify 多行 + 新文件场景。
/// agent 多步编辑后凭 summary 选 undo target 必须有信息量。
#[tokio::test]
async fn list_summary_carries_line_diff_for_selection() {
    let work = tmpdir("summary_work");
    let store = tmpdir("summary_store");
    let a = work.join("alpha.rs");
    let b = work.join("beta.rs");
    std::fs::write(&a, "line1\nline2\nline3\n").unwrap();
    let u = uid();
    txn_write(&store, &a, "line1\nMODIFIED\nline3\nline4\n", u).await;
    txn_write(&store, &b, "first\n", u).await;
    commit_at(&store, u).await.unwrap();

    let r = list_at(&store).await.unwrap();
    let txns = r["txns"].as_array().unwrap();
    assert_eq!(txns.len(), 1);
    let s = txns[0]["summary"].as_str().unwrap();
    // first 文件 alpha.rs 3 行→4 行（+1/-0，扩行不算减行）+ 余文件 beta.rs
    // 新建 1 行 → 总量 +2/-0。
    assert!(
        s.contains("alpha.rs +1/-0 lines"),
        "first 文件 modify 应带 +/- 行数: {s}"
    );
    assert!(
        s.contains("+1 files, +2/-0 total"),
        "余文件聚合应含 total: {s}"
    );

    // 单文件 create 场景：另一事务只新建一个文件。
    let c = work.join("new.rs");
    let u2 = uid();
    txn_write(&store, &c, "x\ny\nz\n", u2).await;
    commit_at(&store, u2).await.unwrap();
    let r = list_at(&store).await.unwrap();
    let txns = r["txns"].as_array().unwrap();
    let newest = txns.iter().max_by_key(|t| t["txn_id"].as_u64().unwrap()).unwrap();
    assert_eq!(
        newest["summary"].as_str().unwrap(),
        "new.rs created (+3 lines)"
    );
}

/// 空栈 undo/redo = no-op，不报错（IDE 语义），skipped 恒在。
#[tokio::test]
async fn empty_stack_is_noop() {
    let store = tmpdir("empty_store");
    let r = undo_at(&store, 1).await.unwrap();
    assert_eq!(r["undone"].as_array().unwrap().len(), 0);
    assert_eq!(r["skipped"].as_array().unwrap().len(), 0);
    let r = redo_at(&store).await.unwrap();
    assert_eq!(r["redone"].as_array().unwrap().len(), 0);
    assert_eq!(r["skipped"].as_array().unwrap().len(), 0);
}

/// recorded_write 无事务上下文（uid=0）退化为裸写且不入栈。
#[tokio::test]
async fn bare_write_outside_scope_skips_ledger() {
    let work = tmpdir("bare_work");
    let store = tmpdir("bare_store");
    let a = work.join("a.txt");
    recorded_write(&a, "direct").await.unwrap(); // 无 scope → uid=0
    commit_at(&store, 0).await.unwrap();
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
    txn_write(&store, &a, &format!("v2{big}"), u).await;
    commit_at(&store, u).await.unwrap();

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
    let t = tokio::spawn({
        let store = store.clone();
        async move { undo_at(&store, 1).await }
    });
    let _ = crate::write_gate::acquire("test").await; // 与 undo_at 抢门
    drop(crate::write_gate::acquire("test").await);
    t.await.unwrap().unwrap();
}

/// 时间无泄漏哨兵：单测套件整体墙钟上界（防未来加入真实 sleep/超长 IO）。
#[tokio::test]
async fn suite_runs_fast() {
    let start = Instant::now();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(start.elapsed() < Duration::from_secs(5));
}

/// P2-b：undo/redo 整事务恢复成功 → TOUCHED 登记（created 标志保真）→
/// take_touched 取走即清空；uid 作用域外 take 返回空（--direct 路径）。
#[tokio::test]
async fn touched_files_registered_on_restore_and_drained_by_take() {
    let work = tmpdir("touched_work");
    let store = tmpdir("touched_store");
    let a = work.join("a.txt");
    std::fs::write(&a, "v1").unwrap();
    let b = work.join("b.txt"); // created 文件（写入前不存在）
    let u = uid();
    txn_write(&store, &a, "v2", u).await;
    txn_write(&store, &b, "created-content", u).await;
    commit_at(&store, u).await.unwrap();

    // 模拟 execute_tool：undo 工具调用在 TXN_UID 作用域内执行，收口同作用域取走。
    let undo_uid = uid();
    let touched = TXN_UID
        .scope(undo_uid, async {
            undo_at(&store, 1).await.unwrap();
            take_touched()
        })
        .await;
    assert_eq!(touched.len(), 2, "both files registered: {touched:?}");
    let created = touched
        .iter()
        .find(|t| t.created)
        .expect("created flag preserved");
    assert!(created.path.ends_with("b.txt"), "{touched:?}");
    // 取走即清空：二次 take = 空（daemon 长跑不泄漏）。
    let drained = TXN_UID.scope(undo_uid, async { take_touched() }).await;
    assert!(drained.is_empty(), "registry drained after take");

    // redo 重放同样登记。
    let redo_uid = uid();
    let touched = TXN_UID
        .scope(redo_uid, async {
            redo_at(&store).await.unwrap();
            take_touched()
        })
        .await;
    assert_eq!(touched.len(), 2, "redo re-registers: {touched:?}");

    // 作用域外 take（--direct / 无 uid）：恒空，不误取他事务登记。
    assert!(take_touched().is_empty(), "out-of-scope take is empty");
}

/// audit 竞锁 #10 / 内存 F8：execute_tool future 取消（drop 未收口）→ TxnGuard
/// 兜底 abort，OPEN_TXNS/TOUCHED 的 uid 条目不滞留、WAL 目录被删。
#[test]
fn txn_guard_drop_without_settle_cleans_open_and_touched() {
    let store = tmpdir("guard_store");
    let dir = store.join("txn-999");
    std::fs::create_dir_all(&dir).unwrap();
    let u = uid();
    OPEN_TXNS
        .lock()
        .expect("open")
        .push((u, dir.clone()));
    TOUCHED.lock().expect("touched").push((
        u,
        TouchedFile {
            path: "Z:/no/such/x.txt".into(),
            created: false,
        },
    ));
    {
        let _txn = TxnGuard::new(u);
        assert!(
            OPEN_TXNS.lock().unwrap().iter().any(|(v, _)| *v == u),
            "entry alive while guard held"
        );
    } // drop 未 settle → 兜底 abort
    assert!(
        !OPEN_TXNS.lock().unwrap().iter().any(|(v, _)| *v == u),
        "OPEN_TXNS must be cleaned on cancel path"
    );
    assert!(!dir.exists(), "WAL dir removed on cancel path");
    assert!(
        !TOUCHED.lock().unwrap().iter().any(|(v, _)| *v == u),
        "TOUCHED must be cleaned on cancel path"
    );
}

/// settle（commit / 显式 abort 已收口）后 drop 不再动账目——守卫只兜底取消路径，
/// 不重复处置正常路径的账本。
#[test]
fn txn_guard_settled_drop_is_noop() {
    let store = tmpdir("guard_settle_store");
    let dir = store.join("txn-998");
    std::fs::create_dir_all(&dir).unwrap();
    let u = uid();
    OPEN_TXNS.lock().unwrap().push((u, dir.clone()));
    let txn = TxnGuard::new(u);
    txn.settle();
    drop(txn);
    assert!(
        OPEN_TXNS.lock().unwrap().iter().any(|(v, _)| *v == u),
        "settled guard must not touch entries"
    );
    assert!(dir.exists(), "settled drop must not delete WAL dir");
    abort(u); // 清场，不给其他用例留垃圾
}
