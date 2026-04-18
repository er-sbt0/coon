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
    if super::check_and_send_lsp_error(&message, &request_id, "document symbols", response_tx).await
    {
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
    if super::check_and_send_lsp_error(&message, &request_id, "workspace symbols", response_tx)
        .await
    {
        return;
    }

    if let Ok(Some(ws_response)) = state.client.parse_workspace_symbol_response(&message) {
        log::info!(
            "LSP Workspace Symbols Response: found {} symbols for request {}",
            ws_response.symbols.len(),
            request_id
        );

        for (i, sym) in ws_response.symbols.iter().enumerate() {
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

        let function_symbols: Vec<model::FunctionNode> = ws_response
            .symbols
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
}

/// Handle a document-symbols response that was issued as a sub-request of the
/// enhanced-references flow.  `base_request_id` is the original
/// `FindReferencesWithSymbols` service ID, passed via the typed
/// `RequestType::DocumentSymbolsForEnhancedRefs` variant (no string parsing).
pub(super) async fn handle_document_symbols_for_enhanced_refs(
    message: Value,
    base_request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if super::check_and_send_lsp_error(
        &message,
        &base_request_id,
        "document symbols (enhanced refs)",
        response_tx,
    )
    .await
    {
        return;
    }

    log::debug!(
        "Processing document symbol response for enhanced references: {}",
        base_request_id
    );

    let Some(result) = message.get("result") else {
        log::warn!(
            "No result field in document symbols response for enhanced references request: {}",
            base_request_id
        );
        return;
    };

    if result.is_null() {
        log::warn!(
            "Document symbols response was null for enhanced references request: {}",
            base_request_id
        );
        return;
    }

    // clangd may return either DocumentSymbol[] or SymbolInformation[].
    // Use the shared helper from parsing.rs that handles both shapes.
    let document_symbols = match crate::parsing::parse_document_symbols_from_result(result) {
        Ok(symbols) => {
            log::info!(
                "Parsed {} document symbol entries for enhanced references request {}",
                symbols.len(),
                base_request_id,
            );
            symbols
        }
        Err(e) => {
            log::error!(
                "Failed to parse document symbols for enhanced references {}: {:?}",
                base_request_id,
                e,
            );
            return;
        }
    };

    references::handle_document_symbols_for_enhanced_references(
        &base_request_id,
        &document_symbols,
        state,
        response_tx,
    )
    .await;
}
