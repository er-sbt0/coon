use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallGraph, SymbolId};

use crate::events::AppEvent;

/// Timeout duration for pending LSP requests before they are considered failed.
const LSP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Loading state for LSP operations.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadingState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

/// Who asked for a request, and so whether its answer is worth a redraw.
///
/// The bootstrap edge warmup sends one call-hierarchy request per symbol in the
/// project, and every reply used to be treated exactly like a user expanding a
/// node: `LayoutDirty`, which every out-of-process frontend turns into a full
/// snapshot. On jemalloc that was 767 whole-graph snapshots in the 110 seconds
/// of loading, of which one changed anything on screen — the warmup fills in
/// edges for nodes the canvas is not drawing, because the map it opens on has
/// no root and shows no call edges at all.
///
/// So a request carries who wanted it, and only the user's answers redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOrigin {
    /// The user asked: a root selected, a node expanded, a refresh.
    User,
    /// The background warmup asked, with nobody waiting on this one reply.
    Warmup,
}

/// Pending LSP request information.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub symbol_id: Option<SymbolId>,
    pub timestamp: Instant,
    /// Whether anyone is waiting to see this answer. See [`RequestOrigin`].
    pub origin: RequestOrigin,
    /// What was asked for, in words, so a log line about a stuck request says
    /// which symbol it is stuck on rather than only a uuid. Carried on the
    /// request because by the time the wait is interesting the caller that knew
    /// the name is long gone.
    pub label: String,
}

impl PendingRequest {
    pub fn new(
        symbol_id: Option<SymbolId>,
        label: impl Into<String>,
        origin: RequestOrigin,
    ) -> Self {
        Self {
            symbol_id,
            timestamp: Instant::now(),
            origin,
            label: label.into(),
        }
    }

    /// How long this request has been outstanding.
    pub fn age(&self) -> std::time::Duration {
        self.timestamp.elapsed()
    }
}

/// Manages all LSP communication channels and request tracking.
///
/// Lives in `core` so it can be shared by any frontend.
pub struct LspBridge {
    /// Channel to send requests to the LSP service worker
    pub request_tx: Option<mpsc::UnboundedSender<LspRequest>>,
    /// Channel to receive responses from the LSP service worker
    pub response_rx: Option<mpsc::UnboundedReceiver<LspResponse>>,
    /// In-flight requests awaiting responses, keyed by request ID
    pub pending_requests: HashMap<String, PendingRequest>,
    /// Per-symbol loading state
    pub loading_states: HashMap<SymbolId, LoadingState>,
    /// Current phase of the background LSP loading process
    pub status: LspLoadPhase,
    /// Channel for progress/add-function messages from the background loader
    pub loader_rx: Option<mpsc::UnboundedReceiver<LspUiMessage>>,
    /// One-shot channel to receive the LSP request/response channels once the loader is ready
    pub channels_rx: Option<
        oneshot::Receiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    >,
}

impl Default for LspBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl LspBridge {
    /// Create an `LspBridge` with no active channels (demo / offline mode).
    pub fn new() -> Self {
        Self {
            request_tx: None,
            response_rx: None,
            pending_requests: HashMap::new(),
            loading_states: HashMap::new(),
            status: LspLoadPhase::NotStarted,
            loader_rx: None,
            channels_rx: None,
        }
    }

    /// Create an `LspBridge` wired to a background LSP loader.
    pub fn new_with_loader(
        loader_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        channels_rx: oneshot::Receiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    ) -> Self {
        Self {
            request_tx: None,
            response_rx: None,
            pending_requests: HashMap::new(),
            loading_states: HashMap::new(),
            status: LspLoadPhase::NotStarted,
            loader_rx: Some(loader_rx),
            channels_rx: Some(channels_rx),
        }
    }

    /// Wire up the request/response channels for direct LSP communication.
    pub fn set_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.response_rx = Some(response_rx);
        self.request_tx = Some(request_tx);
    }

    /// Poll for LSP channels delivered by the background loader and wire them up.
    pub fn poll_channels(&mut self) {
        if let Some(mut rx) = self.channels_rx.take() {
            match rx.try_recv() {
                Ok((response_rx, request_tx)) => {
                    log::info!("Received LSP channels from loader - wiring up for lazy loading");
                    self.set_channels(response_rx, request_tx);
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.channels_rx = Some(rx);
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    log::warn!("LSP channels sender dropped before sending");
                }
            }
        }
    }

    /// Poll progress/add-function messages from the background loader.
    ///
    /// Returns a vec of [`model::WorkspaceSymbolInfo`] to add to the call graph;
    /// the caller is responsible for ingesting them.
    pub fn poll_loader_messages(&mut self) -> Vec<model::WorkspaceSymbolInfo> {
        let mut new_symbols = Vec::new();
        if let Some(rx) = &mut self.loader_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    LspUiMessage::Progress(phase) => {
                        self.status = phase;
                    }
                    LspUiMessage::AddFunction(symbol) => {
                        new_symbols.push(symbol);
                    }
                }
            }
        }
        new_symbols
    }

    /// Drain all available responses from the LSP response channel.
    ///
    /// Also checks for timed-out requests. Returns `(responses, events)`.
    pub fn drain_responses(&mut self) -> (Vec<LspResponse>, Vec<AppEvent>) {
        let events = self.check_timed_out_requests();

        let mut responses = Vec::new();
        if let Some(response_rx) = &mut self.response_rx {
            while let Ok(response) = response_rx.try_recv() {
                log::info!("core::LspBridge received LSP response: {:?}", response);
                responses.push(response);
            }
        }
        (responses, events)
    }

    /// Check for timed-out requests and emit `AppEvent::LspRequestTimedOut` for each.
    fn check_timed_out_requests(&mut self) -> Vec<AppEvent> {
        let timeout_duration = std::time::Duration::from_secs(LSP_REQUEST_TIMEOUT_SECS);
        let now = Instant::now();
        let mut events = Vec::new();
        let mut timed_out = Vec::new();

        for (request_id, pending) in &self.pending_requests {
            if now.duration_since(pending.timestamp) > timeout_duration {
                timed_out.push((request_id.clone(), pending.clone()));
            }
        }

        for (request_id, pending) in timed_out {
            self.pending_requests.remove(&request_id);
            // The one place a request disappears without clangd ever answering.
            // Silent until now, which made a warmup that crawled at the timeout
            // interval look identical to one that was simply slow.
            log::warn!(
                "core::LspBridge request timed out after {:?}: {} (request_id={}, {} still in flight)",
                pending.age(),
                pending.label,
                request_id,
                self.pending_requests.len()
            );
            if let Some(symbol_id) = pending.symbol_id {
                self.loading_states.insert(
                    symbol_id,
                    LoadingState::Failed("Request timed out".to_string()),
                );
            }
            events.push(AppEvent::LspRequestTimedOut);
        }
        events
    }

    /// Send a call hierarchy request for the given function.
    pub fn send_call_hierarchy(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
        origin: RequestOrigin,
    ) -> Option<AppEvent> {
        let (file_path, line, column, _name) = {
            if let Some(function) = call_graph.get_function(function_id) {
                (
                    function.definition_location.file_path.clone(),
                    function.definition_location.line,
                    function.definition_location.column,
                    function.name.clone(),
                )
            } else {
                // The caller has already taken this symbol off whatever queue it
                // came from, so a silent return leaves it unresolved forever.
                log::warn!(
                    "core::LspBridge no call hierarchy for {:?}: not in the call graph",
                    function_id
                );
                return None;
            }
        };

        let Some(request_tx) = self.request_tx.clone() else {
            log::warn!(
                "core::LspBridge no call hierarchy for {}: LSP channel not wired up yet",
                _name
            );
            return None;
        };

        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.loading_states.insert(
                    *function_id,
                    LoadingState::Failed("Invalid file path".to_string()),
                );
                return Some(AppEvent::InvalidFilePath);
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest::new(
                Some(*function_id),
                format!("prepare {} ({}:{})", _name, file_path, line),
                origin,
            ),
        );

        log::info!(
            "core::LspBridge prepare call hierarchy: request_id={} symbol={} at {}:{}:{} ({} in flight)",
            request_id,
            _name,
            file_path,
            line,
            column,
            self.pending_requests.len()
        );

        let request = LspRequest::PrepareCallHierarchy {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            self.loading_states.insert(
                *function_id,
                LoadingState::Failed("Failed to send request".to_string()),
            );
            return Some(AppEvent::FailedToSendRequest);
        }
        None
    }

    /// Send a find-references request for the given function.
    pub fn send_references(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
    ) -> Option<AppEvent> {
        let (file_path, line, column, name) = {
            if let Some(function) = call_graph.get_function(function_id) {
                (
                    function.definition_location.file_path.clone(),
                    function.definition_location.line,
                    function.definition_location.column,
                    function.name.clone(),
                )
            } else {
                return None;
            }
        };

        let request_tx = match self.request_tx.as_ref() {
            Some(tx) => tx.clone(),
            None => return None,
        };

        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                return Some(AppEvent::FailedToCreateUri);
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest::new(
                Some(*function_id),
                format!("references {}", name),
                RequestOrigin::User,
            ),
        );

        let request = LspRequest::FindReferencesWithSymbols {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            return Some(AppEvent::FailedToSendRequest);
        }
        Some(AppEvent::FindingReferences { name })
    }

    /// Send a workspace symbols request.
    pub fn send_workspace_symbols(&mut self) -> Option<AppEvent> {
        let request_tx = match self.request_tx.as_ref() {
            Some(tx) => tx.clone(),
            None => return None,
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest::new(None, "workspace symbols", RequestOrigin::User),
        );

        let request = LspRequest::GetWorkspaceSymbols {
            request_id: request_id.clone(),
            query: "".to_string(),
        };

        log::info!(
            "core::LspBridge sending workspace symbols request: {}",
            request_id
        );
        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            Some(AppEvent::FailedToSendRequest)
        } else {
            log::info!("Successfully sent workspace symbols request");
            Some(AppEvent::RefreshingWorkspaceSymbols)
        }
    }

    /// Send a follow-up call hierarchy request (incoming or outgoing).
    pub fn send_follow_up_call(
        &mut self,
        symbol_id: &SymbolId,
        item: lsp_types::CallHierarchyItem,
        outgoing: bool,
        origin: RequestOrigin,
    ) -> Option<AppEvent> {
        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest::new(
                Some(*symbol_id),
                format!(
                    "{} calls {}",
                    if outgoing { "outgoing" } else { "incoming" },
                    item.name
                ),
                // The prepare and its follow-up are two halves of one question,
                // so the second half is asked by whoever asked the first.
                origin,
            ),
        );

        let request_tx = match self.request_tx.as_ref() {
            Some(tx) => tx,
            None => {
                log::error!("No LSP request channel available");
                self.loading_states.insert(
                    *symbol_id,
                    LoadingState::Failed("No LSP channel".to_string()),
                );
                return Some(AppEvent::NoLspChannel);
            }
        };

        let request = if outgoing {
            log::info!("Requesting outgoing calls for: {}", item.name);
            lsp::LspRequest::GetOutgoingCalls {
                request_id,
                call_hierarchy_item: item,
            }
        } else {
            log::info!("Requesting incoming calls for: {}", item.name);
            lsp::LspRequest::GetIncomingCalls {
                request_id,
                call_hierarchy_item: item,
            }
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send calls request: {}", e);
            self.loading_states.insert(
                *symbol_id,
                LoadingState::Failed(format!(
                    "Failed to request {} calls",
                    if outgoing { "outgoing" } else { "incoming" }
                )),
            );
            None
        } else {
            Some(AppEvent::LoadingCalls {
                direction: if outgoing { "outgoing" } else { "incoming" }.to_string(),
            })
        }
    }

    /// Remove and return a pending request by ID.
    pub fn take_pending(&mut self, request_id: &str) -> Option<PendingRequest> {
        let pending = self.pending_requests.remove(request_id);
        match &pending {
            Some(p) => log::debug!(
                "core::LspBridge answered after {:?}: {} ({} still in flight)",
                p.age(),
                p.label,
                self.pending_requests.len()
            ),
            // A reply nobody is waiting for: either it already timed out, or it
            // belongs to a request the loader issued under its own id. Both are
            // worth seeing when the warmup stops moving.
            None => log::warn!(
                "core::LspBridge response for unknown request_id={} ({} in flight)",
                request_id,
                self.pending_requests.len()
            ),
        }
        pending
    }

    /// Everything currently outstanding, oldest first, as one line per request.
    ///
    /// The warmup only sends while there is room under its in-flight ceiling,
    /// so when it stalls this list *is* the reason: whatever is in here is what
    /// the queue is waiting behind.
    pub fn pending_summary(&self) -> String {
        let mut pending: Vec<_> = self.pending_requests.iter().collect();
        pending.sort_by_key(|(_, p)| p.timestamp);
        pending
            .iter()
            .map(|(id, p)| format!("{} [{}ms, {}]", p.label, p.age().as_millis(), id))
            .collect::<Vec<_>>()
            .join(", ")
    }

    // ---- Query helpers ----

    pub fn is_function_loaded(&self, symbol_id: &SymbolId) -> bool {
        matches!(
            self.loading_states.get(symbol_id),
            Some(LoadingState::Loaded)
        )
    }

    pub fn is_function_loading(&self, symbol_id: &SymbolId) -> bool {
        matches!(
            self.loading_states.get(symbol_id),
            Some(LoadingState::Loading)
        )
    }

    pub fn get_loading_state(&self, symbol_id: &SymbolId) -> &LoadingState {
        self.loading_states
            .get(symbol_id)
            .unwrap_or(&LoadingState::NotLoaded)
    }

    pub fn set_loading_state(&mut self, symbol_id: SymbolId, state: LoadingState) {
        self.loading_states.insert(symbol_id, state);
    }

    /// Clear all pending requests and loading states (used on refresh).
    pub fn clear(&mut self) {
        self.loading_states.clear();
        self.pending_requests.clear();
    }
}
