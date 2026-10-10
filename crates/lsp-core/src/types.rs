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
/// bd P2-2：wire 序列化统一字符串形态——默认 external-tagged 会把未收窄的 LSP
/// kind 序列化成 `{"Other":13}`（Rust enum 泄漏，与同响应里的 `"Function"` 字符串
/// 混用）。现统一：已知 LSP kind → 上游名字（"Struct"/"Variable"/...），未知号 →
/// `"Other(n)"`。反序列化方向无需求（本类型只出现在出站结构里），不实现
/// Deserialize。
///
/// ↖ mirror: ls_types.py@43ae021 `UnifiedSymbolInformation` (field subset)
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// LSP 3.17 SymbolKind 线号 → 上游名字；表外号码 → `Other(n)`。
    fn wire_name(n: u8) -> String {
        match n {
            1 => "File",
            2 => "Module",
            3 => "Namespace",
            4 => "Package",
            5 => "Class",
            6 => "Method",
            7 => "Property",
            8 => "Field",
            9 => "Constructor",
            10 => "Enum",
            11 => "Interface",
            12 => "Function",
            13 => "Variable",
            14 => "Constant",
            15 => "String",
            16 => "Number",
            17 => "Boolean",
            18 => "Array",
            19 => "Object",
            20 => "Key",
            21 => "Null",
            22 => "EnumMember",
            23 => "Struct",
            24 => "Event",
            25 => "Operator",
            26 => "TypeParameter",
            other => return format!("Other({other})"),
        }
        .to_string()
    }
}

impl Serialize for SymbolKindTag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let name = match self {
            Self::File => "File".to_string(),
            Self::Module => "Module".to_string(),
            Self::Class => "Class".to_string(),
            Self::Method => "Method".to_string(),
            Self::Function => "Function".to_string(),
            Self::Field => "Field".to_string(),
            Self::Variable => "Variable".to_string(),
            Self::Other(n) => Self::wire_name(*n),
        };
        serializer.serialize_str(&name)
    }
}
