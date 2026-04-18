use model::SymbolId;

use crate::actions::Action;
use crate::graph_adapter::CallDirection;

use super::App;

impl App {
    pub fn handle_action(&mut self, action: Action) {
        log::info!("Handling action: {:?}", action);
        match action {
            Action::MoveUp => self.handle_move_up(),
            Action::MoveDown => self.handle_move_down(),
            Action::MoveLeft => self.handle_move_left(),
            Action::MoveRight => self.handle_move_right(),
            Action::ExpandNode => self.handle_expand_node(),
            Action::CollapseNode => self.handle_collapse_node(),
            Action::ExpandOrCollapse => self.handle_expand_or_collapse(),
            Action::SwitchTab => {} // Removed - tabs no longer exist
            Action::FindReferences => self.handle_find_references(),
            Action::Refresh => self.handle_refresh(),
            Action::Quit => self.quit(),
            Action::Help => self.toggle_help(),
            Action::ToggleCallDirection => self.handle_toggle_call_direction(),
            Action::ResetView => self.handle_reset_view(),
            Action::NavigateParent => self.handle_navigate_parent(),
            Action::NavigateChild => self.handle_navigate_child(),
            Action::NavigateNextSibling => self.handle_navigate_next_sibling(),
            Action::NavigatePrevSibling => self.handle_navigate_prev_sibling(),
            Action::NewWorkspace => self.handle_new_workspace(),
            Action::CloseWorkspace => self.handle_close_workspace(),
            Action::NextWorkspace => self.next_workspace(),
            Action::PreviousWorkspace => self.previous_workspace(),
            Action::RenameWorkspace => {} // TODO: Implement rename UI
        }
    }

    fn handle_move_up(&mut self) {
        // Pan up in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace.graph_view_state.viewport.pan(0.0, -3.0);
            self.status_message = "Panned up".to_string();
        }
    }

    fn handle_move_down(&mut self) {
        // Pan down in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace.graph_view_state.viewport.pan(0.0, 3.0);
            self.status_message = "Panned down".to_string();
        }
    }

    fn handle_expand_node(&mut self) {
        // Tree view removed - no-op
    }

    fn handle_collapse_node(&mut self) {
        // Tree view removed - no-op
    }

    fn handle_expand_or_collapse(&mut self) {
        // Request call hierarchy for the selected node without changing the root
        let selected = self
            .get_current_workspace()
            .and_then(|w| w.graph_view_state.selected_node.clone());

        if let Some(selected) = selected {
            // Request call hierarchy for this node to load its data
            // This will populate the graph with children without changing the root
            self.request_call_hierarchy(&selected);

            // Mark layout dirty to refresh the view with updated data
            if let Some(workspace) = self.get_current_workspace_mut() {
                workspace.graph_view_state.mark_layout_dirty();
            }

            let name = self
                .call_graph
                .get_function(&selected)
                .map(|f| f.name.as_str())
                .unwrap_or("unknown");
            self.status_message = format!("Expanded node: {}", name);
        } else {
            self.status_message = "No node selected to expand".to_string();
        }
    }

    fn handle_find_references(&mut self) {
        let symbol_id = self
            .get_current_workspace()
            .and_then(|w| w.root_symbol.clone());

        if let Some(symbol_id) = symbol_id {
            self.request_references(&symbol_id);
        } else {
            self.status_message = "No function selected for finding references".to_string();
        }
    }

    fn handle_refresh(&mut self) {
        log::info!("Refresh action triggered - clearing state and requesting fresh data");
        // Clear all loading states and cached data to force fresh requests
        self.loading_states.clear();
        self.pending_requests.clear();

        // Request fresh workspace symbols to update the project state
        self.request_workspace_symbols();

        // If there's a selected function, refresh its call hierarchy
        if let Some(selected_id) = &self.selected_function.clone() {
            self.request_call_hierarchy(selected_id);
        }

        // Reset tree view expanded states to force reload when re-expanded
        for node in &mut self.tree_view_state.nodes {
            node.children_loaded = false;
            node.is_loading = false;
        }

        self.status_message = "Refreshing project data from LSP server...".to_string();
    }

    fn handle_toggle_call_direction(&mut self) {
        // Toggle direction and get the new direction value
        let new_direction = if let Some(workspace) = self.get_current_workspace_mut() {
            workspace.graph_view_state.toggle_direction();
            workspace.graph_view_state.mark_layout_dirty();
            Some(workspace.graph_view_state.direction)
        } else {
            None
        };

        // Update status message after releasing the mutable borrow
        if let Some(direction) = new_direction {
            let direction_str = match direction {
                CallDirection::Outgoing => "outgoing",
                CallDirection::Incoming => "incoming",
            };
            self.status_message = format!("Switched to {} calls view", direction_str);
        }
    }

    fn handle_move_left(&mut self) {
        // Pan left in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace.graph_view_state.viewport.pan(-5.0, 0.0);
            self.status_message = "Panned left".to_string();
        }
    }

    fn handle_move_right(&mut self) {
        // Pan right in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace.graph_view_state.viewport.pan(5.0, 0.0);
            self.status_message = "Panned right".to_string();
        }
    }

    fn handle_reset_view(&mut self) {
        // Capture viewport size before mutable borrow
        let viewport_size = self.last_viewport_size;
        if let Some(workspace) = self.get_current_workspace_mut() {
            // Recenter the viewport on the root node
            workspace.graph_view_state.recenter_viewport(viewport_size);
            self.status_message = "Reset view - centered on root".to_string();
        }
    }

    fn handle_navigate_parent(&mut self) {
        let current_idx = self.current_workspace_index;
        if let Some(workspace) = self.workspaces.get_mut(current_idx) {
            if workspace
                .graph_view_state
                .navigate_to_parent(&self.call_graph)
            {
                if let Some(selected) = &workspace.graph_view_state.selected_node {
                    let name = self
                        .call_graph
                        .get_function(selected)
                        .map(|f| f.name.as_str())
                        .unwrap_or("unknown");
                    self.status_message = format!("Navigated to parent: {}", name);
                } else {
                    self.status_message = "Navigated to parent".to_string();
                }
            } else {
                self.status_message = "No parent node (at root)".to_string();
            }
        }
    }

    fn handle_navigate_child(&mut self) {
        let current_idx = self.current_workspace_index;
        if let Some(workspace) = self.workspaces.get_mut(current_idx) {
            if workspace
                .graph_view_state
                .navigate_to_child(&self.call_graph)
            {
                if let Some(selected) = &workspace.graph_view_state.selected_node {
                    let name = self
                        .call_graph
                        .get_function(selected)
                        .map(|f| f.name.as_str())
                        .unwrap_or("unknown");
                    self.status_message = format!("Navigated to child: {}", name);
                } else {
                    self.status_message = "Navigated to child".to_string();
                }
            } else {
                self.status_message = "No child nodes".to_string();
            }
        }
    }

    fn handle_navigate_next_sibling(&mut self) {
        let current_idx = self.current_workspace_index;
        if let Some(workspace) = self.workspaces.get_mut(current_idx) {
            if workspace
                .graph_view_state
                .navigate_next_sibling(&self.call_graph)
            {
                if let Some(selected) = &workspace.graph_view_state.selected_node {
                    let name = self
                        .call_graph
                        .get_function(selected)
                        .map(|f| f.name.as_str())
                        .unwrap_or("unknown");
                    self.status_message = format!("Navigated to next sibling: {}", name);
                } else {
                    self.status_message = "Navigated to next sibling".to_string();
                }
            } else {
                self.status_message = "No sibling nodes".to_string();
            }
        }
    }

    fn handle_navigate_prev_sibling(&mut self) {
        let current_idx = self.current_workspace_index;
        if let Some(workspace) = self.workspaces.get_mut(current_idx) {
            if workspace
                .graph_view_state
                .navigate_prev_sibling(&self.call_graph)
            {
                if let Some(selected) = &workspace.graph_view_state.selected_node {
                    let name = self
                        .call_graph
                        .get_function(selected)
                        .map(|f| f.name.as_str())
                        .unwrap_or("unknown");
                    self.status_message = format!("Navigated to previous sibling: {}", name);
                } else {
                    self.status_message = "Navigated to previous sibling".to_string();
                }
            } else {
                self.status_message = "No sibling nodes".to_string();
            }
        }
    }

    fn handle_new_workspace(&mut self) {
        let name = format!("Graph {}", self.next_workspace_id);
        self.create_workspace(name);
    }

    fn handle_close_workspace(&mut self) {
        self.close_workspace(self.current_workspace_index);
    }

    pub fn start_call_graph_with_function(&mut self, symbol_id: SymbolId) {
        log::info!("Starting graph view with function: {:?}", symbol_id);

        // Get function name for workspace
        let function_name = self
            .call_graph
            .get_function(&symbol_id)
            .map(|f| {
                let name = f.name.clone();
                if name.len() > 20 {
                    format!("{}...", &name[..17])
                } else {
                    name
                }
            })
            .unwrap_or_else(|| format!("Graph {}", self.next_workspace_id));

        // Create new workspace with this function as root
        self.create_workspace_with_function(function_name, symbol_id.clone());
        self.selected_function = Some(symbol_id.clone());
        self.status_message = "Graph workspace created".to_string();

        log::info!("Graph workspace started with root: {:?}", symbol_id);
    }
}
