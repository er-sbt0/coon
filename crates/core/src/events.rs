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
    /// The function the user asked for a cluster map of has nothing to group:
    /// whatever it reaches in this direction forms no community at all —
    /// typically a leaf, or a function whose calls clangd has not resolved yet.
    /// The view stays where it was rather than switching to a blank map.
    FunctionHasNoCommunities,
    /// The graph layout must be recomputed (e.g. new calls loaded).
    LayoutDirty,
    /// The cached clustering was recomputed and the grouping moved. Carries the
    /// number of top-level clusters, which is what a frontend showing the map
    /// would report; the grouping itself is read from `CoreApp::clustering`.
    ClusteringUpdated {
        clusters: usize,
    },

    // ---- Export ----
    /// The current view was rendered to a file's worth of text.
    ///
    /// `CoreApp` builds the contents and stops there: where a file belongs is
    /// the one part of exporting that differs per frontend — the TUI's working
    /// directory, a browser download, a desktop save dialog — and a web server
    /// writing to its own disk on behalf of a tab on another machine would be
    /// the wrong answer in the only case that has a network in it.
    GraphExported {
        filename: String,
        contents: String,
    },
    /// There was nothing on screen to export: a tab with no root, or a cluster
    /// map of a graph whose edges have not arrived yet.
    NothingToExport,

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
    /// A refresh was asked for while the backend is still loading, and was
    /// not started: the load under way already reads the project as it
    /// stands. Only a whole-graph backend says this, since its refresh is a
    /// load.
    AlreadyLoading,
    /// Workspace symbols loaded after a refresh.
    WorkspaceSymbolsLoaded {
        count: usize,
    },
    /// The initial edge warmup is walking the loaded symbols, asking clangd for
    /// each one's calls.
    ///
    /// `LspProgress(Completed)` means clangd has finished handing over symbols,
    /// not that the graph is ready to look at: the edges arrive afterwards, and
    /// on a large project that is the part that takes the time. A frontend that
    /// reported only the phase would say "ready" over a graph still filling in.
    /// `done == total` marks the end, and is sent exactly once.
    ResolvingCalls {
        done: usize,
        total: usize,
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
