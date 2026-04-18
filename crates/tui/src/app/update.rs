use lsp::LspResponse;
use model::SymbolId;

use crate::graph_adapter::CallDirection;

use super::lsp_bridge::LoadingState;
use super::App;

impl App {
    /// Handle an LSP response
    pub(super) fn handle_lsp_response(&mut self, response: LspResponse) {
        log::info!("TUI handling LSP response: {:?}", response);
        match response {
            LspResponse::OutgoingCalls { request_id, calls } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
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
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!("Processing references for symbol_id: {:?}", symbol_id);

                        // Process references and update the function
                        if let Some(function) = self.call_graph.get_function_mut(&symbol_id) {
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
                if let Some(_pending) = self.lsp.take_pending(&request_id) {
                    self.status_message = "Document symbols loaded".to_string();
                }
            }
            LspResponse::WorkspaceSymbols {
                request_id,
                symbols,
            } => {
                if let Some(_pending) = self.lsp.take_pending(&request_id) {
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

                    self.status_message =
                        format!("Loaded {} functions from workspace", function_count);
                }
            }
            LspResponse::Error { request_id, error } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
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
                if let Some(pending) = self.lsp.take_pending(&request_id) {
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
                if let Some(pending) = self.lsp.take_pending(&request_id) {
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
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        log::info!(
                            "Received call hierarchy prepare response for symbol: {:?}, {} items",
                            symbol_id,
                            items.len()
                        );

                        if !items.is_empty() {
                            let first_item = items[0].clone();

                            let current_direction = self
                                .get_current_workspace()
                                .map(|w| w.graph_view_state.direction)
                                .unwrap_or(CallDirection::Incoming);

                            let outgoing = matches!(current_direction, CallDirection::Outgoing);

                            if let Some(msg) = self
                                .lsp
                                .send_follow_up_call(&symbol_id, first_item, outgoing)
                            {
                                self.status_message = msg;
                            }
                        } else {
                            log::info!("No call hierarchy items found for symbol: {:?}", symbol_id);
                            self.update_loading_state(&symbol_id, LoadingState::Loaded);
                            self.status_message = "Function has no callees".to_string();
                        }
                    }
                }
            }
        }
    }

    fn update_function_outgoing_calls(
        &mut self,
        symbol_id: SymbolId,
        calls: Vec<lsp_types::CallHierarchyOutgoingCall>,
    ) {
        for call in calls {
            self.process_call_entry(
                symbol_id.clone(),
                call.to.name,
                &call.to.uri,
                call.to.range,
                &call.from_ranges,
                true,
            );
        }
    }

    fn update_function_incoming_calls(
        &mut self,
        symbol_id: SymbolId,
        calls: Vec<lsp_types::CallHierarchyIncomingCall>,
    ) {
        for call in calls {
            self.process_call_entry(
                symbol_id.clone(),
                call.from.name,
                &call.from.uri,
                call.from.range,
                &call.from_ranges,
                false,
            );
        }
    }

    fn process_call_entry(
        &mut self,
        symbol_id: SymbolId,
        other_name: String,
        other_uri: &lsp_types::Url,
        other_range: lsp_types::Range,
        from_ranges: &[lsp_types::Range],
        is_outgoing: bool,
    ) {
        let file_path = other_uri.path().to_string();
        let location = model::Location::new(
            file_path.clone(),
            ((other_range.start.line + 1)),
            ((other_range.start.character + 1)),
        );
        let qualified_name = format!("{}::{}", other_name, file_path);
        let other_id = self.call_graph.add_function(model::FunctionNode::new(
            other_name,
            qualified_name,
            location,
        ));
        for from_range in from_ranges {
            let call_location = model::Location::new(
                file_path.clone(),
                ((from_range.start.line + 1)),
                ((from_range.start.character + 1)),
            );
            let (caller, callee) = if is_outgoing {
                (symbol_id.clone(), other_id.clone())
            } else {
                (other_id.clone(), symbol_id.clone())
            };
            self.call_graph.add_call(caller, callee, call_location);
        }
    }
}
