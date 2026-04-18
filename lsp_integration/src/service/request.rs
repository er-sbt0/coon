use lsp_types::{Position, Url};
use tokio::sync::mpsc;
use super::worker::{LspWorkerState, RequestType};
use super::LspResponse;
use super::document;

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

pub(super) async fn handle_call_hierarchy_request(
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
    request_id: String,
    document_uri: Url,
    position: Position,
) {
    log::info!(
        "Handling call hierarchy request: uri={}, position={}:{}, request_id={}",
        document_uri,
        position.line,
        position.character,
        request_id
    );

    if let Err(e) = document::ensure_document_opened(state, &document_uri).await {
        log::error!("Failed to open document for call hierarchy: {}", e);
        let _ = response_tx
            .send(LspResponse::Error {
                request_id,
                error: format!("Failed to open document: {}", e),
            })
            .await;
        return;
    }

    match state
        .client
        .prepare_call_hierarchy(document_uri, position)
        .await
    {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent call hierarchy request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::CallHierarchy);
        }
        Err(e) => {
            log::error!("Failed to send call hierarchy request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request call hierarchy: {}", e),
                })
                .await;
        }
    }
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

    match state.client.get_outgoing_calls(call_hierarchy_item).await {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent outgoing calls request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::OutgoingCalls);
        }
        Err(e) => {
            log::error!("Failed to send outgoing calls request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request outgoing calls: {}", e),
                })
                .await;
        }
    }
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

    match state.client.get_incoming_calls(call_hierarchy_item).await {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent incoming calls request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::IncomingCalls);
        }
        Err(e) => {
            log::error!("Failed to send incoming calls request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request incoming calls: {}", e),
                })
                .await;
        }
    }
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

    match state
        .client
        .prepare_call_hierarchy(document_uri, position)
        .await
    {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent prepare call hierarchy request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(
                lsp_request_id,
                request_id,
                RequestType::PrepareCallHierarchy,
            );
        }
        Err(e) => {
            log::error!("Failed to send prepare call hierarchy request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to prepare call hierarchy: {}", e),
                })
                .await;
        }
    }
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
    match state.client.find_references(text_document, position).await {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent references request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::References);
        }
        Err(e) => {
            log::error!("Failed to send references request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request references: {}", e),
                })
                .await;
        }
    }
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
    match state
        .client
        .find_references_with_symbols(text_document, position)
        .await
    {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent enhanced references request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(
                lsp_request_id,
                request_id,
                RequestType::ReferencesWithSymbols,
            );
            state.enhanced_lsp_requests.insert(lsp_request_id);
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
    match state.client.document_symbol(text_document).await {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent document symbols request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::DocumentSymbols);
        }
        Err(e) => {
            log::error!("Failed to send document symbols request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request document symbols: {}", e),
                })
                .await;
        }
    }
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

    match state.client.workspace_symbol(&query).await {
        Ok(lsp_request_id) => {
            log::info!(
                "Sent workspace symbols request to LSP server: lsp_request_id={}",
                lsp_request_id
            );
            state.track_request(lsp_request_id, request_id, RequestType::WorkspaceSymbols);
        }
        Err(e) => {
            log::error!("Failed to send workspace symbols request: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to request workspace symbols: {}", e),
                })
                .await;
        }
    }
}
