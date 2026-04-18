use crate::service::response::{call_hierarchy, references, symbols};
use crate::service::worker::LspWorkerState;
use crate::service::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

pub(super) async fn handle_legacy_response_detection(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    log::warn!("Using legacy response detection for request {}", request_id);

    if super::check_and_send_lsp_error(&message, &request_id, "legacy", response_tx).await {
        return;
    }

    if let Ok(Some(_)) = state.client.parse_prepare_call_hierarchy_response(&message) {
        call_hierarchy::handle_call_hierarchy_response(message, request_id, state, response_tx)
            .await;
    } else if let Ok(Some(_)) = state.client.parse_outgoing_calls_response(&message) {
        call_hierarchy::handle_outgoing_calls_response(message, request_id, state, response_tx)
            .await;
    } else if references::is_references_response(&message) {
        if references::was_enhanced_references_request(&message, state) {
            references::handle_references_with_symbols_response(
                message,
                request_id,
                state,
                response_tx,
            )
            .await;
        } else {
            references::handle_references_response(message, request_id, state, response_tx).await;
        }
    } else if let Ok(Some(_)) = state.client.parse_hover_response(&message) {
        references::handle_hover_response(message, request_id, state, response_tx).await;
    } else if let Ok(Some(_)) = state.client.parse_document_symbol_response(&message) {
        symbols::handle_document_symbols_response(message, request_id, state, response_tx).await;
    } else if message.get("result").is_some() {
        log::warn!(
            "Falling back to workspace symbols parsing for request {} - this may be incorrect!",
            request_id
        );
        symbols::handle_workspace_symbols_response(message, request_id, state, response_tx).await;
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
