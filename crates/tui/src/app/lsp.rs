use std::time::Instant;

use tokio::sync::mpsc;

use lsp::{LspRequest, LspResponse};
use model::SymbolId;

use super::{App, LoadingState, LspRequestType, PendingRequest};

impl App {
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
    pub(super) fn update_loading_state(&mut self, symbol_id: &SymbolId, state: LoadingState) {
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
    pub(super) fn update_node_loading_state(&mut self, symbol_id: &SymbolId, is_loading: bool) {
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
}
