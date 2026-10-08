//! ct 批2 单测：预算裁剪与 porcelain 解析（纯函数）、undo store 派生
//! last-edited（fs 夹具）、真 LS 用例走 SERENA_SKIP_LS_E2E 门禁（同
//! warm/repo_map 惯例：runner 语义就绪窗口不可控，真机/nightly 覆盖）。

use super::*;

use std::sync::atomic::{AtomicU32, Ordering};

// ============ 夹具 ============

/// 唯一临时目录（进程内计数器避免并行撞名）。
fn tmpdir(label: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ct_b2_{label}_{}",
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmpdir");
    dir
}

/// 手工 txn 目录（undo::commit_at 落盘 schema 的解析必需子集：
/// txn_id/timestamp/files[].path）。
fn make_txn(store: &std::path::Path, n: u64, ts: u64, paths: &[&str]) {
    let dir = store.join(format!("txn-{n}"));
    std::fs::create_dir_all(&dir).expect("txn dir");
    let files: Vec<Value> = paths
        .iter()
        .map(|p| json!({ "path": p }))
        .collect();
    std::fs::write(
        dir.join("manifest.json"),
        json!({ "txn_id": n, "timestamp": ts, "files": files }).to_string(),
    )
    .expect("manifest");
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

/// 本仓库根（crates/supervisor 上两级）。
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

// ============ 预算裁剪（AC① 单测半边：超限 truncated+original_count） ============

#[test]
fn tldr_budget_trims_head_then_hits() {
    let fat_head: String = (0..200)
        .map(|i| format!("line {i}: {}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    let fat_hits: Vec<Value> = (0..80)
        .map(|i| {
            json!({"file": format!("tests/t{i}.rs"), "line": 1, "col": 1, "source": "mirror"})
        })
        .collect();
    let mut env = json!({
        "file": "lib.rs",
        "symbol_count": 2,
        "symbols_top": [json!({"name": "a", "kind": "Function", "line": 1})],
        "find_test_symbol": "a",
        "find_test": { "symbol": "a", "hits": fat_hits },
        "head": fat_head,
    });
    fit_tldr(&mut env);
    assert!(
        env_len(&env) <= TLDR_BUDGET_TOKENS * 4,
        "serialized {} > {}",
        env_len(&env),
        TLDR_BUDGET_TOKENS * 4
    );
    // head 缩行旗（original_count = 原行数）。
    assert_eq!(env["truncated"], json!(true));
    assert_eq!(env["original_count"], json!(200));
    assert_eq!(env["head"], json!(""), "head 语义优先级最低，先清空");
    // hits 体量超骨架余量 → 二段截断旗落在 find_test 内。
    assert_eq!(env["find_test"]["truncated"], json!(true));
}

#[test]
fn tldr_within_budget_untouched() {
    let mut env = json!({
        "file": "a.rs",
        "symbol_count": 1,
        "symbols_top": [],
        "find_test": { "symbol": "a", "hits": [] },
        "head": "fn main() {}",
    });
    fit_tldr(&mut env);
    assert!(env.get("truncated").is_none());
    assert_eq!(env["head"], json!("fn main() {}"));
}

#[test]
fn clamp_json_strings_clamps_all_strings() {
    let mut v = json!({ "a": "y".repeat(300), "b": ["z".repeat(300), 1] });
    clamp_json_strings(&mut v, 50);
    assert_eq!(v["a"].as_str().unwrap().chars().count(), 51); // 50 + '…'
    assert_eq!(v["b"][0].as_str().unwrap().chars().count(), 51);
    assert_eq!(v["b"][1], json!(1));
}

#[test]
fn file_stem_variants() {
    assert_eq!(file_stem("src/lib.rs"), "lib");
    assert_eq!(file_stem(r"a\b\c.test.ts"), "c");
    assert_eq!(file_stem("Makefile"), "Makefile");
}

// ============ git porcelain 解析（纯函数） ============

#[test]
fn git_log_porcelain_parses_records() {
    let raw = "\x01abc123\t1700000000\tfix: handle nil\nM\tsrc/lib.rs\n\n\
               \x01def456\t1700000100\trename mod\nR100\told.rs\tnew.rs\nA\tadded.rs\n";
    let got = parse_git_log_porcelain(raw);
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0]["sha"], json!("abc123"));
    assert_eq!(got[0]["epoch"], json!(1_700_000_000u64));
    assert_eq!(got[0]["subject"], json!("fix: handle nil"));
    assert_eq!(got[0]["files"][0]["status"], json!("M"));
    assert_eq!(got[0]["files"][0]["path"], json!("src/lib.rs"));
    assert_eq!(got[1]["files"][0]["path"], json!("old.rs"));
    assert_eq!(got[1]["files"][0]["path2"], json!("new.rs"));
    assert_eq!(got[1]["files"][1]["status"], json!("A"));
}

#[test]
fn git_log_porcelain_tolerates_malformed() {
    // 空头记录跳过；epoch 非数字容忍为 0，不 panic。
    let got = parse_git_log_porcelain("\x01\nM\tx.rs\n\x01nosha\tnotanum\tsub\n");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["epoch"], json!(0));
    assert_eq!(got[0]["files"], json!([]));
}

// ============ ct_recent_activity：store 派生（AC④） ============

#[tokio::test]
async fn recent_activity_contains_written_file_and_dedups() {
    let dir = tmpdir("activity");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    make_txn(&store, 1, 100, &["D:/abs/a.rs", "D:/abs/b.rs"]);
    make_txn(&store, 2, 200, &["D:/abs/a.rs"]); // a.rs 再写 → 只留最新
    // 已回滚事务不算编辑。
    let undone = store.join("undone-3");
    std::fs::create_dir_all(&undone).unwrap();
    std::fs::write(
        undone.join("manifest.json"),
        json!({"txn_id": 3, "timestamp": 300, "files": [{"path": "D:/abs/z.rs"}]}).to_string(),
    )
    .unwrap();

    let env = ct_recent_activity_at(&store, &dir, 10)
        .await
        .expect("activity");
    let edited = env["last_edited"].as_array().unwrap();
    assert_eq!(edited.len(), 2, "{edited:?}");
    // 写后 last-edited 含该文件且最新在前（AC④）。
    assert_eq!(edited[0]["path"], json!("D:/abs/a.rs"));
    assert_eq!(edited[0]["txn_id"], json!(2));
    assert_eq!(edited[1]["path"], json!("D:/abs/b.rs"));
    assert!(!edited.iter().any(|e| e["path"] == json!("D:/abs/z.rs")));
    // git_root 非 repo → git_log 空 + 显式 skipped（不静默）。
    assert!(env["git_log"].as_array().unwrap().is_empty());
    assert!(env["git_log_skipped"].is_string());
}

#[tokio::test]
async fn recent_activity_counts_corrupt_manifests() {
    let dir = tmpdir("activity_corrupt");
    let store = dir.join("store");
    std::fs::create_dir_all(store.join("txn-1")).unwrap();
    std::fs::write(store.join("txn-1").join("manifest.json"), "{not json").unwrap();
    make_txn(&store, 2, 200, &["D:/abs/ok.rs"]);
    let env = ct_recent_activity_at(&store, &dir, 10).await.unwrap();
    assert_eq!(env["manifest_skipped"], json!(1));
    assert_eq!(env["last_edited"][0]["path"], json!("D:/abs/ok.rs"));
}

#[tokio::test]
async fn recent_activity_store_missing_is_empty() {
    let dir = tmpdir("activity_empty");
    let env = ct_recent_activity_at(&dir.join("no_store"), &dir, 10).await.unwrap();
    assert!(env["last_edited"].as_array().unwrap().is_empty());
    assert!(env.get("manifest_skipped").is_none());
}

// ============ 真 LS e2e（SERENA_SKIP_LS_E2E 门禁外，真机/nightly 覆盖） ============

/// AC②+③：TEMP 独立 workspace 夹具（防 workspace 收编语义死）——坏文件返
/// 诊断清单、干净文件返空 + format_ok:true；guarded_join refs 非空。
#[tokio::test]
async fn verify_and_impact_fixture_e2e() {
    if skip_ls_e2e() {
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skipped: rust-analyzer not on PATH");
        return;
    }
    let root = tmpdir("fixture");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"ct_b2_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[lib]\npath = \"lib.rs\"\n\n[[bin]]\nname = \"ct_b2_broken\"\npath = \"broken.rs\"\n",
    )
    .unwrap();
    // rustfmt 规整源码（format_ok:true 依赖此）。
    std::fs::write(
        root.join("lib.rs"),
        "pub fn guarded_join(root: &str, file: &str) -> String {\n    format!(\"{root}/{file}\")\n}\n\npub fn render(root: &str, file: &str) -> String {\n    guarded_join(root, file)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("broken.rs"),
        "fn main() {\n    let x: i32 = \"not a number\";\n    let _ = x;\n}\n",
    )
    .unwrap();

    let sup = crate::Supervisor::direct().await.expect("supervisor");
    let ready = crate::warm::warm(&sup, &root, "rust", Duration::from_secs(120))
        .await
        .expect("warm");
    assert_eq!(ready["ready"], json!(true), "warm report: {ready}");

    // AC② 坏文件：轮询到诊断清单非空（RA 分析有窗口期）。
    let mut broken = ct_verify(&sup, &root, "broken.rs", None)
        .await
        .expect("verify broken");
    for _ in 0..30 {
        if !broken["diagnostics"].as_array().unwrap_or(&Vec::new()).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        broken = ct_verify(&sup, &root, "broken.rs", None).await.expect("verify broken");
    }
    assert!(
        !broken["diagnostics"].as_array().unwrap().is_empty(),
        "坏文件应返诊断清单: {broken}"
    );

    // AC② 干净文件：诊断空 + format_ok:true（rustfmt 缺席环境显式 skip，
    // format_ok 缺席不冒充断言）。
    let clean = ct_verify(&sup, &root, "lib.rs", None).await.expect("verify clean");
    assert!(
        clean["diagnostics"].as_array().unwrap().is_empty(),
        "干净文件诊断应为空: {clean}"
    );
    if clean.get("format_skipped").is_none() {
        assert_eq!(clean["format_ok"], json!(true), "{clean}");
    }

    // AC③ guarded_join refs 非空（声明 + render 内调用）。
    let mut impact = ct_impact(&sup, &root, "guarded_join").await.expect("impact");
    for _ in 0..30 {
        if impact["refs_count"].as_u64().unwrap_or(0) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        impact = ct_impact(&sup, &root, "guarded_join").await.expect("impact");
    }
    assert!(
        impact["refs_count"].as_u64().unwrap_or(0) >= 2,
        "guarded_join refs 应非空: {impact}"
    );
    assert_eq!(impact["definition"]["file"], json!("lib.rs"), "{impact}");

    // AC①：ct_tldr 序列化 ≤ 800 tok 估算（fixture 真管线口径；本仓库真文件
    // 的重型口径见下方 #[ignore] 的 real-repo 变体，扰动窗口外手动跑）。
    let tldr = ct_tldr(&sup, &root, "lib.rs").await.expect("ct_tldr");
    let tldr_len = env_len(&tldr);
    assert!(
        tldr_len <= TLDR_BUDGET_TOKENS * 4,
        "serialized {tldr_len} > {}",
        TLDR_BUDGET_TOKENS * 4
    );
    assert!(tldr["symbol_count"].as_u64().unwrap() >= 1, "{tldr}");
    assert!(tldr["head"].as_str().unwrap().lines().count() >= 1, "{tldr}");
}

/// AC① 真仓库口径：path_guard.rs 实测 ≤ 800 tok。重型（RA 冷启动 + 全仓
/// workspace）：并行波次编辑工作树时与其它 RA 实例/ cargo 锁互等会假挂死
/// （SweepA1 两轮独立复现），降为 `#[ignore]` 手动档——静默机器上跑
/// `cargo test -p supervisor --lib ct:: -- --ignored`；首轮实测 85s 绿。
#[ignore]
#[tokio::test]
async fn tldr_budget_on_real_repo_file_e2e() {
    if skip_ls_e2e() {
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skipped: rust-analyzer not on PATH");
        return;
    }
    let sup = crate::Supervisor::direct().await.expect("supervisor");
    let root = repo_root();
    let rel = "crates/supervisor/src/path_guard.rs";
    // 墙钟死线 + Err 容忍：并行波次编辑工作树时 RA 反复重索引，单次 overview
    // 可能卡满 30s 超时——固定轮数会放大成半小时级假挂死。死线尽未就绪 = 环境
    // 窗口不可控，显式跳过（同 SERENA_SKIP_LS_E2E 哲学），不冒充失败。
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut hits = Vec::new();
    while std::time::Instant::now() < deadline {
        match sup.tool_overview(&root, rel, None).await {
            Ok(h) if !h.is_empty() => {
                hits = h;
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => {
                eprintln!("overview not ready (RA churn?): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if hits.is_empty() {
        eprintln!("skipped: overview 240s 内未就绪（工作树并行编辑扰动窗口）");
        return;
    }
    let env = match tokio::time::timeout(Duration::from_secs(300), ct_tldr(&sup, &root, rel)).await
    {
        Ok(Ok(env)) => env,
        Ok(Err(e)) => panic!("ct_tldr failed: {e:?}"),
        Err(_) => {
            eprintln!("skipped: ct_tldr 300s 超时（工作树并行编辑扰动窗口）");
            return;
        }
    };
    let len = env_len(&env);
    assert!(
        len <= TLDR_BUDGET_TOKENS * 4,
        "serialized {len} bytes > {} ({} tokens)",
        TLDR_BUDGET_TOKENS * 4,
        TLDR_BUDGET_TOKENS
    );
    assert!(env["symbol_count"].as_u64().unwrap() >= 1, "{env}");
    assert!(!env["head"].as_str().unwrap().lines().count().lt(&1));
}

// ============ 批3 纯函数单测：签名解析 / 调用量重建 / stub / 事务收缩 ============

#[test]
fn locate_signature_name_finds_keyword_position() {
    // 普通签名。
    let item = "pub fn guarded_join(root: &str) -> String {\n    format!(\"x\")\n}\n";
    assert_eq!(locate_signature_name(item, "guarded_join"), Some((7, 0, 7)));
    // 文档注释先出现名字 → 跳过，命中签名。
    let doc = "/// uses fn guarded_join internally.\npub fn guarded_join() {}\n";
    let (off, line, col) = locate_signature_name(doc, "guarded_join").expect("sig hit");
    assert_eq!((line, col), (1, 7), "signature line, not doc line: off={off}");
    // 属性行提及 → 跳过。
    let attr = "#[doc = \"see guarded_join\"]\nfn guarded_join() {}\n";
    let (off, line, _) = locate_signature_name(attr, "guarded_join").expect("sig hit");
    assert_eq!(line, 1, "signature line, not attribute line: off={off}");
    // 体内调用不冒充签名（名字前不是 fn 关键字）。
    let call = "fn outer() {\n    guarded_join(\"a\")\n}\n";
    assert_eq!(locate_signature_name(call, "guarded_join"), None);
    // 同名嵌套 fn：首个命中 = 外层签名（"fn dup" 的名字在字节 3 / 列 3）。
    let nested = "fn dup() {\n    fn dup() {}\n}\n";
    assert_eq!(locate_signature_name(nested, "dup"), Some((3, 0, 3)));
    // 非 fn item → None。
    assert_eq!(locate_signature_name("pub struct Widget;\n", "Widget"), None);
    // 名字是更长标识符的子串 → 不命中。
    assert_eq!(locate_signature_name("fn guarded_join2() {}\n", "guarded_join"), None);
}

#[test]
fn split_fn_params_body_variants() {
    let item = "pub fn f(x: i32, y: &str) -> String {\n    x.to_string()\n}\n";
    let (params, body_open) = split_fn_params_body(item, 7).expect("params");
    assert_eq!(params, "x: i32, y: &str");
    assert_eq!(&item[body_open..body_open + 1], "{");
    // 泛型 + where。
    let g = "fn g<T: Clone>(x: T) -> T\nwhere\n    T: Debug,\n{\n    x\n}\n";
    let (params, _) = split_fn_params_body(g, 3).expect("generic params");
    assert_eq!(params, "x: T");
    // tuple struct：名字后无 `(` → None。
    assert_eq!(split_fn_params_body("struct W(u8);\n", 7), None);
    // trait 声明无 body → None（name_off 指向 `fn f();` 的 `f`）。
    assert_eq!(split_fn_params_body("trait T {\n    fn f();\n}\n", 17), None);
}

#[test]
fn param_bindings_rebuilds_call_args() {
    assert_eq!(
        param_bindings("root: &str, file: &str").unwrap(),
        ["root", "file"]
    );
    // self 三形态。
    assert_eq!(param_bindings("&self"), Some(vec!["self".to_string()]));
    assert_eq!(
        param_bindings("&mut self, x: i32"),
        Some(vec!["self".to_string(), "x".to_string()])
    );
    assert_eq!(param_bindings("mut self"), Some(vec!["self".to_string()]));
    // mut 前缀剥除。
    assert_eq!(param_bindings("mut n: i32"), Some(vec!["n".to_string()]));
    // 深度感知：impl Fn / 泛型内逗号不切分。
    assert_eq!(
        param_bindings("f: impl Fn(i32, i32) -> i32, m: HashMap<String, u8>").unwrap(),
        ["f", "m"]
    );
    // 解构形参 / `_` → None（调用侧无法重建）。
    assert_eq!(param_bindings("(a, b): (u8, u8)"), None);
    assert_eq!(param_bindings("_: u8"), None);
    assert_eq!(param_bindings(""), Some(Vec::<String>::new()));
}

#[test]
fn build_call_expr_self_and_free() {
    assert_eq!(build_call_expr("impl_f", &["root".into(), "file".into()]), "impl_f(root, file)");
    assert_eq!(build_call_expr("m", &["self".into(), "x".into()]), "self.m(x)");
    assert_eq!(build_call_expr("z", &[]), "z()");
}

#[test]
fn body_indent_keeps_file_style() {
    assert_eq!(body_indent("fn f() {\n    a();\n}\n", 8), "    ");
    assert_eq!(body_indent("fn f() {\n        deep();\n}\n", 8), "        ");
    // 单行空 body：首行 `}` 非空 → 缩进空串（原位调用表达式仍合法）。
    assert_eq!(body_indent("fn f() {}", 7), "");
}

#[test]
fn rust_stub_shape() {
    let s = rust_stub("fresh_thing");
    assert!(s.starts_with("pub fn fresh_thing() {"), "{s}");
    assert!(s.contains("todo!(\"implement fresh_thing\")"), "{s}");
    assert!(s.ends_with("}\n"), "{s}");
}

#[test]
fn unsupported_error_contract_shape() {
    let e = unsupported("extract", "`x` is not a function item".into());
    match e {
        ToolError::BadArgs { detail } => {
            assert!(detail.starts_with("language server does not support extract"), "{detail}");
        }
        other => panic!("expected BadArgs, got {other:?}"),
    }
}

#[test]
fn shrink_nested_list_trims_and_flags() {
    let fat: Vec<Value> = (0..60)
        .map(|i| json!({"file": format!("tests/t{i}.rs"), "line": 1, "col": 1, "source": "mirror"}))
        .collect();
    let mut env = json!({
        "txn_id": 1,
        "test": { "backend": "cargo", "passed": 1, "failed": 0, "failures": fat },
    });
    let budget = 900; // 故意小于信封实际体积
    shrink_nested_list(&mut env, "test", "failures", budget);
    assert!(env_len(&env) <= budget, "serialized {}", env_len(&env));
    assert_eq!(env["test"]["truncated"], json!(true));
    assert_eq!(env["test"]["original_count"], json!(60));
    assert!(env["test"]["failures"].as_array().unwrap().len() < 60);
    // 预算充足 → 不动。
    let mut env2 = json!({ "test": { "failures": [] } });
    shrink_nested_list(&mut env2, "test", "failures", 10_000);
    assert!(env2["test"].get("truncated").is_none());
}

// ============ 批3 真 LS e2e（SERENA_SKIP_LS_E2E 门禁外；单 fn 串行占位，禁拆并行） ============

/// AC①-⑥：fixture1（pick/goto_callers/rename/extract/error 路径）+ fixture2
/// （write_with_tests/review_diff/define_feature）。批2 教训：并行 RA + cargo
/// 锁互等会假挂死——全部真机步压进一个 fn，cargo test 线程模型下天然串行。
#[tokio::test]
async fn batch3_deep_six_fixture_e2e() {
    if skip_ls_e2e() {
        return;
    }
    if !rust_analyzer_available() {
        eprintln!("skipped: rust-analyzer not on PATH");
        return;
    }
    // ---- fixture1：纯读 + smart_edit（无 cargo，find_test 走 fs 探针）----
    let root = tmpdir("b3_edit");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"ct_b3_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[lib]\npath = \"lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("lib.rs"),
        "pub fn resolve_lang_name(id: &str) -> &str {\n    match id {\n        \"rs\" => \"rust\",\n        _ => id,\n    }\n}\n\n\
         pub fn resolve_lang(id: &str) -> &str {\n    resolve_lang_name(id)\n}\n\n\
         pub fn lang_name_hint(id: &str) -> String {\n    resolve_lang_name(id).to_string()\n}\n\n\
         pub fn guarded_join(root: &str, file: &str) -> String {\n    format!(\"{root}/{file}\")\n}\n\n\
         pub fn render(root: &str, file: &str) -> String {\n    guarded_join(root, file)\n}\n\n\
         pub struct Widget;\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/extract.rs"),
        "use ct_b3_demo::{guarded_join, hint_name, render};\n\n#[test]\nfn extracted_and_renamed_still_work() {\n    assert_eq!(guarded_join(\"a\", \"b\"), \"a/b\");\n    assert_eq!(render(\"x\", \"y\"), \"x/y\");\n    assert_eq!(hint_name(\"rs\"), \"rust\");\n}\n",
    )
    .unwrap();

    let sup = crate::Supervisor::direct().await.expect("supervisor");
    let ready = crate::warm::warm(&sup, &root, "rust", Duration::from_secs(120))
        .await
        .expect("warm");
    assert_eq!(ready["ready"], json!(true), "warm report: {ready}");

    // AC③ pick "lang name resolve" → 候选含 resolve_lang_name 且得分最高
    // （3 token 全命中；resolve_lang/lang_name_hint 各 2）。轮询 workspace/symbol 索引。
    let mut pick = ct_pick(&sup, &root, "lang name resolve").await.expect("pick");
    for _ in 0..30 {
        let has = pick["candidates"].as_array().is_some_and(|c| {
            c.iter().any(|x| x["name"] == json!("resolve_lang_name"))
        });
        if has {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        pick = ct_pick(&sup, &root, "lang name resolve").await.expect("pick");
    }
    let cands = pick["candidates"].as_array().expect("candidates");
    let top = cands
        .iter()
        .find(|c| c["name"] == json!("resolve_lang_name"))
        .expect("resolve_lang_name in candidates: {pick}");
    assert_eq!(top["score"], json!(3), "{pick}");
    assert_eq!(
        cands.first().map(|c| c["name"].clone()),
        Some(json!("resolve_lang_name")),
        "resolve_lang_name 应排首位: {pick}"
    );
    assert!(pick["candidates_count"].as_u64().unwrap() >= 1, "{pick}");

    // AC① goto_callers guarded_join：caller 集合 == {render}；与
    // find-referencing-symbols 直拉交叉对拍（同源 API，集合必须一致）。
    let def = def_hit(&sup, &root, "guarded_join").await.expect("def");
    // 轮询窗口内 workspace/symbol 未就绪会确定性返 not-found（Err）——重试而非 fail。
    let mut callers_env = match ct_goto_callers(&sup, &root, "guarded_join").await {
        Ok(env) => env,
        Err(_) => {
            tokio::time::sleep(Duration::from_secs(1)).await;
            ct_goto_callers(&sup, &root, "guarded_join").await.expect("goto_callers")
        }
    };
    for _ in 0..30 {
        // 等 render + 集成测试 target 双 caller（RA 对 tests/ 目标的索引更慢，
        // >=1 会提前退出漏掉测试侧引用）。
        if callers_env["callers_count"].as_u64().unwrap_or(0) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        callers_env = ct_goto_callers(&sup, &root, "guarded_join").await.expect("goto_callers");
    }
    assert_eq!(callers_env["definition"]["file"], json!("lib.rs"), "{callers_env}");
    // callers = render（同文件调用）+ extracted_and_renamed_still_work（集成测试
    // 调用）；定义点自身回显与 use 顶层导入（container 空）不算 caller。
    let mut got_callers: Vec<String> = callers_env["callers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    got_callers.sort();
    assert_eq!(
        got_callers,
        ["extracted_and_renamed_still_work", "render"],
        "{callers_env}"
    );
    // workspace/symbol location 即标识符位置（0-based 直传，同 ct_goto_callers）。
    // 第二返回值 = aap4 快照（对拍不消费）。
    let (direct, _) = sup
        .tool_referencing_symbols(&root, &def.file, def.line0, def.col0, None)
        .await
        .expect("direct referencing");
    let mut direct_set: Vec<(String, u32, u32, String)> = direct
        .iter()
        // RefSymbolHit 原始 0-based → 信封 1-based 同基线对拍。
        .map(|r| (r.file.clone(), r.line + 1, r.col + 1, r.container_name.clone()))
        .collect();
    direct_set.sort();
    let mut env_set: Vec<(String, u32, u32, String)> = callers_env["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["file"].as_str().unwrap().to_string(),
                r["line"].as_u64().unwrap() as u32,
                r["col"].as_u64().unwrap() as u32,
                r["container"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    env_set.sort();
    assert_eq!(env_set, direct_set, "AC① 交叉对拍失败: env={callers_env} direct={direct:?}");

    // AC② rename：真机 TEMP fixture → 符号+refs 同步。
    let renamed = ct_smart_edit(&sup, &root, "lib.rs", "lang_name_hint", "rename", "hint_name")
        .await
        .expect("rename");
    assert_eq!(renamed["transform"], json!("rename"), "{renamed}");
    assert!(renamed["files_modified"].as_u64().unwrap() >= 1, "{renamed}");
    assert_eq!(renamed["skipped"].as_array().map(Vec::len), Some(0), "{renamed}");
    let lib_text = std::fs::read_to_string(root.join("lib.rs")).unwrap();
    assert!(lib_text.contains("pub fn hint_name(id: &str)"), "{lib_text}");
    assert!(!lib_text.contains("lang_name_hint"), "{lib_text}");
    let overview = sup.tool_overview(&root, "lib.rs", None).await.expect("overview");
    assert!(overview.iter().any(|h| h.name == "hint_name"), "refs 未同步: {overview:?}");

    // AC② extract：guarded_join 整符号抽取 → 新 fn + 原位调用。
    let extracted = ct_smart_edit(
        &sup,
        &root,
        "lib.rs",
        "guarded_join",
        "extract",
        "guarded_join_impl",
    )
    .await
    .expect("extract");
    assert_eq!(extracted["transform"], json!("extract"), "{extracted}");
    let lib_text = std::fs::read_to_string(root.join("lib.rs")).unwrap();
    assert!(
        lib_text.contains("pub fn guarded_join_impl(root: &str, file: &str) -> String"),
        "新 fn 未出现: {lib_text}"
    );
    assert!(
        lib_text.contains("guarded_join_impl(root, file)"),
        "原位调用缺失: {lib_text}"
    );
    assert!(lib_text.contains("pub fn guarded_join(root: &str, file: &str) -> String"), "{lib_text}");

    // AC② 错误路径（R1）：能力外操作 → 确定性 BadArgs（码 + detail 形态）。
    let bad_transform = ct_smart_edit(&sup, &root, "lib.rs", "render", "refactor", "x")
        .await
        .expect_err("refactor must be rejected");
    match bad_transform {
        ToolError::BadArgs { detail } => assert!(detail.contains("transform must be"), "{detail}"),
        other => panic!("expected BadArgs, got {other:?}"),
    }
    let not_a_fn = ct_smart_edit(&sup, &root, "lib.rs", "Widget", "extract", "widget_impl")
        .await
        .expect_err("struct extract must be rejected");
    match not_a_fn {
        ToolError::BadArgs { detail } => assert!(
            detail.starts_with("language server does not support extract"),
            "{detail}"
        ),
        other => panic!("expected BadArgs, got {other:?}"),
    }

    // AC② test 后端过：rename+extract 后 fixture 编译且行为不变。
    let test = crate::recipe::run_test(&root, "tests/extract.rs", None).await.expect("run test");
    assert_eq!(test["raw_exit"], json!(0), "{test}");
    assert!(test["passed"].as_u64().unwrap() >= 1, "{test}");

    // ---- fixture2：write_with_tests / review_diff / define_feature（cargo）----
    let root2 = tmpdir("b3_wt");
    std::fs::write(
        root2.join("Cargo.toml"),
        "[package]\nname = \"ct_b3_wt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[lib]\npath = \"lib.rs\"\n",
    )
    .unwrap();
    // warm 需 root 下已有 rust 源文件；先落 stub（后续 write_with_tests 走
    // replace-lines 覆盖路径）。
    std::fs::write(
        root2.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    let ready2 = crate::warm::warm(&sup, &root2, "rust", Duration::from_secs(120))
        .await
        .expect("warm2");
    assert_eq!(ready2["ready"], json!(true), "warm2 report: {ready2}");

    // AC④ 好 test：写 fn + test → 绿；txn 只含写步（两文件，无 runner 产物）。
    let good = ct_write_with_tests(
        &sup,
        &root2,
        "lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        "tests/add.rs",
        "use ct_b3_wt::add;\n\n#[test]\nfn add_works() {\n    assert_eq!(add(1, 2), 3);\n}\n",
        None,
    )
    .await
    .expect("write_with_tests good");
    assert_eq!(good["test"]["raw_exit"], json!(0), "{good}");
    assert!(good["test"]["passed"].as_u64().unwrap() >= 1, "{good}");
    let t1 = good["txn_id"].as_u64().expect("txn_id");
    let snap1 = crate::undo::read_txn(&root2, Some(t1)).await.expect("txn snapshot");
    let mut paths: Vec<String> = snap1
        .files
        .iter()
        .map(|f| crate::recipe::rel_forward(&root2, &f.path))
        .collect();
    paths.sort();
    assert_eq!(paths, ["lib.rs", "tests/add.rs"], "txn 只含写步: {paths:?}");

    // AC⑥ review_diff(None) = 最近事务（good 写事务）→ diff + 关联 test + 跑测。
    let review = ct_review_diff(&sup, &root2, None).await.expect("review_diff none");
    assert_eq!(review["txn_id"], json!(t1), "{review}");
    assert_eq!(review["files"].as_array().unwrap().len(), 2, "{review}");
    assert!(review["test_skipped"].is_null() || review.get("test_skipped").is_none(), "{review}");
    assert!(review["test"]["passed"].as_u64().unwrap() >= 1, "{review}");
    assert!(
        review["related_tests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == json!("tests/add.rs")),
        "{review}"
    );

    // AC④ 坏 test：失败清单在报告里，txn 仍只含写步。
    let bad = ct_write_with_tests(
        &sup,
        &root2,
        "lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        "tests/broken.rs",
        "use ct_b3_wt::add;\n\n#[test]\nfn add_broken() {\n    assert_eq!(add(1, 2), 4);\n}\n",
        None,
    )
    .await
    .expect("write_with_tests broken");
    assert!(bad["test"]["failed"].as_u64().unwrap() >= 1, "{bad}");
    assert!(
        !bad["test"]["failures"].as_array().expect("failures").is_empty(),
        "失败清单缺失: {bad}"
    );
    let t2 = bad["txn_id"].as_u64().expect("txn_id2");
    let snap2 = crate::undo::read_txn(&root2, Some(t2)).await.expect("txn snapshot2");
    let mut paths2: Vec<String> = snap2
        .files
        .iter()
        .map(|f| crate::recipe::rel_forward(&root2, &f.path))
        .collect();
    paths2.sort();
    assert_eq!(paths2, ["lib.rs", "tests/broken.rs"], "坏 test txn 只含写步: {paths2:?}");

    // AC⑥ review_diff(Some(坏事务)) → diff + 失败清单聚合。
    let review_bad = ct_review_diff(&sup, &root2, Some(t2)).await.expect("review_diff some");
    assert_eq!(review_bad["txn_id"], json!(t2), "{review_bad}");
    assert!(review_bad["test"]["failed"].as_u64().unwrap() >= 1, "{review_bad}");
    assert!(
        !review_bad["test"]["failures"].as_array().expect("failures").is_empty(),
        "{review_bad}"
    );

    // AC⑤ define_feature 新名 → stub 落盘。
    let fresh = ct_define_feature(&sup, &root2, "fresh_thing", Some("lib.rs"))
        .await
        .expect("define_feature new");
    assert_eq!(fresh["exists"], json!(false), "{fresh}");
    assert!(fresh["txn_id"].as_u64().is_some(), "{fresh}");
    let lib2 = std::fs::read_to_string(root2.join("lib.rs")).unwrap();
    assert!(lib2.contains("pub fn fresh_thing() {"), "{lib2}");
    assert!(lib2.contains("todo!(\"implement fresh_thing\")"), "{lib2}");

    // AC⑤ define_feature 已有名 → caller 检索报告 + 不写 stub（轮询 RA 跨
    // target 索引 tests/add.rs 的引用）。
    let mut exists = ct_define_feature(&sup, &root2, "add", Some("lib.rs"))
        .await
        .expect("define_feature exists");
    for _ in 0..30 {
        if exists["callers_count"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        exists = ct_define_feature(&sup, &root2, "add", Some("lib.rs"))
            .await
            .expect("define_feature exists");
    }
    assert_eq!(exists["exists"], json!(true), "{exists}");
    assert!(exists["stub_skipped"].is_string(), "{exists}");
    assert!(
        exists["callers_count"].as_u64().unwrap_or(0) >= 1,
        "caller 检索报告缺失: {exists}"
    );
}
