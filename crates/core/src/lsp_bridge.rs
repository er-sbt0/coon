use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};

use lsp::{LspRequest, LspResponse};
use model::{
    lsp_status::LspLoadPhase, lsp_status::LspUiMessage, CallDirection, CallGraph, SymbolId,
};

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
    /// Which way round the question was asked, for the requests where that
    /// means anything — a call-hierarchy prepare, and the incoming/outgoing
    /// fetch it turns into. `None` for everything else: a workspace-symbol
    /// sweep or a find-references has no direction to have, and inventing one
    /// for them would only make the field read as a default rather than as an
    /// answer somebody actually gave.
    ///
    /// It rides on the request for the same reason [`RequestOrigin`] does. A
    /// prepare and its follow-up are two halves of one question, and clangd
    /// answers the first half whenever it gets round to it — by then the user
    /// may have hit `t`, or moved to a tab reading the other way. Resolving the
    /// direction at reply time therefore answered the tab that happened to be
    /// in front instead of the tab that asked: the outgoing view sent a
    /// prepare, got its callers fetched, and was marked `Loaded` all the same,
    /// so `+` on that node did nothing ever again.
    pub direction: Option<CallDirection>,
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
            direction: None,
        }
    }

    /// Stamp the call direction this request was asked in.
    ///
    /// Separate from [`PendingRequest::new`] rather than a fourth argument to
    /// it, because three of the four kinds of request tracked here have no
    /// direction and would have to pass a `None` that reads like an oversight
    /// next to the `None` symbol id already there.
    pub fn in_direction(mut self, direction: CallDirection) -> Self {
        self.direction = Some(direction);
        self
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

    /// Hand a request to the LSP worker, and record it as in flight only once
    /// the channel has taken it. Returns whether it went.
    ///
    /// This is the only place anything is put into `pending_requests`, and the
    /// order — send first, register second — is the whole reason it exists.
    /// Every sender used to insert its entry before the send and then return
    /// an event on the failure paths, leaving the entry behind with nothing in
    /// the world that would ever answer it. That is not untidy, it is a stall:
    /// `CoreApp::warming_up` reads a non-empty `pending_requests` as an answer
    /// still being on its way, which holds `CoreApp::loading` true, which stops
    /// `sync_clusterings` starting *any* clustering and pins the status bar on
    /// "Resolving calls". One failed send therefore cost the cluster map the
    /// whole `LSP_REQUEST_TIMEOUT_SECS` it takes `check_timed_out_requests` to
    /// retire the entry. Four senders each remembering to clean up on three
    /// paths is the kind of invariant that survives exactly until the fifth is
    /// written, so the send is the registration.
    ///
    /// Registering afterwards cannot lose a reply. The answer comes back over
    /// `response_rx`, which is read in exactly one place — `drain_responses`,
    /// itself called only from `CoreApp::tick` — so nothing looks a response up
    /// in `pending_requests` until a later tick, long after this has returned.
    /// The worker is a separate task and the channel is unbounded, so `send`
    /// only queues; it never runs a receiver inline and can never re-enter us.
    ///
    /// Takes a builder rather than a finished request so that the id the entry
    /// is filed under is by construction the one the request was built with.
    fn dispatch(
        &mut self,
        request_tx: &mpsc::UnboundedSender<LspRequest>,
        pending: PendingRequest,
        build: impl FnOnce(String) -> LspRequest,
    ) -> bool {
        let request_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = request_tx.send(build(request_id.clone())) {
            log::error!("Failed to send LSP request ({}): {}", pending.label, e);
            return false;
        }
        log::info!(
            "core::LspBridge sent {} (request_id={}, direction={:?}, origin={:?}, {} in flight)",
            pending.label,
            request_id,
            pending.direction,
            pending.origin,
            self.pending_requests.len() + 1
        );
        self.pending_requests.insert(request_id, pending);
        true
    }

    /// Send a call hierarchy request for the given function.
    ///
    /// `direction` is the way the *asker* was reading the graph at this moment,
    /// and it is recorded on the pending entry rather than looked up again when
    /// the reply lands. See [`PendingRequest::direction`].
    pub fn send_call_hierarchy(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
        origin: RequestOrigin,
        direction: CallDirection,
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

        let pending = PendingRequest::new(
            Some(*function_id),
            format!("prepare {} ({}:{}:{})", _name, file_path, line, column),
            origin,
        )
        .in_direction(direction);

        if !self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::PrepareCallHierarchy {
                request_id,
                document_uri,
                position,
            }
        }) {
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

        let pending = PendingRequest::new(
            Some(*function_id),
            format!("references {}", name),
            RequestOrigin::User,
        );

        if !self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::FindReferencesWithSymbols {
                request_id,
                document_uri,
                position,
            }
        }) {
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

        let pending = PendingRequest::new(None, "workspace symbols", RequestOrigin::User);

        if self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::GetWorkspaceSymbols {
                request_id,
                query: "".to_string(),
            }
        }) {
            Some(AppEvent::RefreshingWorkspaceSymbols)
        } else {
            Some(AppEvent::FailedToSendRequest)
        }
    }

    /// Send a follow-up call hierarchy request (incoming or outgoing).
    ///
    /// Takes the domain [`CallDirection`] rather than an `outgoing: bool`. The
    /// bool was spelled back out as the string `"outgoing"` or `"incoming"` in
    /// three places in this one function — the label, the failure message and
    /// the event the status bar reads — which is the shape a type is for.
    pub fn send_follow_up_call(
        &mut self,
        symbol_id: &SymbolId,
        item: lsp_types::CallHierarchyItem,
        direction: CallDirection,
        origin: RequestOrigin,
    ) -> Option<AppEvent> {
        let which = match direction {
            CallDirection::Outgoing => "outgoing",
            CallDirection::Incoming => "incoming",
        };

        // Cloned, and resolved before anything is recorded: a request only
        // becomes pending once `dispatch` has handed it over, and there is
        // nothing to hand it to on this path. See [`LspBridge::dispatch`].
        let Some(request_tx) = self.request_tx.clone() else {
            log::error!("No LSP request channel available");
            self.loading_states.insert(
                *symbol_id,
                LoadingState::Failed("No LSP channel".to_string()),
            );
            return Some(AppEvent::NoLspChannel);
        };

        let pending = PendingRequest::new(
            Some(*symbol_id),
            format!("{} calls {}", which, item.name),
            // The prepare and its follow-up are two halves of one question,
            // so the second half is asked by whoever asked the first.
            origin,
        )
        // And in the direction the first half was asked in, so a reply that
        // is itself slow cannot drift onto a different tab's reading either.
        .in_direction(direction);

        if self.dispatch(&request_tx, pending, |request_id| match direction {
            CallDirection::Outgoing => lsp::LspRequest::GetOutgoingCalls {
                request_id,
                call_hierarchy_item: item,
            },
            CallDirection::Incoming => lsp::LspRequest::GetIncomingCalls {
                request_id,
                call_hierarchy_item: item,
            },
        }) {
            Some(AppEvent::LoadingCalls {
                direction: which.to_string(),
            })
        } else {
            self.loading_states.insert(
                *symbol_id,
                LoadingState::Failed(format!("Failed to request {} calls", which)),
            );
            None
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

    /// Drop everything in flight, and hand back the requests that were
    /// dropped so the caller can put the work they stood for somewhere it will
    /// be picked up again.
    ///
    /// Clearing is not free, and the cost is invisible from here. A reply to a
    /// request that is no longer pending is unrecognisable: `take_pending`
    /// answers `None`, `CoreApp::handle_lsp_response` only does anything
    /// inside the `Some`, and the whole reply goes on the floor. The symbol's
    /// `loading_states` entry goes with it, so it reads `NotLoaded` again —
    /// a function in the graph with no edges, nothing on its way, and nothing
    /// that will ask a second time, which is exactly the isolated node
    /// `logic::cluster` drops from the map.
    ///
    /// So the discarded requests come back rather than being cleaned up in the
    /// caller: the caller is the only place that knows where unfinished work
    /// goes — for `CoreApp` that is the edge-warmup queue, which this module
    /// knows nothing about — and `#[must_use]` makes the next caller of
    /// `clear` say in code what happens to it. Reading `pending_requests`
    /// before calling this would work just as well and would be just as easy
    /// to forget, which is the bug this had.
    #[must_use = "these requests are unfinished work: requeue the symbols they name, or say why not"]
    pub fn clear(&mut self) -> Vec<PendingRequest> {
        self.loading_states.clear();
        self.pending_requests.drain().map(|(_, req)| req).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    /// A graph holding one function, at an *absolute* path: the senders turn a
    /// definition location into a `file://` URL, and a relative path fails that
    /// conversion before a request is ever built.
    fn one_function() -> (CallGraph, SymbolId) {
        let mut graph = CallGraph::new();
        let id = graph.add_function(FunctionNode::new(
            "alpha".to_string(),
            "alpha".to_string(),
            Location::new("/src/test.c".to_string(), 1, 1),
        ));
        (graph, id)
    }

    fn hierarchy_item() -> lsp_types::CallHierarchyItem {
        let position = lsp_types::Position {
            line: 0,
            character: 0,
        };
        let range = lsp_types::Range {
            start: position,
            end: position,
        };
        lsp_types::CallHierarchyItem {
            name: "alpha".to_string(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: lsp_types::Url::parse("file:///src/test.c").expect("a valid file url"),
            range,
            selection_range: range,
            data: None,
        }
    }

    /// A bridge whose worker has died: the sender is live, so every caller gets
    /// past its channel check, and the receiving half is gone, so every `send`
    /// fails — which is what this happens to be in the field, an LSP service
    /// that exited while the app kept asking it questions.
    fn dead_worker() -> LspBridge {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut bridge = LspBridge::new();
        bridge.request_tx = Some(tx);
        bridge
    }

    /// A bridge wired to a worker that is listening. The receiver is handed
    /// back because dropping it here would turn every send into a failure.
    fn live_worker() -> (LspBridge, mpsc::UnboundedReceiver<LspRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut bridge = LspBridge::new();
        bridge.request_tx = Some(tx);
        (bridge, rx)
    }

    /// A request that never left is not in flight, and saying it is stalls the
    /// app: see [`LspBridge::dispatch`]. Every sender, every failure path.
    #[test]
    fn a_send_that_fails_leaves_nothing_pending() {
        let (graph, id) = one_function();

        let mut bridge = dead_worker();
        let event =
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Outgoing);
        assert!(matches!(event, Some(AppEvent::FailedToSendRequest)));
        assert!(bridge.pending_requests.is_empty(), "prepare");

        let mut bridge = dead_worker();
        let event = bridge.send_references(&graph, &id);
        assert!(matches!(event, Some(AppEvent::FailedToSendRequest)));
        assert!(bridge.pending_requests.is_empty(), "references");

        let mut bridge = dead_worker();
        let event = bridge.send_workspace_symbols();
        assert!(matches!(event, Some(AppEvent::FailedToSendRequest)));
        assert!(bridge.pending_requests.is_empty(), "workspace symbols");

        let mut bridge = dead_worker();
        let event = bridge.send_follow_up_call(
            &id,
            hierarchy_item(),
            CallDirection::Outgoing,
            RequestOrigin::Warmup,
        );
        // `None` here is this sender's spelling of failure, and deliberately
        // the opposite way round from the three above.
        assert!(event.is_none());
        assert!(bridge.pending_requests.is_empty(), "follow-up");
    }

    /// The follow-up is the one sender that used to record the request before
    /// it had even looked for a channel to send it on, so a bridge with no
    /// worker at all — demo mode, or a loader that has not handed its channels
    /// over yet — left an entry behind without a single byte going anywhere.
    #[test]
    fn a_follow_up_with_no_channel_leaves_nothing_pending() {
        let (_, id) = one_function();
        let mut bridge = LspBridge::new();

        let event = bridge.send_follow_up_call(
            &id,
            hierarchy_item(),
            CallDirection::Incoming,
            RequestOrigin::User,
        );

        assert!(matches!(event, Some(AppEvent::NoLspChannel)));
        assert!(bridge.pending_requests.is_empty());
        assert!(matches!(
            bridge.get_loading_state(&id),
            LoadingState::Failed(_)
        ));
    }

    /// The other half of the fix: a request that *did* go out is still tracked.
    /// Untracking everything would pass the tests above and lose every reply,
    /// since `take_pending` is what tells a response which symbol it answers.
    #[test]
    fn a_send_that_works_records_exactly_one_request() {
        let (graph, id) = one_function();

        let (mut bridge, mut rx) = live_worker();
        assert!(bridge
            .send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Incoming)
            .is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::PrepareCallHierarchy { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "prepare");
        let pending = bridge.pending_requests.values().next().expect("one entry");
        assert_eq!(pending.symbol_id, Some(id));
        assert_eq!(pending.origin, RequestOrigin::Warmup);
        assert_eq!(pending.direction, Some(CallDirection::Incoming));

        let (mut bridge, mut rx) = live_worker();
        assert!(matches!(
            bridge.send_references(&graph, &id),
            Some(AppEvent::FindingReferences { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::FindReferencesWithSymbols { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "references");

        let (mut bridge, mut rx) = live_worker();
        assert!(matches!(
            bridge.send_workspace_symbols(),
            Some(AppEvent::RefreshingWorkspaceSymbols)
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::GetWorkspaceSymbols { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "workspace symbols");

        let (mut bridge, mut rx) = live_worker();
        assert!(matches!(
            bridge.send_follow_up_call(
                &id,
                hierarchy_item(),
                CallDirection::Outgoing,
                RequestOrigin::User
            ),
            Some(AppEvent::LoadingCalls { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::GetOutgoingCalls { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "follow-up");
    }

    /// The id a request is filed under has to be the id it was sent with, or
    /// the reply matches nothing and `take_pending` logs it as unknown.
    #[test]
    fn a_request_is_filed_under_the_id_it_was_sent_with() {
        let (mut bridge, mut rx) = live_worker();
        bridge.send_workspace_symbols();

        let sent = match rx.try_recv().expect("the request went out") {
            LspRequest::GetWorkspaceSymbols { request_id, .. } => request_id,
            other => panic!("expected a workspace symbol query, got {other:?}"),
        };
        assert!(bridge.take_pending(&sent).is_some());
        assert!(bridge.pending_requests.is_empty());
    }
}
