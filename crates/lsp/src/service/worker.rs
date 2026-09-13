use super::document::OpenDocuments;
use super::{LspRequest, LspResponse};
use crate::LspClient;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::sync::mpsc;

/// How often the worker reports what clangd still owes it an answer for.
///
/// The one place that knows the difference between "we never sent it" and "we
/// sent it and clangd has not replied": everything upstream sees only a request
/// id that has gone quiet.
const OUTSTANDING_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub(super) enum RequestType {
    PrepareCallHierarchy,
    OutgoingCalls,
    IncomingCalls,
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
    /// When each outstanding clangd request was sent, so the heartbeat below can
    /// say how long it has been waiting.
    pub(super) request_sent_at: HashMap<i64, Instant>,
    pub(super) opened_documents: OpenDocuments,
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
        self.request_sent_at.insert(lsp_id, Instant::now());
        // At info, because this line is the only thing tying clangd's own log
        // ("--> reply:callHierarchy/outgoingCalls(37)") to the service request
        // id the rest of the app talks in.
        log::info!(
            "LSP worker: sent request {} to clangd: type={:?}, service_id={} ({} outstanding)",
            lsp_id,
            request_type,
            service_id,
            self.service_requests.len()
        );
    }

    /// Forget the bookkeeping for a request clangd has answered, returning how
    /// long the answer took.
    pub(super) fn finish_request(&mut self, lsp_id: i64) -> Option<std::time::Duration> {
        self.request_sent_at
            .remove(&lsp_id)
            .map(|sent| sent.elapsed())
    }

    /// Everything clangd has been asked and has not answered, oldest first.
    pub(super) fn outstanding_summary(&self) -> String {
        let mut outstanding: Vec<_> = self
            .service_requests
            .iter()
            .map(|(lsp_id, service_id)| {
                let waited = self
                    .request_sent_at
                    .get(lsp_id)
                    .map(|sent| sent.elapsed().as_millis())
                    .unwrap_or_default();
                (waited, *lsp_id, service_id.clone())
            })
            .collect();
        outstanding.sort_by_key(|(waited, _, _)| std::cmp::Reverse(*waited));
        outstanding
            .iter()
            .map(|(waited, lsp_id, service_id)| format!("{lsp_id} [{waited}ms, {service_id}]"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A request in one line, for the log: which kind, for what, under which id.
fn describe_request(request: &LspRequest) -> String {
    match request {
        LspRequest::PrepareCallHierarchy {
            request_id,
            document_uri,
            position,
        } => format!(
            "PrepareCallHierarchy {}:{}:{} ({})",
            document_uri.path(),
            position.line,
            position.character,
            request_id
        ),
        LspRequest::GetOutgoingCalls {
            request_id,
            call_hierarchy_item,
        } => format!(
            "GetOutgoingCalls {} ({})",
            call_hierarchy_item.name, request_id
        ),
        LspRequest::GetIncomingCalls {
            request_id,
            call_hierarchy_item,
        } => format!(
            "GetIncomingCalls {} ({})",
            call_hierarchy_item.name, request_id
        ),
        LspRequest::FindReferences { request_id, .. } => format!("FindReferences ({request_id})"),
        LspRequest::FindReferencesWithSymbols { request_id, .. } => {
            format!("FindReferencesWithSymbols ({request_id})")
        }
        LspRequest::GetDocumentSymbols { request_id, .. } => {
            format!("GetDocumentSymbols ({request_id})")
        }
        LspRequest::GetWorkspaceSymbols { request_id, query } => {
            format!("GetWorkspaceSymbols query={query:?} ({request_id})")
        }
        LspRequest::PreloadDocuments {
            request_id,
            document_uris,
        } => format!(
            "PreloadDocuments {} document(s) ({})",
            document_uris.len(),
            request_id
        ),
        LspRequest::CloseDocuments => "CloseDocuments".to_string(),
        LspRequest::SetProjectFiles { project_files } => {
            format!("SetProjectFiles {} file(s)", project_files.len())
        }
        LspRequest::Shutdown => "Shutdown".to_string(),
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
        request_sent_at: HashMap::new(),
        opened_documents: OpenDocuments::default(),
        project_files: HashSet::new(),
        pending_enhanced_requests: HashMap::new(),
        enhanced_lsp_requests: HashSet::new(),
    };

    // Ticks only while the loop is back at the `select!`, so a silent heartbeat
    // says as much as a loud one: the worker is stuck inside a request handler.
    let mut heartbeat = tokio::time::interval(OUTSTANDING_LOG_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            request = request_rx.recv() => {
                let Some(request) = request else {
                    log::info!("LSP worker: request channel closed, stopping");
                    break;
                };
                let label = describe_request(&request);
                let started = Instant::now();
                log::debug!("LSP worker: handling {label}");
                match request {
                    LspRequest::PrepareCallHierarchy { request_id, document_uri, position } => {
                        super::request::handle_prepare_call_hierarchy_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    LspRequest::GetOutgoingCalls { request_id, call_hierarchy_item } => {
                        super::request::handle_outgoing_calls_request(&mut state, &response_tx, request_id, call_hierarchy_item).await;
                    }
                    LspRequest::GetIncomingCalls { request_id, call_hierarchy_item } => {
                        super::request::handle_incoming_calls_request(&mut state, &response_tx, request_id, call_hierarchy_item).await;
                    }
                    LspRequest::FindReferences { request_id, document_uri, position } => {
                        super::request::handle_references_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    LspRequest::FindReferencesWithSymbols { request_id, document_uri, position } => {
                        super::request::handle_references_with_symbols_request(&mut state, &response_tx, request_id, document_uri, position).await;
                    }
                    LspRequest::GetDocumentSymbols { request_id, document_uri } => {
                        super::request::handle_document_symbols_request(&mut state, &response_tx, request_id, document_uri).await;
                    }
                    LspRequest::GetWorkspaceSymbols { request_id, query } => {
                        super::request::handle_workspace_symbols_request(&mut state, &response_tx, request_id, query).await;
                    }
                    LspRequest::PreloadDocuments { request_id, document_uris } => {
                        super::request::handle_preload_documents(&mut state, &response_tx, request_id, document_uris).await;
                    }
                    LspRequest::CloseDocuments => {
                        super::document::close_all_documents(&mut state).await;
                    }
                    LspRequest::SetProjectFiles { project_files } => {
                        state.project_files = project_files.into_iter().collect();
                        log::info!("Updated project files: {} files", state.project_files.len());
                    }
                    LspRequest::Shutdown => {
                        log::info!("LSP worker: shutdown requested, stopping");
                        break;
                    }
                }
                // Only the dispatch, not clangd's answer — but a handler that
                // takes long here is one that blocked the loop, and while it was
                // blocked no reply from clangd was being read either.
                let elapsed = started.elapsed();
                if elapsed >= std::time::Duration::from_millis(500) {
                    log::warn!("LSP worker: dispatching {label} blocked the loop for {elapsed:?}");
                } else {
                    log::debug!("LSP worker: dispatched {label} in {elapsed:?}");
                }
            }

            message = lsp_message_rx.recv() => {
                match message {
                    Some(message) => {
                        super::response::handle_lsp_message(message, &mut state, &response_tx).await;
                    }
                    None => {
                        log::warn!("LSP worker: clangd message channel closed — no further replies will arrive");
                        break;
                    }
                }
            }

            _ = heartbeat.tick() => {
                if !state.service_requests.is_empty() {
                    let oldest = state
                        .request_sent_at
                        .values()
                        .map(|sent| sent.elapsed())
                        .max()
                        .unwrap_or_default();
                    let summary = format!(
                        "LSP worker: {} request(s) still unanswered by clangd (oldest {:.1}s): {}",
                        state.service_requests.len(),
                        oldest.as_secs_f32(),
                        state.outstanding_summary()
                    );
                    if oldest >= std::time::Duration::from_secs(15) {
                        log::warn!("{summary}");
                    } else {
                        log::info!("{summary}");
                    }
                }
            }
        }
    }

    // Gracefully shut down the LSP server (sends shutdown + exit, kills process)
    if let Err(e) = state.client.shutdown().await {
        log::error!("Error during LSP client shutdown: {}", e);
    }
}
