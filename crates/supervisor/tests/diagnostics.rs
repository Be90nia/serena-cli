//! diagnostics 工具 e2e：走真实 clangd，故意写错代码 → 断言诊断非空。

use supervisor::Supervisor;

fn has_clangd() -> bool {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("clangd.exe");
            if p.is_file() {
                return true;
            }
        }
    }
    for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
        let p = std::path::Path::new(dir).join("clangd.exe");
        if p.is_file() {
            return true;
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_returns_error_on_bad_code() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-diag-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 类型错误：`const char*` 与 int 不兼容 → clangd 必报错。
    std::fs::write(dir.join("bad.cpp"), "const char* s = 12345;\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    let diag = sup
        .tool_diagnostics(&dir, "bad.cpp", None, None)
        .await
        .expect("diagnostics");

    let items = diag
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !items.is_empty(),
        "expected at least 1 diagnostic, got: {diag}"
    );
    // 紧凑格式（2026-09-23）：items 是单行字符串（`[error] L1:14 type mismatch: ...`），
    // 非 LSP 原始 JSON 对象。
    let msg = items[0].as_str().unwrap_or("");
    assert!(!msg.is_empty(), "expected non-empty message, got: {msg}");
    assert!(
        msg.starts_with('[') && msg.contains('L'),
        "compact format expected `[sev] L..:.. message`, got: {msg}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_returns_empty_on_clean_code() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-diag-clean-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("ok.cpp"), "int main() { return 0; }\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    let diag = sup
        .tool_diagnostics(&dir, "ok.cpp", None, None)
        .await
        .expect("diagnostics");

    let items = diag
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        items.is_empty(),
        "expected 0 diagnostics on clean code, got: {diag}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_wait_gen_zero_returns_immediately() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-diag-wgen0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ok.cpp"), "int main() { return 0; }\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    // wait_gen=0 → 立即返回：响应形态合法（{ items: [...] }），不 panic、不超时。
    let diag = sup
        .tool_diagnostics(&dir, "ok.cpp", None, Some(0))
        .await
        .expect("diagnostics");
    assert!(
        diag.get("items").and_then(|i| i.as_array()).is_some(),
        "expected items array, got: {diag}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_wait_gen_at_current_returns_immediately_with_items() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-diag-wgencur-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // 类型错误 → clangd 必推送非空 items，generation 必然 > 0。
    std::fs::write(dir.join("bad.cpp"), "const char* s = 12345;\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    // 先以默认路径触发 publish，建立 generation + items 缓存。
    let _ = sup
        .tool_diagnostics(&dir, "bad.cpp", None, None)
        .await
        .expect("seed diagnostics");
    let cur = sup.diag_generation();
    assert!(cur > 0, "expected generation > 0 after seed, got {cur}");

    // wait_gen = 当前 generation → 立即通过。
    let start = std::time::Instant::now();
    let diag = sup
        .tool_diagnostics(&dir, "bad.cpp", None, Some(cur))
        .await
        .expect("wait-gen-at-current");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "wait_gen=cur should return fast, took {:?}",
        start.elapsed()
    );
    let items = diag
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!items.is_empty(), "expected items from cache, got: {diag}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_wait_gen_huge_times_out_with_empty_items() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-diag-wgentoo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ok.cpp"), "int main() { return 0; }\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    // wait_gen = u64::MAX → 不可能到达 → 必超时（5s 上限），返 {items:[]} 不 panic。
    let start = std::time::Instant::now();
    let diag = sup
        .tool_diagnostics(&dir, "ok.cpp", None, Some(u64::MAX))
        .await
        .expect("diagnostics");
    let elapsed = start.elapsed();
    assert!(
        elapsed <= std::time::Duration::from_millis(6_500),
        "wait_gen=huge should hit 5s timeout, took {:?}",
        elapsed
    );
    let items = diag
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        items.is_empty(),
        "expected empty items on timeout (clean file), got: {diag}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
