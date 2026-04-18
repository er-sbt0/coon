use super::worker::{LspWorkerState, RequestType};
use super::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

mod call_hierarchy;
mod legacy;
mod references;
mod symbols;

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
                    call_hierarchy::handle_call_hierarchy_response(
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
                Some(RequestType::PrepareCallHierarchy) => {
                    call_hierarchy::handle_prepare_call_hierarchy_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
                Some(RequestType::References) => {
                    references::handle_references_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
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
                    symbols::handle_document_symbols_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
                Some(RequestType::WorkspaceSymbols) => {
                    symbols::handle_workspace_symbols_response(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
                Some(RequestType::Hover) => {
                    references::handle_hover_response(message, request_id, state, response_tx)
                        .await;
                }
                None => {
                    log::warn!("No request type tracked for LSP request {}, falling back to content detection", id);
                    legacy::handle_legacy_response_detection(
                        message,
                        request_id,
                        state,
                        response_tx,
                    )
                    .await;
                }
            }

            state.enhanced_lsp_requests.remove(&id);
        }
    }
}
