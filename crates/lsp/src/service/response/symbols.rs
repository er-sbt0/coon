use crate::service::response::references;
use crate::service::worker::LspWorkerState;
use crate::service::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

pub(super) async fn handle_document_symbols_response(
    message: Value,
    request_id: String,
    _state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if super::check_and_send_lsp_error(&message, &request_id, "document symbols", response_tx).await
    {
        return;
    }

    let Some(result) = message.get("result") else {
        log::warn!(
            "No result field in document symbols response for request: {}",
            request_id
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: "No result in document symbols response".to_string(),
            })
            .await;
        return;
    };

    if result.is_null() {
        log::warn!(
            "Document symbols response was null for request: {}",
            request_id
        );
        let _ = response_tx
            .send(LspResponse::DocumentSymbols {
                request_id,
                symbols: Vec::new(),
            })
            .await;
        return;
    }

    // Parse directly into Vec<DocumentSymbol>, preserving range data.
    // clangd may return either DocumentSymbol[] or SymbolInformation[];
    // the shared helper handles both shapes.
    match crate::parsing::parse_document_symbols_from_result(result) {
        Ok(symbols) => {
            log::info!(
                "LSP Document Symbols Response: found {} symbols for request {}",
                symbols.len(),
                request_id
            );
            for (i, sym) in symbols.iter().enumerate() {
                log::info!(
                    "  Document symbol {}: name='{}', kind={:?}, detail={:?}, range={:?}",
                    i,
                    sym.name,
                    sym.kind,
                    sym.detail.as_deref().unwrap_or("None"),
                    sym.range
                );
            }
            let _ = response_tx
                .send(LspResponse::DocumentSymbols {
                    request_id,
                    symbols,
                })
                .await;
        }
        Err(e) => {
            log::error!(
                "Failed to parse document symbols response for request {}: {:?}",
                request_id,
                e
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

        // Trace, not info: an unlimited `workspace/symbol` over jemalloc answers
        // with ~19,600 symbols, and a line each buried the run in 20k lines of
        // log — and cost half a second to write.
        for (i, sym) in ws_response.symbols.iter().enumerate() {
            log::trace!(
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

        let function_symbols = project_functions(state, ws_response.symbols);

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

/// The functions among `symbols` that are defined in the project.
///
/// One definition for the plain query and for every chunk of a discovery
/// sweep, so the two cannot disagree about what counts.
fn project_functions(
    state: &LspWorkerState,
    symbols: Vec<model::WorkspaceSymbolInfo>,
) -> Vec<model::FunctionNode> {
    symbols
        .into_iter()
        .filter(|sym| {
            let is_function = matches!(
                sym.kind,
                lsp_types::SymbolKind::FUNCTION
                    | lsp_types::SymbolKind::METHOD
                    | lsp_types::SymbolKind::CONSTRUCTOR
            );
            // Under the project root, not in the compilation database:
            // a C project's headers are in no database entry, and
            // jemalloc defines ~736 functions in them. See
            // `service::project`.
            is_function && state.project_scope.contains_path(&sym.location.file_path)
        })
        .map(|sym| model::FunctionNode::new(sym.name, sym.qualified_name, sym.location))
        .collect()
}

/// Take in one chunk of the discovery sweep answering `request_id`, and send
/// the next — or, when the sweep has nothing left to ask, the whole answer.
///
/// Every chunk is reported as progress, which is how a caller tells a sweep
/// that is still moving from a server that has stopped answering: on the
/// kernel the sweep takes most of a minute, and each of its replies arrives
/// within a second or two of the last. See `service::discovery`.
pub(super) async fn handle_workspace_symbols_chunk(
    message: Value,
    request_id: String,
    query: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if super::check_and_send_lsp_error(&message, &request_id, "workspace symbols", response_tx)
        .await
    {
        state.sweeps.remove(&request_id);
        return;
    }
    // A `null` reply — a query that matches nothing — parses as an empty
    // chunk. `Ok(None)` is a reply the client was not expecting, which for a
    // chunk it sent itself means the sweep lost track of one, and a sweep
    // with a hole in it is not the whole project.
    let symbols = match state.client.parse_workspace_symbol_response(&message) {
        Ok(Some(ws_response)) => ws_response.symbols,
        failed => {
            let reason = match failed {
                Err(e) => e.to_string(),
                _ => "reply matched no pending workspace/symbol request".to_string(),
            };
            state.sweeps.remove(&request_id);
            log::error!("Failed to parse discovery chunk {query:?} for {request_id}: {reason}");
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: "Failed to parse workspace symbols response".to_string(),
                })
                .await;
            return;
        }
    };
    let raw_count = symbols.len();
    let functions = project_functions(state, symbols);

    let Some(sweep) = state.sweeps.get_mut(&request_id) else {
        log::warn!("discovery chunk {query:?} for {request_id} has no sweep; dropped");
        return;
    };
    sweep.absorb(&query, raw_count, functions);
    let found = sweep.found();
    log::debug!("discovery chunk {query:?}: {raw_count} raw result(s), {found} function(s) so far");

    if let Some(next) = sweep.next_query() {
        let _ = response_tx
            .send(LspResponse::WorkspaceSymbolsProgress {
                request_id: request_id.clone(),
                found,
            })
            .await;
        crate::service::request::send_sweep_chunk(state, response_tx, request_id, next).await;
        return;
    }

    let sweep = state
        .sweeps
        .remove(&request_id)
        .expect("the sweep was just read under this id");
    let (functions, summary) = sweep.finish();
    log::info!("{summary}");
    let _ = response_tx
        .send(LspResponse::WorkspaceSymbols {
            request_id,
            symbols: functions,
        })
        .await;
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
