use serde::{Deserialize, Serialize};

/// Phases of LSP loading lifecycle for UI feedback
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LspLoadPhase {
    NotStarted,
    SpawningServer,
    Initializing,
    Initialized,
    DiscoveringFiles,
    PreloadingDocuments { done: usize, total: usize },
    LoadingWorkspaceSymbols { loaded: usize },
    Completed,
    Failed(String),
}

/// Messages sent from the background LSP loader into the TUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspUiMessage {
    Progress(LspLoadPhase),
    AddFunction(crate::WorkspaceSymbolInfo),
}
