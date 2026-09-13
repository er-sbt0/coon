use std::collections::HashSet;
use std::time::Instant;

use logic::ClusterKey;
use model::{CallDirection, SymbolId};
use serde::{Deserialize, Serialize};

use crate::layout::{LayoutOrientation, DEFAULT_DEPTH, MAX_DEPTH, MIN_DEPTH};

use crate::events::AppEvent;

/// Which of the two views a workspace is showing.
///
/// `ClusterMap` draws the top-level communities as boxes and lets the user open
/// them; `Rooted` is the familiar call tree grown out of one function.
///
/// It lives beside the workspace rather than beside `LayoutOrientation` in
/// `layout` because it is not a layout parameter. Orientation says how to draw
/// a view; the view mode says which view there is to draw at all, and the
/// workspace is the thing that remembers it. Layout will read it in Phase 3,
/// the way it already reads `depth` and `collapsed`, without owning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ViewMode {
    ClusterMap,
    /// The default, for the moment. A workspace is meant to open on the cluster
    /// map and have the user drill down from there, but `compute_layout` cannot
    /// place cluster boxes until Phase 3 and every frontend draws exactly what
    /// the layout hands it. Defaulting to a view the layout cannot yet serve
    /// would open every tab on an empty canvas, so the default flips when the
    /// layout that serves it lands, and not before.
    #[default]
    Rooted,
}

/// Domain state of a single graph workspace (tab).
///
/// Contains only data — no rendering or view state. The TUI crate stores
/// `GraphViewState` separately, keyed by workspace id.
pub struct CoreWorkspace {
    pub id: usize,
    pub name: String,
    pub root_symbol: Option<SymbolId>,
    pub direction: CallDirection,
    /// Levels of callees/callers to show from the root.
    pub depth: usize,
    /// Nodes whose children the user has collapsed. The node stays visible.
    pub collapsed: HashSet<SymbolId>,
    /// Which way the graph flows on the canvas.
    pub orientation: LayoutOrientation,
    /// Which view this workspace is showing.
    pub view: ViewMode,
    /// Clusters the user has opened on the map. Everything else stays a box.
    ///
    /// Keyed by [`ClusterKey`] rather than `ClusterId` because this set outlives
    /// the clustering it was filled from: the graph grows, the clustering is
    /// recomputed, and the ids are handed out afresh. A key goes on naming the
    /// community the user opened; an id would quietly start naming another one.
    pub expanded_clusters: HashSet<ClusterKey>,
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
            orientation: LayoutOrientation::default(),
            view: ViewMode::default(),
            expanded_clusters: HashSet::new(),
            created_at: now,
            last_accessed: now,
        }
    }

    pub fn new_with_root(id: usize, name: String, symbol: SymbolId) -> Self {
        let mut ws = Self::new(id, name);
        ws.root_symbol = Some(symbol);
        ws
    }

    pub fn set_root(&mut self, symbol: SymbolId) {
        self.root_symbol = Some(symbol);
        // Collapse state describes a particular tree; a new root is a new tree.
        self.collapsed.clear();
        // Which clusters stood open describes a particular map, and for the
        // same reason it does not survive being pointed at a new root.
        self.expanded_clusters.clear();
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

    /// Open or close one cluster on the map. Returns true if now expanded.
    pub fn toggle_cluster_expanded(&mut self, cluster: ClusterKey) -> bool {
        self.touch();
        if self.expanded_clusters.remove(&cluster) {
            false
        } else {
            self.expanded_clusters.insert(cluster);
            true
        }
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

    /// Switch to a specific workspace by index. Returns an optional event.
    pub fn switch_to(&mut self, index: usize) -> Option<AppEvent> {
        if index >= self.workspaces.len() {
            return None;
        }
        self.current_index = index;
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
    }

    /// Switch to the next workspace (wrapping). Returns an optional event.
    pub fn next_workspace(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        self.current_index = (self.current_index + 1) % self.workspaces.len();
        if let Some(workspace) = self.workspaces.get_mut(self.current_index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
    }

    /// Switch to the previous workspace (wrapping). Returns an optional event.
    pub fn previous(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        if self.current_index == 0 {
            self.current_index = self.workspaces.len() - 1;
        } else {
            self.current_index -= 1;
        }
        if let Some(workspace) = self.workspaces.get_mut(self.current_index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
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
            ViewMode::Rooted,
            "the map is the intended default, but only once Phase 3 can lay it out"
        );

        assert!(ws.set_view(ViewMode::ClusterMap));
        assert_eq!(ws.view, ViewMode::ClusterMap);
        assert!(
            !ws.set_view(ViewMode::ClusterMap),
            "re-picking the view already shown is not a change"
        );
    }

    #[test]
    fn cluster_expansion_toggles_and_resets_with_the_root() {
        let mut ws = CoreWorkspace::new(1, "w".to_string());
        let cluster = ClusterKey {
            level: 1,
            anchor: SymbolId::from_content("anchor", "test.c", 1),
        };

        assert!(ws.toggle_cluster_expanded(cluster), "first toggle opens");
        assert!(ws.expanded_clusters.contains(&cluster));
        assert!(!ws.toggle_cluster_expanded(cluster), "second toggle closes");
        assert!(ws.expanded_clusters.is_empty());

        ws.toggle_cluster_expanded(cluster);
        ws.set_root(SymbolId::from_content("b", "b.c", 1));
        assert!(
            ws.expanded_clusters.is_empty(),
            "a new root starts with nothing opened"
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
