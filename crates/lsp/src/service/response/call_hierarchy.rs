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
    super::parse_client_response(
        message,
        request_id,
        "call hierarchy",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_prepare_call_hierarchy_response(msg),
        |id, resp| LspResponse::CallHierarchy {
            request_id: id,
            items: resp.items,
        },
    )
    .await;
}

pub(super) async fn handle_outgoing_calls_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    super::parse_client_response(
        message,
        request_id,
        "outgoing calls",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_outgoing_calls_response(msg),
        |id, resp| LspResponse::OutgoingCalls {
            request_id: id,
            calls: resp.calls,
        },
    )
    .await;
}

pub(super) async fn handle_incoming_calls_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    super::parse_client_response(
        message,
        request_id,
        "incoming calls",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_incoming_calls_response(msg),
        |id, resp| LspResponse::IncomingCalls {
            request_id: id,
            calls: resp.calls,
        },
    )
    .await;
}

pub(super) async fn handle_prepare_call_hierarchy_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    super::parse_client_response(
        message,
        request_id,
        "prepare call hierarchy",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_prepare_call_hierarchy_response(msg),
        |id, resp| LspResponse::CallHierarchyPrepared {
            request_id: id,
            items: resp.items,
        },
    )
    .await;
}
