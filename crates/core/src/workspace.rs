use std::collections::HashSet;
use std::time::Instant;

use logic::ClusterMethod;
use model::{CallDirection, SymbolId};
use serde::{Deserialize, Serialize};

use crate::layout::{LayoutOrientation, DEFAULT_DEPTH, MAX_DEPTH, MIN_DEPTH};

use crate::events::AppEvent;

/// Which of the two views a workspace is showing.
///
/// `ClusterMap` draws one rung of the coarsening ladder, every cluster at that
/// rung a box; `Rooted` is the familiar call tree grown out of one function.
///
/// It lives beside the workspace rather than beside `LayoutOrientation` in
/// `layout` because it is not a layout parameter. Orientation says how to draw
/// a view; the view mode says which view there is to draw at all, and the
/// workspace is the thing that remembers it. `protocol::layout_of` and
/// `protocol::cluster_layout_of` each read it to decide whether the view they
/// build is the one on screen — the way `protocol::layout_options` reads
/// `depth` and `collapsed` — so `layout` stays two functions with no mode
/// flag between them, and neither owns the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ViewMode {
    /// The default. A workspace has no root until the user picks one, so a tab
    /// that opened on the call tree opened on an empty canvas and left them to
    /// guess which of a thousand functions to start from. The map is the answer
    /// to the question a fresh workspace actually poses — what is in here — and
    /// drilling into a community roots the tree for them.
    #[default]
    ClusterMap,
    Rooted,
}

/// Domain state of a single graph workspace (tab).
///
/// Contains only data — no rendering or view state. The TUI crate stores
/// `GraphViewState` separately, keyed by workspace id.
pub struct CoreWorkspace {
    pub id: usize,
    pub name: String,
    root_symbol: Option<SymbolId>,
    direction: CallDirection,
    /// Levels of callees/callers to show from the root.
    depth: usize,
    /// Nodes whose children the user has collapsed. The node stays visible.
    collapsed: HashSet<SymbolId>,
    /// Nodes the user asked to load the calls of, so the layout walks one level
    /// past the depth limit at each of them.
    ///
    /// The counterpart of `collapsed`: one says stop short of where the limit
    /// would reach, the other says go one step further. Both describe this
    /// tree, so both are cleared when the root moves.
    expanded: HashSet<SymbolId>,
    /// Which way the graph flows on the canvas.
    orientation: LayoutOrientation,
    /// Which view this workspace is showing.
    view: ViewMode,
    /// Which rung of the coarsening ladder the map is drawn at, or `None` for
    /// wherever the ladder says a map should open — which is *not* the
    /// coarsest: `Clustering::opening_level` is the last rung built at plain
    /// modularity, and the rungs above it exist only to force a wide optimum
    /// down to a box count, so they are where a user zooms out *to* rather than
    /// where they should land. Which rung that is depends on a clustering the
    /// workspace does not hold, so `None` is resolved in
    /// `CoreApp::map_level_for`.
    ///
    /// Held as a level rather than as a set of opened boxes because the ladder
    /// is a zoom: every rung partitions the same functions, so the map shows
    /// exactly one of them and moving is a step, not an expansion. The number
    /// survives a recompute on its own terms — rung 4 of the next ladder is
    /// still about as coarse as rung 4 of this one — and `CoreApp` clamps it to
    /// what the current ladder has.
    map_level: Option<usize>,
    /// Which algorithm builds the ladder this workspace is looking at.
    method: ClusterMethod,
    /// The function whose neighbourhood the cluster map is drawn from, or
    /// `None` for the whole program.
    ///
    /// Separate from `root_symbol`, and deliberately not cleared when the root
    /// moves: the map is where a rooted tree was entered *from*, and the way
    /// back has to lead to the map the user was actually looking at. Rooting at
    /// a function found inside one function's map and then going back to "the
    /// cluster map" should not silently land on the whole program's.
    map_scope: Option<SymbolId>,
    pub created_at: Instant,
    pub last_accessed: Instant,
}

impl CoreWorkspace {
    pub fn new(id: usize, name: String) -> Self {
        let now = Instant::now();
        Self {
            id,
            name,
            root_symbol: None,
            direction: CallDirection::Outgoing,
            depth: DEFAULT_DEPTH,
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            orientation: LayoutOrientation::default(),
            view: ViewMode::default(),
            map_level: None,
            method: ClusterMethod::default(),
            map_scope: None,
            created_at: now,
            last_accessed: now,
        }
    }

    pub fn new_with_root(id: usize, name: String, symbol: SymbolId) -> Self {
        let mut ws = Self::new(id, name);
        // Through `set_root` rather than assigning the field, so a workspace
        // opened on a named symbol starts on that symbol's tree. Otherwise it
        // would open on the cluster map like any other new tab, and the root
        // the caller just asked for would be invisible.
        ws.set_root(symbol);
        ws
    }

    pub fn set_root(&mut self, symbol: SymbolId) {
        self.root_symbol = Some(symbol);
        // Collapse state describes a particular tree; a new root is a new tree.
        self.collapsed.clear();
        // And so does the set of nodes walked past the depth limit.
        self.expanded.clear();
        // How far the map was zoomed in describes a particular map, and for the
        // same reason it does not survive being pointed at a new root.
        self.map_level = None;
        // Naming a root *is* asking for the rooted view — the cluster map draws
        // communities and has nowhere to put one. Doing it here rather than
        // leaving each frontend to pair the two commands is what keeps a
        // frontend a transport: the TUI, the web client and the desktop shell
        // would otherwise each have to know this rule, and each could forget it.
        self.view = ViewMode::Rooted;
        self.touch();
    }

    /// Set the traversal depth, clamped to the supported range.
    /// Returns true when the value actually changed.
    pub fn set_depth(&mut self, depth: usize) -> bool {
        let next = depth.clamp(MIN_DEPTH, MAX_DEPTH);
        let changed = next != self.depth;
        self.depth = next;
        if changed {
            self.touch();
        }
        changed
    }

    /// Lay the graph out left-to-right or top-down.
    /// Returns true when the value actually changed.
    pub fn set_orientation(&mut self, orientation: LayoutOrientation) -> bool {
        let changed = orientation != self.orientation;
        self.orientation = orientation;
        if changed {
            self.touch();
        }
        changed
    }

    /// Show the cluster map or a rooted call tree.
    /// Returns true when the value actually changed.
    pub fn set_view(&mut self, view: ViewMode) -> bool {
        let changed = view != self.view;
        self.view = view;
        if changed {
            self.touch();
        }
        changed
    }

    /// Point the cluster map at one function's neighbourhood, or at the whole
    /// program with `None`. Returns true when the value actually changed.
    ///
    /// Switches to the map for the same reason [`Self::set_root`] switches away
    /// from it: naming what a map should be drawn from *is* asking to see that
    /// map, and leaving each frontend to pair the two commands is how a
    /// frontend stops being a transport. The zoom goes back to the new ladder's
    /// opening rung with it: a new scope is a new ladder, of a different
    /// height, and rung 6 of the old one names nothing in particular on the
    /// new.
    pub fn set_map_scope(&mut self, scope: Option<SymbolId>) -> bool {
        let changed = scope != self.map_scope || self.view != ViewMode::ClusterMap;
        self.map_scope = scope;
        self.map_level = None;
        self.view = ViewMode::ClusterMap;
        if changed {
            self.touch();
        }
        changed
    }

    /// Draw the map at one rung of the ladder. Returns true when that is news.
    ///
    /// Takes the level rather than a direction to step, for the reason
    /// [`Self::set_orientation`] takes an orientation: a zoom control names the
    /// rung it lands on. Nothing is clamped here — how many rungs there are is
    /// a property of the clustering, which lives in `CoreApp`.
    pub fn set_map_level(&mut self, level: usize) -> bool {
        let changed = self.map_level != Some(level);
        self.map_level = Some(level);
        if changed {
            self.touch();
        }
        changed
    }

    /// Build the map with a different method. Returns true when that is news.
    ///
    /// The zoom resets, because the rungs are the method's own: another method
    /// coarsens at its own rate and its ladder is a different height.
    pub fn set_method(&mut self, method: ClusterMethod) -> bool {
        let changed = method != self.method;
        self.method = method;
        if changed {
            self.map_level = None;
            self.touch();
        }
        changed
    }

    /// Collapse or re-expand a node's children. Returns true if now collapsed.
    pub fn toggle_collapsed(&mut self, symbol: SymbolId) -> bool {
        self.touch();
        if self.collapsed.remove(&symbol) {
            false
        } else {
            self.collapsed.insert(symbol);
            true
        }
    }

    /// Walk one level past the depth limit at `symbol` from now on. Returns
    /// true when that is news — a second click on the same node changes
    /// nothing and needs no redraw.
    ///
    /// Re-expanding also un-collapses: the button that asks for a node's calls
    /// is the same one that hid them, so leaving it collapsed would swallow the
    /// click.
    pub fn expand(&mut self, symbol: SymbolId) -> bool {
        self.touch();
        let uncollapsed = self.collapsed.remove(&symbol);
        self.expanded.insert(symbol) || uncollapsed
    }

    /// Reverse the arrows: read the graph as callers rather than callees, or
    /// back again. Returns the direction now in force.
    ///
    /// The expansion set goes with it. Reading the other way is a different
    /// tree, and the nodes the old one was walked one level past have nothing
    /// to say about the new one — the rule [`Self::set_root`] applies when the
    /// tree moves, applied here for the same reason. It lives on the workspace
    /// rather than in the command handler that used to do both halves inline,
    /// so that there is no way to flip one without the other.
    pub fn toggle_direction(&mut self) -> CallDirection {
        self.direction = match self.direction {
            CallDirection::Outgoing => CallDirection::Incoming,
            CallDirection::Incoming => CallDirection::Outgoing,
        };
        self.expanded.clear();
        self.touch();
        self.direction
    }

    // ---- Readers ----
    //
    // The fields behind these are private, and the setters above are why. Each
    // of them is half of a rule — a new root empties `collapsed` and
    // `expanded`, a new scope or method resets `map_level`, naming either a
    // root or a scope decides `view`, a depth outside `MIN_DEPTH..=MAX_DEPTH`
    // is not a depth — and a `pub` field is an invitation to set one half and
    // forget the other. That is not hypothetical: the direction toggle did
    // exactly this, clearing `expanded` by hand next to its own assignment,
    // which is the rule `set_root` owns written out a second time in a place
    // nothing would keep in step.

    /// Which function the tree is grown from, if the user has picked one.
    /// Moved by [`Self::set_root`], which takes the rest of the tree with it.
    pub fn root_symbol(&self) -> Option<SymbolId> {
        self.root_symbol
    }

    /// Which way the graph flows. Reversed by [`Self::toggle_direction`].
    pub fn direction(&self) -> CallDirection {
        self.direction
    }

    /// Levels of callees/callers shown from the root, always inside
    /// `MIN_DEPTH..=MAX_DEPTH`: [`Self::set_depth`] is what keeps it there.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Nodes whose children the user has collapsed. Changed by
    /// [`Self::toggle_collapsed`]; emptied when the tree moves.
    pub fn collapsed(&self) -> &HashSet<SymbolId> {
        &self.collapsed
    }

    /// Nodes the layout walks one level past the depth limit at. Changed by
    /// [`Self::expand`]; emptied when the tree moves.
    pub fn expanded(&self) -> &HashSet<SymbolId> {
        &self.expanded
    }

    /// Which way the canvas lays the levels out.
    pub fn orientation(&self) -> LayoutOrientation {
        self.orientation
    }

    /// Which of the two views this workspace is showing.
    ///
    /// It follows what it is showing rather than moving on its own: naming a
    /// root switches to the tree, naming a map scope switches to the map. See
    /// [`Self::set_root`] and [`Self::set_map_scope`].
    pub fn view(&self) -> ViewMode {
        self.view
    }

    /// Which rung of the ladder the map is drawn at, or `None` for wherever
    /// the ladder says a map should open — resolved in `CoreApp::map_level_for`,
    /// and reset by anything that builds a different ladder.
    pub fn map_level(&self) -> Option<usize> {
        self.map_level
    }

    /// Which algorithm builds the ladder. Changed by [`Self::set_method`],
    /// which resets the zoom because the rungs are the method's own.
    pub fn method(&self) -> ClusterMethod {
        self.method
    }

    /// The function whose neighbourhood the map is drawn from, or `None` for
    /// the whole program. Changed by [`Self::set_map_scope`].
    pub fn map_scope(&self) -> Option<SymbolId> {
        self.map_scope
    }

    pub fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }
}

/// Manages the collection of core workspaces (tabs).
///
/// Returns `AppEvent` variants instead of `StatusMessage` so this struct is
/// frontend-agnostic.
pub struct CoreWorkspaceManager {
    pub workspaces: Vec<CoreWorkspace>,
    pub current_index: usize,
    pub next_id: usize,
}

impl Default for CoreWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreWorkspaceManager {
    /// Create a manager pre-populated with one default workspace.
    pub fn new() -> Self {
        let default_workspace = CoreWorkspace::new(1, "Graph 1".to_string());
        Self {
            workspaces: vec![default_workspace],
            current_index: 0,
            next_id: 2,
        }
    }

    /// Create a new workspace with the given name. Returns `(id, event)`.
    pub fn create(&mut self, name: String) -> (usize, AppEvent) {
        let id = self.next_id;
        self.next_id += 1;
        let workspace = CoreWorkspace::new(id, name.clone());
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, AppEvent::WorkspaceCreated { id, name })
    }

    /// Create a new workspace rooted at `symbol`. Returns `(id, event)`.
    pub fn create_with_function(&mut self, name: String, symbol: SymbolId) -> (usize, AppEvent) {
        let id = self.next_id;
        self.next_id += 1;
        let workspace = CoreWorkspace::new_with_root(id, name.clone(), symbol);
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, AppEvent::WorkspaceCreated { id, name })
    }

    /// Close a workspace by index. Returns `Ok(event)` or `Err(event)`.
    pub fn close(&mut self, index: usize) -> Result<AppEvent, AppEvent> {
        if self.workspaces.len() <= 1 {
            return Err(AppEvent::CannotCloseLastWorkspace);
        }
        if index >= self.workspaces.len() {
            return Err(AppEvent::InvalidWorkspaceIndex);
        }

        self.workspaces.remove(index);

        if self.current_index >= self.workspaces.len() {
            self.current_index = self.workspaces.len() - 1;
        } else if self.current_index > index {
            self.current_index -= 1;
        }

        Ok(AppEvent::WorkspaceClosed)
    }

    /// Switch to a specific workspace by index. Returns an optional event, and
    /// `None` for an index that names no workspace — in which case nothing
    /// moves.
    ///
    /// The one place the current index changes by navigation, and so the one
    /// place that touches the workspace and announces the move.
    /// [`Self::next_workspace`] and [`Self::previous`] are index arithmetic in
    /// front of it: they used to be copies of this body, each with its own
    /// bounds check, its own `touch()` and its own `WorkspaceSwitched`, and an
    /// `else { None }` after a bounds check that had already ruled it out.
    pub fn switch_to(&mut self, index: usize) -> Option<AppEvent> {
        // `get_mut` *is* the bounds check, so there is no unreachable arm for
        // the index the check already rejected.
        let workspace = self.workspaces.get_mut(index)?;
        workspace.touch();
        let event = AppEvent::WorkspaceSwitched {
            id: workspace.id,
            name: workspace.name.clone(),
        };
        self.current_index = index;
        Some(event)
    }

    /// Switch to the next workspace (wrapping). Returns an optional event.
    pub fn next_workspace(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        self.switch_to((self.current_index + 1) % self.workspaces.len())
    }

    /// Switch to the previous workspace (wrapping). Returns an optional event.
    pub fn previous(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        let index = if self.current_index == 0 {
            self.workspaces.len() - 1
        } else {
            self.current_index - 1
        };
        self.switch_to(index)
    }

    /// Rename a workspace. Returns an optional event.
    pub fn rename(&mut self, index: usize, new_name: String) -> Option<AppEvent> {
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.name = new_name.clone();
            Some(AppEvent::WorkspaceRenamed { name: new_name })
        } else {
            None
        }
    }

    /// Get reference to the current workspace.
    pub fn current(&self) -> Option<&CoreWorkspace> {
        self.workspaces.get(self.current_index)
    }

    /// Get mutable reference to the current workspace.
    pub fn current_mut(&mut self) -> Option<&mut CoreWorkspace> {
        self.workspaces.get_mut(self.current_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_one_workspace() {
        let wm = CoreWorkspaceManager::new();
        assert_eq!(wm.workspaces.len(), 1);
        assert_eq!(wm.current_index, 0);
        assert_eq!(wm.next_id, 2);
    }

    #[test]
    fn test_create() {
        let mut wm = CoreWorkspaceManager::new();
        let (id, _) = wm.create("Test".to_string());
        assert_eq!(id, 2);
        assert_eq!(wm.workspaces.len(), 2);
        assert_eq!(wm.current_index, 1);
    }

    #[test]
    fn depth_is_clamped_to_the_supported_range() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        assert_eq!(ws.depth, DEFAULT_DEPTH);

        assert!(ws.set_depth(0) || ws.depth == MIN_DEPTH);
        assert_eq!(ws.depth, MIN_DEPTH, "0 clamps up to the minimum");

        ws.set_depth(999);
        assert_eq!(ws.depth, MAX_DEPTH, "large values clamp down");

        assert!(
            !ws.set_depth(MAX_DEPTH),
            "setting the same depth is not a change"
        );
    }

    #[test]
    fn collapse_toggles_and_resets_with_the_root() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        let sym = SymbolId::from_content("a", "a.c", 1);

        assert!(ws.toggle_collapsed(sym), "first toggle collapses");
        assert!(ws.collapsed.contains(&sym));
        assert!(!ws.toggle_collapsed(sym), "second toggle expands");
        assert!(ws.collapsed.is_empty());

        ws.toggle_collapsed(sym);
        ws.set_root(SymbolId::from_content("b", "b.c", 1));
        assert!(
            ws.collapsed.is_empty(),
            "a new root starts with nothing collapsed"
        );
    }

    #[test]
    fn expanding_is_idempotent_and_resets_with_the_root() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        let sym = SymbolId::from_content("a", "a.c", 1);

        assert!(ws.expand(sym), "the first ask widens the walk");
        assert!(!ws.expand(sym), "asking again changes nothing to redraw");

        ws.toggle_collapsed(sym);
        assert!(
            ws.expand(sym),
            "asking for the calls of a collapsed node un-collapses it"
        );
        assert!(ws.collapsed.is_empty());

        ws.set_root(SymbolId::from_content("b", "b.c", 1));
        assert!(
            ws.expanded.is_empty(),
            "a new root starts with nothing expanded"
        );
    }

    #[test]
    fn reversing_the_direction_drops_the_expansions() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        let sym = SymbolId::from_content("a", "a.c", 1);
        ws.expand(sym);

        assert_eq!(ws.toggle_direction(), CallDirection::Incoming);
        assert!(
            ws.expanded.is_empty(),
            "reading the other way is a different tree, and the nodes walked \
             one level past the limit in the old one say nothing about it"
        );

        assert_eq!(ws.toggle_direction(), CallDirection::Outgoing);
    }

    #[test]
    fn orientation_is_set_absolutely_and_reports_real_changes() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        assert_eq!(ws.orientation, LayoutOrientation::LeftRight);

        assert!(ws.set_orientation(LayoutOrientation::TopDown));
        assert_eq!(ws.orientation, LayoutOrientation::TopDown);
        assert!(
            !ws.set_orientation(LayoutOrientation::TopDown),
            "re-picking the orientation already shown is not a change"
        );
    }

    #[test]
    fn the_view_is_set_absolutely_and_reports_real_changes() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        assert_eq!(
            ws.view,
            ViewMode::ClusterMap,
            "a workspace with no root opens on the map, not on an empty tree"
        );

        assert!(ws.set_view(ViewMode::Rooted));
        assert_eq!(ws.view, ViewMode::Rooted);
        assert!(
            !ws.set_view(ViewMode::Rooted),
            "re-picking the view already shown is not a change"
        );
    }

    #[test]
    fn the_map_zoom_is_absolute_and_resets_with_the_root() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        assert_eq!(ws.map_level, None, "a map opens at the coarsest rung");

        assert!(ws.set_map_level(3), "a new rung is a change");
        assert_eq!(ws.map_level, Some(3));
        assert!(
            !ws.set_map_level(3),
            "re-picking the rung already shown is not a change"
        );

        ws.set_root(SymbolId::from_content("b", "b.c", 1));
        assert_eq!(
            ws.map_level, None,
            "a new root starts at the coarsest rung again"
        );
    }

    #[test]
    fn a_new_method_resets_the_zoom() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        ws.set_map_level(4);

        assert!(
            !ws.set_method(ClusterMethod::Modularity),
            "re-picking the method already shown is not a change"
        );
        assert_eq!(
            ws.map_level,
            Some(4),
            "and so leaves the rung where the user put it"
        );
    }

    #[test]
    fn naming_a_root_is_asking_for_the_rooted_view() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        assert_eq!(
            ws.view,
            ViewMode::ClusterMap,
            "a fresh tab opens on the map"
        );

        ws.set_root(SymbolId::from_content("a", "a.c", 1));
        assert_eq!(
            ws.view,
            ViewMode::Rooted,
            "the map has nowhere to draw a root, so every frontend would \
             otherwise have to pair the two commands itself"
        );

        let opened = CoreWorkspace::new_with_root(
            2,
            "w2".to_string(),
            SymbolId::from_content("b", "b.c", 1),
        );
        assert_eq!(
            opened.view,
            ViewMode::Rooted,
            "a tab opened on a named symbol starts on that symbol"
        );
    }

    #[test]
    fn a_map_scope_switches_to_the_map_and_survives_a_new_root() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        let scope = SymbolId::from_content("handle_request", "req.c", 1);
        ws.set_root(SymbolId::from_content("a", "a.c", 1));
        assert_eq!(ws.view, ViewMode::Rooted);

        assert!(ws.set_map_scope(Some(scope)));
        assert_eq!(ws.map_scope, Some(scope));
        assert_eq!(
            ws.view,
            ViewMode::ClusterMap,
            "naming what a map is drawn from is asking for that map"
        );
        assert!(
            !ws.set_map_scope(Some(scope)),
            "re-picking the map already shown is not a change"
        );

        ws.set_root(SymbolId::from_content("b", "b.c", 1));
        assert_eq!(
            ws.map_scope,
            Some(scope),
            "the scope is where the tree was entered from, and the way back \
             must lead there rather than to the whole program"
        );

        assert!(ws.set_map_scope(None));
        assert_eq!(ws.map_scope, None);
    }

    #[test]
    fn a_new_scope_starts_at_the_coarsest_rung() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        ws.set_map_level(5);

        ws.set_map_scope(Some(SymbolId::from_content("scope", "s.c", 1)));

        assert_eq!(
            ws.map_level, None,
            "a rung names a place on the ladder the old scope built"
        );
    }

    #[test]
    fn test_close_last_fails() {
        let mut wm = CoreWorkspaceManager::new();
        assert!(wm.close(0).is_err());
    }

    #[test]
    fn test_close_and_navigate() {
        let mut wm = CoreWorkspaceManager::new();
        wm.create("Second".to_string());
        assert!(wm.close(1).is_ok());
        assert_eq!(wm.workspaces.len(), 1);
        assert_eq!(wm.current_index, 0);
    }
}
