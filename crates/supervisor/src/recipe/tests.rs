//! recipe 批1 单测：两后端解析器（error/warn/fail 夹具）、unified diff patch
//! 格式断言、find-test 启发式、undo read_txn 数据源、预算截断。

use super::*;

// ============ rustc/cargo 解析器 ============

#[test]
fn rust_parser_covers_error_warn_and_counts() {
    let out = "\
error[E0308]: mismatched types
 --> src/main.rs:10:5
  |
10 |     let x: u8 = \"s\";
  |     ^^^^^^^^^
warning: unused variable: `y`
 --> src\\lib.rs:3:9
  |
3 |     let y = 1;
  |         ^
error: could not compile `demo` (bin \"demo\" test)
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
    let (failures, passed, failed) = parse_rust_output(out);
    assert_eq!(passed, 3);
    assert_eq!(failed, 0);
    assert_eq!(failures.len(), 2, "无 --> 的头部丢弃: {failures:?}");
    assert_eq!(failures[0]["file"], "src/main.rs");
    assert_eq!(failures[0]["line"], 10);
    assert_eq!(failures[0]["col"], 5);
    assert_eq!(failures[0]["msg"], "mismatched types");
    assert_eq!(failures[0]["level"], "error");
    // Windows 反斜杠路径归一。
    assert_eq!(failures[1]["file"], "src/lib.rs");
    assert_eq!(failures[1]["level"], "warning");
    assert_eq!(failures[1]["msg"], "unused variable: `y`");
}

#[test]
fn rust_parser_covers_panic_fail() {
    let out = "\
thread 'tests::my_test' panicked at src/vec.rs:12:5:
bad index
failures:
    tests::my_test
test result: FAILED. 1 passed; 2 failed; 0 ignored
";
    let (failures, passed, failed) = parse_rust_output(out);
    assert_eq!(passed, 1);
    assert_eq!(failed, 2);
    assert_eq!(failures[0]["file"], "src/vec.rs");
    assert_eq!(failures[0]["line"], 12);
    assert_eq!(failures[0]["col"], 5);
    assert_eq!(failures[0]["level"], "fail");
    assert_eq!(failures[0]["msg"], "panicked in `tests::my_test`");
}

#[test]
fn rust_parser_empty_output_is_zero_counts() {
    let (failures, passed, failed) = parse_rust_output("");
    assert!(failures.is_empty());
    assert_eq!((passed, failed), (0, 0));
}

// ============ npm/jest 解析器 ============

#[test]
fn npm_parser_covers_x_blocks_frames_and_summary() {
    let out = "\
 FAIL  src/app.test.js
  ● suite › renders

  ✕ renders (5 ms)
    at Object.<anonymous> (src/app.test.js:12:15)
    at Array.map (<anonymous>)

Tests:       1 failed, 2 passed, 3 total
";
    let (failures, passed, failed) = parse_npm_output(out);
    assert_eq!(passed, 2);
    assert_eq!(failed, 1);
    assert_eq!(failures.len(), 1, "每 ✕ 只取首个项目内栈帧: {failures:?}");
    assert_eq!(failures[0]["file"], "src/app.test.js");
    assert_eq!(failures[0]["line"], 12);
    assert_eq!(failures[0]["col"], 15);
    assert_eq!(failures[0]["level"], "fail");
    assert_eq!(failures[0]["msg"], "renders");
}

#[test]
fn npm_parser_drops_node_modules_frames() {
    let out = "\
✕ crashes
    at Object.<anonymous> (node_modules/jest-runtime/build/index.js:1:1)
";
    let (failures, _passed, _failed) = parse_npm_output(out);
    assert!(failures.is_empty(), "node_modules 帧不入清单: {failures:?}");
}

// ============ 位置/计数解析 ============

#[test]
fn rustc_loc_tolerates_drive_colon_and_trailing_colon() {
    assert_eq!(
        parse_rustc_loc("D:\\proj\\src\\a.rs:12:5"),
        Some(("D:/proj/src/a.rs".into(), 12, 5))
    );
    assert_eq!(
        parse_rustc_loc("src/a.rs:12:5:"),
        Some(("src/a.rs".into(), 12, 5))
    );
    assert_eq!(parse_rustc_loc("src/a.rs"), None);
    assert_eq!(parse_rustc_loc("src/a.rs:x:5"), None);
}

#[test]
fn count_before_parses_summary_segments() {
    assert_eq!(count_before(" ok. 10 passed", "passed"), Some(10));
    assert_eq!(count_before(" 2 failed", "failed"), Some(2));
    assert_eq!(count_before(" 0 filtered out", "failed"), None);
}

#[test]
fn is_test_path_covers_dirs_and_naming() {
    assert!(is_test_path("crates/ls-registry/tests/resolve.rs"));
    assert!(is_test_path("src/foo_test.rs"));
    assert!(is_test_path("src/test_foo.rs"));
    assert!(is_test_path("app/foo.test.js"));
    assert!(is_test_path("app/bar.spec.ts"));
    assert!(is_test_path("__tests__/baz.js"));
    assert!(!is_test_path("src/latest/foo.rs"));
    assert!(!is_test_path("src/main.rs"));
}

#[test]
fn mirror_probes_hit_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("tests")).unwrap();
    std::fs::write(dir.path().join("tests/foo.rs"), "t").unwrap();
    std::fs::write(dir.path().join("tests/bar.test.js"), "t").unwrap();
    let probes = mirror_probes(dir.path(), "foo");
    assert_eq!(probes, vec!["tests/foo.rs".to_string()]);
    let probes = mirror_probes(dir.path(), "bar");
    assert_eq!(probes, vec!["tests/bar.test.js".to_string()]);
    let probes = mirror_probes(dir.path(), "nope");
    assert!(probes.is_empty());
}

// ============ unified diff（patch 格式断言） ============

#[test]
fn unified_diff_modified_file_exact_format() {
    let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    let after = before.replace("l5", "L5 changed");
    let d = unified_diff("f.txt", before, after.as_str(), false);
    let expected = "\
--- a/f.txt
+++ b/f.txt
@@ -2,7 +2,7 @@
 l2
 l3
 l4
-l5
+L5 changed
 l6
 l7
 l8
";
    assert_eq!(d, expected);
}

#[test]
fn unified_diff_created_file_uses_dev_null() {
    let d = unified_diff("new.txt", "", "a\nb\n", true);
    assert_eq!(
        d,
        "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n"
    );
}

#[test]
fn unified_diff_no_newline_marker() {
    let d = unified_diff("f.txt", "x\ny", "x\ny\nz", false);
    assert!(
        d.contains("+z\n\\ No newline at end of file\n"),
        "末行无换行要发标记: {d}"
    );
}

#[test]
fn unified_diff_eof_only_change_still_consumable() {
    // 仅行尾换行差异：退整体替换 hunk（不许产出无 hunk 的裸头 patch）。
    let d = unified_diff("f.txt", "x", "x\n", false);
    assert!(d.contains("@@"), "必须有 hunk: {d}");
    // 标记跟随其修饰的行：旧末行 x 无换行 → 标记插在 -x 之后。
    assert!(d.contains("-x\n\\ No newline at end of file\n+x\n"), "{d}");
}

#[test]
fn unified_diff_identical_is_empty() {
    assert_eq!(unified_diff("f.txt", "a\n", "a\n", false), "");
}

#[test]
fn unified_diff_multi_hunk_groups_far_changes() {
    // 20 行中改第 1 行和第 20 行 → 两组 hunk（间隔 > 2×context）。
    let before: String = (1..=20).map(|i| format!("line{i}\n")).collect();
    let after = before.replace("line1", "LINE1").replace("line20", "LINE20");
    let d = unified_diff("f.txt", before.as_str(), after.as_str(), false);
    assert_eq!(d.matches("@@ ").count(), 2, "远距变更断组: {d}");
    assert!(d.starts_with("--- a/f.txt\n+++ b/f.txt\n"));
}

#[test]
fn diff_line_stats_counts_minus_plus() {
    let d = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+c\n";
    assert_eq!(diff_line_stats(d), (1, 1));
}

// ============ 预算截断 ============

#[test]
fn truncate_list_field_cuts_and_reports_original() {
    let mut v = json!({"passed": 1, "failures": (0..100).map(|i| json!({"msg": format!("m{i}")})).collect::<Vec<_>>()});
    assert!(truncate_list_field(&mut v, "failures", 200));
    assert_eq!(v["truncated"], true);
    assert_eq!(v["original_count"], 100);
    let kept = v["failures"].as_array().unwrap().len();
    assert!((1..100).contains(&kept), "截短但非空: {kept}");
    assert_eq!(v["passed"], 1, "非 list 字段不动");
}

#[test]
fn truncate_list_field_noop_within_budget() {
    let mut v = json!({"failures": [json!({"msg": "m"})]});
    assert!(!truncate_list_field(&mut v, "failures", 10_000));
    assert!(v.get("truncated").is_none());
}

#[test]
fn max_fit_lines_keeps_whole_lines() {
    let text = "aaa\nbbb\nccc\nddd\n";
    // 2 行 join = 7B（+64 标志余量 = 71 ≤ 72）；3 行 = 11B（+64 = 75 > 72）。
    assert_eq!(max_fit_lines(text, 72), 2);
    assert_eq!(max_fit_lines(text, 500), 4);
    assert!(max_fit_lines(text, 8) >= 1, "保底 1 行");
}

// ============ undo store 数据源（read_txn_at） ============

/// 唯一 store 目录（进程内计数器避免并行撞名）。
fn tmpstore(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "serena_recipe_{label}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_txn_dir(store: &Path, name: &str, n: u64, files: &str) {
    let dir = store.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        format!(r#"{{"txn_id":{n},"timestamp":1700000000,"files":{files}}}"#),
    )
    .unwrap();
}

#[tokio::test]
async fn read_txn_default_picks_top_active() {
    let store = tmpstore("top");
    make_txn_dir(&store, "txn-3", 3, r#"[{"path":"D:/w/a.rs","created":false,"before":"old","after":"new","after_sha256":"x"}]"#);
    make_txn_dir(&store, "txn-1", 1, r#"[{"path":"D:/w/b.rs","created":true,"before":null,"after":"n","after_sha256":"y"}]"#);
    let snap = crate::undo::read_txn_at(&store, None).await.unwrap();
    assert_eq!(snap.txn_id, 3, "缺省 = N 最大的活跃事务");
    assert_eq!(snap.files[0].before.as_deref(), Some("old"));
}

#[tokio::test]
async fn read_txn_explicit_id_reads_undone_and_side_files() {
    let store = tmpstore("undone");
    make_txn_dir(&store, "txn-2", 2, r#"[{"path":"D:/w/a.rs","created":false,"before":null,"after":"new","after_sha256":"x","before_file":"before/0"}]"#);
    std::fs::create_dir_all(store.join("txn-2/before")).unwrap();
    std::fs::write(store.join("txn-2/before/0"), "old-from-side").unwrap();
    make_txn_dir(&store, "undone-1", 1, r#"[{"path":"D:/w/c.rs","created":true,"before":null,"after":"n","after_sha256":"z"}]"#);
    // 显式 id 可读 undone-*（复盘已回滚事务）。
    let snap = crate::undo::read_txn_at(&store, Some(1)).await.unwrap();
    assert!(snap.files[0].created);
    assert_eq!(snap.files[0].before, None);
    // 旁路文件读回。
    let snap = crate::undo::read_txn_at(&store, Some(2)).await.unwrap();
    assert_eq!(snap.files[0].before.as_deref(), Some("old-from-side"));
}

#[tokio::test]
async fn read_txn_errors_are_bad_args() {
    let store = tmpstore("err");
    make_txn_dir(&store, "txn-5", 5, r#"[]"#);
    let e = crate::undo::read_txn_at(&store, Some(9)).await.unwrap_err();
    assert!(matches!(e, ToolError::BadArgs { .. }), "{e:?}");
    let empty = tmpstore("empty");
    let e = crate::undo::read_txn_at(&empty, None).await.unwrap_err();
    assert!(
        matches!(&e, ToolError::BadArgs { detail } if detail.contains("no undo transactions")),
        "{e:?}"
    );
}

// ============ 后端选择 ============

#[test]
fn resolve_target_dir_and_file_forms() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    std::fs::create_dir_all(dir.path().join("crates/sub/tests")).unwrap();
    std::fs::write(dir.path().join("crates/sub/tests/it.rs"), "#[test]\nfn t() {}\n").unwrap();

    // 目录 → cargo test
    let t = resolve_target(dir.path(), ".", None).unwrap();
    match &t {
        TestTarget::Cargo { args, .. } => assert_eq!(args, &["test"]),
        _ => panic!("dir with Cargo.toml → cargo: {t:?}"),
    }
    // 集成测试文件 → --test 精准化 + name 过滤。
    let t = resolve_target(dir.path(), "crates/sub/tests/it.rs", Some("it_case")).unwrap();
    match &t {
        TestTarget::Cargo { args, .. } => {
            assert_eq!(args, &["test", "--test", "it", "it_case"])
        }
        _ => panic!("tests/*.rs → cargo --test: {t:?}"),
    }
    // 不认识的扩展名 = 参数错（go 等后端留扩展点）。
    let e = resolve_target(dir.path(), "crates/sub/tests/it.go", None).unwrap_err();
    assert!(matches!(e, ToolError::BadArgs { .. }), "{e:?}");
    // 不存在的路径 = 参数错。
    let e = resolve_target(dir.path(), "nope/nope.rs", None).unwrap_err();
    assert!(matches!(e, ToolError::BadArgs { .. }), "{e:?}");
}
