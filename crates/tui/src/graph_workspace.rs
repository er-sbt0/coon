use crate::graph_view::GraphViewState;
use model::SymbolId;
use std::time::Instant;

/// Represents an independent graph workspace/tab
pub struct GraphWorkspace {
    pub id: usize,
    pub name: String,
    pub root_symbol: Option<SymbolId>,
    pub graph_view_state: GraphViewState,
    pub created_at: Instant,
    pub last_accessed: Instant,
}

impl GraphWorkspace {
    /// Create a new workspace with a default name
    pub fn new(id: usize, name: String) -> Self {
        let now = Instant::now();
        Self {
            id,
            name,
            root_symbol: None,
            graph_view_state: GraphViewState::new(),
            created_at: now,
            last_accessed: now,
        }
    }

    /// Create a new workspace with a root symbol
    pub fn new_with_root(id: usize, name: String, symbol: SymbolId) -> Self {
        let mut workspace = Self::new(id, name);
        workspace.set_root(symbol);
        workspace
    }

    /// Set the root symbol for this workspace
    pub fn set_root(&mut self, symbol: SymbolId) {
        self.root_symbol = Some(symbol);
        self.graph_view_state.set_root(symbol);
        self.touch();
    }

    /// Update the last accessed time
    pub fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }
}
