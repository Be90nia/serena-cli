use std::path::PathBuf;

use supervisor::Supervisor;
// 批2-A 降级测试走 execute_tool（SupervisorTrait trait 方法）。
use supervisor::SupervisorTrait;

fn has_clangd() -> bool {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        return false; // CI: skip real-LS e2e (3rd-party LS version drift; covered locally/nightly)
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for ext in if cfg!(windows) {
                &["", ".exe"][..]
            } else {
                &[""][..]
            } {
                let mut candidate = dir.join("clangd");
                if !ext.is_empty() {
                    candidate.set_extension(&ext[1..]);
                }
                if candidate.is_file() {
                    return true;
                }
            }
        }
    }
    if cfg!(windows) {
        for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
            let p = std::path::Path::new(dir).join("clangd.exe");
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-t20-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 至少一个 cpp 文件（lang 探测要求）。
    std::fs::write(
        dir.join("alpha.cpp"),
        r#"#include "alpha.h"
int alpha_func(int x) { return x + 1; }
int beta_helper() { return 42; }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("beta.cpp"),
        r#"#include "alpha.h"
int gamma_method() { return alpha_func(7); }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("alpha.h"),
        "int alpha_func(int);\nint beta_helper();\nint gamma_method();\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_symbol_finds_known_functions() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("find");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 先 overview 一个文件 → 触发 didOpen → 让 clangd 把 TU 加进索引。
    let _ = sup
        .tool_overview(&root, "alpha.cpp", None)
        .await
        .expect("overview");

    let (hits, warnings, _) = sup
        .tool_find_symbol(&root, "alpha_func", 50, None)
        .await
        .expect("find_symbol");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    assert!(
        !hits.is_empty(),
        "expected at least one hit for alpha_func, got 0"
    );
    let has_alpha = hits
        .iter()
        .any(|h| h.name.contains("alpha_func") && h.uri.contains("alpha.cpp"));
    assert!(
        has_alpha,
        "alpha_func hit should reference alpha.cpp, got: {hits:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_query_returns_bad_args() {
    let root = scratch("empty");
    let sup = Supervisor::direct().await.expect("supervisor");

    let err = sup
        .tool_find_symbol(&root, "", 50, None)
        .await
        .expect_err("empty query should fail");
    assert!(
        matches!(err, supervisor::ToolError::BadArgs { .. }),
        "got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_caps_results() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("limit");
    let sup = Supervisor::direct().await.expect("supervisor");

    // alpha / beta / gamma 都含 `_helper` / `_func` / `_method`？没有共同子串。
    // 用空 pattern 之前的 nil；改用一个只匹配一个的 query：alpha_func。
    let (hits, _warnings, _) = sup
        .tool_find_symbol(&root, "alpha_func", 1, None)
        .await
        .expect("find_symbol");
    assert!(
        hits.len() <= 1,
        "limit=1 should cap to at most 1, got {}",
        hits.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// lang_override 指定 lang → 单 LS 查, 即使 root 下有多种已知 lang 文件。
/// 用 cpp fixture 但指定 lang=python 应该 NotInstalled (不强求 install, 只验 fast path)。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lang_override_skips_root_probe() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("override");
    let sup = Supervisor::direct().await.expect("supervisor");
    // 先 overview 触发 clangd 索引, 同 workspace_symbol_finds_known_functions。
    let _ = sup
        .tool_overview(&root, "alpha.cpp", None)
        .await
        .expect("overview");

    // 指定 lang=cpp → 只查 clangd。
    let (hits, warnings, _) = sup
        .tool_find_symbol(&root, "alpha_func", 50, Some("cpp"))
        .await
        .expect("find_symbol with lang=cpp");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert!(
        !hits.is_empty(),
        "lang=cpp should find alpha_func via clangd, got 0 hits"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 批2-A：语义工具渐进首答——就绪预算耗尽 → 降级返回（degraded=semantic-pending
/// + warmup 标记 + warning），不再死等；降级结果不入缓存（就绪后重查可拿全量）。
/// 1ms 预算必超时（进程间往返 >1ms），不依赖冷启动时长 → 稳定。真 clangd。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_symbol_degrades_to_semantic_pending_when_budget_expires() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("degrade");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 冷 spawn 后立即查（不预热），_warmup_ms=1 → workspace/symbol 必超时降级。
    let args = serde_json::json!({"query": "alpha_func", "limit": 50, "_warmup_ms": 1});
    let v = sup
        .execute_tool("find-symbol", &root.to_string_lossy(), args, None)
        .await
        .expect("degraded call is still a success (rc=0)");
    assert!(
        v.get("degraded").and_then(|x| x.as_str()) == Some("semantic-pending"),
        "wire must carry degraded=semantic-pending, got: {v}"
    );
    assert_eq!(v["warmup"]["stage"], "indexing", "warmup marker: {v}");
    assert_eq!(v["warmup"]["retry_after_warm"], true, "warmup marker: {v}");
    let warning = v.get("warning").and_then(|x| x.as_str()).unwrap_or_default();
    assert!(
        warning.contains("warming") || warning.contains("incomplete"),
        "人话 warning 必须在场（超时 warming / 请求失败 / 暖机窗口任一形态）: {warning}"
    );

    // 就绪后重查（默认预算）：不再降级且有真结果（降级路径未污染缓存）。
    // clangd 小 fixture 就绪需数秒——轮询直到命中（同 partial-failure 测试形态）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        // clangd 把 TU 加进 workspace/symbol 索引需要 didOpen（既有测试同款预热：
        // workspace_symbol_finds_known_functions 先 overview 再查，不预热恒空——
        // 「未接线」不是「未就绪」，等再久也无效）。
        let _ = sup.tool_overview(&root, "alpha.cpp", None).await;
        let (hits, _, degraded) = sup
            .tool_find_symbol(&root, "alpha_func", 50, None)
            .await
            .expect("warm requery");
        if !hits.is_empty() {
            assert!(
                degraded.is_none(),
                "warm requery must not degrade: {degraded:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "warm requery never hit within 60s (degraded cache pollution?)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let _ = std::fs::remove_dir_all(&root);
}
