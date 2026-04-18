use crate::service::document;
use crate::service::response::references;
use crate::service::worker::LspWorkerState;
use crate::service::LspResponse;
use lsp_types::DocumentSymbol;
use serde_json::Value;
use tokio::sync::mpsc;

pub(super) async fn handle_document_symbols_response(
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

pub(super) async fn handle_workspace_symbols_response(
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

                let symbols: Vec<model::WorkspaceSymbolInfo> = workspace_symbols
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
                        model::WorkspaceSymbolInfo {
                            name: symbol.name.clone(),
                            qualified_name: crate::make_qualified_name(
                                &symbol.container_name,
                                &symbol.name,
                            ),
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
                    .map(|sym| model::FunctionNode::new(sym.name, sym.qualified_name, sym.location))
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
                    if let Some(base_request_id) = request_id.strip_prefix("document_symbol_for_") {
                        references::handle_document_symbols_for_enhanced_references(
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
                                references::handle_document_symbols_for_enhanced_references(
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
