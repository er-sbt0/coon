use crate::service::project::ProjectScope;
use crate::service::worker::LspWorkerState;
use crate::service::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

/// Drop the calls whose other end is not project code, and say how many.
///
/// This is where a callee becomes a node: `CoreApp::process_call_entry` adds
/// the function the item names to the graph, learns the item so the symbol
/// never needs a prepare of its own, and promotes it in the edge warmup — so a
/// system-header declaration that gets past here is a node, a queue entry and
/// a `+N` on somebody's badge. The discovery query is filtered by the same
/// rule (`service::project`); a call reply is the other door into the graph,
/// and libc only ever comes through this one.
fn keep_project_calls<T>(
    calls: &mut Vec<T>,
    scope: &ProjectScope,
    other_end: impl Fn(&T) -> &lsp_types::CallHierarchyItem,
    context: &str,
    request_id: &str,
) {
    let before = calls.len();
    calls.retain(|call| scope.contains_uri(&other_end(call).uri));
    let dropped = before - calls.len();
    if dropped > 0 {
        log::debug!(
            "Dropped {dropped} of {before} {context} outside the project for request {request_id}"
        );
    }
}

pub(super) async fn handle_outgoing_calls_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    let scope = &state.project_scope;
    super::parse_client_response(
        message,
        request_id,
        "outgoing calls",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_outgoing_calls_response(msg),
        |id, resp| {
            let mut calls = resp.calls;
            keep_project_calls(&mut calls, scope, |call| &call.to, "callees", &id);
            LspResponse::OutgoingCalls {
                request_id: id,
                calls,
            }
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
    let scope = &state.project_scope;
    super::parse_client_response(
        message,
        request_id,
        "incoming calls",
        &mut state.client,
        response_tx,
        |client, msg| client.parse_incoming_calls_response(msg),
        |id, resp| {
            let mut calls = resp.calls;
            keep_project_calls(&mut calls, scope, |call| &call.from, "callers", &id);
            LspResponse::IncomingCalls {
                request_id: id,
                calls,
            }
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
