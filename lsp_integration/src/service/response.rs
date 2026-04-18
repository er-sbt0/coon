use lsp_types::DocumentSymbol;
use serde_json::Value;
use tokio::sync::mpsc;
use super::worker::{EnhancedRequestInfo, LspWorkerState, RequestType};
use super::LspResponse;
use super::document;

pub(super) async fn handle_lsp_message(
    message: Value,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
        if let Some(request_id) = state.service_requests.remove(&id) {
            let request_type = state.request_types.remove(&id);

            match request_type {
                Some(RequestType::CallHierarchy) => {
                    handle_call_hierarchy_response(message, request_id, state, response_tx).await;
                }
                Some(RequestType::OutgoingCalls) => {
                    handle_outgoing_calls_response(message, request_id, state, response_tx).await;
                }
                Some(RequestType::IncomingCalls) => {
                    handle_incoming_calls_response(message, request_id, state, response_tx).await;
                }
                Some(RequestType::PrepareCallHierarchy) => {
                    handle_prepare_call_hierarchy_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
                Some(RequestType::References) => {
                    handle_references_response(message, request_id, state, response_tx).await;
                }
                Some(RequestType::ReferencesWithSymbols) => {
                    handle_references_with_symbols_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
                Some(RequestType::DocumentSymbols) => {
                    handle_document_symbols_response(message, request_id, state, response_tx)
                        .await;
                }
                Some(RequestType::WorkspaceSymbols) => {
                    handle_workspace_symbols_response(message, request_id, state, response_tx)
                        .await;
                }
                Some(RequestType::Hover) => {
                    handle_hover_response(message, request_id, state, response_tx).await;
                }
                None => {
                    log::warn!("No request type tracked for LSP request {}, falling back to content detection", id);
                    handle_legacy_response_detection(message, request_id, state, response_tx)
                        .await;
                }
            }

            state.enhanced_lsp_requests.remove(&id);
        }
    }
}

async fn handle_call_hierarchy_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for call hierarchy request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(call_hierarchy_response)) =
        state.client.parse_prepare_call_hierarchy_response(&message)
    {
        log::info!(
            "LSP Call Hierarchy Response: found {} items for request {}",
            call_hierarchy_response.items.len(),
            request_id
        );
        for (i, item) in call_hierarchy_response.items.iter().enumerate() {
            log::info!(
                "  Call hierarchy item {}: name='{}', kind={:?}, uri={}, range={}:{}-{}:{}",
                i,
                item.name,
                item.kind,
                item.uri,
                item.range.start.line,
                item.range.start.character,
                item.range.end.line,
                item.range.end.character
            );
        }
        let _ = response_tx
            .send(LspResponse::CallHierarchy {
                request_id,
                items: call_hierarchy_response.items,
            })
            .await;
    } else {
        log::error!(
            "Failed to parse call hierarchy response for request {}",
            request_id
        );
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse call hierarchy response".to_string(),
            })
            .await;
    }
}

async fn handle_outgoing_calls_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for outgoing calls request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(outgoing_calls_response)) =
        state.client.parse_outgoing_calls_response(&message)
    {
        log::info!(
            "LSP Outgoing Calls Response: found {} calls for request {}",
            outgoing_calls_response.calls.len(),
            request_id
        );
        for (i, call) in outgoing_calls_response.calls.iter().enumerate() {
            log::info!(
                "  Outgoing call {}: name='{}', kind={:?}, uri={}, range={}:{}-{}:{}",
                i,
                call.to.name,
                call.to.kind,
                call.to.uri,
                call.to.range.start.line,
                call.to.range.start.character,
                call.to.range.end.line,
                call.to.range.end.character
            );
        }
        let _ = response_tx
            .send(LspResponse::OutgoingCalls {
                request_id,
                calls: outgoing_calls_response.calls,
            })
            .await;
    } else {
        log::error!(
            "Failed to parse outgoing calls response for request {}",
            request_id
        );
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse outgoing calls response".to_string(),
            })
            .await;
    }
}

async fn handle_incoming_calls_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for incoming calls request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(incoming_calls_response)) =
        state.client.parse_incoming_calls_response(&message)
    {
        log::info!(
            "LSP Incoming Calls Response: found {} calls for request {}",
            incoming_calls_response.calls.len(),
            request_id
        );
        for (i, call) in incoming_calls_response.calls.iter().enumerate() {
            log::info!(
                "  Incoming call {}: name='{}', kind={:?}, uri={}, range={}:{}-{}:{}",
                i,
                call.from.name,
                call.from.kind,
                call.from.uri,
                call.from.range.start.line,
                call.from.range.start.character,
                call.from.range.end.line,
                call.from.range.end.character
            );
        }
        let _ = response_tx
            .send(LspResponse::IncomingCalls {
                request_id,
                calls: incoming_calls_response.calls,
            })
            .await;
    } else {
        log::error!(
            "Failed to parse incoming calls response for request {}",
            request_id
        );
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse incoming calls response".to_string(),
            })
            .await;
    }
}

async fn handle_prepare_call_hierarchy_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for prepare call hierarchy request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(call_hierarchy_response)) =
        state.client.parse_prepare_call_hierarchy_response(&message)
    {
        log::info!(
            "LSP Prepare Call Hierarchy Response: found {} items for request {}",
            call_hierarchy_response.items.len(),
            request_id
        );
        for (i, item) in call_hierarchy_response.items.iter().enumerate() {
            log::info!(
                "  Call hierarchy item {}: name='{}', kind={:?}, uri={}, range={}:{}-{}:{}",
                i,
                item.name,
                item.kind,
                item.uri,
                item.range.start.line,
                item.range.start.character,
                item.range.end.line,
                item.range.end.character
            );
        }
        let _ = response_tx
            .send(LspResponse::CallHierarchyPrepared {
                request_id,
                items: call_hierarchy_response.items,
            })
            .await;
    } else {
        log::error!(
            "Failed to parse prepare call hierarchy response for request {}",
            request_id
        );
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse prepare call hierarchy response".to_string(),
            })
            .await;
    }
}

async fn handle_references_response(
    message: Value,
    request_id: String,
    _state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for references request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    let locations = parse_references_response_content(&message);
    log::info!(
        "LSP References Response: found {} references for request {}",
        locations.len(),
        request_id
    );
    for (i, loc) in locations.iter().enumerate() {
        log::info!(
            "  Reference {}: {}:{}:{}",
            i,
            loc.file_path,
            loc.line,
            loc.column
        );
    }
    let _ = response_tx
        .send(LspResponse::References {
            request_id,
            locations,
        })
        .await;
}

async fn handle_references_with_symbols_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for enhanced references request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    let enhanced_references =
        parse_enhanced_references_response(&message, &request_id, state, response_tx).await;

    if !enhanced_references.is_empty() {
        log::info!(
            "LSP Enhanced References Response: found {} references for request {}",
            enhanced_references.len(),
            request_id
        );
        for (i, ref_info) in enhanced_references.iter().enumerate() {
            if let Some(symbol) = &ref_info.referencing_symbol {
                log::info!(
                    "  Enhanced Reference {}: {}:{}:{} (from {}::{})",
                    i,
                    ref_info.location.file_path,
                    ref_info.location.line,
                    ref_info.location.column,
                    symbol.qualified_name,
                    symbol.name
                );
            } else {
                log::info!(
                    "  Enhanced Reference {}: {}:{}:{} (no symbol info)",
                    i,
                    ref_info.location.file_path,
                    ref_info.location.line,
                    ref_info.location.column
                );
            }
        }
        let _ = response_tx
            .send(LspResponse::ReferencesWithSymbols {
                request_id,
                references: enhanced_references,
            })
            .await;
    }
}

async fn handle_document_symbols_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for document symbols request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(document_symbols_response)) =
        state.client.parse_document_symbol_response(&message)
    {
        log::info!(
            "LSP Document Symbols Response: found {} symbols for request {}",
            document_symbols_response.symbols.len(),
            request_id
        );
        for (i, sym) in document_symbols_response.symbols.iter().enumerate() {
            log::info!(
                "  Document symbol {}: name='{}', kind={:?}, container={:?}, location={}:{}:{}",
                i,
                sym.name,
                sym.kind,
                sym.container_name.as_deref().unwrap_or("None"),
                sym.location.file_path,
                sym.location.line,
                sym.location.column
            );
        }

        if request_id.starts_with("document_symbols_for_") {
            handle_legacy_document_symbols_for_enhanced_references(
                message,
                request_id,
                state,
                response_tx,
            )
            .await;
        } else {
            let symbols = document_symbols_response
                .symbols
                .into_iter()
                .map(|sym| DocumentSymbol {
                    name: sym.name,
                    detail: sym.container_name,
                    kind: sym.kind,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: Some(false),
                    range: lsp_types::Range::default(),
                    selection_range: lsp_types::Range::default(),
                    children: None,
                })
                .collect();
            let _ = response_tx
                .send(LspResponse::DocumentSymbols {
                    request_id,
                    symbols,
                })
                .await;
        }
    } else {
        log::error!(
            "Failed to parse document symbols response for request {}",
            request_id
        );
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse document symbols response".to_string(),
            })
            .await;
    }
}

async fn handle_workspace_symbols_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for workspace symbols request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Some(result) = message.get("result") {
        if !result.is_null() {
            if let Ok(workspace_symbols) =
                serde_json::from_value::<Vec<lsp_types::WorkspaceSymbol>>(result.clone())
            {
                log::info!(
                    "LSP Workspace Symbols Response: found {} symbols for request {}",
                    workspace_symbols.len(),
                    request_id
                );

                let symbols: Vec<crate::WorkspaceSymbolInfo> = workspace_symbols
                    .iter()
                    .map(|symbol| {
                        let location = match &symbol.location {
                            lsp_types::OneOf::Left(location) => {
                                crate::convert_lsp_location(location)
                            }
                            lsp_types::OneOf::Right(workspace_location) => model::Location {
                                file_path: workspace_location
                                    .uri
                                    .to_file_path()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| workspace_location.uri.to_string()),
                                line: 0,
                                column: 0,
                                length: None,
                            },
                        };
                        crate::WorkspaceSymbolInfo {
                            name: symbol.name.clone(),
                            kind: symbol.kind,
                            location,
                            container_name: symbol.container_name.clone(),
                        }
                    })
                    .collect();

                for (i, sym) in symbols.iter().enumerate() {
                    log::info!(
                        "  Workspace symbol {}: name='{}', kind={:?}, container={:?}, location={}:{}:{}",
                        i,
                        sym.name,
                        sym.kind,
                        sym.container_name.as_deref().unwrap_or("None"),
                        sym.location.file_path,
                        sym.location.line,
                        sym.location.column
                    );
                }

                let function_symbols: Vec<model::FunctionNode> = symbols
                    .into_iter()
                    .filter(|sym| {
                        let is_function = matches!(
                            sym.kind,
                            lsp_types::SymbolKind::FUNCTION
                                | lsp_types::SymbolKind::METHOD
                                | lsp_types::SymbolKind::CONSTRUCTOR
                        );
                        let is_project_file = if state.project_files.is_empty() {
                            true
                        } else {
                            state.project_files.iter().any(|project_file| {
                                sym.location.file_path.contains(project_file)
                                    || project_file.contains(&sym.location.file_path)
                            })
                        };
                        is_function && is_project_file
                    })
                    .map(|sym| {
                        let qualified_name = if let Some(container) = &sym.container_name {
                            format!("{}::{}", container, sym.name)
                        } else {
                            sym.name.clone()
                        };
                        model::FunctionNode::new(sym.name, qualified_name, sym.location)
                    })
                    .collect();

                log::info!("Filtered to {} function symbols", function_symbols.len());
                let _ = response_tx
                    .send(LspResponse::WorkspaceSymbols {
                        request_id,
                        symbols: function_symbols,
                    })
                    .await;
            } else {
                log::error!(
                    "Failed to parse workspace symbols response for request {}",
                    request_id
                );
                log::debug!("Raw response: {}", message);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: "Failed to parse workspace symbols response".to_string(),
                    })
                    .await;
            }
        } else {
            log::info!(
                "Empty workspace symbols response for request {}",
                request_id
            );
            let _ = response_tx
                .send(LspResponse::WorkspaceSymbols {
                    request_id,
                    symbols: Vec::new(),
                })
                .await;
        }
    } else {
        log::error!(
            "Missing result field in workspace symbols response for request {}",
            request_id
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Missing result field in workspace symbols response".to_string(),
            })
            .await;
    }
}

async fn handle_hover_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(error) = message.get("error") {
        let error_msg = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for hover request {}: {}",
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(hover_response)) = state.client.parse_hover_response(&message) {
        log::debug!(
            "LSP Hover Response for request {}: hover_info={:?}",
            request_id,
            hover_response.hover_info
        );

        if request_id.starts_with("hover_for_") {
            log::debug!("Raw hover response message for {}: {}", request_id, message);
            handle_hover_for_enhanced_references(hover_response, &request_id, state, response_tx)
                .await;
        }
    } else {
        log::error!("Failed to parse hover response for request {}", request_id);
        log::debug!("Raw response: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Failed to parse hover response".to_string(),
            })
            .await;
    }
}

async fn handle_legacy_response_detection(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    log::warn!("Using legacy response detection for request {}", request_id);

    if message.get("error").is_some() {
        let error_msg = message
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error");
        log::error!(
            "LSP Error Response for request {}: {}",
            request_id,
            error_msg
        );
        log::debug!("Full LSP error message: {}", message);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: error_msg.to_string(),
            })
            .await;
        return;
    }

    if let Ok(Some(_)) = state.client.parse_prepare_call_hierarchy_response(&message) {
        handle_call_hierarchy_response(message, request_id, state, response_tx).await;
    } else if let Ok(Some(_)) = state.client.parse_outgoing_calls_response(&message) {
        handle_outgoing_calls_response(message, request_id, state, response_tx).await;
    } else if is_references_response(&message) {
        if was_enhanced_references_request(&message, state) {
            handle_references_with_symbols_response(message, request_id, state, response_tx).await;
        } else {
            handle_references_response(message, request_id, state, response_tx).await;
        }
    } else if let Ok(Some(_)) = state.client.parse_hover_response(&message) {
        handle_hover_response(message, request_id, state, response_tx).await;
    } else if let Ok(Some(_)) = state.client.parse_document_symbol_response(&message) {
        handle_document_symbols_response(message, request_id, state, response_tx).await;
    } else if message.get("result").is_some() {
        log::warn!(
            "Falling back to workspace symbols parsing for request {} - this may be incorrect!",
            request_id
        );
        handle_workspace_symbols_response(message, request_id, state, response_tx).await;
    } else {
        log::error!(
            "Unrecognized LSP response for request {}: {}",
            request_id,
            message
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "Unrecognized LSP response type".to_string(),
            })
            .await;
    }
}

async fn handle_legacy_document_symbols_for_enhanced_references(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    log::debug!(
        "Processing document symbol response for enhanced references: {}",
        request_id
    );

    if let Some(result) = message.get("result") {
        if !result.is_null() {
            match serde_json::from_value::<Vec<lsp_types::DocumentSymbol>>(result.clone()) {
                Ok(document_symbols) => {
                    log::info!(
                        "Successfully parsed document symbols for enhanced references request {}: found {} symbols",
                        request_id,
                        document_symbols.len()
                    );
                    if let Some(base_request_id) =
                        request_id.strip_prefix("document_symbol_for_")
                    {
                        handle_document_symbols_for_enhanced_references(
                            base_request_id,
                            &document_symbols,
                            state,
                            response_tx,
                        )
                        .await;
                    }
                }
                Err(_) => {
                    match serde_json::from_value::<Vec<lsp_types::SymbolInformation>>(
                        result.clone(),
                    ) {
                        Ok(symbol_infos) => {
                            log::info!(
                                "Successfully parsed symbol information for enhanced references request {}: found {} symbols",
                                request_id,
                                symbol_infos.len()
                            );
                            let document_symbols =
                                document::convert_symbol_info_to_document_symbols(&symbol_infos);
                            if let Some(base_request_id) =
                                request_id.strip_prefix("document_symbol_for_")
                            {
                                handle_document_symbols_for_enhanced_references(
                                    base_request_id,
                                    &document_symbols,
                                    state,
                                    response_tx,
                                )
                                .await;
                            }
                        }
                        Err(e) => {
                            log::error!(
                                "Error parsing document symbols or symbol information for enhanced references {}: {:?}",
                                request_id,
                                e
                            );
                            log::debug!(
                                "Raw response: {}",
                                serde_json::to_string_pretty(result)
                                    .unwrap_or_else(|_| "failed to serialize".to_string())
                            );
                        }
                    }
                }
            }
        } else {
            log::warn!(
                "Document symbols response was null for enhanced references request: {}",
                request_id
            );
        }
    }
}

async fn handle_hover_for_enhanced_references(
    hover_response: crate::HoverResponse,
    request_id: &str,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if let Some(remaining) = request_id.strip_prefix("hover_for_") {
        if let Some(last_underscore) = remaining.rfind('_') {
            let service_request_id = &remaining[..last_underscore];
            let index_str = &remaining[last_underscore + 1..];

            if let Ok(index) = index_str.parse::<usize>() {
                log::debug!(
                    "Processing hover response for service_request_id={}, index={}",
                    service_request_id,
                    index
                );

                if let Some(pending) =
                    state.pending_enhanced_requests.get_mut(service_request_id)
                {
                    pending.pending_symbol_requests.remove(index_str);

                    let symbol_name = if let Some(hover_info) = &hover_response.hover_info {
                        crate::extract_function_name_from_signature(hover_info)
                    } else {
                        None
                    };

                    log::debug!(
                        "Extracted symbol name for index {}: {:?}",
                        index,
                        symbol_name
                    );

                    if pending.pending_symbol_requests.is_empty() {
                        let mut enhanced_refs = Vec::new();
                        for (i, location) in pending.locations.iter().enumerate() {
                            let referencing_symbol =
                                if i == index && symbol_name.is_some() {
                                    Some(model::ReferencingSymbol {
                                        name: symbol_name.clone().unwrap(),
                                        qualified_name: symbol_name.clone().unwrap(),
                                        kind: model::ReferenceSymbolKind::Function,
                                    })
                                } else {
                                    None
                                };
                            enhanced_refs.push(model::Reference {
                                location: location.clone(),
                                referencing_symbol,
                            });
                        }

                        log::info!(
                            "Completed enhanced references for {}: {} references with symbol info",
                            service_request_id,
                            enhanced_refs.len()
                        );

                        let _ = response_tx
                            .send(LspResponse::ReferencesWithSymbols {
                                request_id: service_request_id.to_string(),
                                references: enhanced_refs,
                            })
                            .await;

                        state.pending_enhanced_requests.remove(service_request_id);
                    } else {
                        log::debug!(
                            "Still waiting for {} more hover responses for {}",
                            pending.pending_symbol_requests.len(),
                            service_request_id
                        );
                    }
                } else {
                    log::warn!(
                        "No pending enhanced request found for service_request_id: {}",
                        service_request_id
                    );
                }
            } else {
                log::warn!(
                    "Failed to parse index from hover request ID: {}",
                    request_id
                );
            }
        } else {
            log::warn!("Invalid hover request ID format: {}", request_id);
        }
    } else {
        log::warn!(
            "Hover request ID does not start with 'hover_for_': {}",
            request_id
        );
    }
}

async fn handle_document_symbols_for_enhanced_references(
    base_request_id: &str,
    document_symbols: &[lsp_types::DocumentSymbol],
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    log::debug!(
        "Processing document symbols for enhanced references request: {}",
        base_request_id
    );

    if let Some(pending_request) = state.pending_enhanced_requests.get(base_request_id) {
        let mut enhanced_references = Vec::new();

        for location in &pending_request.locations {
            let position = lsp_types::Position {
                line: location.line.saturating_sub(1),
                character: location.column.saturating_sub(1),
            };

            if let Some(containing_symbol) =
                document::find_containing_symbol(document_symbols, &position)
            {
                enhanced_references.push(model::Reference {
                    location: location.clone(),
                    referencing_symbol: Some(model::ReferencingSymbol {
                        name: containing_symbol.name.clone(),
                        qualified_name: document::get_qualified_symbol_name(containing_symbol),
                        kind: document::convert_lsp_symbol_kind(containing_symbol.kind),
                    }),
                });
                log::debug!(
                    "Found containing symbol '{}' for reference at {}:{}:{}",
                    containing_symbol.name,
                    location.file_path,
                    location.line,
                    location.column
                );
            } else {
                enhanced_references.push(model::Reference {
                    location: location.clone(),
                    referencing_symbol: None,
                });
                log::debug!(
                    "No containing symbol found for reference at {}:{}:{}",
                    location.file_path,
                    location.line,
                    location.column
                );
            }
        }

        let response = LspResponse::ReferencesWithSymbols {
            request_id: base_request_id.to_string(),
            references: enhanced_references,
        };

        if let Err(e) = response_tx.send(response).await {
            log::error!("Failed to send enhanced references response: {:?}", e);
        } else {
            log::info!(
                "Sent enhanced references response for request {}",
                base_request_id
            );
        }

        state.pending_enhanced_requests.remove(base_request_id);
    } else {
        log::warn!(
            "No pending request found for enhanced references: {}",
            base_request_id
        );
    }
}

fn is_references_response(response: &Value) -> bool {
    if let Some(result) = response.get("result") {
        if serde_json::from_value::<Vec<lsp_types::Location>>(result.clone()).is_ok() {
            return true;
        }
    }
    response.get("result").map_or(false, |r| r.is_null())
}

fn parse_references_response_content(response: &Value) -> Vec<model::Location> {
    if let Some(result) = response.get("result") {
        if result.is_null() {
            log::debug!("References response has null result, returning empty vec");
            return Vec::new();
        }
        match serde_json::from_value::<Vec<lsp_types::Location>>(result.clone()) {
            Ok(lsp_locations) => {
                log::debug!("Successfully parsed {} LSP locations", lsp_locations.len());
                lsp_locations
                    .iter()
                    .map(crate::convert_lsp_location)
                    .collect()
            }
            Err(e) => {
                log::error!("Failed to parse references response: {}", e);
                Vec::new()
            }
        }
    } else {
        log::debug!("References response has no result field");
        Vec::new()
    }
}

fn was_enhanced_references_request(message: &Value, state: &LspWorkerState) -> bool {
    if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
        let is_enhanced = state.enhanced_lsp_requests.contains(&id);
        log::debug!(
            "Checking if request {} is enhanced: {} (tracked enhanced requests: {:?})",
            id,
            is_enhanced,
            state.enhanced_lsp_requests
        );
        return is_enhanced;
    }
    log::debug!("No request ID found in message for enhanced check");
    false
}

async fn parse_enhanced_references_response(
    message: &Value,
    service_request_id: &str,
    state: &mut LspWorkerState,
    _response_tx: &mpsc::Sender<LspResponse>,
) -> Vec<model::Reference> {
    let locations = parse_references_response_content(message);
    let _lsp_request_id = message.get("id").and_then(|i| i.as_i64());

    log::info!(
        "Enhancing {} reference locations with symbol information using hover requests",
        locations.len()
    );

    if locations.is_empty() {
        return Vec::new();
    }

    log::debug!("Using service request ID: {}", service_request_id);
    log::debug!("Using service request ID: {}", service_request_id);

    state.pending_enhanced_requests.insert(
        service_request_id.to_string(),
        EnhancedRequestInfo {
            request_id: service_request_id.to_string(),
            locations: locations.clone(),
            pending_symbol_requests: locations
                .iter()
                .enumerate()
                .map(|(i, _)| i.to_string())
                .collect(),
        },
    );

    let mut files_to_analyze: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for location in locations.iter() {
        files_to_analyze.insert(location.file_path.clone());
    }

    log::info!(
        "Need to analyze {} unique files for symbol information",
        files_to_analyze.len()
    );

    for file_path in files_to_analyze {
        if let Ok(document_uri) = lsp_types::Url::from_file_path(&file_path) {
            let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
            match state.client.document_symbol(text_document).await {
                Ok(lsp_request_id) => {
                    let request_id = format!("document_symbol_for_{}", service_request_id);
                    state.track_request(
                        lsp_request_id,
                        request_id,
                        RequestType::DocumentSymbols,
                    );
                    state.enhanced_lsp_requests.insert(lsp_request_id);
                    log::debug!(
                        "Sent document symbol request for {} (lsp_request_id: {})",
                        file_path,
                        lsp_request_id
                    );
                }
                Err(e) => {
                    log::error!(
                        "Failed to send document symbol request for {}: {:?}",
                        file_path,
                        e
                    );
                }
            }
        } else {
            log::error!("Failed to convert file path to URI: {}", file_path);
        }
    }

    Vec::new()
}

