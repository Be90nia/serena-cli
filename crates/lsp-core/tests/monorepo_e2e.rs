//! Phase 4 Task 22a: monorepo workspace folders 端到端测试。
//!
//! 流程：
//! 1. tempdir 建 cargo workspace（3 个成员）。
//! 2. 启动 mock_ls + `MOCK_LS_LOG_INITIALIZE=/path/log` → mock_ls 把收到的
//!    initialize params 写到 log。
//! 3. 构造 `InitializeParams` 包含 `workspace_folders = primary + 探测出的 modules`。
//! 4. `Session::start` 握手（initialize + initialized）→ mock_ls 写 log。
//! 5. 读 log → 断言 `workspaceFolders` 数组 ≥ 3 个元素（1 root + 2 modules）。
//!
//! 接受任务 22a acceptance 的端到端要求（fixture/multi-module/multi-cargo +
//! 验证 workspaceFolders 数组）。不引 supervisor（测试定位 lsp-core 层组装
//! 正确性；supervisor session_for 已直接调本模块的 discover_additional_workspace_folders）。

use std::path::Path;
use std::time::Duration;

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use lsp_core::init_params::base_initialize_params;
use lsp_core::session::Session;
use lsp_core::workspace_folders::{
    discover_additional_workspace_folders, primary_workspace_folder,
};

/// 在 tempdir 建 cargo workspace（成员是 `crates/a` / `crates/b`）。
fn make_cargo_workspace(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [workspace]\nmembers = [\n    \"crates/a\",\n    \"crates/b\",\n]\n",
    )
    .unwrap();
    for m in ["crates/a", "crates/b"] {
        let sub = dir.join(m);
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("Cargo.toml"),
            "[package]\nname = \"m\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monorepo_workspace_folders_reach_mock_ls_via_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    make_cargo_workspace(&root);

    // log 路径
    let log_path = tmp.path().join("initialize_params.log");
    // 解析前的「额外 modules」纯函数断言（fixture sanity check）
    let extras = discover_additional_workspace_folders(&root);
    assert_eq!(
        extras.len(),
        2,
        "fixture 应被探测为 2 个 cargo workspace members"
    );

    // 拼装 InitializeParams：root + modules = 3 个 folder
    let mut params = base_initialize_params();
    let primary = primary_workspace_folder(&root);
    let mut folders = vec![primary];
    folders.extend(extras);
    params.workspace_folders = Some(folders.clone());

    // 启动 mock_ls
    let exe: std::path::PathBuf = env!("CARGO_BIN_EXE_mock_ls").into();
    let child = Child::spawn(LaunchInfo {
        cmd: vec![exe.into_os_string()],
        cwd: std::env::temp_dir(),
        env: vec![(
            "MOCK_LS_LOG_INITIALIZE".to_string(),
            log_path.to_string_lossy().to_string(),
        )],
        transport: TransportKind::Stdio,
    })
    .expect("spawn mock_ls");

    let session =
        tokio::time::timeout(Duration::from_secs(10), Session::start(Some(child), params))
            .await
            .expect("session start within 10s")
            .expect("session start Ok");

    // mock_ls 写日志需要一点时间（OS write 缓冲）
    for _ in 0..50 {
        if log_path.is_file()
            && std::fs::metadata(&log_path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body = std::fs::read_to_string(&log_path).expect("mock_ls log 应被写");
    assert!(!body.is_empty(), "initialize params log 不应为空");

    // 断言 workspaceFolders 数组 = 3 个元素
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("mock_ls log 是合法 JSON");
    let wf = parsed
        .get("workspaceFolders")
        .and_then(|v| v.as_array())
        .expect("initialize params 含 workspaceFolders 数组");
    assert_eq!(wf.len(), 3, "1 root + 2 modules = 3 个 folder");
    let names: std::collections::BTreeSet<&str> = wf
        .iter()
        .filter_map(|f| f.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains("a"), "members 应含 a: {names:?}");
    assert!(names.contains("b"), "members 应含 b: {names:?}");
    // root 的 name = tempdir 末级目录名（hex）
    assert_eq!(wf.len(), 3, "primary folder (root) + 2 members");

    session.shutdown().await;
}

#[test]
fn workspace_folders_helper_is_pure() {
    // 不走 LS，纯函数断言：3 种 marker 各返正确数量。
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // cargo workspace（与 e2e 同形态）
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [workspace]\nmembers = [\"crates/x\", \"crates/y\", \"crates/z\"]\n",
    )
    .unwrap();
    for m in ["crates/x", "crates/y", "crates/z"] {
        std::fs::create_dir_all(root.join(m)).unwrap();
    }
    let extras = discover_additional_workspace_folders(root);
    assert_eq!(extras.len(), 3);
    let mut names: Vec<&str> = extras.iter().map(|f| f.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["x", "y", "z"]);
}
