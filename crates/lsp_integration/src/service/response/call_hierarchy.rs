use crate::service::worker::LspWorkerState;
use crate::service::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

pub(super) async fn handle_call_hierarchy_response(
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

pub(super) async fn handle_outgoing_calls_response(
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

    if let Ok(Some(outgoing_calls_response)) = state.client.parse_outgoing_calls_response(&message)
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

pub(super) async fn handle_incoming_calls_response(
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

    if let Ok(Some(incoming_calls_response)) = state.client.parse_incoming_calls_response(&message)
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

pub(super) async fn handle_prepare_call_hierarchy_response(
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
