use lsp_types as lsp;

/// Response from LSP find_references request
#[derive(Debug, Clone)]
pub struct FindReferencesResponse {
    pub request_id: i64,
    pub locations: Vec<model::Location>,
}

/// Enhanced response from LSP find_references request with symbol information
#[derive(Debug, Clone)]
pub struct EnhancedReferencesResponse {
    pub request_id: i64,
    pub references: Vec<model::Reference>,
}

/// Response from LSP workspace/symbol request
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolResponse {
    pub request_id: i64,
    pub symbols: Vec<WorkspaceSymbolInfo>,
}

/// Response from LSP textDocument/documentSymbol request
#[derive(Debug, Clone)]
pub struct DocumentSymbolResponse {
    pub request_id: i64,
    pub symbols: Vec<WorkspaceSymbolInfo>,
}

/// Response from LSP textDocument/hover request
#[derive(Debug, Clone)]
pub struct HoverResponse {
    pub request_id: i64,
    pub hover_info: Option<String>, // Simplified hover content
}

/// Response from LSP textDocument/prepareCallHierarchy request
#[derive(Debug, Clone)]
pub struct PrepareCallHierarchyResponse {
    pub request_id: i64,
    pub items: Vec<lsp::CallHierarchyItem>,
}

/// Response from LSP callHierarchy/outgoingCalls request
#[derive(Debug, Clone)]
pub struct OutgoingCallsResponse {
    pub request_id: i64,
    pub calls: Vec<lsp::CallHierarchyOutgoingCall>,
}

/// Response from LSP callHierarchy/incomingCalls request
#[derive(Debug, Clone)]
pub struct IncomingCallsResponse {
    pub request_id: i64,
    pub calls: Vec<lsp::CallHierarchyIncomingCall>,
}

/// Simplified symbol information from LSP
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolInfo {
    pub name: String,
    pub kind: lsp::SymbolKind,
    pub location: model::Location,
    pub container_name: Option<String>,
}

/// Convert LSP Location to our model Location
pub fn convert_lsp_location(lsp_location: &lsp::Location) -> model::Location {
    model::Location {
        file_path: lsp_location
            .uri
            .to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| lsp_location.uri.to_string()),
        line: lsp_location.range.start.line + 1, // Convert from 0-indexed LSP to 1-indexed
        column: lsp_location.range.start.character + 1, // Convert from 0-indexed LSP to 1-indexed
        length: Some(lsp_location.range.end.character - lsp_location.range.start.character),
    }
}

/// Convert LSP Position to our model Location (without length)
pub fn convert_lsp_position(uri: &lsp::Url, position: &lsp::Position) -> model::Location {
    model::Location {
        file_path: uri
            .to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| uri.to_string()),
        line: position.line + 1, // Convert from 0-indexed LSP to 1-indexed
        column: position.character + 1, // Convert from 0-indexed LSP to 1-indexed
        length: None,
    }
}
