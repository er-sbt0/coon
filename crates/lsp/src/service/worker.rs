use super::{LspRequest, LspResponse};
use crate::LspClient;
use lsp_types::Url;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum RequestType {
    CallHierarchy,
    OutgoingCalls,
    IncomingCalls,
    PrepareCallHierarchy,
    References,
    ReferencesWithSymbols,
    DocumentSymbols,
    /// Document-symbol sub-request spawned by the enhanced-references flow.
    /// `base_request_id` is the original `FindReferencesWithSymbols` service ID.
    DocumentSymbolsForEnhancedRefs {
        base_request_id: String,
    },
    WorkspaceSymbols,
}

pub(super) struct LspWorkerState {
    pub(super) client: LspClient,
    pub(super) service_requests: HashMap<i64, String>,
    pub(super) request_types: HashMap<i64, RequestType>,
    pub(super) opened_documents: HashSet<Url>,
    pub(super) project_files: HashSet<String>,
    pub(super) pending_enhanced_requests: HashMap<String, EnhancedRequestInfo>,
    pub(super) enhanced_lsp_requests: HashSet<i64>,
}

impl LspWorkerState {
    pub(super) fn track_request(
        &mut self,
        lsp_id: i64,
        service_id: String,
        request_type: RequestType,
    ) {
        self.service_requests.insert(lsp_id, service_id.clone());
        self.request_types.insert(lsp_id, request_type.clone());
        log::debug!(
            "Tracking LSP request {}: type={:?}, service_id={}",
            lsp_id,
            request_type,
            service_id
        );
    }
}

#[derive(Debug, Clone)]
pub(super) struct EnhancedRequestInfo {
    pub(super) locations: Vec<model::Location>,
}

pub(super) async fn run_loop(
    client: LspClient,
    mut request_rx: mpsc::Receiver<LspRequest>,
    response_tx: mpsc::Sender<LspResponse>,
    mut lsp_message_rx: mpsc::Receiver<Value>,
) {
    let mut state = LspWorkerState {
        client,
        service_requests: HashMap::new(),
        request_types: HashMap::new(),
        opened_documents: HashSet::new(),
        project_files: HashSet::new(),
        pending_enhanced_requests: HashMap::new(),
        enhanced_lsp_requests: HashSet::new(),
    };

    loop {
        tokio::select! {
            request = request_rx.recv() => {
                match request {
                    Some(LspRequest::GetCallHierarchy { request_id, document_uri, position }) => {
                        super::request::handle_call_hierarchy_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    Some(LspRequest::GetOutgoingCalls { request_id, call_hierarchy_item }) => {
                        super::request::handle_outgoing_calls_request(&mut state, &response_tx, request_id, call_hierarchy_item).await;
                    }
                    Some(LspRequest::GetIncomingCalls { request_id, call_hierarchy_item }) => {
                        super::request::handle_incoming_calls_request(&mut state, &response_tx, request_id, call_hierarchy_item).await;
                    }
                    Some(LspRequest::PrepareCallHierarchy { request_id, document_uri, position }) => {
                        super::request::handle_prepare_call_hierarchy_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    Some(LspRequest::FindReferences { request_id, document_uri, position }) => {
                        super::request::handle_references_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    Some(LspRequest::FindReferencesWithSymbols { request_id, document_uri, position }) => {
                        super::request::handle_references_with_symbols_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    Some(LspRequest::GetDocumentSymbols { request_id, document_uri }) => {
                        super::request::handle_document_symbols_request(&mut state, &response_tx, request_id, document_uri).await;
                    }
                    Some(LspRequest::GetWorkspaceSymbols { request_id, query }) => {
                        super::request::handle_workspace_symbols_request(&mut state, &response_tx, request_id, query).await;
                    }
                    Some(LspRequest::PreloadDocuments { request_id, document_uris }) => {
                        super::request::handle_preload_documents(&mut state, &response_tx, request_id, document_uris).await;
                    }
                    Some(LspRequest::SetProjectFiles { project_files }) => {
                        state.project_files = project_files.into_iter().collect();
                        log::info!("Updated project files: {} files", state.project_files.len());
                    }
                    Some(LspRequest::Shutdown) | None => {
                        break;
                    }
                }
            }

            message = lsp_message_rx.recv() => {
                if let Some(message) = message {
                    super::response::handle_lsp_message(message, &mut state, &response_tx).await;
                }
            }
        }
    }

    // Gracefully shut down the LSP server (sends shutdown + exit, kills process)
    if let Err(e) = state.client.shutdown().await {
        log::error!("Error during LSP client shutdown: {}", e);
    }
}
