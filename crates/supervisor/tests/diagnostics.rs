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
    let diag = sup.tool_diagnostics(&dir, "bad.cpp").await.expect("diagnostics");

    let items = diag.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    assert!(
        !items.is_empty(),
        "expected at least 1 diagnostic, got: {diag}"
    );
    let msg = items[0].get("message").and_then(|m| m.as_str()).unwrap_or("");
    assert!(
        !msg.is_empty(),
        "expected non-empty message, got: {msg}"
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
    let diag = sup.tool_diagnostics(&dir, "ok.cpp").await.expect("diagnostics");

    let items = diag.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    assert!(
        items.is_empty(),
        "expected 0 diagnostics on clean code, got: {diag}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
