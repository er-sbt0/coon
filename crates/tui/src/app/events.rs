use crate::actions::Action;
use crate::graph_view::GraphViewState;
use crate::status_message::StatusMessage;

use super::apply_events;
use super::App;

/// Vertical pan distance (in graph units) per Up/Down key press.
const PAN_DELTA_VERTICAL: f32 = 3.0;
/// Horizontal pan distance (in graph units) per Left/Right key press.
const PAN_DELTA_HORIZONTAL: f32 = 5.0;

impl App {
    pub fn handle_action(&mut self, action: Action) {
        log::info!("Handling action: {:?}", action);
        match action {
            // Viewport panning — handled in-place on TUI state, no core dispatch.
            Action::MoveUp => self.handle_move_up(),
            Action::MoveDown => self.handle_move_down(),
            Action::MoveLeft => self.handle_move_left(),
            Action::MoveRight => self.handle_move_right(),

            // Tree interaction — route through CoreApp
            Action::ExpandOrCollapse => self.handle_expand_or_collapse(),
            Action::FindReferences => self.dispatch_cmd(::core::Command::FindReferences),
            Action::Refresh => self.dispatch_cmd(::core::Command::Refresh),
            Action::Quit => self.quit(),
            Action::Help => self.toggle_help(),
            Action::ToggleCallDirection => self.dispatch_cmd(::core::Command::ToggleCallDirection),
            Action::ResetView => self.handle_reset_view(),

            // Graph navigation (hjkl)
            Action::NavigateParent => self.handle_navigate_parent(),
            Action::NavigateChild => self.handle_navigate_child(),
            Action::NavigateNextSibling => self.handle_navigate_next_sibling(),
            Action::NavigatePrevSibling => self.handle_navigate_prev_sibling(),
            Action::CycleParent => self.handle_cycle_parent(),

            // Workspace management — route through CoreApp
            Action::NewWorkspace => self.dispatch_cmd(::core::Command::NewWorkspace),
            Action::CloseWorkspace => {
                let idx = self.core.workspaces.current_index;
                self.dispatch_cmd(::core::Command::CloseWorkspace(idx));
            }
            Action::NextWorkspace => self.dispatch_cmd(::core::Command::NextWorkspace),
            Action::PreviousWorkspace => self.dispatch_cmd(::core::Command::PreviousWorkspace),
            Action::SwitchWorkspace(index) => {
                self.dispatch_cmd(::core::Command::SwitchWorkspace(index));
            }

            // Search bar — handled entirely in TUI
            Action::ToggleSearch => self.toggle_search_bar(),
            Action::SearchConfirm => self.select_from_search(),
            Action::SearchPrevResult => self.search_bar_state.select_previous(),
            Action::SearchNextResult => self.search_bar_state.select_next(),
            Action::SearchCycleMode => {
                self.search_bar_state.cycle_search_mode();
                self.search_bar_state.update_results(&self.core.call_graph);
            }
            Action::SearchBackspace => self.handle_search_backspace(),
            Action::SearchDeleteForward => {
                self.search_bar_state.delete_char_forward();
                self.search_bar_state.update_results(&self.core.call_graph);
            }
            Action::SearchCursorLeft => self.search_bar_state.move_cursor_left(),
            Action::SearchCursorRight => self.search_bar_state.move_cursor_right(),
            Action::SearchCursorHome => self.search_bar_state.move_cursor_start(),
            Action::SearchCursorEnd => self.search_bar_state.move_cursor_end(),
            Action::SearchInput(c) => self.handle_search_input(c),
        }
    }

    /// Dispatch a `Command` through `CoreApp` and apply the resulting events.
    fn dispatch_cmd(&mut self, cmd: ::core::Command) {
        let events = self.core.dispatch(cmd);
        apply_events(self, events);
    }

    fn handle_move_up(&mut self) {
        if let Some(vs) = self.current_view_state_mut() {
            vs.viewport.pan(0.0, -PAN_DELTA_VERTICAL);
            self.status_message = StatusMessage::PannedUp;
        }
    }

    fn handle_move_down(&mut self) {
        if let Some(vs) = self.current_view_state_mut() {
            vs.viewport.pan(0.0, PAN_DELTA_VERTICAL);
            self.status_message = StatusMessage::PannedDown;
        }
    }

    fn handle_expand_or_collapse(&mut self) {
        let selected = self.current_view_state().and_then(|vs| vs.selected_node);

        if let Some(selected) = selected {
            self.request_call_hierarchy(&selected);

            if let Some(vs) = self.current_view_state_mut() {
                vs.mark_layout_dirty();
            }

            let name = self
                .core
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

    fn handle_move_left(&mut self) {
        if let Some(vs) = self.current_view_state_mut() {
            vs.viewport.pan(-PAN_DELTA_HORIZONTAL, 0.0);
            self.status_message = StatusMessage::PannedLeft;
        }
    }

    fn handle_move_right(&mut self) {
        if let Some(vs) = self.current_view_state_mut() {
            vs.viewport.pan(PAN_DELTA_HORIZONTAL, 0.0);
            self.status_message = StatusMessage::PannedRight;
        }
    }

    fn handle_reset_view(&mut self) {
        let viewport_size = self.last_viewport_size;
        if let Some(vs) = self.current_view_state_mut() {
            vs.recenter_viewport(viewport_size);
            self.status_message = StatusMessage::ResetView;
        }
    }

    fn navigate_and_report(
        &mut self,
        op: fn(&mut GraphViewState) -> bool,
        success_prefix: &str,
        fail_msg: &str,
    ) {
        let id = match self.core.workspaces.current() {
            Some(w) => w.id,
            None => return,
        };
        if let Some(vs) = self.view_states.get_mut(&id) {
            if op(vs) {
                let name = vs
                    .selected_node
                    .as_ref()
                    .and_then(|s| self.core.call_graph.get_function(s))
                    .map(|f| f.name.clone());
                self.status_message = StatusMessage::Navigated {
                    description: success_prefix.to_string(),
                    name,
                };
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

    fn handle_cycle_parent(&mut self) {
        let id = match self.core.workspaces.current() {
            Some(w) => w.id,
            None => return,
        };
        if let Some(vs) = self.view_states.get_mut(&id) {
            if vs.cycle_active_parent() {
                let parent_name = vs
                    .active_parent_symbol()
                    .and_then(|psym| self.core.call_graph.get_function(&psym))
                    .map(|f| f.name.clone());
                self.status_message = StatusMessage::Navigated {
                    description: "Active parent switched to".to_string(),
                    name: parent_name,
                };
            } else {
                self.status_message = StatusMessage::NavigationFailed {
                    reason: "Node has only one parent".to_string(),
                };
            }
        }
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
}
