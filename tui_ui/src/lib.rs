use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Tabs},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::io;
use std::time::Instant;
use tokio::sync::mpsc;

use core_data::{CallGraph, SymbolId};
use logic::query::GraphQueryEngine;
use lsp_integration::{LspRequest, LspResponse};

pub mod actions;
pub mod call_graph_view;
pub mod diagnostic_panel;
pub mod function_list;

pub use actions::{Action, TreeNode, TreeViewState};
pub use call_graph_view::CallGraphView;
pub use diagnostic_panel::DiagnosticPanel;
pub use function_list::FunctionList;

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
    pub current_tab: usize,
    pub selected_function: Option<SymbolId>,
    pub search_query: String,
    pub should_quit: bool,
    pub status_message: String,
    pub function_list_state: ratatui::widgets::ListState,
    pub functions: Vec<SymbolId>,
    pub tree_view_state: TreeViewState,
    pub show_help: bool,

    // Lazy loading fields
    pub loading_states: HashMap<SymbolId, LoadingState>,
    pub lsp_response_rx: Option<mpsc::UnboundedReceiver<LspResponse>>,
    pub lsp_request_tx: Option<mpsc::UnboundedSender<LspRequest>>,
    pub pending_requests: HashMap<String, PendingRequest>,
    pub opened_documents: std::collections::HashSet<lsp_types::Url>,

    // These fields and related methods are currently not used but kept for future expansion
    #[allow(dead_code)]
    query_engine: Option<GraphQueryEngine<'static>>,
    #[allow(dead_code)]
    leaked_graph: Option<&'static CallGraph>,
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

        Self {
            call_graph,
            current_tab: 0,
            selected_function: None,
            search_query: String::new(),
            should_quit: false,
            status_message: "Ready".to_string(),
            function_list_state,
            functions,
            tree_view_state: TreeViewState::new(),
            show_help: false,
            loading_states: HashMap::new(),
            lsp_response_rx: None,
            lsp_request_tx: None,
            pending_requests: HashMap::new(),
            opened_documents: std::collections::HashSet::new(),
            query_engine: None,
            leaked_graph: None,
        }
    }

    // Lazy initialization of query engine - only create when needed
    #[allow(dead_code)]
    fn get_query_engine(&mut self) -> &GraphQueryEngine<'static> {
        if self.query_engine.is_none() {
            // Only create the leaked reference when actually needed
            let leaked_graph = Box::leak(Box::new(self.call_graph.clone()));
            self.leaked_graph = Some(leaked_graph);
            self.query_engine = Some(GraphQueryEngine::new(leaked_graph));
        }
        self.query_engine.as_ref().unwrap()
    }

    pub fn select_function(&mut self, id: SymbolId) {
        self.selected_function = Some(id);
        self.status_message = "Function selected".to_string();
    }

    pub fn select_current_function(&mut self) {
        if let Some(selected_idx) = self.function_list_state.selected() {
            if let Some(function_id) = self.functions.get(selected_idx) {
                self.start_call_graph_with_function(function_id.clone());
            }
        }
    }

    pub fn navigate_function_list_up(&mut self) {
        if self.current_tab == 0 && !self.functions.is_empty() {
            let i = match self.function_list_state.selected() {
                Some(i) => {
                    if i == 0 {
                        self.functions.len() - 1
                    } else {
                        i - 1
                    }
                }
                None => 0,
            };
            self.function_list_state.select(Some(i));
            self.status_message = "Navigating function list".to_string();
        }
    }

    pub fn navigate_function_list_down(&mut self) {
        if self.current_tab == 0 && !self.functions.is_empty() {
            let i = match self.function_list_state.selected() {
                Some(i) => {
                    if i >= self.functions.len() - 1 {
                        0
                    } else {
                        i + 1
                    }
                }
                None => 0,
            };
            self.function_list_state.select(Some(i));
            self.status_message = "Navigating function list".to_string();
        }
    }

    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
    }

    pub fn next_tab(&mut self) {
        self.current_tab = (self.current_tab + 1) % 3;
    }

    pub fn previous_tab(&mut self) {
        if self.current_tab == 0 {
            self.current_tab = 2;
        } else {
            self.current_tab -= 1;
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
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

                        // If we got call hierarchy items, automatically request outgoing calls
                        if !items.is_empty() {
                            let first_item = items[0].clone();
                            log::info!("Requesting outgoing calls for: {}", first_item.name);

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
                                    log::error!("Failed to send outgoing calls request: {}", e);
                                    self.update_loading_state(
                                        &symbol_id,
                                        LoadingState::Failed(
                                            "Failed to request outgoing calls".to_string(),
                                        ),
                                    );
                                } else {
                                    self.status_message = "Loading outgoing calls...".to_string();
                                }
                            } else {
                                log::error!("No LSP request channel available");
                                self.update_loading_state(
                                    &symbol_id,
                                    LoadingState::Failed("No LSP channel".to_string()),
                                );
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

                        // Update loading state to loaded
                        self.update_loading_state(&symbol_id, LoadingState::Loaded);

                        // Load the callees into the tree view if the node is expanded
                        if let Some(node_index) = self.tree_view_state.find_node_index(&symbol_id) {
                            if let Some(node) = self.tree_view_state.nodes.get(node_index) {
                                if node.is_expanded {
                                    self.load_callees_for_node(symbol_id);
                                }
                            }
                        }

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

            // Create or update the callee function
            let callee_function = core_data::FunctionNode::new(
                call.to.name.clone(),
                format!("{}::{}", call.to.name, location.file_path),
                location,
            );

            // Add the callee function to the call graph and get its ID
            let callee_id = self.call_graph.add_function(callee_function);

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

            // Create or update the caller function
            let caller_function = core_data::FunctionNode::new(
                call.from.name.clone(),
                format!("{}::{}", call.from.name, location.file_path),
                location,
            );

            // Add the caller function to the call graph and get its ID
            let caller_id = self.call_graph.add_function(caller_function);

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
            Action::ExpandNode => self.handle_expand_node(),
            Action::CollapseNode => self.handle_collapse_node(),
            Action::ExpandOrCollapse => self.handle_expand_or_collapse(),
            Action::SwitchTab => self.next_tab(),
            Action::FindReferences => self.handle_find_references(),
            Action::Refresh => self.handle_refresh(),
            Action::Quit => self.quit(),
            Action::Help => self.toggle_help(),
        }
    }

    fn handle_move_up(&mut self) {
        match self.current_tab {
            0 => self.navigate_function_list_up(),
            1 => {
                self.tree_view_state.move_up();
                self.status_message = "Moved up in call graph".to_string();
            }
            _ => {}
        }
    }

    fn handle_move_down(&mut self) {
        match self.current_tab {
            0 => self.navigate_function_list_down(),
            1 => {
                self.tree_view_state.move_down();
                self.status_message = "Moved down in call graph".to_string();
            }
            _ => {}
        }
    }

    fn handle_expand_node(&mut self) {
        if self.current_tab == 1 {
            if let Some(node) = self.tree_view_state.get_selected_node_mut() {
                if !node.is_expanded {
                    let symbol_id = node.symbol_id.clone();

                    // Update UI state immediately
                    node.expand();

                    // Only trigger LSP request if not already loaded
                    if !self.is_function_loaded(&symbol_id) && !self.is_function_loading(&symbol_id)
                    {
                        self.loading_states
                            .insert(symbol_id.clone(), LoadingState::Loading);
                        self.request_call_hierarchy(&symbol_id);
                        self.status_message = "Loading call hierarchy...".to_string();
                    } else {
                        // Use cached data if available
                        self.load_callees_for_node(symbol_id);
                        self.status_message = "Expanding node...".to_string();
                    }
                }
            }
        }
    }

    fn handle_collapse_node(&mut self) {
        if self.current_tab == 1 {
            let selected_index = self.tree_view_state.selected_index;
            if let Some(node) = self.tree_view_state.get_selected_node_mut() {
                if node.is_expanded {
                    node.collapse();
                    self.tree_view_state.remove_children(selected_index);
                    self.status_message = "Collapsed node".to_string();
                }
            }
        }
    }

    fn handle_expand_or_collapse(&mut self) {
        if self.current_tab == 0 {
            self.select_current_function();
        } else if self.current_tab == 1 {
            let selected_index = self.tree_view_state.selected_index;
            if let Some(node) = self.tree_view_state.get_selected_node_mut() {
                let was_expanded = node.is_expanded;
                let symbol_id = node.symbol_id.clone();

                if was_expanded {
                    // Was expanded, now collapsed - remove children
                    node.collapse();
                    self.tree_view_state.remove_children(selected_index);
                    self.status_message = "Collapsed node".to_string();
                } else {
                    // Was collapsed, now expanded - load children
                    node.expand();

                    // Only trigger LSP request if not already loaded
                    if !self.is_function_loaded(&symbol_id) && !self.is_function_loading(&symbol_id)
                    {
                        self.loading_states
                            .insert(symbol_id.clone(), LoadingState::Loading);
                        self.request_call_hierarchy(&symbol_id);
                        self.status_message = "Loading call hierarchy...".to_string();
                    } else {
                        // Use cached data if available
                        self.load_callees_for_node(symbol_id);
                        self.status_message = "Expanding node...".to_string();
                    }
                }
            }
        }
    }

    fn load_callees_for_node(&mut self, symbol_id: SymbolId) {
        // Get callees for the symbol
        let callees = self.call_graph.get_callees(&symbol_id);
        let callee_ids: Vec<SymbolId> = callees.iter().map(|f| f.id.clone()).collect();

        // Find the node index and insert children
        if let Some(node_index) = self.tree_view_state.find_node_index(&symbol_id) {
            self.tree_view_state.insert_children(node_index, callee_ids);
        }
    }

    fn handle_find_references(&mut self) {
        let symbol_id = match self.current_tab {
            0 => {
                // Function list tab - get selected function from list
                if let Some(selected_idx) = self.function_list_state.selected() {
                    self.functions.get(selected_idx).cloned()
                } else {
                    None
                }
            }
            1 => {
                // Call graph tab - get selected node from tree
                if let Some(node) = self.tree_view_state.get_selected_node() {
                    Some(node.symbol_id.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

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

    pub fn start_call_graph_with_function(&mut self, symbol_id: SymbolId) {
        log::info!(
            "Starting call graph exploration with function: {:?}",
            symbol_id
        );

        // Check if we're already displaying the same function as root
        if let Some(current_root) = self.tree_view_state.nodes.get(0) {
            if current_root.symbol_id == symbol_id {
                log::info!("Function {:?} is already the root of the call graph, switching to call graph tab", symbol_id);
                self.selected_function = Some(symbol_id);
                self.current_tab = 1; // Switch to call graph tab
                return; // Don't create a new tree
            }
        }

        self.tree_view_state = TreeViewState::new();
        self.tree_view_state.add_root_node(symbol_id.clone());
        self.selected_function = Some(symbol_id.clone());
        self.current_tab = 1; // Switch to call graph tab

        // Automatically expand the root node and load its call hierarchy
        if let Some(root_node) = self.tree_view_state.nodes.get_mut(0) {
            root_node.expand();
        }

        // Immediately request call hierarchy for the root function
        self.loading_states
            .insert(symbol_id.clone(), LoadingState::Loading);
        self.request_call_hierarchy(&symbol_id);
        self.status_message = "Loading call hierarchy...".to_string();

        log::info!(
            "Call graph started - nodes: {}, selected_index: {}",
            self.tree_view_state.nodes.len(),
            self.tree_view_state.selected_index
        );
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

    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.app.set_lsp_channels(response_rx, request_tx);
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // Check for LSP responses first
            self.app.check_lsp_responses();

            // Create a reference to app for the UI drawing
            let app_ref = &self.app;
            self.terminal.draw(|f| ui(f, app_ref))?;

            // Handle input with timeout to allow checking for LSP responses
            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        let action = match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
                            KeyCode::Char('?') => Some(Action::Help),
                            KeyCode::Tab => Some(Action::SwitchTab),
                            KeyCode::BackTab => {
                                self.app.previous_tab();
                                None
                            }
                            KeyCode::Char('1') => {
                                self.app.current_tab = 0;
                                None
                            }
                            KeyCode::Char('2') => {
                                self.app.current_tab = 1;
                                None
                            }
                            KeyCode::Char('3') => {
                                self.app.current_tab = 2;
                                None
                            }
                            KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
                            KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
                            KeyCode::Right | KeyCode::Char('l') => Some(Action::ExpandNode),
                            KeyCode::Left | KeyCode::Char('h') => Some(Action::CollapseNode),
                            KeyCode::Enter => Some(Action::ExpandOrCollapse),
                            KeyCode::Char('r') => Some(Action::Refresh),
                            KeyCode::Char('f') => Some(Action::FindReferences),
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
pub fn ui(f: &mut Frame, app: &App) {
    let size = f.area();

    // Show help overlay if requested
    if app.show_help {
        render_help_overlay(f, size);
        return;
    }

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tab bar
            Constraint::Min(0),    // Main content
            Constraint::Length(3), // Status bar
        ])
        .split(size);

    // Render tab bar
    let tab_titles = vec!["Functions", "Call Graph", "Diagnostics"];
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("Navigation"))
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(app.current_tab);
    f.render_widget(tabs, chunks[0]);

    // Render main content based on selected tab
    match app.current_tab {
        0 => render_function_list(f, chunks[1], app),
        1 => render_tree_call_graph(f, chunks[1], app),
        2 => render_diagnostics(f, chunks[1], app),
        _ => {}
    }

    // Render status bar
    let status_text = match app.current_tab {
        0 => format!(
            "Status: {} | Selected: {} | Use ↑↓/kj to navigate, Enter to explore, Tab to switch tabs, ? for help | Press 'q' to quit",
            app.status_message,
            app.selected_function
                .as_ref()
                .and_then(|id| app.call_graph.get_function(id))
                .map(|f| f.name.as_str())
                .unwrap_or("None")
        ),
        1 => format!(
            "Status: {} | Use ↑↓/kj to navigate, →l to expand, ←h to collapse, Enter to toggle, ? for help | Press 'q' to quit",
            app.status_message
        ),
        2 => format!(
            "Status: {} | Diagnostics View | Press Tab to switch tabs, ? for help | Press 'q' to quit",
            app.status_message
        ),
        _ => format!(
            "Status: {} | Unknown View | Press 'q' to quit",
            app.status_message
        ),
    };

    let status_paragraph =
        Paragraph::new(status_text).block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status_paragraph, chunks[2]);
}

fn render_function_list(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let query_engine = logic::query::GraphQueryEngine::new(&app.call_graph);
    let mut function_list = FunctionList::new(&app.call_graph, &query_engine);

    // Sync the state with the app's function list state
    function_list.state = app.function_list_state.clone();

    // Ensure the function list uses the same order as the app
    function_list.functions = app
        .functions
        .iter()
        .filter_map(|id| app.call_graph.get_function(id))
        .collect();

    // Debug logging
    log::info!(
        "Rendering function list with {} functions, selected: {:?}",
        function_list.functions.len(),
        function_list.state.selected()
    );

    // Log selected function for debugging
    if let Some(selected_idx) = function_list.state.selected() {
        if let Some(func) = function_list.functions.get(selected_idx) {
            log::info!(
                "Selected function: '{}' with {} references",
                func.name,
                func.references.len()
            );
        }
    }

    function_list.render(f, area);
}

fn render_help_overlay(f: &mut Frame, area: ratatui::layout::Rect) {
    let help_text = vec![
        Line::from("📐 lsp-callgraph-tui - Help"),
        Line::from(""),
        Line::from("🎮 Key Bindings:"),
        Line::from(""),
        Line::from("Navigation:"),
        Line::from("  ↑ / k     - Move selection up"),
        Line::from("  ↓ / j     - Move selection down"),
        Line::from(""),
        Line::from("Call Graph (Tree View):"),
        Line::from("  → / l     - Expand selected node"),
        Line::from("  ← / h     - Collapse selected node"),
        Line::from("  Enter     - Toggle expand/collapse"),
        Line::from("  r         - Find references for selected function"),
        Line::from(""),
        Line::from("Functions List:"),
        Line::from("  Enter     - Start exploring from selected function"),
        Line::from("  r         - Find references for selected function"),
        Line::from(""),
        Line::from("General:"),
        Line::from("  Tab       - Switch between tabs"),
        Line::from("  1, 2, 3   - Jump to specific tab"),
        Line::from("  ?         - Show/hide this help"),
        Line::from("  q / Esc   - Quit application"),
        Line::from(""),
        Line::from("🌲 Tree View Behavior:"),
        Line::from("  • Nodes represent functions/symbols"),
        Line::from("  • Indentation shows call hierarchy"),
        Line::from("  • Children are loaded dynamically when expanded"),
        Line::from("  • Navigate with arrow keys or vim-style keys"),
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

fn render_tree_call_graph(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    if app.tree_view_state.nodes.is_empty() {
        let content = "No function selected for call graph exploration.\n\n\
                      Instructions:\n\
                      1. Go to the Functions tab (press '1' or Tab)\n\
                      2. Select a function (↑↓ to navigate)\n\
                      3. Press Enter to start exploring\n\n\
                      Then use:\n\
                      • ↑↓ or kj to navigate\n\
                      • →l to expand nodes\n\
                      • ←h to collapse nodes\n\
                      • Enter to toggle expand/collapse";

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Call Graph - Tree View"),
            )
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = app
        .tree_view_state
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            if let Some(function) = app.call_graph.get_function(&node.symbol_id) {
                let indent = "  ".repeat(node.depth);

                let expand_indicator = if node.is_loading {
                    "⏳"
                } else if node.is_expanded {
                    if node.has_children {
                        "▼"
                    } else {
                        "○"
                    }
                } else if node.children_loaded && !node.has_children {
                    "○"
                } else {
                    "▶"
                };

                let style = if function.diagnostics.is_empty() {
                    Style::default().fg(Color::Green)
                } else {
                    let has_errors = function
                        .diagnostics
                        .iter()
                        .any(|d| matches!(d.severity, core_data::DiagnosticSeverity::Error));
                    if has_errors {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Yellow)
                    }
                };

                // Highlight selected node
                let final_style = if index == app.tree_view_state.selected_index {
                    style.add_modifier(Modifier::BOLD).bg(Color::DarkGray)
                } else {
                    style
                };

                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(expand_indicator, final_style),
                    Span::raw(" "),
                    Span::styled(&function.name, final_style),
                    Span::raw(" "),
                    Span::styled(
                        format!("({})", function.definition_location.file_path),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            } else {
                ListItem::new(Line::from("Invalid function"))
            }
        })
        .collect();

    let title = format!(
        "Call Graph - Call Hierarchy ({} nodes) - Use ↑↓/kj, →l/←h",
        app.tree_view_state.nodes.len()
    );

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .style(Style::default());

    f.render_widget(list, area);
}

fn render_diagnostics(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // For diagnostics, we'll compute basic stats without the query engine to avoid initialization overhead
    let total_functions = app.call_graph.nodes.len();
    let total_call_relationships = app.call_graph.edges.len();

    let functions_with_diagnostics = app
        .call_graph
        .nodes
        .values()
        .filter(|f| !f.diagnostics.is_empty())
        .count();

    let functions_with_errors = app
        .call_graph
        .nodes
        .values()
        .filter(|f| {
            f.diagnostics
                .iter()
                .any(|d| matches!(d.severity, core_data::DiagnosticSeverity::Error))
        })
        .count();

    let functions_with_warnings = app
        .call_graph
        .nodes
        .values()
        .filter(|f| {
            f.diagnostics
                .iter()
                .any(|d| matches!(d.severity, core_data::DiagnosticSeverity::Warning))
        })
        .count();

    // Simple entry point detection (functions not called by others)
    let called_functions: std::collections::HashSet<_> = app
        .call_graph
        .edges
        .iter()
        .map(|edge| &edge.callee)
        .collect();
    let entry_points = app
        .call_graph
        .nodes
        .keys()
        .filter(|id| !called_functions.contains(id))
        .count();

    // Simple leaf function detection (functions that don't call others)
    let calling_functions: std::collections::HashSet<_> = app
        .call_graph
        .edges
        .iter()
        .map(|edge| &edge.caller)
        .collect();
    let leaf_functions = app
        .call_graph
        .nodes
        .keys()
        .filter(|id| !calling_functions.contains(id))
        .count();

    let average_calls = if total_functions > 0 {
        total_call_relationships as f64 / total_functions as f64
    } else {
        0.0
    };

    let content = format!(
        "Graph Statistics:\n\
        Total Functions: {}\n\
        Total Call Relationships: {}\n\
        Entry Points: {}\n\
        Leaf Functions: {}\n\
        Functions with Diagnostics: {}\n\
        Average Calls per Function: {:.2}\n\n\
        Problem Areas:\n\
        Functions with Errors: {}\n\
        Functions with Warnings: {}\n\
        \n\
        Note: Advanced analysis available when needed",
        total_functions,
        total_call_relationships,
        entry_points,
        leaf_functions,
        functions_with_diagnostics,
        average_calls,
        functions_with_errors,
        functions_with_warnings
    );

    let paragraph = Paragraph::new(content).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Diagnostics & Statistics"),
    );
    f.render_widget(paragraph, area);
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
        assert_eq!(app.current_tab, 0);
        assert!(app.selected_function.is_none());
        assert_eq!(app.search_query, "");
        assert!(!app.should_quit);
        assert!(!app.show_help);
        assert_eq!(app.tree_view_state.nodes.len(), 0);
    }

    #[test]
    fn test_tab_navigation() {
        let mut app = create_test_app();

        app.next_tab();
        assert_eq!(app.current_tab, 1);

        app.next_tab();
        assert_eq!(app.current_tab, 2);

        app.next_tab();
        assert_eq!(app.current_tab, 0);

        app.previous_tab();
        assert_eq!(app.current_tab, 2);
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
    fn test_tree_view_initialization() {
        let mut app = create_test_app();
        let func_id = app.call_graph.nodes.keys().next().unwrap().clone();

        app.start_call_graph_with_function(func_id.clone());

        assert_eq!(app.selected_function, Some(func_id.clone()));
        assert_eq!(app.current_tab, 1);
        assert_eq!(app.tree_view_state.nodes.len(), 1);
        assert_eq!(app.tree_view_state.nodes[0].symbol_id, func_id);
        assert_eq!(app.tree_view_state.selected_index, 0);
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
