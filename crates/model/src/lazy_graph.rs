use serde::{Deserialize, Serialize};

use crate::symbols::*;

/// Workspace symbol information for deduplication
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSymbolInfo {
    pub name: String,
    pub qualified_name: String,
    pub kind: lsp_types::SymbolKind,
    pub location: Location,
    pub container_name: Option<String>,
}
