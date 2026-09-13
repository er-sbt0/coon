use model::SymbolId;
use serde::{Deserialize, Serialize};

/// Domain commands dispatched to `CoreApp`.
///
/// These are the frontend-agnostic subset of actions — everything that changes
/// graph or workspace state. TUI-only concerns (viewport pan, search cursor)
/// have no `Command` equivalent and are handled locally by the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    // ---- Graph interaction ----
    /// Select a function as the current focus.
    SelectFunction(SymbolId),
    /// Expand or collapse the currently selected node.
    ExpandOrCollapse,
    /// Find references for the currently selected function.
    FindReferences,
    /// Refresh workspace symbols from clangd.
    Refresh,
    /// Toggle between outgoing and incoming call direction.
    ToggleCallDirection,
    /// Set how many levels of callees/callers the view shows.
    SetGraphDepth(usize),
    /// Collapse or re-expand one node's children. The node stays visible.
    ToggleCollapsed(SymbolId),

    // ---- Workspace management ----
    NewWorkspace,
    CloseWorkspace(usize),
    NextWorkspace,
    PreviousWorkspace,
    SwitchWorkspace(usize),
    RenameWorkspace(usize, String),
    /// Create a new workspace rooted at the given function.
    StartGraphFromFunction(SymbolId),

    // ---- Search (query resolved by the frontend before dispatch) ----
    /// The user confirmed a search selection; switch focus to this symbol.
    SearchSelectFunction(SymbolId),

    /// Load call hierarchy for a specific function without changing the graph root.
    /// Used by the web frontend to expand a node in-place.
    ExpandFunction(SymbolId),
}
