//! hover 工具 e2e：走真实 clangd（mock_ls 不支持 hover）。

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
        let p = std::path::Path::new(dir).join("clangd.exe");
        if p.is_file() {
            return true;
        }
    }
    false
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-hover-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("demo.cpp"),
        "int answer = 42;\nint main() { return answer; }\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_returns_type_info_on_symbol() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("symbol");
    let sup = Supervisor::direct().await.expect("supervisor");
    // `answer` 在第 1 行（0-based line=0），col=4（0-based "answer" 起始）。
    let hover = sup
        .tool_hover(&root, "demo.cpp", 0, 4, None)
        .await
        .expect("hover");
    assert!(hover.is_some(), "expected Some(hover) on 'answer'");
    let hover = hover.unwrap();
    let text = match &hover.contents {
        lsp_types::HoverContents::Markup(m) => m.value.clone(),
        lsp_types::HoverContents::Scalar(s) => match s {
            lsp_types::MarkedString::String(t) => t.clone(),
            lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
        },
        lsp_types::HoverContents::Array(arr) => arr
            .iter()
            .map(|m| match m {
                lsp_types::MarkedString::String(s) => s.clone(),
                lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
            })
            .collect::<Vec<String>>()
            .join("\n"),
    };
    assert!(
        text.contains("int"),
        "expected 'int' in hover contents, got: {text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_on_whitespace_returns_none() {
    if !has_clangd() {
        println!("skipped: clangd not in PATH");
        return;
    }
    let root = scratch("empty");
    let sup = Supervisor::direct().await.expect("supervisor");

    // 文件末尾空行区域（line 3 超出 2 行内容 + 末尾 newline），无符号。
    let hover = sup
        .tool_hover(&root, "demo.cpp", 3, 0, None)
        .await
        .expect("hover");
    assert!(
        hover.is_none(),
        "expected None on whitespace, got: {:?}",
        hover.map(|h| h.contents)
    );

    let _ = std::fs::remove_dir_all(&root);
}
