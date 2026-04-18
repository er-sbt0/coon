use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;

use ::lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallGraph, SymbolId};

use crate::status_message::StatusMessage;

/// Timeout duration for pending LSP requests before they are considered failed.
const LSP_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Loading state for LSP operations
#[derive(Debug, Clone, PartialEq)]
pub enum LoadingState {
    NotLoaded,
    Loading,
    Loaded,
    Failed(String),
}

/// Pending LSP request information
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub symbol_id: Option<SymbolId>,
    pub timestamp: Instant,
}

/// Manages all LSP communication channels and request tracking.
///
/// Extracted from `App` to isolate LSP concerns and make them independently testable.
pub struct LspBridge {
    /// Channel to send requests to the LSP service worker
    pub request_tx: Option<mpsc::UnboundedSender<LspRequest>>,
    /// Channel to receive responses from the LSP service worker
    pub response_rx: Option<mpsc::UnboundedReceiver<LspResponse>>,
    /// In-flight requests awaiting responses, keyed by request ID
    pub pending_requests: HashMap<String, PendingRequest>,
    /// Per-symbol loading state (loaded, loading, failed, etc.)
    pub loading_states: HashMap<SymbolId, LoadingState>,

    // Async LSP loader wiring
    /// Current phase of the background LSP loading process
    pub status: LspLoadPhase,
    /// Channel for progress/add-function messages from the background loader
    pub loader_rx: Option<mpsc::UnboundedReceiver<LspUiMessage>>,
    /// One-shot channel to receive the LSP request/response channels once the loader is ready
    pub channels_rx: Option<
        mpsc::UnboundedReceiver<(
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
    /// Create an LspBridge with no active channels (demo / offline mode)
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

    /// Create an LspBridge wired to a background LSP loader
    pub fn new_with_loader(
        loader_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        channels_rx: mpsc::UnboundedReceiver<(
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

    /// Wire up the request/response channels for direct LSP communication
    pub fn set_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.response_rx = Some(response_rx);
        self.request_tx = Some(request_tx);
    }

    /// Poll for LSP channels delivered by the background loader and wire them up
    pub fn poll_channels(&mut self) {
        if let Some(rx) = &mut self.channels_rx {
            if let Ok((response_rx, request_tx)) = rx.try_recv() {
                log::info!("Received LSP channels from loader - wiring up for lazy loading");
                self.set_channels(response_rx, request_tx);
                self.channels_rx = None;
            }
        }
    }

    /// Poll progress/add-function messages from the background loader.
    ///
    /// Returns a vec of functions to add to the call graph (caller is responsible for that).
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
    /// Also checks for timed-out requests. Returns responses for the caller to dispatch.
    pub fn drain_responses(&mut self) -> (Vec<LspResponse>, Vec<StatusMessage>) {
        // Check for timed-out requests first
        let status_messages = self.check_timed_out_requests();

        let mut responses = Vec::new();
        if let Some(response_rx) = &mut self.response_rx {
            while let Ok(response) = response_rx.try_recv() {
                log::info!("TUI received LSP response: {:?}", response);
                responses.push(response);
            }
            if !responses.is_empty() {
                log::info!("TUI processing {} LSP responses", responses.len());
            }
        }
        (responses, status_messages)
    }

    /// Check for timed-out requests and return status messages for each.
    fn check_timed_out_requests(&mut self) -> Vec<StatusMessage> {
        let timeout_duration = std::time::Duration::from_secs(LSP_REQUEST_TIMEOUT_SECS);
        let now = Instant::now();
        let mut messages = Vec::new();
        let mut timed_out = Vec::new();

        for (request_id, pending) in &self.pending_requests {
            if now.duration_since(pending.timestamp) > timeout_duration {
                timed_out.push((request_id.clone(), pending.clone()));
            }
        }

        for (request_id, pending) in timed_out {
            self.pending_requests.remove(&request_id);
            if let Some(symbol_id) = pending.symbol_id {
                self.loading_states.insert(
                    symbol_id,
                    LoadingState::Failed("Request timed out".to_string()),
                );
            }
            messages.push(StatusMessage::LspRequestTimedOut);
        }
        messages
    }

    /// Send a call hierarchy request for the given function.
    ///
    /// Reads the function's location from `call_graph`. Returns an optional status message.
    pub fn send_call_hierarchy(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
    ) -> Option<StatusMessage> {
        let (file_path, line, column, _name) = {
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

        let request_tx = self.request_tx.as_ref()?.clone();

        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.loading_states.insert(
                    function_id.clone(),
                    LoadingState::Failed("Invalid file path".to_string()),
                );
                return Some(StatusMessage::InvalidFilePath);
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                symbol_id: Some(function_id.clone()),
                timestamp: Instant::now(),
            },
        );

        let request = LspRequest::PrepareCallHierarchy {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            self.loading_states.insert(
                function_id.clone(),
                LoadingState::Failed("Failed to send request".to_string()),
            );
            return Some(StatusMessage::FailedToSendLspRequest);
        }
        None
    }

    /// Send a find-references request for the given function.
    ///
    /// Returns an optional status message.
    pub fn send_references(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
    ) -> Option<StatusMessage> {
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
                return Some(StatusMessage::FailedToCreateUri);
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                symbol_id: Some(function_id.clone()),
                timestamp: Instant::now(),
            },
        );

        let request = LspRequest::FindReferencesWithSymbols {
            request_id,
            document_uri,
            position,
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            return Some(StatusMessage::FailedToSendRequest);
        }
        Some(StatusMessage::FindingReferences { name })
    }

    /// Send a workspace symbols request. Returns an optional status message.
    pub fn send_workspace_symbols(&mut self) -> Option<StatusMessage> {
        let request_tx = match self.request_tx.as_ref() {
            Some(tx) => tx.clone(),
            None => return None,
        };

        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                symbol_id: None,
                timestamp: Instant::now(),
            },
        );

        let request = LspRequest::GetWorkspaceSymbols {
            request_id: request_id.clone(),
            query: "".to_string(),
        };

        log::info!("TUI sending workspace symbols request: {}", request_id);
        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send LSP request: {}", e);
            Some(StatusMessage::FailedToSendRequest)
        } else {
            log::info!("Successfully sent workspace symbols request to channel");
            Some(StatusMessage::RefreshingWorkspaceSymbols)
        }
    }

    /// Send a follow-up call hierarchy request (incoming or outgoing).
    ///
    /// Used internally when processing a CallHierarchy response.
    pub fn send_follow_up_call(
        &mut self,
        symbol_id: &SymbolId,
        item: lsp_types::CallHierarchyItem,
        outgoing: bool,
    ) -> Option<StatusMessage> {
        let request_id = uuid::Uuid::new_v4().to_string();

        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                symbol_id: Some(symbol_id.clone()),
                timestamp: Instant::now(),
            },
        );

        let request_tx = match self.request_tx.as_ref() {
            Some(tx) => tx,
            None => {
                log::error!("No LSP request channel available");
                self.loading_states.insert(
                    symbol_id.clone(),
                    LoadingState::Failed("No LSP channel".to_string()),
                );
                return Some(StatusMessage::NoLspChannel);
            }
        };

        let request = if outgoing {
            log::info!("Requesting outgoing calls for: {}", item.name);
            ::lsp::LspRequest::GetOutgoingCalls {
                request_id,
                call_hierarchy_item: item,
            }
        } else {
            log::info!("Requesting incoming calls for: {}", item.name);
            ::lsp::LspRequest::GetIncomingCalls {
                request_id,
                call_hierarchy_item: item,
            }
        };

        if let Err(e) = request_tx.send(request) {
            log::error!("Failed to send calls request: {}", e);
            self.loading_states.insert(
                symbol_id.clone(),
                LoadingState::Failed(format!(
                    "Failed to request {} calls",
                    if outgoing { "outgoing" } else { "incoming" }
                )),
            );
            None
        } else {
            Some(StatusMessage::LoadingCalls {
                direction: if outgoing { "outgoing" } else { "incoming" }.to_string(),
            })
        }
    }

    /// Remove and return a pending request by ID
    pub fn take_pending(&mut self, request_id: &str) -> Option<PendingRequest> {
        self.pending_requests.remove(request_id)
    }

    // Query helpers

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

    /// Clear all pending requests and loading states (used on refresh)
    pub fn clear(&mut self) {
        self.loading_states.clear();
        self.pending_requests.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_bridge_has_no_channels() {
        let bridge = LspBridge::new();
        assert!(bridge.request_tx.is_none());
        assert!(bridge.response_rx.is_none());
        assert!(bridge.pending_requests.is_empty());
        assert!(bridge.loading_states.is_empty());
        assert_eq!(bridge.status, LspLoadPhase::NotStarted);
    }

    #[test]
    fn test_set_channels() {
        let mut bridge = LspBridge::new();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        bridge.set_channels(resp_rx, req_tx);
        assert!(bridge.request_tx.is_some());
        assert!(bridge.response_rx.is_some());
        drop(resp_tx); // keep compiler happy
    }

    #[test]
    fn test_clear() {
        let mut bridge = LspBridge::new();
        let sym = SymbolId::from_content("test::sym", "test.cpp", 1);
        bridge
            .loading_states
            .insert(sym.clone(), LoadingState::Loaded);
        bridge.pending_requests.insert(
            "req1".to_string(),
            PendingRequest {
                symbol_id: Some(sym),
                timestamp: Instant::now(),
            },
        );
        bridge.clear();
        assert!(bridge.loading_states.is_empty());
        assert!(bridge.pending_requests.is_empty());
    }

    #[test]
    fn test_loading_state_queries() {
        let mut bridge = LspBridge::new();
        let sym = SymbolId::from_content("test::sym", "test.cpp", 1);

        assert!(!bridge.is_function_loaded(&sym));
        assert!(!bridge.is_function_loading(&sym));

        bridge.set_loading_state(sym.clone(), LoadingState::Loading);
        assert!(bridge.is_function_loading(&sym));
        assert!(!bridge.is_function_loaded(&sym));

        bridge.set_loading_state(sym.clone(), LoadingState::Loaded);
        assert!(bridge.is_function_loaded(&sym));
    }
}
