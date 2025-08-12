use core_data::SymbolId;

/// Actions that can be performed in the TUI
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    MoveUp,
    MoveDown,
    ExpandNode,
    CollapseNode,
    ExpandOrCollapse,
    SwitchTab,
    FindReferences,
    Refresh,
    Quit,
    Help,
    ToggleCallDirection,
}

/// Represents a node in the tree view with expand/collapse state
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub symbol_id: SymbolId,
    pub depth: usize,
    pub is_expanded: bool,
    pub is_loading: bool,
    pub children_loaded: bool,
    pub has_children: bool,
}

impl TreeNode {
    pub fn new(symbol_id: SymbolId, depth: usize) -> Self {
        Self {
            symbol_id,
            depth,
            is_expanded: false,
            is_loading: false,
            children_loaded: false,
            has_children: false, // Will be determined when we try to expand
        }
    }

    pub fn new_root(symbol_id: SymbolId) -> Self {
        Self::new(symbol_id, 0)
    }

    pub fn expand(&mut self) {
        if !self.children_loaded {
            self.is_loading = true;
        }
        self.is_expanded = true;
    }

    pub fn collapse(&mut self) {
        self.is_expanded = false;
        self.is_loading = false;
    }

    pub fn toggle_expand(&mut self) {
        if self.is_expanded {
            self.collapse();
        } else {
            self.expand();
        }
    }

    pub fn set_children_loaded(&mut self, has_children: bool) {
        self.children_loaded = true;
        self.has_children = has_children;
        self.is_loading = false;
    }
}

/// State for managing the tree view
#[derive(Debug)]
pub struct TreeViewState {
    pub nodes: Vec<TreeNode>,
    pub selected_index: usize,
    pub scroll_offset: usize,
}

impl TreeViewState {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
        }
    }

    pub fn add_root_node(&mut self, symbol_id: SymbolId) {
        self.nodes.push(TreeNode::new_root(symbol_id));
        if self.nodes.len() == 1 {
            self.selected_index = 0;
        }
    }

    pub fn get_selected_node(&self) -> Option<&TreeNode> {
        self.nodes.get(self.selected_index)
    }

    pub fn get_selected_node_mut(&mut self) -> Option<&mut TreeNode> {
        self.nodes.get_mut(self.selected_index)
    }

    pub fn move_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.adjust_scroll();
        }
    }

    pub fn move_down(&mut self) {
        if self.selected_index < self.nodes.len().saturating_sub(1) {
            self.selected_index += 1;
            self.adjust_scroll();
        }
    }

    pub fn insert_children(&mut self, parent_index: usize, children: Vec<SymbolId>) {
        if parent_index >= self.nodes.len() {
            return;
        }

        let parent_depth = self.nodes[parent_index].depth;
        let children_count = children.len();
        let mut new_nodes = Vec::new();

        for child_id in children {
            new_nodes.push(TreeNode::new(child_id, parent_depth + 1));
        }

        // Mark parent as having children loaded
        self.nodes[parent_index].set_children_loaded(!new_nodes.is_empty());

        // Insert children after parent
        let insert_index = parent_index + 1;
        for (i, node) in new_nodes.into_iter().enumerate() {
            self.nodes.insert(insert_index + i, node);
        }

        // Adjust selected index if needed
        if self.selected_index > parent_index {
            self.selected_index += children_count;
        }
    }

    pub fn remove_children(&mut self, parent_index: usize) {
        if parent_index >= self.nodes.len() {
            return;
        }

        let parent_depth = self.nodes[parent_index].depth;
        let mut remove_count = 0;
        let remove_start = parent_index + 1;

        // Count children to remove
        for i in (parent_index + 1)..self.nodes.len() {
            if self.nodes[i].depth <= parent_depth {
                break;
            }
            remove_count += 1;
        }

        // Adjust selected index if needed
        if self.selected_index > parent_index && self.selected_index < remove_start + remove_count {
            self.selected_index = parent_index;
        } else if self.selected_index >= remove_start + remove_count {
            self.selected_index -= remove_count;
        }

        // Remove children
        for _ in 0..remove_count {
            self.nodes.remove(remove_start);
        }
    }

    fn adjust_scroll(&mut self) {
        // Simple scroll adjustment - can be enhanced later
        // For now, just ensure selected item is visible
        // This would need the viewport height to implement properly
    }

    pub fn find_node_index(&self, symbol_id: &SymbolId) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| &node.symbol_id == symbol_id)
    }
}
