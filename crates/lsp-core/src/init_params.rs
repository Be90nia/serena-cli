//! LSP `InitializeParams` 构造器（PLAN Task 6 / ARCHITECTURE §4.1）。
//!
//! ↖ mirror: initialize_params.py@43ae021 `InitializeParamsBuilder` 的最小可用子集。
//!
//! lsp-core 这一层不依赖任何具体服务器 —— `language_server_adapter::initialize_patches`
//! 在 M3 落地时拿这个 builder 调 `apply_patches`；现在 Task 6 只造 base。
//!
//! 关键能力声明（ARCHITECTURE §3.2 注：与 client.rs 的 ContentModified 重试机制一致）：
//! - `text_document.document_symbol.hierarchical_document_symbol_support = true`
//! - `general.stale_request_support = { cancel: true, retry_on_content_modified: [...] }`
//! - `general.position_encodings = [utf-16, utf-8]`（clangd 默认 utf-16；mock_ls capabilities 同）
//!
//! workspace 能力保持最小集（仅声明 workspaceFolders=true），避免过度声明误导服务器。

use lsp_types::{
    ClientCapabilities, DocumentSymbolClientCapabilities, GeneralClientCapabilities,
    InitializeParams, PositionEncodingKind, StaleRequestSupportClientCapabilities,
    TextDocumentClientCapabilities, WorkspaceClientCapabilities,
};

/// 默认客户端信息。CLI 在 supervisor 层组装时（Task 10）可改 name/version；本模块只给个稳定默认。
const CLIENT_NAME: &str = "serena-rust";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 构造 base `InitializeParams`：填写 processId、capabilities、client_info，
/// rootUri/rootPath/workspaceFolders 由调用方（supervisor）按项目补齐。
///
/// ↖ mirror: ls.py@43ae021 `_create_base_initialize_params` + initialize_params.py builder。
pub fn base_initialize_params() -> InitializeParams {
    let caps = ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            workspace_folders: Some(true),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            document_symbol: Some(DocumentSymbolClientCapabilities {
                hierarchical_document_symbol_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            stale_request_support: Some(StaleRequestSupportClientCapabilities {
                cancel: true,
                // ARCHITECTURE §3.2：与 client.rs 的 ContentModified 重试白名单对齐。
                // Task 6 阶段只声明最常用的两类；adapter patch（Task 8）按需扩展。
                retry_on_content_modified: vec![
                    "textDocument/documentSymbol".into(),
                    "workspace/symbol".into(),
                ],
            }),
            // 优先 utf-16（clangd 默认协商值；mock_ls capabilities 写死 utf-16，对齐便于测试）。
            position_encodings: Some(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8,
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };

    InitializeParams {
        process_id: Some(std::process::id()),
        capabilities: caps,
        client_info: Some(lsp_types::ClientInfo {
            name: CLIENT_NAME.into(),
            version: Some(CLIENT_VERSION.into()),
        }),
        ..Default::default()
    }
}

/// 让调用方把传入的 `InitializeParams` 强制套上 base 能力（除非调用方已声明更强版本）。
///
/// ↖ mirror: initialize_params.py@43ae021 `InitializeParamsBuilder` 的 `build()` 合并策略：
/// 调用方填的字段优先；未填的字段（capabilities 整体）由 base 兜底。
///
/// 注意：`capabilities` 是必填非 Option，base 必须保证总有一个有效的 ClientCapabilities。
/// 调用方填了非默认 caps 时，base 完全沿用调用方版本（避免重置）。
pub fn with_base(mut user: InitializeParams) -> InitializeParams {
    // base 仅在调用方 capabilities 仍为默认空对象时套用。`Default` 即所有字段 None，
    // 已被声明成 Some 的 caps 视为调用方已经自管理。
    let is_default_caps = user.capabilities.workspace.is_none()
        && user.capabilities.text_document.is_none()
        && user.capabilities.general.is_none();

    if is_default_caps {
        user.capabilities = base_initialize_params().capabilities;
    }

    if user.process_id.is_none() {
        user.process_id = Some(std::process::id());
    }
    if user.client_info.is_none() {
        user.client_info = Some(lsp_types::ClientInfo {
            name: CLIENT_NAME.into(),
            version: Some(CLIENT_VERSION.into()),
        });
    }
    user
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_declares_hierarchical_document_symbol_support() {
        let p = base_initialize_params();
        let doc = p
            .capabilities
            .text_document
            .expect("base 必须声明 text_document");
        let ds = doc.document_symbol.expect("base 必须声明 document_symbol");
        assert_eq!(ds.hierarchical_document_symbol_support, Some(true));
    }

    #[test]
    fn base_declares_stale_request_support() {
        let p = base_initialize_params();
        let g = p.capabilities.general.expect("base 必须声明 general");
        let s = g
            .stale_request_support
            .expect("base 必须声明 stale_request_support");
        assert!(s.cancel);
        assert!(!s.retry_on_content_modified.is_empty());
    }

    #[test]
    fn base_includes_position_encodings() {
        let p = base_initialize_params();
        let g = p.capabilities.general.expect("base 必须声明 general");
        let encs = g
            .position_encodings
            .expect("base general 应声明 position_encodings");
        assert!(encs.iter().any(|e| e == &PositionEncodingKind::UTF16));
    }

    #[test]
    fn with_base_respects_explicit_caps() {
        let user = InitializeParams {
            capabilities: ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    configuration: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let merged = with_base(user);
        // 调用方已声明 capabilities → 合并后 workspace.configuration 必须保留。
        assert_eq!(
            merged.capabilities.workspace.unwrap().configuration,
            Some(true)
        );
        // 调用方未声明 text_document/general → base 不应强行覆盖（保留 None）。
        assert!(merged.capabilities.text_document.is_none());
        assert!(merged.capabilities.general.is_none());
    }

    #[test]
    fn with_base_fills_default_caps() {
        // 纯 default → with_base 必须套上 base caps。
        let merged = with_base(InitializeParams::default());
        assert!(merged.capabilities.text_document.is_some());
        assert!(merged.capabilities.general.is_some());
        // processId / clientInfo 应被填上。
        assert!(merged.process_id.is_some());
        assert!(merged.client_info.is_some());
    }
}
