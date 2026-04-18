use tokio::sync::mpsc;

use ::lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspUiMessage, CallGraph, SymbolId};

/// Default viewport size used before the actual terminal size is known.
const DEFAULT_VIEWPORT_SIZE: (f32, f32) = (100.0, 100.0);

use crate::actions::TreeViewState;
use crate::search_bar::SearchBarState;

mod events;
mod lsp;
pub mod lsp_bridge;
mod update;
mod workspace;
pub mod workspace_manager;

pub use lsp_bridge::{LoadingState, LspBridge, PendingRequest};
pub use workspace_manager::WorkspaceManager;

/// Main application state
pub struct App {
    // Core data
    pub call_graph: CallGraph,
    pub selected_function: Option<SymbolId>,
    pub functions: Vec<SymbolId>,
    pub function_list_state: ratatui::widgets::ListState,
    pub tree_view_state: TreeViewState,

    // UI chrome
    pub should_quit: bool,
    pub show_help: bool,
    pub status_message: String,
    pub last_viewport_size: (f32, f32),

    // Search
    pub search_bar_state: SearchBarState,
    pub show_search_bar: bool,
    pub show_function_search: bool,
    pub function_search_query: String,
    pub search_query: String,

    // Extracted subsystems
    pub lsp: LspBridge,
    pub workspaces: WorkspaceManager,
}

impl App {
    pub fn new(call_graph: CallGraph) -> Self {
        let functions: Vec<SymbolId> = call_graph.nodes.keys().cloned().collect();

        let mut function_list_state = ratatui::widgets::ListState::default();
        if !functions.is_empty() {
            function_list_state.select(Some(0));
        }

        Self {
            call_graph,
            selected_function: None,
            functions,
            function_list_state,
            tree_view_state: TreeViewState::new(),
            should_quit: false,
            show_help: false,
            status_message: "Ready".to_string(),
            last_viewport_size: DEFAULT_VIEWPORT_SIZE,
            search_bar_state: SearchBarState::new(),
            show_search_bar: false,
            show_function_search: false,
            function_search_query: String::new(),
            search_query: String::new(),
            lsp: LspBridge::new(),
            workspaces: WorkspaceManager::new(),
        }
    }

    pub fn new_with_lsp_async(
        call_graph: CallGraph,
        lsp_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        lsp_channels_rx: mpsc::UnboundedReceiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    ) -> Self {
        let mut app = Self::new(call_graph);
        app.lsp = LspBridge::new_with_loader(lsp_rx, lsp_channels_rx);
        app
    }

    /// Poll and handle any messages from the background LSP loader
    pub fn poll_lsp_loader_messages(&mut self) {
        let new_symbols = self.lsp.poll_loader_messages();
        for symbol in new_symbols {
            let function = model::FunctionNode::new(
                symbol.name.clone(),
                symbol.qualified_name.clone(),
                symbol.location.clone(),
            );
            let id = self.call_graph.add_function(function);
            if !self.functions.contains(&id) {
                self.functions.push(id);
            }
        }
    }

    /// Poll for LSP channels and wire them up when they arrive
    pub fn poll_lsp_channels(&mut self) {
        self.lsp.poll_channels();
    }

    /// Set the LSP channels for communication
    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.lsp.set_channels(response_rx, request_tx);
    }

    pub fn select_function(&mut self, id: SymbolId) {
        self.selected_function = Some(id);
        self.status_message = "Function selected".to_string();
    }

    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    // Convenience delegation methods for WorkspaceManager

    pub fn get_current_workspace(&self) -> Option<&crate::graph_workspace::GraphWorkspace> {
        self.workspaces.current()
    }

    pub fn get_current_workspace_mut(
        &mut self,
    ) -> Option<&mut crate::graph_workspace::GraphWorkspace> {
        self.workspaces.current_mut()
    }

    pub fn create_workspace(&mut self, name: String) -> usize {
        let (id, msg) = self.workspaces.create(name);
        self.status_message = msg;
        id
    }

    pub fn create_workspace_with_function(&mut self, name: String, symbol: SymbolId) -> usize {
        let (id, msg) = self.workspaces.create_with_function(name, symbol);
        self.status_message = msg;
        id
    }

    pub fn close_workspace(&mut self, index: usize) -> bool {
        match self.workspaces.close(index) {
            Ok(msg) => {
                self.status_message = msg;
                true
            }
            Err(msg) => {
                self.status_message = msg;
                false
            }
        }
    }

    pub fn switch_workspace(&mut self, index: usize) -> bool {
        if let Some(msg) = self.workspaces.switch_to(index) {
            self.status_message = msg;
            true
        } else {
            false
        }
    }

    pub fn next_workspace(&mut self) {
        if let Some(msg) = self.workspaces.next() {
            self.status_message = msg;
        }
    }

    pub fn previous_workspace(&mut self) {
        if let Some(msg) = self.workspaces.previous() {
            self.status_message = msg;
        }
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
        assert_eq!(app.workspaces.workspaces.len(), 1);
        assert_eq!(app.workspaces.current_index, 0);
        assert!(app.selected_function.is_none());
        assert_eq!(app.search_query, "");
        assert!(!app.should_quit);
        assert!(!app.show_help);
    }

    #[test]
    fn test_workspace_creation() {
        let mut app = create_test_app();

        app.create_workspace("Test Workspace".to_string());
        assert_eq!(app.workspaces.workspaces.len(), 2);
        assert_eq!(app.workspaces.current_index, 1);
        assert_eq!(app.workspaces.workspaces[1].name, "Test Workspace");
    }

    #[test]
    fn test_workspace_switching() {
        let mut app = create_test_app();
        app.create_workspace("Workspace 2".to_string());

        app.next_workspace();
        assert_eq!(app.workspaces.current_index, 0);

        app.previous_workspace();
        assert_eq!(app.workspaces.current_index, 1);

        app.switch_workspace(0);
        assert_eq!(app.workspaces.current_index, 0);
    }

    #[test]
    fn test_workspace_closing() {
        let mut app = create_test_app();

        // Can't close last workspace
        assert!(!app.close_workspace(0));
        assert_eq!(app.workspaces.workspaces.len(), 1);

        // Add another workspace and close it
        app.create_workspace("Workspace 2".to_string());
        assert_eq!(app.workspaces.workspaces.len(), 2);

        assert!(app.close_workspace(1));
        assert_eq!(app.workspaces.workspaces.len(), 1);
    }

    #[test]
    fn test_minimum_one_workspace() {
        let mut app = create_test_app();

        // Try to close the only workspace - should fail
        assert!(!app.close_workspace(0));
        assert_eq!(app.workspaces.workspaces.len(), 1);
    }

    #[test]
    fn test_function_selection() {
        let mut app = create_test_app();
        let func_id = app.call_graph.nodes.keys().next().unwrap().clone();

        app.select_function(func_id.clone());
        assert_eq!(app.selected_function, Some(func_id));
        assert_eq!(app.status_message, "Function selected");
    }

    #[test]
    fn test_graph_view_initialization() {
        let mut app = create_test_app();
        let func_id = app.call_graph.nodes.keys().next().unwrap().clone();

        app.start_call_graph_with_function(func_id.clone());

        assert_eq!(app.selected_function, Some(func_id.clone()));
        // Should have created a new workspace
        assert_eq!(app.workspaces.workspaces.len(), 2);
        // New workspace should be current
        assert_eq!(app.workspaces.current_index, 1);
        // New workspace should have the function as root
        assert_eq!(app.workspaces.workspaces[1].root_symbol, Some(func_id));
    }

    #[test]
    fn test_action_handling() {
        let mut app = create_test_app();

        // Test help toggle
        app.handle_action(Action::Help);
        assert!(app.show_help);

        app.handle_action(Action::Help);
        assert!(!app.show_help);

        // Test quit
        app.handle_action(Action::Quit);
        assert!(app.should_quit);
    }
}
