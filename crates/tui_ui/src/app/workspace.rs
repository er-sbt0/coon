use model::SymbolId;

use crate::graph_workspace::GraphWorkspace;

use super::App;

impl App {
    // Workspace management methods

    /// Create a new workspace with an auto-generated name
    pub fn create_workspace(&mut self, name: String) -> usize {
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;

        let workspace = GraphWorkspace::new(id, name);
        self.workspaces.push(workspace);
        self.current_workspace_index = self.workspaces.len() - 1;
        self.status_message = format!("Created workspace #{}", id);
        id
    }

    /// Create a new workspace with a specific root function
    pub fn create_workspace_with_function(&mut self, name: String, symbol: SymbolId) -> usize {
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;

        let workspace = GraphWorkspace::new_with_root(id, name, symbol);
        self.workspaces.push(workspace);
        self.current_workspace_index = self.workspaces.len() - 1;
        self.status_message = format!("Created workspace #{} with function", id);
        id
    }

    /// Close a workspace by index (cannot close last workspace)
    pub fn close_workspace(&mut self, index: usize) -> bool {
        if self.workspaces.len() <= 1 {
            self.status_message = "Cannot close the last workspace".to_string();
            return false;
        }

        if index >= self.workspaces.len() {
            return false;
        }

        self.workspaces.remove(index);

        // Adjust current index if needed
        if self.current_workspace_index >= self.workspaces.len() {
            self.current_workspace_index = self.workspaces.len() - 1;
        } else if self.current_workspace_index > index {
            self.current_workspace_index -= 1;
        }

        self.status_message = "Workspace closed".to_string();
        true
    }

    /// Switch to a specific workspace
    pub fn switch_workspace(&mut self, index: usize) -> bool {
        if index >= self.workspaces.len() {
            return false;
        }

        self.current_workspace_index = index;
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.touch();
            self.status_message = format!("Switched to workspace: {}", workspace.name);
        }
        true
    }

    /// Switch to next workspace
    pub fn next_workspace(&mut self) {
        if !self.workspaces.is_empty() {
            self.current_workspace_index =
                (self.current_workspace_index + 1) % self.workspaces.len();
            if let Some(workspace) = self.workspaces.get_mut(self.current_workspace_index) {
                workspace.touch();
                self.status_message = format!("Switched to workspace: {}", workspace.name);
            }
        }
    }

    /// Switch to previous workspace
    pub fn previous_workspace(&mut self) {
        if !self.workspaces.is_empty() {
            if self.current_workspace_index == 0 {
                self.current_workspace_index = self.workspaces.len() - 1;
            } else {
                self.current_workspace_index -= 1;
            }
            if let Some(workspace) = self.workspaces.get_mut(self.current_workspace_index) {
                workspace.touch();
                self.status_message = format!("Switched to workspace: {}", workspace.name);
            }
        }
    }

    /// Rename a workspace
    pub fn rename_workspace(&mut self, index: usize, new_name: String) {
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.name = new_name.clone();
            self.status_message = format!("Renamed workspace to: {}", new_name);
        }
    }

    /// Get current workspace reference
    pub fn get_current_workspace(&self) -> Option<&GraphWorkspace> {
        self.workspaces.get(self.current_workspace_index)
    }

    /// Get current workspace mutable reference
    pub fn get_current_workspace_mut(&mut self) -> Option<&mut GraphWorkspace> {
        self.workspaces.get_mut(self.current_workspace_index)
    }

    /// Toggle function search modal
    pub fn toggle_function_search(&mut self) {
        self.show_function_search = !self.show_function_search;
        if self.show_function_search {
            self.function_search_query.clear();
        }
    }

    /// Toggle workspace manager modal
    pub fn toggle_workspace_manager(&mut self) {
        self.show_workspace_manager = !self.show_workspace_manager;
    }

    // Search bar methods

    /// Toggle search bar visibility
    pub fn toggle_search_bar(&mut self) {
        if self.show_search_bar {
            self.search_bar_state.deactivate();
            self.show_search_bar = false;
        } else {
            self.search_bar_state.activate();
            self.show_search_bar = true;
            // Update results immediately
            self.search_bar_state.update_results(&self.call_graph);
        }
    }

    /// Handle search bar text input
    pub fn handle_search_input(&mut self, c: char) {
        if self.show_search_bar {
            self.search_bar_state.insert_char(c);
            self.search_bar_state.update_results(&self.call_graph);
        }
    }

    /// Handle search bar backspace
    pub fn handle_search_backspace(&mut self) {
        if self.show_search_bar {
            self.search_bar_state.delete_char();
            self.search_bar_state.update_results(&self.call_graph);
        }
    }

    /// Select from search bar and create workspace
    pub fn select_from_search(&mut self) {
        if let Some(result) = self.search_bar_state.get_selected() {
            let symbol_id = result.symbol_id.clone();
            let name = result.name.clone();

            // Create new workspace with selected symbol
            self.create_workspace_with_function(name, symbol_id);

            // Close search bar
            self.toggle_search_bar();
            self.status_message = "Workspace created from search".to_string();
        }
    }
}
