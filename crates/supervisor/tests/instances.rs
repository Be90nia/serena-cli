use std::path::{Path, PathBuf};
use std::sync::Arc;

use supervisor::Supervisor;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn key_normalizes_case_and_trailing_separator() {
    let root = workspace_root().join("fixtures/cpp_demo");
    let mixed = root.parent().unwrap().join("Cpp_Demo");
    let k1 = Supervisor::key(&root, "cpp");
    let k2 = Supervisor::key(&mixed, "cpp");
    let k3 = Supervisor::key(&root.join(""), "cpp");
    assert_eq!(k1, k2);
    assert_eq!(k1, k3);
}

#[tokio::test]
async fn load_gate_is_per_key() {
    let root = workspace_root().join("fixtures/cpp_demo");
    let sup = Supervisor::direct().await.expect("supervisor direct init");
    let g1 = sup.load_gate_for(&root, "cpp");
    let g2 = sup.load_gate_for(&root, "cpp");
    let g3 = sup.load_gate_for(&root.join("math.h"), "cpp");
    assert!(Arc::ptr_eq(&g1, &g2), "同 key 应返回同一 gate");
    assert!(!Arc::ptr_eq(&g1, &g3), "不同 key 应不同 gate");
}
