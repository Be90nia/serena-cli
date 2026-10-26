//! bd serena-rust-1282 版本账本端到端（无 LS 链路）。
//!
//! create-text-file 是唯一不依赖 LS 的写类工具（纯文件写 + undo 事务）——用它
//! 走通「写 → 版本账本 bump → expected_version 校验 → STALE_READ 拒绝/放行 →
//! 读类响应带 content_version → others_active 推导」全链。符号级写（replace-body
//! 等）的 STALE_READ 同一校验函数（recorded_write 单点），真 LS 路径由
//! local/fix-collab-version-stamp.md 的碰撞实验 e2e 覆盖。
//!
//! 测试 ×4：
//! 1. 写成功 bump + 响应带 content_version；多文件各归各账
//! 2. expected_version 过期 → STALE_READ（expected/current 结构化），文件未被改，
//!    undo 栈无新事务
//! 3. expected_version = 当前值 → 放行；未提供 → 放行（向后兼容）
//! 4. others_active：他人写过的文件进推导，无活动省略字段；read-file 读类响应带
//!    content_version

use serde_json::json;
use supervisor::Supervisor;
use supervisor::SupervisorTrait;
use supervisor::ToolError;

async fn sup() -> Supervisor {
    Supervisor::direct().await.expect("direct supervisor")
}

#[tokio::test]
async fn write_bumps_version_and_response_carries_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let s = sup().await;
    let r = s
        .execute_tool(
            "create-text-file",
            &root.to_string_lossy(),
            json!({"file": "a.txt", "content": "A1\n"}),
            None,
        )
        .await
        .expect("create a.txt");
    assert_eq!(r["content_version"], json!(1), "首写 = 1: {r}");
    let r2 = s
        .execute_tool(
            "create-text-file",
            &root.to_string_lossy(),
            json!({"file": "b.txt", "content": "B1\n"}),
            None,
        )
        .await
        .expect("create b.txt");
    assert_eq!(r2["content_version"], json!(1), "各归各账: {r2}");
    // 读类响应带 content_version（read-file 无 LS）
    let r3 = s
        .execute_tool(
            "read-file",
            &root.to_string_lossy(),
            json!({"file": "a.txt"}),
            None,
        )
        .await
        .expect("read a.txt");
    assert_eq!(r3["content_version"], json!(1), "读类响应带版本: {r3}");
}

#[tokio::test]
async fn stale_expected_rejects_without_touching_file_or_txn_stack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().to_string();
    let s = sup().await;
    s.execute_tool(
        "create-text-file",
        &root,
        json!({"file": "a.txt", "content": "BASE\n"}),
        None,
    )
    .await
    .expect("seed a.txt");
    // 回滚 seed（created 文件被删），账本仍 =1（版本单调，undo 不回退——防 ABA）
    s.execute_tool("undo", &root, json!({"steps": 1}), None)
        .await
        .expect("undo seed");
    assert!(!dir.path().join("a.txt").exists(), "undo 后文件应已删除");
    // agent 拿着旧读的 expected_version=0 重建 → STALE_READ（无 LS 的真校验链路：
    // exists 门通过（文件不存在）→ recorded_write 门内校验拦截）
    let err = s
        .execute_tool(
            "create-text-file",
            &root,
            json!({"file": "a.txt", "content": "OVERWRITE\n", "expected_version": 0}),
            None,
        )
        .await
        .expect_err("过期版本必须 STALE_READ 拒绝");
    match &err {
        ToolError::StaleRead {
            path,
            expected,
            current,
            changed_since,
        } => {
            assert_eq!(path, "a.txt");
            assert_eq!(*expected, 0);
            assert_eq!(*current, 1);
            assert!(
                changed_since.contains("create-text-file by tx-"),
                "changed_since 指认写者: {changed_since}"
            );
        }
        other => panic!("应为 StaleRead，得 {other:?}"),
    }
    // 文件未被建（拒绝即零写）
    assert!(
        !dir.path().join("a.txt").exists(),
        "STALE_READ 拒绝后文件不得存在"
    );
    // undo 栈无新事务（stale 事务已 abort）
    let list_before = s
        .execute_tool("undo", &root, json!({"list": true}), None)
        .await
        .expect("undo list");
    // 提供"正确 version"（=1）→ 放行重建
    let ok = s
        .execute_tool(
            "create-text-file",
            &root,
            json!({"file": "a.txt", "content": "REBUILT\n", "expected_version": 1}),
            None,
        )
        .await
        .expect("当前版本放行");
    assert_eq!(ok["content_version"], json!(2));
    let list_after = s
        .execute_tool("undo", &root, json!({"list": true}), None)
        .await
        .expect("undo list after");
    let before_txns = format!("{list_before:?}").matches("txn").count();
    let after_txns = format!("{list_after:?}").matches("txn").count();
    assert_eq!(
        before_txns, after_txns,
        "stale 拒绝不得入 undo 栈：before={list_before} after={list_after}"
    );
}

#[tokio::test]
async fn current_or_absent_expected_passes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().to_string();
    let s = sup().await;
    // 首建文件 expected_version=0（账本初始 0 = 当前）→ 放行
    let r = s
        .execute_tool(
            "create-text-file",
            &root,
            json!({"file": "a.txt", "content": "x\n", "expected_version": 0}),
            None,
        )
        .await
        .expect("expected_version=当前值放行");
    assert_eq!(r["content_version"], json!(1));
    // 未提供 → 照旧放行（向后兼容）
    let r2 = s
        .execute_tool(
            "create-text-file",
            &root,
            json!({"file": "b.txt", "content": "y\n"}),
            None,
        )
        .await
        .expect("未提供 expected_version 放行");
    assert_eq!(r2["content_version"], json!(1));
}

#[tokio::test]
async fn others_active_flows_to_write_response() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_string_lossy().to_string();
    let s = sup().await;
    s.execute_tool(
        "create-text-file",
        &root,
        json!({"file": "a.txt", "content": "A\n"}),
        None,
    )
    .await
    .expect("seed a.txt");
    // 写 b.txt 的响应里 others_active 应见到 a.txt（他人近期活动）
    let r = s
        .execute_tool(
            "create-text-file",
            &root,
            json!({"file": "b.txt", "content": "B\n"}),
            None,
        )
        .await
        .expect("create b.txt");
    let others = r["others_active"].as_array().expect("others_active in {r}");
    assert_eq!(others.len(), 1, "{others:?}");
    assert_eq!(others[0]["file"], json!("a.txt"));
    assert_eq!(others[0]["tool"], json!("create-text-file"));
    assert!(others[0]["age_s"].is_u64());
    assert!(others[0]["invocation"].as_str().unwrap().starts_with("tx-"));
    // 首写（无先前活动）→ 字段省略
    let dir2 = tempfile::tempdir().expect("tempdir2");
    let r2 = s
        .execute_tool(
            "create-text-file",
            &dir2.path().to_string_lossy(),
            json!({"file": "solo.txt", "content": "S\n"}),
            None,
        )
        .await
        .expect("create solo");
    assert!(
        r2.get("others_active").is_none(),
        "无他人活动必须省略字段: {r2}"
    );
}
