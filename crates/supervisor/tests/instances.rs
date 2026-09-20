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

/// 回归（BD serena-rust-81m）：Key.root 必须保留 canonical 真实大小写。
/// 小写化的 root 会作为 rootUri/cwd 传给 rust-analyzer，与其 didOpen 的原始
/// 大小写 URI 不匹配 → 文件脱挂所有 crate → 语义层恒 null。
#[test]
fn key_root_keeps_real_case_canonical_path() {
    let root = workspace_root().join("fixtures/cpp_demo");
    let k1 = Supervisor::key(&root, "cpp");
    assert_eq!(
        k1.root,
        dunce::canonicalize(&root).unwrap(),
        "Key.root 必须是 canonical 真实大小写，而非小写化副本"
    );
    // Windows 大小写不敏感 FS：变体路径 canonicalize 归一到同一真实大小写；
    // 类 Unix 上 canonicalize 失败走原样 fallback，靠 Key::eq 的小写归一保持相等。
    let mixed = root.parent().unwrap().join("Cpp_Demo");
    let k2 = Supervisor::key(&mixed, "cpp");
    assert_eq!(k1, k2, "大小写变体路径必须映射到同一 Key");
}

/// 大小写变体（仅大小写不同的两个真实存在路径）必须落到同一 map 槽：
/// Key 的 Hash/Eq 按 root 小写归一，instances/load_gates 等调用点零改动。
#[cfg(windows)]
#[test]
fn key_case_variants_share_hash_bucket() {
    use std::collections::HashSet;
    let root = workspace_root().join("fixtures/cpp_demo");
    let variant = root.parent().unwrap().join("CPP_DEMO");
    let mut set = HashSet::new();
    set.insert(Supervisor::key(&root, "cpp"));
    assert!(
        set.contains(&Supervisor::key(&variant, "cpp")),
        "大小写变体 Key 必须命中同一 HashSet 槽（Hash 归一生效）"
    );
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
