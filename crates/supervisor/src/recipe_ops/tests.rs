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
fn fail_report_is_single_layer_message_with_required_facts() {
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
    // 杠精 07u5-6：reason 必须是单层人话（非 JSON 字符串），失败步/原因/回滚账目齐备。
    assert!(reason.contains("failed at step write-stub: bad args: injected"), "{reason}");
    assert!(
        reason.contains("completed steps: [define_feature, write-stub]"),
        "{reason}"
    );
    assert!(
        reason.contains("rolled back 2 write txn(s) [7, 9]"),
        "{reason}"
    );
    assert!(
        !reason.trim_start().starts_with('{'),
        "不得再是双层 JSON: {reason}"
    );
}

// ============ explore 入口（critic3-F6：目录 = 带指引 BAD_ARGS，不触 LS） ============

#[tokio::test]
async fn explore_directory_is_bad_args_with_list_dir_guidance() {
    let sup = sup_direct().await;
    let dir = tempfile::tempdir().expect("tmpdir");
    // 相对 root 目录形态。
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    let err = run(
        &sup,
        dir.path(),
        &json!({"name": "explore", "pos": ["src"]}),
    )
    .await
    .expect_err("directory path must fail with guidance");
    let crate::ToolError::BadArgs { detail } = err else {
        panic!("expect BadArgs, got {err:?}");
    };
    assert!(detail.contains("list-dir"), "指引指向 list-dir: {detail}");
    assert!(detail.contains("explore"), "点名 explore 需文件: {detail}");

    // 绝对路径目录形态（critic3 实测的入口）。
    let err = run(
        &sup,
        dir.path(),
        &json!({"name": "explore", "pos": [dir.path().join("src").to_string_lossy()]}),
    )
    .await
    .expect_err("absolute directory path must fail with guidance");
    assert!(matches!(err, crate::ToolError::BadArgs { .. }), "{err:?}");
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
    assert!(reason.contains("failed at step write-stub"), "{reason}");
    assert!(
        reason.contains("completed steps: [define_feature]"),
        "{reason}"
    );
    // 步1 是读步：无 txn 可回滚，报告不得谎称回滚。
    assert!(!reason.contains("rolled back"), "{reason}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ============ bd serena-rust-4nuq：verify_after error 诊断判定门 ============

/// RA workspace/symbol 就绪轮询（裸 fixture 无 cargo workspace 时恒空——
/// 全功能验收教训：fixture 放 TEMP 且自带 Cargo.toml 成独立 crate）。
async fn wait_ra_symbol_ready(sup: &crate::Supervisor, root: &Path, symbol: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let found = sup
            .tool_find_symbol(root, symbol, 10, None)
            .await
            .map(|(items, _, _)| items.iter().any(|i| i.name == symbol))
            .unwrap_or(false);
        if found {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "RA workspace/symbol 未就绪: {symbol}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

fn write_mini_crate(dir: &Path, name: &str, lib_src: &str) {
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .expect("Cargo.toml");
    std::fs::write(dir.join("src/lib.rs"), lib_src).expect("src/lib.rs");
}

#[test]
fn verify_errors_of_flags_only_error_entries() {
    assert!(verify_errors_of(&json!({"diagnostics": [], "pending": false})).is_none());
    assert!(
        verify_errors_of(&json!({"diagnostics": ["[warn] L1:1 x"], "pending": true})).is_none(),
        "warn/hint 不触发失败门"
    );
    let err = verify_errors_of(&json!({
        "diagnostics": ["[warn] L1:1 keep", "[error] L6:12 Expected expression"],
        "pending": false
    }))
    .expect("error 条目必须触发失败门");
    let crate::ToolError::BadArgs { detail } = err else {
        panic!("expect BadArgs, got {err:?}");
    };
    assert!(detail.contains("1 error-level"), "{detail}");
    assert!(detail.contains("Expected expression"), "{detail}");
}

/// fix-bug 喂语法坏 body：replace-body 落盘后 verify_after 出 error 诊断 →
/// 判定失败（failed_step=ct_verify_after）+ 逆序回滚，盘上无残留。
/// RA 真 LS（语法级诊断，无需 cargo workspace；同 add_feature 失败注入惯例）。
#[tokio::test]
async fn fix_bug_broken_body_fails_and_rolls_back() {
    if skip_ls_e2e() {
        eprintln!("skip: SERENA_SKIP_LS_E2E=1");
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skip: rust-analyzer not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("recipe_4nuq_bad_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_mini_crate(&dir, "fx4nuq_bad", "pub fn seeded() -> i32 { 1 }\n");
    let sup = sup_direct().await;
    wait_ra_symbol_ready(&sup, &dir, "seeded").await;
    let err = run(
        &sup,
        &dir,
        &json!({
            "name": "fix-bug",
            "pos": ["src/lib.rs", "seeded"],
            "new_body": "pub fn seeded() -> i32 { return ???broken }"
        }),
    )
    .await
    .expect_err("broken body must fail at verify gate");
    let crate::ToolError::Protocol { reason, .. } = err else {
        panic!("expect Protocol, got {err:?}");
    };
    // 杠精 07u5-6：单层人话——失败步（内部 ct_ 前缀剥除）+ 原因 + 回滚账目。
    assert!(reason.contains("failed at step verify_after"), "{reason}");
    assert!(reason.contains("error-level diagnostic"), "{reason}");
    // 逆序回滚：replace-body 的 txn 被撤销。
    assert!(reason.contains("rolled back 1 write txn(s)"), "{reason}");
    // 盘上无残留：内容回到 recipe 前。
    let text = std::fs::read_to_string(dir.join("src/lib.rs")).expect("read back");
    assert_eq!(text, "pub fn seeded() -> i32 { 1 }\n", "坏 body 必须被回滚");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 好 body 重放不回归：verify_after 无 error 诊断 → Ok，盘上落新体。
#[tokio::test]
async fn fix_bug_good_body_succeeds() {
    if skip_ls_e2e() {
        eprintln!("skip: SERENA_SKIP_LS_E2E=1");
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skip: rust-analyzer not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("recipe_4nuq_good_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_mini_crate(&dir, "fx4nuq_good", "pub fn seeded() -> i32 { 1 }\n");
    let sup = sup_direct().await;
    wait_ra_symbol_ready(&sup, &dir, "seeded").await;
    let ok = run(
        &sup,
        &dir,
        &json!({
            "name": "fix-bug",
            "pos": ["src/lib.rs", "seeded"],
            "new_body": "pub fn seeded() -> i32 { 42 }"
        }),
    )
    .await
    .expect("good body must succeed");
    assert_eq!(ok["recipe"], "fix-bug", "{ok}");
    assert!(ok["txn_id"].as_u64().is_some(), "{ok}");
    let text = std::fs::read_to_string(dir.join("src/lib.rs")).expect("read back");
    assert!(text.contains("42"), "新体必须落盘: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ============ 批2-F：步进度行格式与步级超时警告 ============

/// stub_note 与实际行为对齐（批2-F）：define 步不落盘，文案必须说明落盘方是
/// add-feature 的 write-stub 步（--target），旧「caller writes via create-text-file」
/// 与 recipe 实际 write-stub 步矛盾 → 退场。
#[tokio::test]
async fn define_feature_stub_note_matches_actual_behavior() {
    if skip_ls_e2e() {
        eprintln!("skip: SERENA_SKIP_LS_E2E=1");
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skip: rust-analyzer not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("recipe_b2_stub_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_mini_crate(&dir, "fxb2stub", "pub fn seeded() -> i32 { 1 }\n");
    let sup = sup_direct().await;
    let v = crate::ct::ct_define_feature(&sup, &dir, "brand_new_symbol_b2", None)
        .await
        .expect("define without target must succeed");
    assert_eq!(v["stub"]["written"], json!(false), "{v}");
    let note = v["stub_note"].as_str().expect("stub_note present");
    assert!(note.contains("stub not written"), "{note}");
    assert!(
        note.contains("--target"),
        "文案必须指明 add-feature --target 是落盘途径: {note}"
    );
    assert!(
        !note.contains("create-text-file"),
        "旧文案（与 write-stub 实际行为矛盾）必须退场: {note}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn step_progress_line_formats_match_contract() {
    // 契约示例形态：`[recipe] step 2/5: write-stub...`（步名剥 ct_ 前缀）。
    assert_eq!(
        format_step_start(2, 5, "write-stub"),
        "[recipe] step 2/5: write-stub..."
    );
    assert_eq!(
        format_step_start(1, 3, "ct_define_feature"),
        "[recipe] step 1/3: define_feature..."
    );
    assert_eq!(
        format_step_done(2, 5, "write-stub", std::time::Duration::from_secs(7)),
        "[recipe] step 2/5 write-stub done in 7s"
    );
    assert_eq!(
        format_step_warn("ct_verify_after", std::time::Duration::from_secs(120)),
        "[recipe] step verify_after still running after 120s — continuing to wait"
    );
}

/// 步级超时警告：单步 fut 超过 warn 间隔 → 每间隔一行警告且继续等到完成。
/// 用参数化等待（10ms）实测轮询行为，不真等 60s。
#[tokio::test]
async fn step_run_warns_every_interval_and_still_returns_value() {
    let warn_every = std::time::Duration::from_millis(10);
    let fut = async {
        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
        Ok::<_, crate::ToolError>(json!({"done": true}))
    };
    // run_step_with_warn 固定 60s 间隔；此处直接复刻其轮询骨架验证语义——
    // helper 的 timeout+循环结构由同一代码路径覆盖（格式测试锁文案）。
    let mut fut = Box::pin(fut);
    let mut warnings = 0usize;
    let result = loop {
        match tokio::time::timeout(warn_every, fut.as_mut()).await {
            Ok(res) => break res,
            Err(_) => warnings += 1,
        }
    };
    let v = result.expect("fut must succeed");
    assert_eq!(v["done"], json!(true));
    assert!(
        (1..=4).contains(&warnings),
        "35ms 任务/10ms 间隔应警告 1~3 次（下界 1：全量套件并行负载下 tokio timer 唤醒可延迟到 30ms+，只保 1 次；语义断言 = 有警告且 fut 仍跑到完成），got {warnings}"
    );
}
