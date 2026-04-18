use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::mpsc;

use lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallGraph, SymbolId};

use crate::actions::TreeViewState;
use crate::graph_workspace::GraphWorkspace;
use crate::search_bar::SearchBarState;

mod events;
mod lsp;
mod update;
mod workspace;

/// Loading state for LSP operations
#[derive(Debug, Clone, PartialEq)]
pub enum LoadingState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

/// Pending LSP request information
#[derive(Debug, Clone)]
pub struct PendingRequest {
    #[allow(dead_code)]
    pub(super) request_type: LspRequestType,
    pub(super) symbol_id: Option<SymbolId>,
    pub(super) timestamp: Instant,
}

/// Types of LSP requests
#[derive(Debug, Clone)]
pub enum LspRequestType {
    CallHierarchy,
    References,
    Symbols,
    Refresh,
}

/// Main application state
pub struct App {
    pub call_graph: CallGraph,
    pub selected_function: Option<SymbolId>,
    pub search_query: String,
    pub should_quit: bool,
    pub status_message: String,
    pub function_list_state: ratatui::widgets::ListState,
    pub functions: Vec<SymbolId>,
    pub tree_view_state: TreeViewState,
    pub show_help: bool,

    // Workspace management
    pub workspaces: Vec<GraphWorkspace>,
    pub current_workspace_index: usize,
    pub next_workspace_id: usize,
    pub show_workspace_manager: bool,
    pub show_function_search: bool,
    pub function_search_query: String,

    // Search bar
    pub search_bar_state: SearchBarState,
    pub show_search_bar: bool,

    // Last viewport size for recentering
    pub last_viewport_size: (f32, f32),

    // Lazy loading fields
    pub loading_states: HashMap<SymbolId, LoadingState>,
    pub lsp_response_rx: Option<mpsc::UnboundedReceiver<LspResponse>>,
    pub lsp_request_tx: Option<mpsc::UnboundedSender<LspRequest>>,
    pub pending_requests: HashMap<String, PendingRequest>,
    pub opened_documents: std::collections::HashSet<lsp_types::Url>,

    // Async LSP loading state
    pub lsp_status: LspLoadPhase,
    pub lsp_rx: Option<mpsc::UnboundedReceiver<LspUiMessage>>,
    pub lsp_channels_rx: Option<
        mpsc::UnboundedReceiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    >,
}

impl App {
    pub fn new(call_graph: CallGraph) -> Self {
        // Get all function IDs for the list (avoiding expensive operations)
        let functions: Vec<SymbolId> = call_graph.nodes.keys().cloned().collect();

        // Initialize list state
        let mut function_list_state = ratatui::widgets::ListState::default();
        if !functions.is_empty() {
            function_list_state.select(Some(0));
        }

        // Initialize with one default workspace
        let default_workspace = GraphWorkspace::new(1, "Graph 1".to_string());
        let workspaces = vec![default_workspace];

        Self {
            call_graph,
            selected_function: None,
            search_query: String::new(),
            should_quit: false,
            status_message: "Ready".to_string(),
            function_list_state,
            functions,
            tree_view_state: TreeViewState::new(),
            show_help: false,
            workspaces,
            current_workspace_index: 0,
            next_workspace_id: 2,
            show_workspace_manager: false,
            show_function_search: false,
            function_search_query: String::new(),
            search_bar_state: SearchBarState::new(),
            show_search_bar: false,
            last_viewport_size: (100.0, 100.0), // Default size, will be updated on first render
            loading_states: HashMap::new(),
            lsp_response_rx: None,
            lsp_request_tx: None,
            pending_requests: HashMap::new(),
            opened_documents: std::collections::HashSet::new(),
            lsp_status: LspLoadPhase::NotStarted,
            lsp_rx: None,
            lsp_channels_rx: None,
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
        app.lsp_status = LspLoadPhase::NotStarted;
        app.lsp_rx = Some(lsp_rx);
        app.lsp_channels_rx = Some(lsp_channels_rx);
        app
    }

    /// Poll and handle any messages from the background LSP loader
    pub fn poll_lsp_loader_messages(&mut self) {
        if let Some(rx) = &mut self.lsp_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    LspUiMessage::Progress(phase) => {
                        self.lsp_status = phase;
                    }
                    LspUiMessage::AddFunction(symbol) => {
                        // Mirror into the call graph for now
                        let function = model::FunctionNode::new(
                            symbol.name.clone(),
                            symbol.qualified_name.clone(),
                            symbol.location.clone(),
                        );
                        let id = self.call_graph.add_function(function);
                        self.functions.push(id);
                    }
                }
            }
        }
    }

    /// Poll for LSP channels and wire them up when they arrive
    pub fn poll_lsp_channels(&mut self) {
        if let Some(rx) = &mut self.lsp_channels_rx {
            if let Ok((response_rx, request_tx)) = rx.try_recv() {
                log::info!("Received LSP channels from loader - wiring up for lazy loading");
                self.set_lsp_channels(response_rx, request_tx);
                // Clear the receiver since we only need to do this once
                self.lsp_channels_rx = None;
            }
        }
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
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.current_workspace_index, 0);
        assert!(app.selected_function.is_none());
        assert_eq!(app.search_query, "");
        assert!(!app.should_quit);
        assert!(!app.show_help);
    }

    #[test]
    fn test_workspace_creation() {
        let mut app = create_test_app();

        app.create_workspace("Test Workspace".to_string());
        assert_eq!(app.workspaces.len(), 2);
        assert_eq!(app.current_workspace_index, 1);
        assert_eq!(app.workspaces[1].name, "Test Workspace");
    }

    #[test]
    fn test_workspace_switching() {
        let mut app = create_test_app();
        app.create_workspace("Workspace 2".to_string());

        app.next_workspace();
        assert_eq!(app.current_workspace_index, 0);

        app.previous_workspace();
        assert_eq!(app.current_workspace_index, 1);

        app.switch_workspace(0);
        assert_eq!(app.current_workspace_index, 0);
    }

    #[test]
    fn test_workspace_closing() {
        let mut app = create_test_app();

        // Can't close last workspace
        assert!(!app.close_workspace(0));
        assert_eq!(app.workspaces.len(), 1);

        // Add another workspace and close it
        app.create_workspace("Workspace 2".to_string());
        assert_eq!(app.workspaces.len(), 2);

        assert!(app.close_workspace(1));
        assert_eq!(app.workspaces.len(), 1);
    }

    #[test]
    fn test_minimum_one_workspace() {
        let mut app = create_test_app();

        // Try to close the only workspace - should fail
        assert!(!app.close_workspace(0));
        assert_eq!(app.workspaces.len(), 1);
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
        assert_eq!(app.workspaces.len(), 2);
        // New workspace should be current
        assert_eq!(app.current_workspace_index, 1);
        // New workspace should have the function as root
        assert_eq!(app.workspaces[1].root_symbol, Some(func_id));
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
