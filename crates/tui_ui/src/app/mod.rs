use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::mpsc;

use lsp_integration::{LspRequest, LspResponse};
use model::{lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallGraph, SymbolId};

use crate::graph_adapter::CallDirection;
use crate::graph_workspace::GraphWorkspace;
use crate::search_bar::SearchBarState;
use crate::actions::{Action, TreeViewState};

mod events;
mod update;

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

    // Lazy loading methods for LSP integration

    /// Set the LSP channels for communication
    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.lsp_response_rx = Some(response_rx);
        self.lsp_request_tx = Some(request_tx);
    }

    /// Request call hierarchy for a function
    pub fn request_call_hierarchy(&mut self, function_id: &SymbolId) {
        // First, gather all needed data from the function
        let (file_path, line, column, _name) = {
            if let Some(function) = self.call_graph.nodes.get(function_id) {
                (
                    function.definition_location.file_path.clone(),
                    function.definition_location.line,
                    function.definition_location.column,
                    function.name.clone(),
                )
            } else {
                return;
            }
        };

        // Check if we have the request channel
        let request_tx = if let Some(tx) = &self.lsp_request_tx {
            tx.clone()
        } else {
            return;
        };

        // Get document URI and position
        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.loading_states.insert(
                    function_id.clone(),
                    LoadingState::Failed("Invalid file path".to_string()),
                );
                self.update_node_loading_state(function_id, false);
                return;
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1), // LSP is 0-indexed
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        // Record pending request with timestamp for timeout handling
        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                request_type: LspRequestType::CallHierarchy,
                symbol_id: Some(function_id.clone()),
                timestamp: Instant::now(),
            },
        );

        // Update UI to show loading state
        self.update_node_loading_state(function_id, true);

        // Send request via channel (non-blocking)
        let request = LspRequest::GetCallHierarchy {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            self.loading_states.insert(
                function_id.clone(),
                LoadingState::Failed("Failed to send request".to_string()),
            );
            self.update_node_loading_state(function_id, false);
        }
    }

    /// Request references for a function
    pub fn request_references(&mut self, function_id: &SymbolId) {
        // First, gather all needed data from the function
        let (file_path, line, column, name) = {
            if let Some(function) = self.call_graph.nodes.get(function_id) {
                (
                    function.definition_location.file_path.clone(),
                    function.definition_location.line,
                    function.definition_location.column,
                    function.name.clone(),
                )
            } else {
                return;
            }
        };

        // Check if we have the request channel
        let request_tx = if let Some(tx) = &self.lsp_request_tx {
            tx.clone()
        } else {
            return;
        };

        // Get document URI and position
        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.status_message = "Failed to create URI from file path".to_string();
                return;
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1), // LSP is 0-indexed
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        // Record pending request
        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                request_type: LspRequestType::References,
                symbol_id: Some(function_id.clone()),
                timestamp: Instant::now(),
            },
        );

        // Update status message to show we're loading
        self.status_message = format!("Finding references for '{}'...", name);

        // Send request via channel (non-blocking)
        let request = LspRequest::FindReferencesWithSymbols {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            self.status_message = "Failed to send request".to_string();
        }
    }

    /// Request fresh workspace symbols from LSP server
    pub fn request_workspace_symbols(&mut self) {
        // Check if we have the request channel
        let request_tx = if let Some(tx) = &self.lsp_request_tx {
            tx.clone()
        } else {
            return;
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        // Record pending request
        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                request_type: LspRequestType::Symbols,
                symbol_id: None,
                timestamp: Instant::now(),
            },
        );

        // Update status message
        self.status_message = "Refreshing workspace symbols...".to_string();

        // Send request via channel (non-blocking)
        let request = LspRequest::GetWorkspaceSymbols {
            request_id: request_id.clone(),
            query: "".to_string(),
        };

        log::info!("TUI sending workspace symbols request: {}", request_id);
        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            self.status_message = "Failed to send request".to_string();
        } else {
            log::info!("Successfully sent workspace symbols request to channel");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
