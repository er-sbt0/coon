use super::document;
use super::worker::{LspWorkerState, RequestType};
use super::LspResponse;
use lsp_types::{Position, Url};
use tokio::sync::mpsc;

/// Tracks a successful LSP request or sends an error response on failure.
async fn track_or_error(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    result: anyhow::Result<i64>,
    request_id: String,
    request_type: RequestType,
    context: &str,
) {
    match result {
        Ok(lsp_id) => state.track_request(lsp_id, request_id, request_type),
        Err(e) => {
            log::error!("Failed to send {} request: {}", context, e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to {}: {}", context, e),
                })
                .await;
        }
    }
}

pub(super) async fn handle_preload_documents(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uris: Vec<Url>,
) {
    let mut loaded_count = 0;
    let mut failed_count = 0;

    for uri in document_uris {
        match document::ensure_document_opened(state, &uri).await {
            Ok(()) => {
                loaded_count += 1;
                log::debug!("Preloaded document: {}", uri);
            }
            Err(e) => {
                failed_count += 1;
                log::warn!("Failed to preload document {}: {}", uri, e);
            }
        }
    }

    let _ = response_tx
        .send(LspResponse::PreloadComplete {
            request_id,
            loaded_count,
            failed_count,
        })
        .await;
}

pub(super) async fn handle_outgoing_calls_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    call_hierarchy_item: lsp_types::CallHierarchyItem,
) {
    log::info!(
        "Handling outgoing calls request for: name='{}', request_id={}",
        call_hierarchy_item.name,
        request_id
    );

    let result = state.client.get_outgoing_calls(call_hierarchy_item).await;
    track_or_error(state, response_tx, result, request_id, RequestType::OutgoingCalls, "outgoing calls").await;
}

pub(super) async fn handle_incoming_calls_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    call_hierarchy_item: lsp_types::CallHierarchyItem,
) {
    log::info!(
        "Handling incoming calls request for: name='{}', request_id={}",
        call_hierarchy_item.name,
        request_id
    );

    let result = state.client.get_incoming_calls(call_hierarchy_item).await;
    track_or_error(state, response_tx, result, request_id, RequestType::IncomingCalls, "incoming calls").await;
}

pub(super) async fn handle_prepare_call_hierarchy_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uri: Url,
    position: Position,
) {
    log::info!(
        "Handling prepare call hierarchy request: uri={}, position={}:{}, request_id={}",
        document_uri,
        position.line,
        position.character,
        request_id
    );

    if let Err(e) = document::ensure_document_opened(state, &document_uri).await {
        log::error!(
            "Failed to open document for call hierarchy preparation: {}",
            e
        );
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: format!("Failed to open document: {}", e),
            })
            .await;
        return;
    }

    let result = state.client.prepare_call_hierarchy(document_uri, position).await;
    track_or_error(state, response_tx, result, request_id, RequestType::PrepareCallHierarchy, "prepare call hierarchy").await;
}

pub(super) async fn handle_references_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uri: Url,
    position: Position,
) {
    log::info!(
        "Handling references request: uri={}, position={}:{}, request_id={}",
        document_uri,
        position.line,
        position.character,
        request_id
    );

    if let Err(e) = document::ensure_document_opened(state, &document_uri).await {
        log::error!("Failed to open document for references: {}", e);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: format!("Failed to open document: {}", e),
            })
            .await;
        return;
    }

    let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
    let result = state.client.find_references(text_document, position).await;
    track_or_error(state, response_tx, result, request_id, RequestType::References, "references").await;
}

pub(super) async fn handle_references_with_symbols_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uri: Url,
    position: Position,
) {
    log::info!(
        "Handling enhanced references request: uri={}, position={}:{}, request_id={}",
        document_uri,
        position.line,
        position.character,
        request_id
    );

    if let Err(e) = document::ensure_document_opened(state, &document_uri).await {
        log::error!("Failed to open document for enhanced references: {}", e);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: format!("Failed to open document: {}", e),
            })
            .await;
        return;
    }

    let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
    let result = state.client.find_references_with_symbols(text_document, position).await;
    match result {
        Ok(lsp_id) => {
            state.track_request(lsp_id, request_id, RequestType::ReferencesWithSymbols);
            state.enhanced_lsp_requests.insert(lsp_id);
        }
        Err(e) => {
            log::error!("Failed to send enhanced references request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request enhanced references: {}", e),
                })
                .await;
        }
    }
}

pub(super) async fn handle_document_symbols_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uri: Url,
) {
    log::info!(
        "Handling document symbols request: uri={}, request_id={}",
        document_uri,
        request_id
    );

    if let Err(e) = document::ensure_document_opened(state, &document_uri).await {
        log::error!("Failed to open document for document symbols: {}", e);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: format!("Failed to open document: {}", e),
            })
            .await;
        return;
    }

    let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
    let result = state.client.document_symbol(text_document).await;
    track_or_error(state, response_tx, result, request_id, RequestType::DocumentSymbols, "document symbols").await;
}

pub(super) async fn handle_workspace_symbols_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    query: String,
) {
    log::info!(
        "Handling workspace symbols request: query='{}', request_id={}",
        query,
        request_id
    );

    let result = state.client.workspace_symbol(&query).await;
    track_or_error(state, response_tx, result, request_id, RequestType::WorkspaceSymbols, "workspace symbols").await;
}
