//! completion 工具 e2e（M2 #1, design: local/completion-design.md §6）。
//!
//! 走真实 clangd（mock_ls 不支持 completion）：
//! - `add` 函数定义在 `math.h`；触发位置 line 5 col 13（1-based） = `add` 标识符起首
//!   位置（0-based line=4, col=12）→ clangd 至少回 1 条 `add` 候选。
//! - `limit`/`trigger`/`kind`/`doc` 字段裁剪在 supervisor 层完成。
//!
//! PATH 探测同 hover/e2e_concurrency：clangd.exe in PATH or LLVM 默认安装目录。
//! 缺 clangd 时 `println!("skipped: ...")` 并 early-return —— CI 上变 no-op，**不要**
//! `#[ignore]`（参考 hover.rs 注释）。

use std::path::{Path, PathBuf};

use supervisor::Supervisor;

fn has_clangd() -> bool {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        return false; // CI: skip real-LS e2e (3rd-party LS version drift; covered locally/nightly)
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("clangd.exe");
            if p.is_file() {
                return true;
            }
        }
    }
    for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
        let p = Path::new(dir).join("clangd.exe");
        if p.is_file() {
            return true;
        }
    }
    false
}

/// 拷贝 fixtures/cpp_demo 到临时目录 —— 每个测试独占一个（clangd 一次会话一个 root）。
fn scratch(tag: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("fixtures")
        .join("cpp_demo");
    let dir = std::env::temp_dir().join(format!("serena-completion-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    for entry in std::fs::read_dir(&src).expect("read fixtures") {
        let entry = entry.unwrap();
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).expect("copy fixture");
        }
    }
    dir
}

/// main.cpp 文件内容：
///     1: // main.cpp ...
///     2: #include "math.h"
///     3:
///     4: int main() {
///     5:     int s = add(1, 2);
///     6:     return s;
///     7: }
/// 触发位置：第 5 行（0-based=4）`add` 标识符起始 col（0-based=12，1-based=13）。
const TRIGGER_LINE: u32 = 4;
const TRIGGER_COL: u32 = 12;

/// 命中条件：候选里至少有一条 label 是 "add" 或 trimmed 后是 "add"。
/// clangd 在某些上下文里返回 `" add"`（前导空格，text kind）—— 取 trim 后比较。
fn has_add_label(items: &[supervisor::CompletionItemLite]) -> bool {
    items.iter().any(|i| i.label.trim() == "add")
}

/// 端到端：clangd 在 add 调用点返回 ≥1 条候选。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_returns_candidates_at_call_site() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("hit");
    let sup = Supervisor::direct().await.expect("supervisor");
    let resp = sup
        .tool_completion(&root, "main.cpp", TRIGGER_LINE, TRIGGER_COL, 0, None, None)
        .await
        .expect("completion call");
    assert!(
        has_add_label(&resp.items),
        "expected 'add' candidate; got labels: {:?}",
        resp.items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 字段裁剪：返回 items 含 label/kind/insert；不含 LSP 内部字段。
/// 静态部分（struct 定义）保证 sortText/filterText/commitCharacters 不在 struct 内；
/// 这里动态验证序列化结果干净（daemon / shell 透传形态）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_response_json_shape_is_ai_friendly() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("shape");
    let sup = Supervisor::direct().await.expect("supervisor");
    let resp = sup
        .tool_completion(&root, "main.cpp", TRIGGER_LINE, TRIGGER_COL, 3, None, None)
        .await
        .expect("completion call");
    assert!(!resp.items.is_empty(), "no items returned");
    let serialized = serde_json::to_value(&resp).expect("serialize");
    let items = serialized
        .get("items")
        .and_then(|v| v.as_array())
        .expect("items array");
    assert!(items.len() <= 3, "limit=3 must cap");
    for (i, item) in items.iter().enumerate() {
        assert!(item.get("label").is_some(), "items[{i}].label missing");
        assert!(item.get("kind").is_some(), "items[{i}].kind missing");
        assert!(item.get("insert").is_some(), "items[{i}].insert missing");
        // 丢弃字段不得出现：
        assert!(item.get("sortText").is_none(), "items[{i}].sortText leaked");
        assert!(
            item.get("filterText").is_none(),
            "items[{i}].filterText leaked"
        );
        assert!(
            item.get("commitCharacters").is_none(),
            "items[{i}].commitCharacters leaked"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// limit 默认 5：limit=3 → items.len() ≤ 3；若总候选 > 3，truncated 含 "3 of N"。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_limit_caps_results() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("limit");
    let sup = Supervisor::direct().await.expect("supervisor");
    let limit = 3_usize;
    let resp = sup
        .tool_completion(
            &root,
            "main.cpp",
            TRIGGER_LINE,
            TRIGGER_COL,
            limit,
            None,
            None,
        )
        .await
        .expect("completion call");
    assert!(
        resp.items.len() <= limit,
        "len {} > limit {limit}",
        resp.items.len()
    );
    // 截断提示存在 ⇔ 截断发生过；没截断时 None 是合法的（候选总数 ≤ limit）。
    if let Some(t) = &resp.truncated {
        let prefix = format!("{limit} of ");
        assert!(
            t.starts_with(&prefix),
            "truncated='{t}' doesn't start with '{prefix}'"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
