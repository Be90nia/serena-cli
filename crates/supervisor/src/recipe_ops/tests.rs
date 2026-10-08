//! recipe 批4 单测：入口分派校验（确定性参数错）、失败报告形态（纯函数）、
//! budget 词汇。真 LS 编排 e2e 见 local/agent-reports/recipe-b4.md（fixture
//! 语义就绪窗口不可控，真机/nightly 覆盖，同 warm/repo_map 惯例）。

use super::*;

async fn sup_direct() -> crate::Supervisor {
    // --direct 同款空 supervisor：分派校验不触 LS（错误在入口先于 session 拉起）。
    crate::Supervisor::direct().await.expect("direct supervisor")
}

fn skip_ls_e2e() -> bool {
    std::env::var_os("SERENA_SKIP_LS_E2E").is_some_and(|v| v == "1")
}

fn rust_analyzer_available() -> bool {
    let exe = if cfg!(windows) {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    };
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(exe).is_file())
    })
}

// ============ 入口分派（确定性 BAD_ARGS，不触 LS） ============

#[tokio::test]
async fn unknown_recipe_name_lists_available() {
    let sup = sup_direct().await;
    let root = std::env::temp_dir();
    let err = run(&sup, &root, &json!({"name": "nope"}))
        .await
        .expect_err("unknown name must fail");
    let crate::ToolError::BadArgs { detail } = err else {
        panic!("expect BadArgs, got {err:?}");
    };
    for r in [
        "fix-bug",
        "add-feature",
        "rename",
        "add-test",
        "refactor-extract",
        "refactor-rename",
        "review-diff",
        "explore",
    ] {
        assert!(detail.contains(r), "detail must list {r}: {detail}");
    }
}

#[tokio::test]
async fn missing_name_is_bad_args() {
    let sup = sup_direct().await;
    let err = run(&sup, &std::env::temp_dir(), &json!({}))
        .await
        .expect_err("missing name must fail");
    assert!(matches!(err, crate::ToolError::BadArgs { .. }), "{err:?}");
}

#[tokio::test]
async fn missing_positional_args_are_bad_args() {
    let sup = sup_direct().await;
    for name in ["fix-bug", "rename", "add-feature", "add-test", "refactor-extract", "refactor-rename", "explore"] {
        let err = run(&sup, &std::env::temp_dir(), &json!({"name": name, "pos": []}))
            .await
            .expect_err("missing pos must fail");
        let crate::ToolError::BadArgs { detail } = err else {
            panic!("{name}: expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("positional"), "{name}: {detail}");
    }
}

#[tokio::test]
async fn rename_without_to_flag_is_bad_args() {
    let sup = sup_direct().await;
    let dir = std::env::temp_dir().join("recipe_b4_no_to");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let err = run(&sup, &dir, &json!({"name": "rename", "pos": ["a.rs", "sym"]}))
        .await
        .expect_err("missing --to must fail");
    let crate::ToolError::BadArgs { detail } = err else {
        panic!("expect BadArgs, got {err:?}");
    };
    assert!(detail.contains("--to"), "{detail}");
}

// ============ 失败报告形态（protocol_fail 纯函数） ============

#[test]
fn fail_report_is_parseable_json_with_required_fields() {
    let err = protocol_fail(
        "add-feature",
        "write-stub",
        vec![
            ("ct_define_feature".to_string(), json!({"name": "f"})),
            ("write-stub".to_string(), json!({"file": "x.rs"})),
        ],
        vec![7, 9],
        vec![json!({"txn_id": 9, "undone_files": 1})],
        &crate::ToolError::BadArgs { detail: "injected".into() },
    );
    let crate::ToolError::Protocol { tool, reason } = err else {
        panic!("expect Protocol");
    };
    assert_eq!(tool, "recipe:add-feature");
    let report: Value = serde_json::from_str(&reason).expect("reason must be JSON");
    assert_eq!(report["failed_step"], "write-stub");
    assert_eq!(report["error"], "bad args: injected");
    assert_eq!(
        report["completed_steps"],
        json!(["ct_define_feature", "write-stub"])
    );
    assert_eq!(report["txn_ids"], json!([7, 9]));
    assert_eq!(report["undo_results"][0]["txn_id"], 9);
}

// ============ 失败中断 + 逆序 undo（真 LS：rust-analyzer fixture） ============

/// add-feature 在 stub 步注入失败（--target 指向非 .rs → append_or_create
/// 确定性 BAD_ARGS）→ 报告含已完成步（ct_define_feature）且 undo_results 空
/// 清单（步1 是读步，无 txn 可回滚——报告如实反映）。
#[tokio::test]
async fn add_feature_stub_failure_reports_completed_steps() {
    if skip_ls_e2e() {
        eprintln!("skip: SERENA_SKIP_LS_E2E=1");
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skip: rust-analyzer not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("recipe_b4_fail_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmpdir");
    std::fs::write(dir.join("lib.rs"), "pub fn seeded() {}\n").expect("fixture");
    let sup = sup_direct().await;
    let err = run(
        &sup,
        &dir,
        &json!({"name": "add-feature", "pos": ["fresh_symbol"], "target": "stub.txt"}),
    )
    .await
    .expect_err("stub step must fail (.rs gate)");
    let crate::ToolError::Protocol { reason, .. } = err else {
        panic!("expect Protocol, got {err:?}");
    };
    let report: Value = serde_json::from_str(&reason).expect("reason must be JSON");
    assert_eq!(report["failed_step"], "write-stub", "{report}");
    assert_eq!(
        report["completed_steps"],
        json!(["ct_define_feature"]),
        "{report}"
    );
    assert_eq!(report["txn_ids"], json!([]), "{report}");
    // 步1 是读步：无 txn 可回滚，undo_results 如实为空。
    assert_eq!(report["undo_results"], json!([]), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}
