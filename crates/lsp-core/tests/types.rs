use lsp_core::types::*;

#[test]
fn symbol_kind_maps_number() {
    assert!(matches!(SymbolKindTag::from_lsp(6), SymbolKindTag::Method));
    assert!(matches!(
        SymbolKindTag::from_lsp(99),
        SymbolKindTag::Other(99)
    ));
}

#[test]
fn symbol_hit_serializes_flat() {
    let hit = SymbolHit {
        name: "main".into(),
        kind: SymbolKindTag::Function,
        uri: "file:///p/main.cpp".into(),
        range: Default::default(),
        container: None,
    };
    let j = serde_json::to_string(&hit).unwrap();
    assert!(j.contains("\"name\":\"main\""));
}

/// bd P2-2：kind wire 统一字符串形态——收窄变体给专名，未收窄 LSP 线号给上游
/// 名字（不再 `{"Other":13}` enum 泄漏），表外号码回退 `Other(n)`。
#[test]
fn symbol_kind_serializes_as_plain_string() {
    assert_eq!(
        serde_json::to_value(SymbolKindTag::Function).unwrap(),
        serde_json::json!("Function")
    );
    assert_eq!(
        serde_json::to_value(SymbolKindTag::Other(23)).unwrap(),
        serde_json::json!("Struct")
    );
    assert_eq!(
        serde_json::to_value(SymbolKindTag::Other(13)).unwrap(),
        serde_json::json!("Variable")
    );
    assert_eq!(
        serde_json::to_value(SymbolKindTag::Other(99)).unwrap(),
        serde_json::json!("Other(99)")
    );
}
