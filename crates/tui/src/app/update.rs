use std::time::Instant;

use lsp::LspResponse;
use model::SymbolId;

use crate::graph_adapter::CallDirection;

use super::{App, LoadingState, PendingRequest};

impl App {
    /// Handle an LSP response
    pub(super) fn handle_lsp_response(&mut self, response: LspResponse) {
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
                                            timestamp: Instant::now(),
                                            symbol_id: Some(symbol_id.clone()),
                                        },
                                    );

                                    // Send outgoing calls request
                                    if let Some(tx) = &self.lsp_request_tx {
                                        let request = lsp::LspRequest::GetOutgoingCalls {
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
                                            timestamp: Instant::now(),
                                            symbol_id: Some(symbol_id.clone()),
                                        },
                                    );

                                    // Send incoming calls request
                                    if let Some(tx) = &self.lsp_request_tx {
                                        let request = lsp::LspRequest::GetIncomingCalls {
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
            let location = model::Location::new(
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
                    model::FunctionNode::new(call.to.name.clone(), qualified_name, location);
                self.call_graph.add_function(callee_function)
            };

            // Create the call edge from the original function to this callee
            // Use the call location from the LSP response
            for from_range in &call.from_ranges {
                let call_location = model::Location::new(
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
            let location = model::Location::new(
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
                    model::FunctionNode::new(call.from.name.clone(), qualified_name, location);
                self.call_graph.add_function(caller_function)
            };

            // Create the call edge from the caller to the original function
            // Use the call location from the LSP response
            for from_range in &call.from_ranges {
                let call_location = model::Location::new(
                    call.from.uri.path().to_string(), // Use the caller's file for call location
                    (from_range.start.line + 1) as u32,
                    (from_range.start.character + 1) as u32,
                );
                self.call_graph
                    .add_call(caller_id.clone(), symbol_id.clone(), call_location);
            }
        }
    }
}
