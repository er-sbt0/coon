use model::{LspLoadPhase, SymbolId};
use serde::{Deserialize, Serialize};

/// Events emitted by `CoreApp` in response to commands and LSP messages.
///
/// Frontends translate these into their own display representation.
/// The TUI maps them to `StatusMessage`; the web frontend serialises them to JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppEvent {
    // ---- Graph data changes ----
    /// Workspace symbols were fetched/refreshed from clangd.
    GraphSymbolsUpdated {
        count: usize,
    },
    /// Outgoing call hierarchy was loaded for a function.
    OutgoingCallsLoaded {
        symbol_id: SymbolId,
    },
    /// Incoming call hierarchy was loaded for a function.
    IncomingCallsLoaded {
        symbol_id: SymbolId,
    },
    /// Find-references returned results.
    ReferencesFound {
        symbol_id: SymbolId,
        count: usize,
    },
    /// Find-references returned no results.
    NoReferencesFound {
        symbol_id: SymbolId,
    },
    /// The selected function has no callees.
    FunctionHasNoCallees,
    /// The graph layout must be recomputed (e.g. new calls loaded).
    LayoutDirty,
    /// The cached clustering was recomputed and the grouping moved. Carries the
    /// number of top-level clusters, which is what a frontend showing the map
    /// would report; the grouping itself is read from `CoreApp::clustering`.
    ClusteringUpdated {
        clusters: usize,
    },

    // ---- LSP lifecycle ----
    LspProgress(LspLoadPhase),
    LspRequestTimedOut,
    /// A call hierarchy request is in flight.
    LoadingCalls {
        direction: String,
    },
    /// A references request is in flight.
    FindingReferences {
        name: String,
    },
    /// Workspace symbols refresh is in flight.
    RefreshingWorkspaceSymbols,
    /// Workspace symbols loaded after a refresh.
    WorkspaceSymbolsLoaded {
        count: usize,
    },

    // ---- Workspace lifecycle ----
    WorkspaceCreated {
        id: usize,
        name: String,
    },
    WorkspaceClosed,
    WorkspaceSwitched {
        id: usize,
        name: String,
    },
    WorkspaceRenamed {
        name: String,
    },
    CannotCloseLastWorkspace,
    InvalidWorkspaceIndex,

    // ---- Errors ----
    InvalidFilePath,
    FailedToCreateUri,
    FailedToSendRequest,
    NoLspChannel,
    LspError(String),
}
