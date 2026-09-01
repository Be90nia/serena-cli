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
