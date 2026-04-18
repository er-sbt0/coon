use model::SymbolId;

use crate::actions::Action;
use crate::graph_adapter::CallDirection;
use crate::graph_view::GraphViewState;
use crate::status_message::StatusMessage;

use super::App;

/// Vertical pan distance (in graph units) per Up/Down key press.
const PAN_DELTA_VERTICAL: f32 = 3.0;
/// Horizontal pan distance (in graph units) per Left/Right key press.
const PAN_DELTA_HORIZONTAL: f32 = 5.0;

impl App {
    pub fn handle_action(&mut self, action: Action) {
        log::info!("Handling action: {:?}", action);
        match action {
            // Viewport panning
            Action::MoveUp => self.handle_move_up(),
            Action::MoveDown => self.handle_move_down(),
            Action::MoveLeft => self.handle_move_left(),
            Action::MoveRight => self.handle_move_right(),

            // Tree interaction
            Action::ExpandOrCollapse => self.handle_expand_or_collapse(),
            Action::FindReferences => self.handle_find_references(),
            Action::Refresh => self.handle_refresh(),
            Action::Quit => self.quit(),
            Action::Help => self.toggle_help(),
            Action::ToggleCallDirection => self.handle_toggle_call_direction(),
            Action::ResetView => self.handle_reset_view(),

            // Graph navigation (hjkl)
            Action::NavigateParent => self.handle_navigate_parent(),
            Action::NavigateChild => self.handle_navigate_child(),
            Action::NavigateNextSibling => self.handle_navigate_next_sibling(),
            Action::NavigatePrevSibling => self.handle_navigate_prev_sibling(),

            // Workspace management
            Action::NewWorkspace => self.handle_new_workspace(),
            Action::CloseWorkspace => self.handle_close_workspace(),
            Action::NextWorkspace => self.next_workspace(),
            Action::PreviousWorkspace => self.previous_workspace(),
            Action::RenameWorkspace => {} // TODO: Implement rename UI
            Action::SwitchWorkspace(index) => {
                self.switch_workspace(index);
            }

            // Search bar
            Action::ToggleSearch => self.toggle_search_bar(),
            Action::SearchConfirm => self.select_from_search(),
            Action::SearchPrevResult => self.search_bar_state.select_previous(),
            Action::SearchNextResult => self.search_bar_state.select_next(),
            Action::SearchCycleMode => {
                self.search_bar_state.cycle_search_mode();
                self.search_bar_state.update_results(&self.call_graph);
            }
            Action::SearchBackspace => self.handle_search_backspace(),
            Action::SearchDeleteForward => {
                self.search_bar_state.delete_char_forward();
                self.search_bar_state.update_results(&self.call_graph);
            }
            Action::SearchCursorLeft => self.search_bar_state.move_cursor_left(),
            Action::SearchCursorRight => self.search_bar_state.move_cursor_right(),
            Action::SearchCursorHome => self.search_bar_state.move_cursor_start(),
            Action::SearchCursorEnd => self.search_bar_state.move_cursor_end(),
            Action::SearchInput(c) => self.handle_search_input(c),
        }
    }

    fn handle_move_up(&mut self) {
        // Pan up in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace
                .graph_view_state
                .viewport
                .pan(0.0, -PAN_DELTA_VERTICAL);
            self.status_message = StatusMessage::PannedUp;
        }
    }

    fn handle_move_down(&mut self) {
        // Pan down in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace
                .graph_view_state
                .viewport
                .pan(0.0, PAN_DELTA_VERTICAL);
            self.status_message = StatusMessage::PannedDown;
        }
    }

    fn handle_expand_or_collapse(&mut self) {
        // Request call hierarchy for the selected node without changing the root
        let selected = self
            .get_current_workspace()
            .and_then(|w| w.graph_view_state.selected_node);

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
            self.status_message = StatusMessage::ExpandedNode {
                name: name.to_string(),
            };
        } else {
            self.status_message = StatusMessage::NoNodeSelected;
        }
    }

    fn handle_find_references(&mut self) {
        let symbol_id = self.get_current_workspace().and_then(|w| w.root_symbol);

        if let Some(symbol_id) = symbol_id {
            self.request_references(&symbol_id);
        } else {
            self.status_message = StatusMessage::NoFunctionSelectedForReferences;
        }
    }

    fn handle_refresh(&mut self) {
        log::info!("Refresh action triggered - clearing state and requesting fresh data");
        // Clear all loading states and cached data to force fresh requests
        self.lsp.clear();

        // Request fresh workspace symbols to update the project state
        self.request_workspace_symbols();

        // If there's a selected function, refresh its call hierarchy
        if let Some(selected_id) = self.selected_function {
            self.request_call_hierarchy(&selected_id);
        }

        self.status_message = StatusMessage::RefreshingProject;
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
            self.status_message = StatusMessage::SwitchedCallDirection {
                direction: direction_str.to_string(),
            };
        }
    }

    fn handle_move_left(&mut self) {
        // Pan left in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace
                .graph_view_state
                .viewport
                .pan(-PAN_DELTA_HORIZONTAL, 0.0);
            self.status_message = StatusMessage::PannedLeft;
        }
    }

    fn handle_move_right(&mut self) {
        // Pan right in graph view
        if let Some(workspace) = self.get_current_workspace_mut() {
            workspace
                .graph_view_state
                .viewport
                .pan(PAN_DELTA_HORIZONTAL, 0.0);
            self.status_message = StatusMessage::PannedRight;
        }
    }

    fn handle_reset_view(&mut self) {
        // Capture viewport size before mutable borrow
        let viewport_size = self.last_viewport_size;
        if let Some(workspace) = self.get_current_workspace_mut() {
            // Recenter the viewport on the root node
            workspace.graph_view_state.recenter_viewport(viewport_size);
            self.status_message = StatusMessage::ResetView;
        }
    }

    fn navigate_and_report(
        &mut self,
        op: fn(&mut GraphViewState) -> bool,
        success_prefix: &str,
        fail_msg: &str,
    ) {
        let current_idx = self.workspaces.current_index;
        if let Some(workspace) = self.workspaces.workspaces.get_mut(current_idx) {
            if op(&mut workspace.graph_view_state) {
                if let Some(selected) = &workspace.graph_view_state.selected_node {
                    let name = self
                        .call_graph
                        .get_function(selected)
                        .map(|f| f.name.as_str())
                        .unwrap_or("unknown");
                    self.status_message = StatusMessage::Navigated {
                        description: success_prefix.to_string(),
                        name: Some(name.to_string()),
                    };
                } else {
                    self.status_message = StatusMessage::Navigated {
                        description: success_prefix.to_string(),
                        name: None,
                    };
                }
            } else {
                self.status_message = StatusMessage::NavigationFailed {
                    reason: fail_msg.to_string(),
                };
            }
        }
    }

    fn handle_navigate_parent(&mut self) {
        self.navigate_and_report(
            GraphViewState::navigate_to_parent,
            "Navigated to parent",
            "No parent node (at root)",
        );
    }

    fn handle_navigate_child(&mut self) {
        self.navigate_and_report(
            GraphViewState::navigate_to_child,
            "Navigated to child",
            "No child nodes",
        );
    }

    fn handle_navigate_next_sibling(&mut self) {
        self.navigate_and_report(
            GraphViewState::navigate_next_sibling,
            "Navigated to next sibling",
            "No sibling nodes",
        );
    }

    fn handle_navigate_prev_sibling(&mut self) {
        self.navigate_and_report(
            GraphViewState::navigate_prev_sibling,
            "Navigated to previous sibling",
            "No sibling nodes",
        );
    }

    fn handle_new_workspace(&mut self) {
        let name = format!("Graph {}", self.workspaces.next_id);
        self.create_workspace(name);
    }

    fn handle_close_workspace(&mut self) {
        let idx = self.workspaces.current_index;
        self.close_workspace(idx);
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
            .unwrap_or_else(|| format!("Graph {}", self.workspaces.next_id));

        // Create new workspace with this function as root
        self.create_workspace_with_function(function_name, symbol_id);
        self.selected_function = Some(symbol_id);
        self.status_message = StatusMessage::GraphWorkspaceCreated;

        log::info!("Graph workspace started with root: {:?}", symbol_id);
    }
}
