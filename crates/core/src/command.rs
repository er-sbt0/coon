use logic::{ClusterKey, ClusterMethod, ClusterSettings};
use model::SymbolId;

use crate::export::ExportFormat;
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
    /// Draw the map at one rung of the coarsening ladder — 0 the finest, the
    /// top the coarsest.
    ///
    /// Absolute rather than "zoom in one", for the reason the view and
    /// orientation commands are: the control names where it lands. It is also
    /// what lets the client offer both a step and a jump to either end without
    /// a second command. Out-of-range levels are clamped to the ladder, so a
    /// frontend holding a level from a taller ladder is never wrong, only
    /// coarse.
    SetMapLevel(usize),
    /// Build the cluster map with a different algorithm, leaving its knobs
    /// where they are.
    ///
    /// Kept beside [`Command::SetClusterSettings`] rather than folded into it
    /// because picking a method is a gesture on its own — the palette offers
    /// one entry per method, and a caller making that gesture has nothing to
    /// say about γ and should not have to send a value for it.
    SetClusterMethod(ClusterMethod),
    /// Build the cluster map to these settings: the method, the grain that
    /// method reads, and how far the ladder coarsens.
    ///
    /// Absolute rather than a nudge per knob, for the reason the view and
    /// orientation commands are absolute, and because a panel holding all of
    /// them sends what it shows. Out-of-range and off-step values are snapped
    /// by `ClusterSettings` as it deserialises, so what reaches `CoreApp` is
    /// always a clustering it can build.
    SetClusterSettings(ClusterSettings),
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

    /// Load call hierarchy for a specific function without changing the graph root.
    /// Used by the web frontend to expand a node in-place.
    ExpandFunction(SymbolId),

    // ---- Export ----
    /// Write the current view out as a file the user can keep.
    ///
    /// The view, not the graph: what comes back is the subgraph on screen, with
    /// this workspace's depth limit, collapsed nodes, direction and orientation
    /// already applied. See [`crate::export`].
    ExportGraph(ExportFormat),

    /// Move the selection to this function and do nothing else.
    ///
    /// Not `SelectFunction`, which re-roots the tree, and not `ExpandFunction`,
    /// which fetches its callees: clicking a node is how the user points at
    /// one, and pointing must not redraw what they are pointing at. The
    /// selection is what the breadcrumb and the "cluster this" scope read, so
    /// it still belongs to `CoreApp` rather than living in one frontend.
    FocusFunction(SymbolId),
}
