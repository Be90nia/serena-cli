//! Shared LSP-domain types used by every tool output structure.

use serde::Serialize;

/// Flat symbol hit — the output currency of all symbol tools.
///
/// ↖ mirror: ls_types.py@43ae021 `UnifiedSymbolInformation` (field subset)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolHit {
    pub name: String,
    pub kind: SymbolKindTag,
    pub uri: String,
    pub range: lsp_types::Range,
    pub container: Option<String>,
}

/// Narrowed symbol kind: the kinds this project's tools care about survive;
/// everything else degrades to `Other` with the raw LSP number.
///
/// ↖ mirror: ls_types.py@43ae021 `UnifiedSymbolInformation` (field subset)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum SymbolKindTag {
    File,
    Module,
    Class,
    Method,
    Function,
    Field,
    Variable,
    Other(u8),
}

impl SymbolKindTag {
    /// Map an LSP `SymbolKind` wire number (LSP 3.17 §documentSymbol).
    pub fn from_lsp(kind: u8) -> Self {
        match kind {
            6 => Self::Method,
            12 => Self::Function,
            5 => Self::Class,
            other => Self::Other(other),
        }
    }
}
