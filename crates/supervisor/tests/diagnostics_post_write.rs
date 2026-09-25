//! F2 验收：写工具返回值挂 `post_write_diagnostics`（happy path 走真 LS）。
//!
//! 拉真 clangd，故意写错代码触发非空 diagnostics → 调 `replace-lines` 行级写工具
//! → 断言返回值含 `post_write_diagnostics` 数组且非空。
//!
//! 走 `replace-lines` 而非 `replace-body`：前者只需 file/line/content，不依赖符号解析
//! （mock_ls 的符号能力很弱，避开）。clangd 看到非法源码会推 diagnostics 错误。
//!
//! 与 `diagnostics.rs::diagnostics_returns_error_on_bad_code` 共用同一 fixture 模式
//! （类型错误 `const char* = int`）。无 clangd 即 skip（不构成 false failure）。

use std::path::PathBuf;

use supervisor::Supervisor;
use supervisor::SupervisorTrait;

fn has_clangd() -> bool {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        return false; // CI: skip real-LS e2e (3rd-party LS version drift; covered locally/nightly)
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir
                .join(if cfg!(windows) {
                    "clangd.exe"
                } else {
                    "clangd"
                })
                .is_file()
            {
                return true;
            }
        }
    }
    for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
        if PathBuf::from(dir)
            .join(if cfg!(windows) {
                "clangd.exe"
            } else {
                "clangd"
            })
            .is_file()
        {
            return true;
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_write_diagnostics_appears_in_tool_result() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("serena-f2-postwrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 类型错误 → clangd 必报错。
    std::fs::write(dir.join("bad.cpp"), "const char* s = 12345;\n").unwrap();

    let sup = Supervisor::direct().await.expect("supervisor");
    let args = serde_json::json!({
        "file": "bad.cpp",
        "start_line": 1u32,
        "end_line": 1u32,
        "content": "garbage_unmatched_brace_{{{"
    });

    // replace-lines 第 1 行替换为非法语法 → 必触发 clangd diagnostic。
    let result = sup
        .execute_tool("replace-lines", &dir.to_string_lossy(), args, None)
        .await
        .expect("replace-lines OK");

    let diags = result
        .get("post_write_diagnostics")
        .expect("post_write_diagnostics 必须存在");
    let items = diags
        .get("items")
        .and_then(|v| v.as_array())
        .expect("必须是 {items, pending} 快照");

    assert!(
        !items.is_empty(),
        "clangd 真触发错误，必须有非空 diagnostics；got: {result}"
    );
    assert_eq!(
        diags.get("pending").and_then(|v| v.as_bool()),
        Some(false),
        "clangd 已推送错误（非空 items）→ pending 必为 false；got: {result}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
