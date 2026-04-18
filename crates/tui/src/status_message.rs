use std::fmt;

/// Typed status messages displayed in the TUI status bar.
///
/// Replaces ad-hoc string literals scattered across the codebase,
/// making it easy to audit, test, and evolve the set of user-visible messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusMessage {
    // ── General ───────────────────────────────────────────────
    /// Initial idle state.
    Ready,
    /// Cleared / nothing to show.
    Empty,

    // ── Selection ─────────────────────────────────────────────
    FunctionSelected,

    // ── Viewport panning ──────────────────────────────────────
    PannedUp,
    PannedDown,
    PannedLeft,
    PannedRight,
    ResetView,

    // ── Graph node interaction ────────────────────────────────
    ExpandedNode {
        name: String,
    },
    NoNodeSelected,

    // ── Navigation ────────────────────────────────────────────
    /// Successfully navigated (e.g. "Navigated to parent: foo").
    Navigated {
        description: String,
        name: Option<String>,
    },
    /// Navigation failed (e.g. "No parent node (at root)").
    NavigationFailed {
        reason: String,
    },

    // ── Call direction ────────────────────────────────────────
    SwitchedCallDirection {
        direction: String,
    },
    OutgoingCallsLoaded,
    IncomingCallsLoaded,
    FunctionHasNoCallees,
    LoadingCalls {
        direction: String,
    },

    // ── References ────────────────────────────────────────────
    NoFunctionSelectedForReferences,
    FindingReferences {
        name: String,
    },
    ReferencesFound {
        count: usize,
        name: Option<String>,
    },
    NoReferencesFound {
        name: String,
    },

    // ── Symbols / loading ─────────────────────────────────────
    DocumentSymbolsLoaded,
    WorkspaceSymbolsLoaded {
        count: usize,
    },
    FunctionsLoadedFromWorkspace {
        count: usize,
    },
    PreloadComplete {
        loaded: usize,
        failed: usize,
    },

    // ── LSP lifecycle ─────────────────────────────────────────
    LoadingFunctionData,
    RefreshingProject,
    RefreshingWorkspaceSymbols,
    LspRequestTimedOut,

    // ── Workspace management ──────────────────────────────────
    CreatedWorkspace {
        id: usize,
    },
    CreatedWorkspaceWithFunction {
        id: usize,
    },
    WorkspaceClosed,
    CannotCloseLastWorkspace,
    InvalidWorkspaceIndex,
    SwitchedToWorkspace {
        name: String,
    },
    WorkspaceCreatedFromSearch,
    GraphWorkspaceCreated,

    // ── Errors ────────────────────────────────────────────────
    LspError {
        error: String,
    },
    Error {
        error: String,
    },
    InvalidFilePath,
    FailedToSendLspRequest,
    FailedToSendRequest,
    FailedToCreateUri,
    NoLspChannel,
}

impl fmt::Display for StatusMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // General
            Self::Ready => write!(f, "Ready"),
            Self::Empty => Ok(()),

            // Selection
            Self::FunctionSelected => write!(f, "Function selected"),

            // Viewport panning
            Self::PannedUp => write!(f, "Panned up"),
            Self::PannedDown => write!(f, "Panned down"),
            Self::PannedLeft => write!(f, "Panned left"),
            Self::PannedRight => write!(f, "Panned right"),
            Self::ResetView => write!(f, "Reset view - centered on root"),

            // Graph node interaction
            Self::ExpandedNode { name } => write!(f, "Expanded node: {}", name),
            Self::NoNodeSelected => write!(f, "No node selected to expand"),

            // Navigation
            Self::Navigated {
                description,
                name: Some(name),
            } => {
                write!(f, "{}: {}", description, name)
            }
            Self::Navigated {
                description,
                name: None,
            } => write!(f, "{}", description),
            Self::NavigationFailed { reason } => write!(f, "{}", reason),

            // Call direction
            Self::SwitchedCallDirection { direction } => {
                write!(f, "Switched to {} calls view", direction)
            }
            Self::OutgoingCallsLoaded => write!(f, "Outgoing calls loaded"),
            Self::IncomingCallsLoaded => write!(f, "Incoming calls loaded"),
            Self::FunctionHasNoCallees => write!(f, "Function has no callees"),
            Self::LoadingCalls { direction } => write!(f, "Loading {} calls...", direction),

            // References
            Self::NoFunctionSelectedForReferences => {
                write!(f, "No function selected for finding references")
            }
            Self::FindingReferences { name } => write!(f, "Finding references for '{}'...", name),
            Self::ReferencesFound {
                count,
                name: Some(name),
            } => {
                write!(f, "Found {} reference(s) for '{}'", count, name)
            }
            Self::ReferencesFound { count, name: None } => {
                write!(f, "Found {} reference(s)", count)
            }
            Self::NoReferencesFound { name } => {
                write!(f, "No references found for '{}'", name)
            }

            // Symbols / loading
            Self::DocumentSymbolsLoaded => write!(f, "Document symbols loaded"),
            Self::WorkspaceSymbolsLoaded { count } => {
                write!(f, "Loaded {} workspace symbols", count)
            }
            Self::FunctionsLoadedFromWorkspace { count } => {
                write!(f, "Loaded {} functions from workspace", count)
            }
            Self::PreloadComplete { loaded, failed } if *failed > 0 => {
                write!(f, "Preloaded {} documents ({} failed)", loaded, failed)
            }
            Self::PreloadComplete { loaded, .. } => {
                write!(f, "Preloaded {} documents successfully", loaded)
            }

            // LSP lifecycle
            Self::LoadingFunctionData => write!(f, "Loading data for function..."),
            Self::RefreshingProject => {
                write!(f, "Refreshing project data from LSP server...")
            }
            Self::RefreshingWorkspaceSymbols => write!(f, "Refreshing workspace symbols..."),
            Self::LspRequestTimedOut => write!(f, "LSP request timed out"),

            // Workspace management
            Self::CreatedWorkspace { id } => write!(f, "Created workspace #{}", id),
            Self::CreatedWorkspaceWithFunction { id } => {
                write!(f, "Created workspace #{} with function", id)
            }
            Self::WorkspaceClosed => write!(f, "Workspace closed"),
            Self::CannotCloseLastWorkspace => write!(f, "Cannot close the last workspace"),
            Self::InvalidWorkspaceIndex => write!(f, "Invalid workspace index"),
            Self::SwitchedToWorkspace { name } => {
                write!(f, "Switched to workspace: {}", name)
            }
            Self::WorkspaceCreatedFromSearch => write!(f, "Workspace created from search"),
            Self::GraphWorkspaceCreated => write!(f, "Graph workspace created"),

            // Errors
            Self::LspError { error } => write!(f, "LSP error: {}", error),
            Self::Error { error } => write!(f, "Error: {}", error),
            Self::InvalidFilePath => write!(f, "Invalid file path"),
            Self::FailedToSendLspRequest => write!(f, "Failed to send LSP request"),
            Self::FailedToSendRequest => write!(f, "Failed to send request"),
            Self::FailedToCreateUri => write!(f, "Failed to create URI from file path"),
            Self::NoLspChannel => write!(f, "No LSP channel available"),
        }
    }
}

impl Default for StatusMessage {
    fn default() -> Self {
        Self::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_ready() {
        assert_eq!(StatusMessage::Ready.to_string(), "Ready");
    }

    #[test]
    fn display_empty_is_blank() {
        assert_eq!(StatusMessage::Empty.to_string(), "");
    }

    #[test]
    fn display_navigated_with_name() {
        let msg = StatusMessage::Navigated {
            description: "Navigated to parent".into(),
            name: Some("foo".into()),
        };
        assert_eq!(msg.to_string(), "Navigated to parent: foo");
    }

    #[test]
    fn display_preload_with_failures() {
        let msg = StatusMessage::PreloadComplete {
            loaded: 10,
            failed: 2,
        };
        assert_eq!(msg.to_string(), "Preloaded 10 documents (2 failed)");
    }

    #[test]
    fn display_preload_success() {
        let msg = StatusMessage::PreloadComplete {
            loaded: 10,
            failed: 0,
        };
        assert_eq!(msg.to_string(), "Preloaded 10 documents successfully");
    }

    #[test]
    fn display_references_found_with_name() {
        let msg = StatusMessage::ReferencesFound {
            count: 5,
            name: Some("bar".into()),
        };
        assert_eq!(msg.to_string(), "Found 5 reference(s) for 'bar'");
    }

    #[test]
    fn display_references_found_no_name() {
        let msg = StatusMessage::ReferencesFound {
            count: 3,
            name: None,
        };
        assert_eq!(msg.to_string(), "Found 3 reference(s)");
    }
}
