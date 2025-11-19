use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::io;
use std::time::Instant;
use tokio::sync::mpsc;

use core_data::{lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallGraph, SymbolId};
use lsp_integration::{LspRequest, LspResponse};

pub mod actions;
pub mod call_graph_view;
pub mod function_list;
pub mod graph_adapter;
pub mod graph_view;
pub mod graph_workspace;
pub mod search_bar;

pub use actions::{Action, TreeNode, TreeViewState};
pub use call_graph_view::CallGraphView;
pub use function_list::FunctionList;
pub use graph_adapter::{CallDirection, CallGraphAdapter};
pub use graph_view::{GraphView, GraphViewState};
pub use graph_workspace::GraphWorkspace;
pub use search_bar::{SearchBar, SearchBarState};

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
    request_type: LspRequestType,
    symbol_id: Option<SymbolId>,
    timestamp: Instant,
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
                        let function = core_data::FunctionNode::new(
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

    /// Check for LSP responses and update state
    pub fn check_lsp_responses(&mut self) {
        // First, check for timed out requests
        self.check_timed_out_requests();

        // Process all available responses from the channel
        if let Some(response_rx) = &mut self.lsp_response_rx {
            let mut responses = Vec::new();

            // Collect all available responses without blocking
            while let Ok(response) = response_rx.try_recv() {
                log::info!("TUI received LSP response: {:?}", response);
                responses.push(response);
            }

            if !responses.is_empty() {
                log::info!("TUI processing {} LSP responses", responses.len());
            }

            // Process responses after collecting them all
            for response in responses {
                self.handle_lsp_response(response);
            }
        }
    }

    /// Check for timed out requests and mark them as failed
    fn check_timed_out_requests(&mut self) {
        let timeout_duration = std::time::Duration::from_secs(30); // 30 second timeout
        let now = Instant::now();
        let mut timed_out_requests = Vec::new();

        // Find timed out requests
        for (request_id, pending_request) in &self.pending_requests {
            if now.duration_since(pending_request.timestamp) > timeout_duration {
                timed_out_requests.push((request_id.clone(), pending_request.clone()));
            }
        }

        // Handle timed out requests
        for (request_id, pending_request) in timed_out_requests {
            self.pending_requests.remove(&request_id);

            if let Some(symbol_id) = pending_request.symbol_id {
                self.update_loading_state(
                    &symbol_id,
                    LoadingState::Failed("Request timed out".to_string()),
                );
            }

            self.status_message = "LSP request timed out".to_string();
        }
    }

    /// Handle an LSP response
    fn handle_lsp_response(&mut self, response: LspResponse) {
        log::info!("TUI handling LSP response: {:?}", response);
        match response {
            LspResponse::CallHierarchy { request_id, items } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!(
                            "Received call hierarchy prepare response for symbol: {:?}, {} items",
                            symbol_id,
                            items.len()
                        );

                        // If we got call hierarchy items, automatically request calls based on direction
                        if !items.is_empty() {
                            let first_item = items[0].clone();

                            // Get the current workspace direction
                            let current_direction = self
                                .get_current_workspace()
                                .map(|w| w.graph_view_state.direction)
                                .unwrap_or(CallDirection::Incoming);

                            match current_direction {
                                CallDirection::Outgoing => {
                                    log::info!(
                                        "Requesting outgoing calls for: {}",
                                        first_item.name
                                    );

                                    // Generate a new request ID for the outgoing calls request
                                    let outgoing_request_id = uuid::Uuid::new_v4().to_string();

                                    // Record pending request for outgoing calls
                                    self.pending_requests.insert(
                                        outgoing_request_id.clone(),
                                        PendingRequest {
                                            request_type: LspRequestType::CallHierarchy,
                                            timestamp: Instant::now(),
                                            symbol_id: Some(symbol_id.clone()),
                                        },
                                    );

                                    // Send outgoing calls request
                                    if let Some(tx) = &self.lsp_request_tx {
                                        let request = LspRequest::GetOutgoingCalls {
                                            request_id: outgoing_request_id,
                                            call_hierarchy_item: first_item,
                                        };

                                        if let Err(e) = tx.send(request) {
                                            log::error!(
                                                "Failed to send outgoing calls request: {}",
                                                e
                                            );
                                            self.update_loading_state(
                                                &symbol_id,
                                                LoadingState::Failed(
                                                    "Failed to request outgoing calls".to_string(),
                                                ),
                                            );
                                        } else {
                                            self.status_message =
                                                "Loading outgoing calls...".to_string();
                                        }
                                    } else {
                                        log::error!("No LSP request channel available");
                                        self.update_loading_state(
                                            &symbol_id,
                                            LoadingState::Failed("No LSP channel".to_string()),
                                        );
                                    }
                                }
                                CallDirection::Incoming => {
                                    log::info!(
                                        "Requesting incoming calls for: {}",
                                        first_item.name
                                    );

                                    // Generate a new request ID for the incoming calls request
                                    let incoming_request_id = uuid::Uuid::new_v4().to_string();

                                    // Record pending request for incoming calls
                                    self.pending_requests.insert(
                                        incoming_request_id.clone(),
                                        PendingRequest {
                                            request_type: LspRequestType::CallHierarchy,
                                            timestamp: Instant::now(),
                                            symbol_id: Some(symbol_id.clone()),
                                        },
                                    );

                                    // Send incoming calls request
                                    if let Some(tx) = &self.lsp_request_tx {
                                        let request = LspRequest::GetIncomingCalls {
                                            request_id: incoming_request_id,
                                            call_hierarchy_item: first_item,
                                        };

                                        if let Err(e) = tx.send(request) {
                                            log::error!(
                                                "Failed to send incoming calls request: {}",
                                                e
                                            );
                                            self.update_loading_state(
                                                &symbol_id,
                                                LoadingState::Failed(
                                                    "Failed to request incoming calls".to_string(),
                                                ),
                                            );
                                        } else {
                                            self.status_message =
                                                "Loading incoming calls...".to_string();
                                        }
                                    } else {
                                        log::error!("No LSP request channel available");
                                        self.update_loading_state(
                                            &symbol_id,
                                            LoadingState::Failed("No LSP channel".to_string()),
                                        );
                                    }
                                }
                            }
                        } else {
                            // No call hierarchy items - this function has no callees
                            log::info!("No call hierarchy items found for symbol: {:?}", symbol_id);
                            self.update_loading_state(&symbol_id, LoadingState::Loaded);
                            self.status_message = "Function has no callees".to_string();
                        }
                    }
                }
            }
            LspResponse::OutgoingCalls { request_id, calls } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!(
                            "Processing outgoing calls for symbol_id: {:?}, {} calls",
                            symbol_id,
                            calls.len()
                        );

                        // Process outgoing calls and update the call graph
                        self.update_function_outgoing_calls(symbol_id.clone(), calls);

                        // Mark layout as dirty to force recomputation with new data
                        if let Some(workspace) = self.get_current_workspace_mut() {
                            workspace.graph_view_state.mark_layout_dirty();
                        }

                        // Update loading state to loaded
                        self.update_loading_state(&symbol_id, LoadingState::Loaded);

                        self.status_message = "Outgoing calls loaded".to_string();
                    }
                }
            }
            LspResponse::References {
                request_id,
                locations,
            } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!("Processing references for symbol_id: {:?}", symbol_id);

                        // Process references and update the function
                        if let Some(function) = self.call_graph.nodes.get_mut(&symbol_id) {
                            log::info!("Found function '{}' in call graph, clearing existing {} references", function.name, function.references.len());

                            // Clear existing references first to avoid duplicates
                            function.references.clear();

                            for location in &locations {
                                function.add_reference(location.clone());
                            }

                            self.status_message = if locations.is_empty() {
                                format!("No references found for '{}'", function.name)
                            } else {
                                format!(
                                    "Found {} reference(s) for '{}'",
                                    locations.len(),
                                    function.name
                                )
                            };

                            log::info!(
                                "Updated function '{}' with {} references",
                                function.name,
                                function.references.len()
                            );
                        } else {
                            log::warn!(
                                "Could not find function with symbol_id {:?} in call graph",
                                symbol_id
                            );
                            self.status_message = format!("Found {} reference(s)", locations.len());
                        }
                    } else {
                        log::warn!("Pending request had no symbol_id");
                    }
                } else {
                    log::warn!("No pending request found for request_id: {}", request_id);
                }
            }
            LspResponse::DocumentSymbols {
                request_id,
                symbols: _,
            } => {
                if let Some(_pending) = self.pending_requests.remove(&request_id) {
                    self.status_message = "Document symbols loaded".to_string();
                }
            }
            LspResponse::WorkspaceSymbols {
                request_id,
                symbols,
            } => {
                if let Some(_pending) = self.pending_requests.remove(&request_id) {
                    self.status_message = format!("Loaded {} workspace symbols", symbols.len());
                    log::info!("Successfully loaded {} workspace symbols", symbols.len());

                    // Add the function symbols to the call graph
                    let mut function_count = 0;
                    for symbol in symbols {
                        log::info!(
                            "Adding function to call graph: '{}' (qualified: '{}') from {}:{}:{}",
                            symbol.name,
                            symbol.qualified_name,
                            symbol.definition_location.file_path,
                            symbol.definition_location.line,
                            symbol.definition_location.column
                        );

                        self.call_graph.add_function(symbol);
                        function_count += 1;
                    }

                    log::info!(
                        "Added {} functions to call graph from workspace symbols",
                        function_count
                    );

                    // Update the functions list to reflect the new call graph state
                    self.functions = self.call_graph.nodes.keys().cloned().collect();
                    log::info!(
                        "Updated function list, now contains {} functions",
                        self.functions.len()
                    );

                    // Reset selection to show the updated function list
                    if function_count > 0 && self.function_list_state.selected().is_none() {
                        self.function_list_state.select(Some(0));
                    }

                    self.status_message =
                        format!("Loaded {} functions from workspace", function_count);
                }
            }
            LspResponse::Error { request_id, error } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        self.update_loading_state(&symbol_id, LoadingState::Failed(error.clone()));
                    }
                    self.status_message = format!("LSP error: {}", error);
                }
            }
            LspResponse::PreloadComplete {
                request_id: _,
                loaded_count,
                failed_count,
            } => {
                if failed_count > 0 {
                    self.status_message = format!(
                        "Preloaded {} documents ({} failed)",
                        loaded_count, failed_count
                    );
                } else {
                    self.status_message =
                        format!("Preloaded {} documents successfully", loaded_count);
                }
            }
            LspResponse::ReferencesWithSymbols {
                request_id,
                references,
            } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        // Update the function with enhanced references
                        if let Some(function) = self.call_graph.get_function_mut(&symbol_id) {
                            // Clear existing references and add new enhanced ones
                            function.references = references;
                        }

                        // Update loading state
                        self.update_loading_state(&symbol_id, LoadingState::Loaded);
                    }
                }
            }
            LspResponse::IncomingCalls { request_id, calls } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!(
                            "Processing incoming calls for symbol_id: {:?}, {} calls",
                            symbol_id,
                            calls.len()
                        );

                        // Process incoming calls and update the call graph
                        self.update_function_incoming_calls(symbol_id.clone(), calls);

                        // Mark layout as dirty to force recomputation with new data
                        if let Some(workspace) = self.get_current_workspace_mut() {
                            workspace.graph_view_state.mark_layout_dirty();
                        }

                        // Update loading state to loaded
                        self.update_loading_state(&symbol_id, LoadingState::Loaded);

                        self.status_message = "Incoming calls loaded".to_string();
                    }
                }
            }
            LspResponse::CallHierarchyPrepared { request_id, items } => {
                if let Some(pending) = self.pending_requests.remove(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!(
                            "Call hierarchy prepared for symbol: {:?}, {} items",
                            symbol_id,
                            items.len()
                        );

                        // Mark the call hierarchy as prepared and cache the first item
                        if !items.is_empty() {
                            // We'll implement lazy call graph integration here later
                            self.status_message = format!("Call hierarchy prepared for function");
                        } else {
                            self.status_message =
                                "No call hierarchy available for this function".to_string();
                        }

                        self.update_loading_state(&symbol_id, LoadingState::Loaded);
                    }
                }
            }
        }
    }

    /// Update function outgoing calls from LSP response
    fn update_function_outgoing_calls(
        &mut self,
        symbol_id: SymbolId,
        calls: Vec<lsp_types::CallHierarchyOutgoingCall>,
    ) {
        // Process outgoing calls and convert them to call graph relationships
        for call in calls {
            // Convert LSP location to our Location type
            let location = core_data::Location::new(
                call.to.uri.path().to_string(),
                (call.to.range.start.line + 1) as u32, // Convert from 0-indexed to 1-indexed
                (call.to.range.start.character + 1) as u32,
            );

            // Create or find existing callee function
            let qualified_name = format!("{}::{}", call.to.name, location.file_path);

            // Check if function already exists
            let callee_id = if let Some(existing_func) = self
                .call_graph
                .find_function_by_qualified_name_and_location(&qualified_name, &location)
            {
                // Use existing function's ID
                existing_func.id.clone()
            } else {
                // Create new function
                let callee_function =
                    core_data::FunctionNode::new(call.to.name.clone(), qualified_name, location);
                self.call_graph.add_function(callee_function)
            };

            // Create the call edge from the original function to this callee
            // Use the call location from the LSP response
            for from_range in &call.from_ranges {
                let call_location = core_data::Location::new(
                    call.to.uri.path().to_string(), // Use the callee's file for call location
                    (from_range.start.line + 1) as u32,
                    (from_range.start.character + 1) as u32,
                );
                self.call_graph
                    .add_call(symbol_id.clone(), callee_id.clone(), call_location);
            }
        }
    }

    /// Update function incoming calls from LSP response
    fn update_function_incoming_calls(
        &mut self,
        symbol_id: SymbolId,
        calls: Vec<lsp_types::CallHierarchyIncomingCall>,
    ) {
        // Process incoming calls and convert them to call graph relationships
        for call in calls {
            // Convert LSP location to our Location type
            let location = core_data::Location::new(
                call.from.uri.path().to_string(),
                (call.from.range.start.line + 1) as u32, // Convert from 0-indexed to 1-indexed
                (call.from.range.start.character + 1) as u32,
            );

            // Create or find existing caller function
            let qualified_name = format!("{}::{}", call.from.name, location.file_path);

            // Check if function already exists
            let caller_id = if let Some(existing_func) = self
                .call_graph
                .find_function_by_qualified_name_and_location(&qualified_name, &location)
            {
                // Use existing function's ID
                existing_func.id.clone()
            } else {
                // Create new function
                let caller_function =
                    core_data::FunctionNode::new(call.from.name.clone(), qualified_name, location);
                self.call_graph.add_function(caller_function)
            };

            // Create the call edge from the caller to the original function
            // Use the call location from the LSP response
            for from_range in &call.from_ranges {
                let call_location = core_data::Location::new(
                    call.from.uri.path().to_string(), // Use the caller's file for call location
                    (from_range.start.line + 1) as u32,
                    (from_range.start.character + 1) as u32,
                );
                self.call_graph
                    .add_call(caller_id.clone(), symbol_id.clone(), call_location);
            }
        }
    }

    /// Check if a function is already loaded
    pub fn is_function_loaded(&self, symbol_id: &SymbolId) -> bool {
        matches!(
            self.loading_states.get(symbol_id),
            Some(LoadingState::Loaded)
        )
    }

    /// Check if a function is currently loading
    pub fn is_function_loading(&self, symbol_id: &SymbolId) -> bool {
        matches!(
            self.loading_states.get(symbol_id),
            Some(LoadingState::Loading)
        )
    }

    /// Get the loading state of a function
    pub fn get_loading_state(&self, symbol_id: &SymbolId) -> &LoadingState {
        self.loading_states
            .get(symbol_id)
            .unwrap_or(&LoadingState::NotLoaded)
    }

    /// Update loading state for a symbol and corresponding UI elements
    fn update_loading_state(&mut self, symbol_id: &SymbolId, state: LoadingState) {
        // Update internal state
        self.loading_states.insert(symbol_id.clone(), state.clone());

        // Update UI elements
        match state {
            LoadingState::Loading => {
                self.update_node_loading_state(symbol_id, true);
                self.status_message = format!("Loading data for function...");
            }
            LoadingState::Loaded => {
                self.update_node_loading_state(symbol_id, false);
                if let Some(node_index) = self.tree_view_state.find_node_index(symbol_id) {
                    if let Some(node) = self.tree_view_state.nodes.get_mut(node_index) {
                        node.children_loaded = true;
                    }
                }
                self.status_message = String::new();
            }
            LoadingState::Failed(error) => {
                self.update_node_loading_state(symbol_id, false);
                self.status_message = format!("Error: {}", error);
            }
            LoadingState::NotLoaded => {
                self.update_node_loading_state(symbol_id, false);
                if let Some(node_index) = self.tree_view_state.find_node_index(symbol_id) {
                    if let Some(node) = self.tree_view_state.nodes.get_mut(node_index) {
                        node.children_loaded = false;
                    }
                }
            }
        }
    }

    /// Update the loading state of a tree node
    fn update_node_loading_state(&mut self, symbol_id: &SymbolId, is_loading: bool) {
        if let Some(node_index) = self.tree_view_state.find_node_index(symbol_id) {
            if let Some(node) = self.tree_view_state.nodes.get_mut(node_index) {
                node.is_loading = is_loading;
            }
        }
    }

    /// Preload documents that will likely be needed
    #[allow(dead_code)]
    fn preload_documents(&mut self, function_id: &SymbolId) {
        if let Some(function) = self.call_graph.nodes.get(function_id) {
            let file_path = &function.definition_location.file_path;

            // Create document URI from file path
            if let Ok(uri) = lsp_types::Url::from_file_path(file_path) {
                // Check if we've already opened this document
                if !self.opened_documents.contains(&uri) {
                    // Track that we've marked it for opening
                    // The actual document opening will be handled by the LSP service
                    // when it processes the request
                    self.opened_documents.insert(uri);
                }
            }
        }
    }

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

/// Main TUI application runner
pub struct TuiApp {
    app: App,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TuiApp {
    pub fn new(call_graph: CallGraph) -> Result<Self, Box<dyn std::error::Error>> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let app = App::new(call_graph);

        Ok(Self { app, terminal })
    }

    pub fn new_with_lsp_async(
        call_graph: CallGraph,
        lsp_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        lsp_channels_rx: mpsc::UnboundedReceiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Setup terminal (same as new)
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let app = App::new_with_lsp_async(call_graph, lsp_rx, lsp_channels_rx);

        Ok(Self { app, terminal })
    }

    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.app.set_lsp_channels(response_rx, request_tx);
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // Drain any async LSP loader messages
            self.app.poll_lsp_loader_messages();

            // Poll for LSP channels and wire them up when they arrive
            self.app.poll_lsp_channels();

            // Check for LSP responses first
            self.app.check_lsp_responses();

            // Draw UI
            self.terminal.draw(|f| ui(f, &mut self.app))?;

            // Handle input with timeout to allow checking for LSP responses
            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        // Handle search bar input separately
                        if self.app.show_search_bar {
                            match key.code {
                                KeyCode::Esc => {
                                    self.app.toggle_search_bar();
                                }
                                KeyCode::Enter => {
                                    self.app.select_from_search();
                                }
                                KeyCode::Up => {
                                    self.app.search_bar_state.select_previous();
                                }
                                KeyCode::Down => {
                                    self.app.search_bar_state.select_next();
                                }
                                KeyCode::Tab => {
                                    self.app.search_bar_state.cycle_search_mode();
                                    self.app
                                        .search_bar_state
                                        .update_results(&self.app.call_graph);
                                }
                                KeyCode::Backspace => {
                                    self.app.handle_search_backspace();
                                }
                                KeyCode::Delete => {
                                    self.app.search_bar_state.delete_char_forward();
                                    self.app
                                        .search_bar_state
                                        .update_results(&self.app.call_graph);
                                }
                                KeyCode::Left => {
                                    self.app.search_bar_state.move_cursor_right();
                                }
                                KeyCode::Right => {
                                    self.app.search_bar_state.move_cursor_left();
                                }
                                KeyCode::Home => {
                                    self.app.search_bar_state.move_cursor_start();
                                }
                                KeyCode::End => {
                                    self.app.search_bar_state.move_cursor_end();
                                }
                                KeyCode::Char(c) => {
                                    self.app.handle_search_input(c);
                                }
                                _ => {}
                            }
                            continue; // Don't process other actions when search is active
                        }

                        let action = match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
                            KeyCode::Char('?') => Some(Action::Help),
                            KeyCode::Char('n')
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NewWorkspace)
                            }
                            KeyCode::Char('t')
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NewWorkspace)
                            }
                            KeyCode::Char('W') => Some(Action::CloseWorkspace),
                            KeyCode::Char('f') => {
                                self.app.toggle_search_bar();
                                None
                            }
                            KeyCode::Char(']') => Some(Action::NextWorkspace),
                            KeyCode::Char('[') => Some(Action::PreviousWorkspace),
                            KeyCode::Tab
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NextWorkspace)
                            }
                            KeyCode::BackTab
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::PreviousWorkspace)
                            }
                            KeyCode::Char('1') => {
                                self.app.switch_workspace(0);
                                None
                            }
                            KeyCode::Char('2') => {
                                self.app.switch_workspace(1);
                                None
                            }
                            KeyCode::Char('3') => {
                                self.app.switch_workspace(2);
                                None
                            }
                            KeyCode::Char('4') => {
                                self.app.switch_workspace(3);
                                None
                            }
                            KeyCode::Char('5') => {
                                self.app.switch_workspace(4);
                                None
                            }
                            KeyCode::Char('6') => {
                                self.app.switch_workspace(5);
                                None
                            }
                            KeyCode::Char('7') => {
                                self.app.switch_workspace(6);
                                None
                            }
                            KeyCode::Char('8') => {
                                self.app.switch_workspace(7);
                                None
                            }
                            KeyCode::Char('9') => {
                                self.app.switch_workspace(8);
                                None
                            }
                            KeyCode::Up => Some(Action::MoveDown),
                            KeyCode::Down => Some(Action::MoveUp),
                            KeyCode::Right => Some(Action::MoveLeft),
                            KeyCode::Left => Some(Action::MoveRight),
                            KeyCode::Char('h') => Some(Action::NavigateParent),
                            KeyCode::Char('l') => Some(Action::NavigateChild),
                            KeyCode::Char('k') => Some(Action::NavigatePrevSibling),
                            KeyCode::Char('j') => Some(Action::NavigateNextSibling),
                            KeyCode::Enter => Some(Action::ExpandOrCollapse),
                            KeyCode::Char('r') => Some(Action::ResetView),
                            KeyCode::Char('F') => Some(Action::FindReferences),
                            KeyCode::Char('t') => Some(Action::ToggleCallDirection),
                            KeyCode::Char('R') => Some(Action::Refresh),
                            _ => None,
                        };

                        if let Some(action) = action {
                            self.app.handle_action(action);
                        }
                    }
                }
            }

            if self.app.should_quit {
                break;
            }
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;

        Ok(())
    }
}

/// UI rendering function (separate from TuiApp to avoid borrowing issues)
pub fn ui(f: &mut Frame, app: &mut App) {
    let size = f.area();

    // Show help overlay if requested
    if app.show_help {
        render_help_overlay(f, size);
        return;
    }

    // Show search bar if active
    if app.show_search_bar {
        render_search_bar_overlay(f, size, app);
        return;
    }

    // Show function search modal if requested
    if app.show_function_search {
        render_function_search_modal(f, size, app);
        return;
    }

    // Show workspace manager if requested
    if app.show_workspace_manager {
        render_workspace_manager_modal(f, size, app);
        return;
    }

    // Create main layout with conditional LSP status bar above graph view
    let show_lsp_status = !matches!(app.lsp_status, LspLoadPhase::Completed);

    let chunks = if show_lsp_status {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Workspace tabs
                Constraint::Length(1), // LSP Status bar
                Constraint::Length(1), // Blank line separator
                Constraint::Min(0),    // Main content (graph view)
            ])
            .split(size)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Workspace tabs
                Constraint::Min(0),    // Main content (graph view)
            ])
            .split(size)
    };

    // Render workspace tabs
    render_workspace_tabs(f, chunks[0], app);

    if show_lsp_status {
        // Render LSP loading status bar above graph view
        render_lsp_status_bar(f, chunks[1], app);
        // chunks[2] is blank separator
        // Render graph view (current workspace)
        render_graph_view(f, chunks[3], app);
    } else {
        // Render graph view (current workspace)
        render_graph_view(f, chunks[1], app);
    }
}

fn render_lsp_status_bar(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let status_text = match &app.lsp_status {
        LspLoadPhase::NotStarted => "LSP: not started".to_string(),
        LspLoadPhase::SpawningServer => "LSP: starting clangd…".to_string(),
        LspLoadPhase::Initializing => "LSP: initializing…".to_string(),
        LspLoadPhase::Initialized => "LSP: initialized".to_string(),
        LspLoadPhase::DiscoveringFiles => "LSP: discovering project files…".to_string(),
        LspLoadPhase::PreloadingDocuments { done, total } => {
            format!("LSP: preloading documents… {}/{}", done, total)
        }
        LspLoadPhase::LoadingWorkspaceSymbols { loaded } => {
            format!("LSP: loading workspace symbols… {}", loaded)
        }
        LspLoadPhase::Completed => "LSP: ready".to_string(),
        LspLoadPhase::Failed(err) => format!("LSP: failed ({})", err),
    };

    let paragraph = Paragraph::new(Line::from(status_text)).block(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().fg(Color::Gray)),
    );

    f.render_widget(paragraph, area);
}

fn render_help_overlay(f: &mut Frame, area: ratatui::layout::Rect) {
    let help_text = vec![
        Line::from("📐 lsp-callgraph-tui - Help"),
        Line::from(""),
        Line::from("🎮 Key Bindings:"),
        Line::from(""),
        Line::from("Workspace Management:"),
        Line::from("  CtrlN/T   - Create new workspace"),
        Line::from("  W         - Close current workspace"),
        Line::from("  CtrlTab/] - Next workspace"),
        Line::from("  [/Ctrl⇧Tab - Previous workspace"),
        Line::from("  f         - Search symbols (create workspace"),
        Line::from("  1-9       - Jump to workspace 1-9"),
        Line::from(""),
        Line::from("Graph View Navigation:"),
        Line::from("  ↑↓←→/hjkl - Pan the view"),
        Line::from("  r         - Reset view"),
        Line::from("  t         - Toggle call direction"),
        Line::from("  Enter     - Select next node"),
        Line::from(""),
        Line::from("General:"),
        Line::from("  F         - Find references"),
        Line::from("  R         - Refresh from LSP"),
        Line::from("  ?         - Show/hide this help"),
        Line::from("  q / Esc   - Quit application"),
        Line::from(""),
        Line::from("🌲 Graph View Behavior:"),
        Line::from("  • Each workspace shows an independent call graph"),
        Line::from("  • Create multiple workspaces to compare graphs"),
        Line::from("  • Panning is per-workspace"),
        Line::from(""),
        Line::from("💡 Key Changes:"),
        Line::from("  • 'W' (Shift+w) closes the current workspace"),
        Line::from("  • 'q' or 'Esc' exits the entire application"),
        Line::from("  • 'f' opens search/symbol finder"),
        Line::from(""),
        Line::from("Press ? again to close this help"),
    ];

    // Create a centered popup
    let popup_area = ratatui::layout::Rect {
        x: area.width / 8,
        y: area.height / 8,
        width: area.width * 3 / 4,
        height: area.height * 3 / 4,
    };

    // Clear the background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    let help_paragraph = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Help - Key Bindings")
                .style(Style::default().fg(Color::Yellow)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(help_paragraph, popup_area);
}

fn render_search_bar_overlay(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    // Create centered overlay
    let popup_width = area.width.min(100);
    let popup_height = area.height.min(30);
    let popup_area = ratatui::layout::Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Dim background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    // Render search bar
    let search_bar = SearchBar::new();
    search_bar.render(popup_area, f.buffer_mut(), &app.search_bar_state);
}

fn render_workspace_tabs(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let mut tab_titles = Vec::new();

    for (i, workspace) in app.workspaces.iter().enumerate() {
        let mut name = workspace.name.clone();

        // Truncate long names
        if name.len() > 15 {
            name = format!("{}...", &name[..12]);
        }

        // Add active indicator
        if i == app.current_workspace_index {
            name = format!("[{}*]", name);
        } else {
            name = format!("[{}]", name);
        }

        tab_titles.push(name);
    }

    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("Workspaces"))
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(app.current_workspace_index);
    f.render_widget(tabs, area);
}

fn render_graph_view(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    // Get current workspace
    let workspace_index = app.current_workspace_index;

    if let Some(workspace) = app.workspaces.get_mut(workspace_index) {
        // Store viewport size for potential recentering
        let viewport_size = (area.width as f32, area.height as f32);
        app.last_viewport_size = viewport_size;

        // Update layout if dirty (efficient - only recomputes when needed)
        if let Err(e) = workspace
            .graph_view_state
            .update_layout(&app.call_graph, viewport_size)
        {
            // If layout update fails, show error
            let error_text = vec![
                Line::from("Layout Error"),
                Line::from(""),
                Line::from(format!("Failed to compute layout: {}", e)),
            ];

            let paragraph = Paragraph::new(error_text)
                .block(Block::default().borders(Borders::ALL).title("Graph View"))
                .style(Style::default().fg(Color::Red));

            f.render_widget(paragraph, area);
            return;
        }

        // Create the graph view widget
        let graph_view = GraphView::new(&app.call_graph).show_help(app.show_help);

        // Render with workspace's graph view state
        f.render_stateful_widget(graph_view, area, &mut workspace.graph_view_state);
    } else {
        // No workspace available - render empty state
        let empty_text = vec![
            Line::from("No workspace available"),
            Line::from(""),
            Line::from("Press CtrlN to create a new workspace"),
        ];

        let paragraph = Paragraph::new(empty_text)
            .block(Block::default().borders(Borders::ALL).title("Graph View"))
            .style(Style::default().fg(Color::Gray));

        f.render_widget(paragraph, area);
    }
}

fn render_function_search_modal(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // Create a centered popup
    let popup_width = area.width.min(80);
    let popup_height = area.height.min(30);
    let popup_area = ratatui::layout::Rect {
        x: (area.width - popup_width) / 2,
        y: (area.height - popup_height) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Clear the background with semi-transparent effect
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    // Filter functions based on search query
    let filtered_functions: Vec<_> = app
        .functions
        .iter()
        .filter_map(|id| app.call_graph.get_function(id))
        .filter(|func| {
            if app.function_search_query.is_empty() {
                true
            } else {
                func.name
                    .to_lowercase()
                    .contains(&app.function_search_query.to_lowercase())
            }
        })
        .take(popup_height as usize - 5)
        .collect();

    let mut text = vec![
        Line::from("Search Functions"),
        Line::from(""),
        Line::from(format!("Query: {}_", app.function_search_query)),
        Line::from(""),
        Line::from(format!("Found {} function(s):", filtered_functions.len())),
        Line::from(""),
    ];

    for (i, func) in filtered_functions.iter().enumerate() {
        text.push(Line::from(format!("  {}. {}", i + 1, func.name)));
    }

    text.push(Line::from(""));
    text.push(Line::from(
        "Type to search, Enter to select first, Esc to cancel",
    ));

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Function Search")
                .style(Style::default().fg(Color::Cyan)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(paragraph, popup_area);
}

fn render_workspace_manager_modal(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // Create a centered popup
    let popup_width = area.width.min(80);
    let popup_height = area.height.min(30);
    let popup_area = ratatui::layout::Rect {
        x: (area.width - popup_width) / 2,
        y: (area.height - popup_height) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Clear the background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    let mut text = vec![
        Line::from("Workspace Manager"),
        Line::from(""),
        Line::from(format!("Total Workspaces: {}", app.workspaces.len())),
        Line::from(""),
    ];

    for (i, workspace) in app.workspaces.iter().enumerate() {
        let marker = if i == app.current_workspace_index {
            "→"
        } else {
            " "
        };

        let root_info = workspace
            .root_symbol
            .as_ref()
            .and_then(|id| app.call_graph.get_function(id))
            .map(|f| f.name.as_str())
            .unwrap_or("(empty)");

        text.push(Line::from(format!(
            "{} {}. {} - Root: {}",
            marker,
            i + 1,
            workspace.name,
            root_info
        )));
    }

    text.push(Line::from(""));
    text.push(Line::from("Use 1-9 to switch, Esc to close"));

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Workspaces")
                .style(Style::default().fg(Color::Green)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(paragraph, popup_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_data::{FunctionNode, Location};

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
