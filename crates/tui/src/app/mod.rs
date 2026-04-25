use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

use ::lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspUiMessage, CallGraph, SymbolId};

use crate::graph_view::{GraphViewState, LayoutSettings};
use crate::search_bar::SearchBarState;
use crate::status_message::StatusMessage;

mod event_translate;
mod events;
mod lsp;
pub mod lsp_bridge;
mod update;
mod workspace;

pub use event_translate::{app_event_to_status, apply_events};
pub use lsp_bridge::{LoadingState, LspBridge, PendingRequest};

/// Default viewport size used before the actual terminal size is known.
const DEFAULT_VIEWPORT_SIZE: (f32, f32) = (100.0, 100.0);

/// Main application state
pub struct App {
    /// Frontend-agnostic core: owns call_graph, selected_function, lsp, core workspaces.
    pub core: ::core::CoreApp,

    // UI chrome
    pub should_quit: bool,
    pub show_help: bool,
    pub status_message: StatusMessage,
    pub last_viewport_size: (f32, f32),

    // Search
    pub search_bar_state: SearchBarState,
    pub show_search_bar: bool,

    /// Per-workspace view state (GraphViewState), keyed by CoreWorkspace id.
    pub view_states: HashMap<usize, GraphViewState>,

    /// Whether the workspace-manager modal overlay is visible.
    pub show_workspace_manager: bool,

    /// Layout settings used when creating new GraphViewState instances.
    pub layout_settings: LayoutSettings,
}

impl App {
    pub fn new(call_graph: CallGraph) -> Self {
        Self::new_with_settings(call_graph, LayoutSettings::default())
    }

    pub fn new_with_settings(call_graph: CallGraph, settings: LayoutSettings) -> Self {
        let core = ::core::CoreApp::new(call_graph);
        let initial_id = core.workspaces.current().map(|w| w.id).unwrap_or(1);
        let mut view_states = HashMap::new();
        view_states.insert(initial_id, GraphViewState::new(&settings));
        Self {
            core,
            should_quit: false,
            show_help: false,
            status_message: StatusMessage::Ready,
            last_viewport_size: DEFAULT_VIEWPORT_SIZE,
            search_bar_state: SearchBarState::new(),
            show_search_bar: false,
            view_states,
            show_workspace_manager: false,
            layout_settings: settings,
        }
    }

    pub fn new_with_lsp_async(
        call_graph: CallGraph,
        lsp_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        lsp_channels_rx: oneshot::Receiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
        settings: LayoutSettings,
    ) -> Self {
        let lsp = LspBridge::new_with_loader(lsp_rx, lsp_channels_rx);
        let core = ::core::CoreApp::new_with_lsp(call_graph, lsp);
        let initial_id = core.workspaces.current().map(|w| w.id).unwrap_or(1);
        let mut view_states = HashMap::new();
        view_states.insert(initial_id, GraphViewState::new(&settings));
        Self {
            core,
            should_quit: false,
            show_help: false,
            status_message: StatusMessage::Ready,
            last_viewport_size: DEFAULT_VIEWPORT_SIZE,
            search_bar_state: SearchBarState::new(),
            show_search_bar: false,
            view_states,
            show_workspace_manager: false,
            layout_settings: settings,
        }
    }

    /// Set the LSP channels for communication
    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.core.lsp.set_channels(response_rx, request_tx);
    }

    pub fn select_function(&mut self, id: SymbolId) {
        self.core.selected_function = Some(id);
        self.status_message = StatusMessage::FunctionSelected;
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    // -------------------------------------------------------------------------
    // View-state helpers
    // -------------------------------------------------------------------------

    /// Id of the currently active workspace, if any.
    pub fn current_workspace_id(&self) -> Option<usize> {
        self.core.workspaces.current().map(|w| w.id)
    }

    /// Immutable reference to the current workspace's view state.
    pub fn current_view_state(&self) -> Option<&GraphViewState> {
        let id = self.core.workspaces.current()?.id;
        self.view_states.get(&id)
    }

    /// Mutable reference to the current workspace's view state.
    pub fn current_view_state_mut(&mut self) -> Option<&mut GraphViewState> {
        let id = self.core.workspaces.current()?.id;
        self.view_states.get_mut(&id)
    }

    pub fn start_call_graph_with_function(&mut self, symbol_id: SymbolId) {
        log::info!("Starting graph view with function: {:?}", symbol_id);
        let events = self
            .core
            .dispatch(::core::Command::StartGraphFromFunction(symbol_id));
        apply_events(self, events);
        self.status_message = StatusMessage::GraphWorkspaceCreated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Action;
    use model::{FunctionNode, Location};

    fn create_test_app() -> App {
        let mut graph = CallGraph::new();
        let func = FunctionNode::new(
            "test_func".to_string(),
            "test::test_func".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        graph.add_function(func);
        App::new(graph)
    }

    #[test]
    fn test_app_creation() {
        let app = create_test_app();
        assert_eq!(app.core.workspaces.workspaces.len(), 1);
        assert_eq!(app.core.workspaces.current_index, 0);
        assert!(app.core.selected_function.is_none());
        assert_eq!(app.search_bar_state.query, "");
        assert!(!app.should_quit);
        assert!(!app.show_help);
        assert_eq!(app.view_states.len(), 1);
    }

    #[test]
    fn test_workspace_creation() {
        let mut app = create_test_app();

        let events = app.core.dispatch(::core::Command::NewWorkspace);
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.workspaces.len(), 2);
        assert_eq!(app.core.workspaces.current_index, 1);
        assert_eq!(app.view_states.len(), 2);
    }

    #[test]
    fn test_workspace_switching() {
        let mut app = create_test_app();
        let events = app.core.dispatch(::core::Command::NewWorkspace);
        apply_events(&mut app, events);

        let events = app.core.dispatch(::core::Command::NextWorkspace);
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.current_index, 0);

        let events = app.core.dispatch(::core::Command::PreviousWorkspace);
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.current_index, 1);

        let events = app.core.dispatch(::core::Command::SwitchWorkspace(0));
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.current_index, 0);
    }

    #[test]
    fn test_workspace_closing() {
        let mut app = create_test_app();

        // Can't close last workspace
        let events = app.core.dispatch(::core::Command::CloseWorkspace(0));
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.workspaces.len(), 1);

        // Add another workspace and close it
        let events = app.core.dispatch(::core::Command::NewWorkspace);
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.workspaces.len(), 2);

        let events = app.core.dispatch(::core::Command::CloseWorkspace(1));
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.workspaces.len(), 1);
    }

    #[test]
    fn test_minimum_one_workspace() {
        let mut app = create_test_app();

        let events = app.core.dispatch(::core::Command::CloseWorkspace(0));
        apply_events(&mut app, events);
        assert_eq!(app.core.workspaces.workspaces.len(), 1);
    }

    #[test]
    fn test_function_selection() {
        let mut app = create_test_app();
        let func_id = *app.core.call_graph.function_ids().next().unwrap();

        app.select_function(func_id);
        assert_eq!(app.core.selected_function, Some(func_id));
        assert_eq!(app.status_message, StatusMessage::FunctionSelected);
    }

    #[test]
    fn test_graph_view_initialization() {
        let mut app = create_test_app();
        let func_id = *app.core.call_graph.function_ids().next().unwrap();

        app.start_call_graph_with_function(func_id);

        assert_eq!(app.core.selected_function, Some(func_id));
        // Should have created a new workspace
        assert_eq!(app.core.workspaces.workspaces.len(), 2);
        // New workspace should be current
        assert_eq!(app.core.workspaces.current_index, 1);
        // New workspace should have the function as root
        assert_eq!(app.core.workspaces.workspaces[1].root_symbol, Some(func_id));
        // view_states should have entries for both workspaces
        assert_eq!(app.view_states.len(), 2);
    }

    #[test]
    fn test_action_handling() {
        let mut app = create_test_app();

        app.handle_action(Action::Help);
        assert!(app.show_help);

        app.handle_action(Action::Help);
        assert!(!app.show_help);

        app.handle_action(Action::Quit);
        assert!(app.should_quit);
    }
}
