use super::worker::{LspWorkerState, RequestType};
use super::LspResponse;
use crate::LspClient;
use serde_json::Value;
use tokio::sync::mpsc;

mod call_hierarchy;
mod legacy;
mod progress;
mod references;
mod symbols;

// ---------------------------------------------------------------------------
// Shared response-handling helpers
// ---------------------------------------------------------------------------

/// Extract the error message from an LSP response, if present.
fn extract_lsp_error(message: &Value) -> Option<String> {
    message.get("error").map(|error| {
        error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown LSP error")
            .to_string()
    })
}

/// Check for an LSP error in `message` and send `LspResponse::Error` when
/// found.  Returns `true` if an error was sent (caller should return early).
async fn check_and_send_lsp_error(
    message: &Value,
    request_id: &str,
    context: &str,
    response_tx: &mpsc::Sender<LspResponse>,
) -> bool {
    if let Some(error_msg) = extract_lsp_error(message) {
        log::error!(
            "LSP Error Response for {} request {}: {}",
            context,
            request_id,
            error_msg
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id: request_id.to_string(),
                error: error_msg,
            })
            .await;
        true
    } else {
        false
    }
}

/// Generic handler for responses that are parsed via an `LspClient` method.
///
/// 1. Checks for an LSP error and sends `LspResponse::Error` if found.
/// 2. Calls `parse` to parse the response via the client.
/// 3. On success, converts to `LspResponse` via `to_response` and sends it.
/// 4. On failure, sends a parse-error `LspResponse::Error`.
async fn parse_client_response<T>(
    message: Value,
    request_id: String,
    context: &str,
    client: &mut LspClient,
    response_tx: &mpsc::Sender<LspResponse>,
    parse: impl FnOnce(&mut LspClient, &Value) -> anyhow::Result<Option<T>>,
    to_response: impl FnOnce(String, T) -> LspResponse,
) {
    if check_and_send_lsp_error(&message, &request_id, context, response_tx).await {
        return;
    }

    match parse(client, &message) {
        Ok(Some(parsed)) => {
            log::debug!("Parsed {} response for request {}", context, request_id);
            let _ = response_tx.send(to_response(request_id, parsed)).await;
        }
        Ok(None) => {
            log::error!(
                "No parsed result for {} response for request {}",
                context,
                request_id
            );
            log::debug!("Raw response: {}", message);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to parse {} response", context),
                })
                .await;
        }
        Err(e) => {
            log::error!(
                "Error parsing {} response for request {}: {}",
                context,
                request_id,
                e
            );
            log::debug!("Raw response: {}", message);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to parse {} response", context),
                })
                .await;
        }
    }
}

pub(super) async fn handle_lsp_message(
    message: Value,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    // A `method` field means clangd is talking to us rather than answering us:
    // a notification, or a request of its own. Checked *before* the id, because
    // clangd numbers its own requests from zero — the same space the ids in
    // `service_requests` live in, so matching on the id first can route
    // clangd's question into the handler for our question of the same number.
    if let Some(method) = message
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_owned)
    {
        match method.as_str() {
            "$/progress" => progress::handle_progress(message, response_tx).await,
            "window/workDoneProgress/create" => {
                progress::handle_work_done_progress_create(message, state).await
            }
            // Diagnostics, log messages, the rest. Nothing waits on them, so
            // they are only interesting in full-trace mode.
            _ => log::trace!("LSP notification: {method}"),
        }
        return;
    }

    let Some(id) = message.get("id").and_then(|i| i.as_i64()) else {
        log::warn!("LSP worker: message with neither a method nor an id, dropped");
        return;
    };

    if let Some(request_id) = state.service_requests.remove(&id) {
        let waited = state.finish_request(id);
        let request_type = state.request_types.remove(&id);
        log::info!(
            "LSP worker: clangd answered request {} after {:?}: type={:?}, service_id={} ({} still outstanding)",
            id,
            waited.unwrap_or_default(),
            request_type,
            request_id,
            state.service_requests.len()
        );

        match request_type {
            Some(RequestType::PrepareCallHierarchy) => {
                call_hierarchy::handle_prepare_call_hierarchy_response(
                    message,
                    request_id,
                    state,
                    response_tx,
                )
                .await;
            }
            Some(RequestType::OutgoingCalls) => {
                call_hierarchy::handle_outgoing_calls_response(
                    message,
                    request_id,
                    state,
                    response_tx,
                )
                .await;
            }
            Some(RequestType::IncomingCalls) => {
                call_hierarchy::handle_incoming_calls_response(
                    message,
                    request_id,
                    state,
                    response_tx,
                )
                .await;
            }
            Some(RequestType::References) => {
                references::handle_references_response(message, request_id, state, response_tx)
                    .await;
            }
            Some(RequestType::ReferencesWithSymbols) => {
                references::handle_references_with_symbols_response(
                    message,
                    request_id,
                    state,
                    response_tx,
                )
                .await;
            }
            Some(RequestType::DocumentSymbols) => {
                symbols::handle_document_symbols_response(message, request_id, state, response_tx)
                    .await;
            }
            Some(RequestType::DocumentSymbolsForEnhancedRefs { base_request_id }) => {
                symbols::handle_document_symbols_for_enhanced_refs(
                    message,
                    base_request_id,
                    state,
                    response_tx,
                )
                .await;
            }
            Some(RequestType::WorkspaceSymbols) => {
                symbols::handle_workspace_symbols_response(message, request_id, state, response_tx)
                    .await;
            }
            Some(RequestType::WorkspaceSymbolsChunk { query }) => {
                symbols::handle_workspace_symbols_chunk(
                    message,
                    request_id,
                    query,
                    state,
                    response_tx,
                )
                .await;
            }
            None => {
                log::warn!(
                    "No request type tracked for LSP request {}, falling back to content detection",
                    id
                );
                legacy::handle_legacy_response_detection(message, request_id, state, response_tx)
                    .await;
            }
        }

        state.enhanced_lsp_requests.remove(&id);
    } else {
        // Server-initiated traffic is routed above, so this is only ever clangd
        // answering something upstream has already given up on.
        state.finish_request(id);
        log::warn!(
            "LSP worker: reply to untracked request {} — dropped (was it already retired?)",
            id
        );
    }
}
