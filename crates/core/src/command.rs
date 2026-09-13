use logic::ClusterKey;
use model::SymbolId;

use crate::layout::LayoutOrientation;
use crate::workspace::ViewMode;
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
    /// Lay the graph out left-to-right or top-down. Absolute rather than a
    /// toggle, because the control naming it names one orientation, not "the
    /// other one" — the same reason `SetGraphDepth` takes a level.
    SetGraphOrientation(LayoutOrientation),

    // ---- Clustering ----
    /// Show the cluster map or the rooted call tree. Absolute rather than a
    /// toggle, because the control naming it names one view, not "the other
    /// one" — the same reason `SetGraphOrientation` takes an orientation.
    SetViewMode(ViewMode),
    /// Draw the cluster map from one function's neighbourhood instead of from
    /// the whole program — or, with `None`, widen it back out.
    ///
    /// The whole-program map answers "what is in this project"; scoped to a
    /// function it answers "what is *this* made of", which is the question the
    /// user is already asking by having a function on screen. Absolute rather
    /// than a toggle for the reason the view and orientation commands are:
    /// what names the scope names one function, not "some other function".
    SetMapScope(Option<SymbolId>),
    /// Open or close one cluster's contents on the map. The box stays visible.
    ///
    /// Named by [`ClusterKey`], not `ClusterId`: this arrives from a frontend
    /// holding a snapshot that the next recompute may already have invalidated,
    /// and an id would then open some other community without complaint.
    ToggleClusterExpanded(ClusterKey),
    /// Leave the map for the call tree rooted at this cluster's entry point —
    /// the member the clustering picked as its front door. Keyed for the same
    /// reason as `ToggleClusterExpanded`.
    DrillIntoCluster(ClusterKey),
    /// Leave the call tree and go back to the map.
    ZoomOutToClusters,

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
