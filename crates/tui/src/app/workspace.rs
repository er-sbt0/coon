use crate::status_message::StatusMessage;

use super::App;

impl App {
    /// Toggle workspace manager modal
    pub fn toggle_workspace_manager(&mut self) {
        self.workspaces.toggle_manager();
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
            let symbol_id = result.symbol_id;
            let name = result.name.clone();

            // Create new workspace with selected symbol
            self.create_workspace_with_function(name, symbol_id);

            // Close search bar
            self.toggle_search_bar();
            self.status_message = StatusMessage::WorkspaceCreatedFromSearch;
        }
    }
}
