use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{mpsc, oneshot, Notify};

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

/// What became of a request, and the one thing the frontend is told about it.
///
/// This is the convention every `LspBridge::send_*` method answers in, and it
/// has exactly three outcomes: the request went out, there was nothing to ask
/// or nowhere to ask it, or it could not be made. Each of the two that reach
/// the worker's door carries the single [`AppEvent`] a frontend should hear —
/// a status line naming what is being fetched, or a failure worth reporting.
/// A `None` inside a variant means "and nothing to say about it"; it never
/// means "and it worked". The outcome says that.
///
/// The outcome is in the type because it used to be inferred from a bare
/// `Option<AppEvent>`, and `Some` meant failure in [`LspBridge::send_call_hierarchy`],
/// success in [`LspBridge::send_follow_up_call`], and either one in the other
/// two. Every call site read all four with the same `if let Some(ev)`, which
/// was right only by coincidence: the one caller that *branches* on the
/// outcome — `CoreApp::cmd_expand_function`, which redraws a failed fetch even
/// when the expansion itself moved nothing — spelled "did this fail" as "did
/// an event come back", and that is true only while the success path of that
/// one sender stays silent. The first success event added to it would have
/// turned a working expansion into a spurious redraw, with nothing in the type
/// to catch it.
#[derive(Debug)]
#[must_use = "the event inside is the frontend's whole report of this request"]
pub enum SendOutcome {
    /// The request is with the worker and recorded in flight.
    Sent(Option<AppEvent>),
    /// Nothing was asked and nothing went wrong: the symbol is not in the
    /// graph, or no channel is wired up yet (demo mode, or a loader that has
    /// not handed its channels over). Nothing is coming, and nothing failed.
    Declined,
    /// The request could not be made. Whatever per-symbol [`LoadingState`] the
    /// failure deserves has already been recorded by the sender; this carries
    /// only what to tell the user.
    Failed(Option<AppEvent>),
}

impl SendOutcome {
    /// The event to emit, whichever outcome this is.
    ///
    /// What most call sites want: they append it to the events they were
    /// returning anyway, and the outcome itself changes nothing for them. The
    /// two that do care which outcome it was match on the variants.
    pub fn event(self) -> Option<AppEvent> {
        match self {
            SendOutcome::Sent(event) | SendOutcome::Failed(event) => event,
            SendOutcome::Declined => None,
        }
    }
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
    /// Whether clangd has to open — and, until it answers, keep open — the
    /// symbol's file to answer this. A prepare and a find-references do; the
    /// follow-up that fetches a symbol's calls does not, because clangd
    /// answers it from its index. The warmup budgets the two kinds apart, and
    /// this is how it tells them apart: see `CoreApp::pump_edge_warmup`.
    pub opens_document: bool,
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
            opens_document: false,
        }
    }

    /// Mark this as a request clangd needs the symbol's file open for.
    pub fn opening_a_document(mut self) -> Self {
        self.opens_document = true;
        self
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
    /// Channel to send requests to the LSP service worker.
    ///
    /// Private, and paired with `response_rx` by [`LspBridge::set_channels`]:
    /// a bridge that can ask questions but has nowhere to hear the answers is
    /// a state no loader ever produces, and every reply sent into one is lost.
    request_tx: Option<mpsc::UnboundedSender<LspRequest>>,
    /// Channel to receive responses from the LSP service worker
    response_rx: Option<mpsc::UnboundedReceiver<LspResponse>>,
    /// Raised whenever a reply is waiting in `response_rx`.
    ///
    /// `CoreApp::tick` is polled, and a reply that lands between polls sits
    /// in the channel until the next one. The edge warmup runs in lockstep
    /// with those polls — eight prepares, wait a tick, eight follow-ups, wait
    /// a tick — so at 100 ms it spent about 120 ms of every 200 ms batch with
    /// clangd idle; on the 221-symbol test project that was 5.6 s for work
    /// clangd did in about 2. A frontend that awaits this alongside its
    /// interval ticks as soon as there is something to read instead. It is
    /// a [`Notify`] rather than a second channel because many replies between
    /// two ticks want one wake, not one each, and `notify_one` coalesces them
    /// into a single stored permit. It is *raised*, not signalled by the
    /// bridge itself: the bridge only ever reads the channel, so whoever feeds
    /// it — [`crate::boot::spawn_lsp_bridge`]'s relay — is the one that knows
    /// when a reply has arrived. Never touched by a bridge with no worker, so
    /// a frontend in demo mode falls back to its interval alone.
    response_ready: Arc<Notify>,
    /// In-flight requests awaiting responses, keyed by request ID.
    ///
    /// Private because [`LspBridge::dispatch`] is the only thing that may put
    /// an entry in here, and the reason is spelled out there: an entry with no
    /// request behind it is read by `CoreApp::warming_up` as an answer still
    /// on its way, which holds `CoreApp::loading` true and stops every
    /// clustering for the whole `LSP_REQUEST_TIMEOUT_SECS` it takes
    /// [`LspBridge::check_timed_out_requests`] to retire it. While the field
    /// was `pub` that was a convention; now nothing outside this module can
    /// break it. Callers get [`LspBridge::pending_count`] and
    /// [`LspBridge::pending_summary`], which is all any of them read.
    pending_requests: HashMap<String, PendingRequest>,
    /// Per-symbol loading state. Reached through `is_function_loaded`,
    /// `is_function_loading`, `get_loading_state` and `set_loading_state`,
    /// which is the whole of what anyone outside has ever wanted from it.
    loading_states: HashMap<SymbolId, LoadingState>,
    /// Every call-hierarchy item clangd has produced, by the symbol it names.
    ///
    /// An item is the ticket for `callHierarchy/outgoingCalls` and
    /// `incomingCalls`: the protocol preserves its `data` between the prepare
    /// that minted it and the calls requests that spend it, and clangd's
    /// `data` is the symbol's index id, which is all those requests read.
    /// A prepare is the expensive way to get one — it opens the file and
    /// parses it — but every calls reply carries an item for each caller or
    /// callee it names, minted by the same code from the same index, and the
    /// spec means those to be spent the same way: that is how an editor's
    /// call-hierarchy tree expands a level without preparing every node.
    /// So a symbol met as somebody's callee never needs its own prepare, for
    /// the warmup or for a click. On jemalloc 2125 of the 2897 symbols the
    /// warmup prepared had been named by an earlier reply.
    ///
    /// Kept across [`LspBridge::clear`]: a refresh re-queries the symbols,
    /// not the server, and an item stays valid for the life of the clangd
    /// session.
    hierarchy_items: HashMap<SymbolId, lsp_types::CallHierarchyItem>,
    /// Current phase of the background LSP loading process
    status: LspLoadPhase,
    /// Channel for progress/add-function messages from the background loader
    loader_rx: Option<mpsc::UnboundedReceiver<LspUiMessage>>,
    /// One-shot channel to receive the LSP request/response channels once the loader is ready
    channels_rx: Option<
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
            response_ready: Arc::new(Notify::new()),
            pending_requests: HashMap::new(),
            loading_states: HashMap::new(),
            hierarchy_items: HashMap::new(),
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
            response_ready: Arc::new(Notify::new()),
            pending_requests: HashMap::new(),
            loading_states: HashMap::new(),
            hierarchy_items: HashMap::new(),
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

    /// The signal raised when a reply is waiting to be drained.
    ///
    /// A frontend awaits `notified()` on it beside its poll interval so a
    /// tick follows a reply at once rather than at the next interval. See
    /// the field for why the warmup's pace depends on it.
    pub fn response_ready(&self) -> Arc<Notify> {
        Arc::clone(&self.response_ready)
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
    ///
    /// Answers in the convention of [`SendOutcome`]. The one sender whose
    /// success says nothing: by the time this returns the caller has already
    /// set the symbol `Loading` and is about to redraw for its own reasons, so
    /// a second event would only be the same news twice.
    pub fn send_call_hierarchy(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
        origin: RequestOrigin,
        direction: CallDirection,
    ) -> SendOutcome {
        let (file_path, line, column, name) = {
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
                return SendOutcome::Declined;
            }
        };

        let Some(request_tx) = self.request_tx.clone() else {
            log::warn!(
                "core::LspBridge no call hierarchy for {}: LSP channel not wired up yet",
                name
            );
            return SendOutcome::Declined;
        };

        // An item already in hand is the prepare's answer: skip to the
        // question it was for. See `hierarchy_items`.
        if let Some(item) = self.hierarchy_items.get(function_id).cloned() {
            log::debug!(
                "core::LspBridge {} has a call-hierarchy item in hand, no prepare needed",
                name
            );
            return self.send_follow_up_call(function_id, item, direction, origin);
        }

        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.loading_states.insert(
                    *function_id,
                    LoadingState::Failed("Invalid file path".to_string()),
                );
                return SendOutcome::Failed(Some(AppEvent::InvalidFilePath));
            }
        };

        let position = lsp_types::Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };

        let pending = PendingRequest::new(
            Some(*function_id),
            format!("prepare {} ({}:{}:{})", name, file_path, line, column),
            origin,
        )
        .in_direction(direction)
        .opening_a_document();

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
            return SendOutcome::Failed(Some(AppEvent::FailedToSendRequest));
        }
        SendOutcome::Sent(None)
    }

    /// Send a find-references request for the given function.
    ///
    /// Answers in the convention of [`SendOutcome`].
    pub fn send_references(
        &mut self,
        call_graph: &CallGraph,
        function_id: &SymbolId,
    ) -> SendOutcome {
        let (file_path, line, column, name) = match call_graph.get_function(function_id) {
            Some(function) => (
                function.definition_location.file_path.clone(),
                function.definition_location.line,
                function.definition_location.column,
                function.name.clone(),
            ),
            None => return SendOutcome::Declined,
        };

        let Some(request_tx) = self.request_tx.clone() else {
            return SendOutcome::Declined;
        };

        let document_uri = match lsp_types::Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                return SendOutcome::Failed(Some(AppEvent::FailedToCreateUri));
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
        )
        // clangd finds references from the file's AST, so this holds its
        // document open until the reply like a prepare does.
        .opening_a_document();

        if !self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::FindReferencesWithSymbols {
                request_id,
                document_uri,
                position,
            }
        }) {
            return SendOutcome::Failed(Some(AppEvent::FailedToSendRequest));
        }
        SendOutcome::Sent(Some(AppEvent::FindingReferences { name }))
    }

    /// Send a workspace symbols request.
    ///
    /// Answers in the convention of [`SendOutcome`].
    pub fn send_workspace_symbols(&mut self) -> SendOutcome {
        let Some(request_tx) = self.request_tx.clone() else {
            return SendOutcome::Declined;
        };

        let pending = PendingRequest::new(None, "workspace symbols", RequestOrigin::User);

        if self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::GetWorkspaceSymbols {
                request_id,
                query: "".to_string(),
            }
        }) {
            SendOutcome::Sent(Some(AppEvent::RefreshingWorkspaceSymbols))
        } else {
            SendOutcome::Failed(Some(AppEvent::FailedToSendRequest))
        }
    }

    /// Send a follow-up call hierarchy request (incoming or outgoing).
    ///
    /// Takes the domain [`CallDirection`] rather than an `outgoing: bool`. The
    /// bool was spelled back out as the string `"outgoing"` or `"incoming"` in
    /// three places in this one function — the label, the failure message and
    /// the event the status bar reads — which is the shape a type is for.
    ///
    /// Answers in the convention of [`SendOutcome`]. It has no `Declined`: a
    /// missing channel is a failure here rather than a polite no, because this
    /// is the second half of a question whose first half already went out, and
    /// the symbol is sitting in `LoadingState::Loading` waiting for it.
    pub fn send_follow_up_call(
        &mut self,
        symbol_id: &SymbolId,
        item: lsp_types::CallHierarchyItem,
        direction: CallDirection,
        origin: RequestOrigin,
    ) -> SendOutcome {
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
            return SendOutcome::Failed(Some(AppEvent::NoLspChannel));
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
            match origin {
                RequestOrigin::User => SendOutcome::Sent(Some(AppEvent::LoadingCalls {
                    direction: which.to_string(),
                })),
                // Nobody is watching for this one, and the warmup sends
                // thousands: a status line for each is the noise
                // `RequestOrigin` exists to stop.
                RequestOrigin::Warmup => SendOutcome::Sent(None),
            }
        } else {
            self.loading_states.insert(
                *symbol_id,
                LoadingState::Failed(format!("Failed to request {} calls", which)),
            );
            // The one failure that says nothing: the symbol's `LoadingState`
            // is the whole report. Kept silent deliberately — what a frontend
            // shows is not this convention's business to change — but now
            // spelled as a failure with nothing to add, rather than as a bare
            // `None` indistinguishable from the success of its neighbour.
            SendOutcome::Failed(None)
        }
    }

    /// Ask the worker to open `documents` now, ahead of any request for them.
    ///
    /// clangd builds a file's preamble when the file is opened, and a prepare
    /// sent into a file it has not seen waits for that build: a quarter of a
    /// second on jemalloc, against tens of milliseconds once it is built. The
    /// warmup knows which files its lanes will reach next, so it has them
    /// opened while the current ones are still being read, and the build
    /// overlaps the reading instead of preceding it. The documents go through
    /// the worker's LRU like any other open, so they cost nothing the limit
    /// does not already allow, and are evicted like any other once idle.
    ///
    /// The worker acknowledges the open at once — it is a notification to
    /// clangd, not a question — so the pending entry this records is gone by
    /// the next tick; it is registered at all only because every request that
    /// gets an answer is, see [`LspBridge::dispatch`].
    pub fn send_preload(&mut self, documents: Vec<lsp_types::Url>) -> SendOutcome {
        let Some(request_tx) = self.request_tx.clone() else {
            return SendOutcome::Declined;
        };
        let pending = PendingRequest::new(
            None,
            format!("open {} file(s) ahead", documents.len()),
            RequestOrigin::Warmup,
        );
        if self.dispatch(&request_tx, pending, |request_id| {
            LspRequest::PreloadDocuments {
                request_id,
                document_uris: documents,
            }
        }) {
            SendOutcome::Sent(None)
        } else {
            SendOutcome::Failed(Some(AppEvent::FailedToSendRequest))
        }
    }

    /// Record an item clangd produced for `symbol_id`, from whichever reply
    /// carried it. The first one wins: they are the same ticket.
    pub fn learn_hierarchy_item(
        &mut self,
        symbol_id: SymbolId,
        item: lsp_types::CallHierarchyItem,
    ) {
        self.hierarchy_items.entry(symbol_id).or_insert(item);
    }

    /// Whether `symbol_id` can be asked about without a prepare.
    pub fn has_hierarchy_item(&self, symbol_id: &SymbolId) -> bool {
        self.hierarchy_items.contains_key(symbol_id)
    }

    /// How many requests in flight hold a document open. See
    /// [`PendingRequest::opens_document`].
    pub fn documents_in_flight(&self) -> usize {
        self.pending_requests
            .values()
            .filter(|p| p.opens_document)
            .count()
    }

    /// How many requests in flight the warmup sent, as opposed to the user.
    pub fn warmup_in_flight(&self) -> usize {
        self.pending_requests
            .values()
            .filter(|p| p.origin == RequestOrigin::Warmup)
            .count()
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

    /// How far the background load has got.
    pub fn status(&self) -> &LspLoadPhase {
        &self.status
    }

    /// Report a new phase.
    ///
    /// The phase is the loader's to say: in a running app the only caller is
    /// [`LspBridge::poll_loader_messages`], which sets it from the messages it
    /// drains. It is a method rather than a public field so that anywhere a
    /// phase is claimed that no loader reported — a test standing a finished
    /// load up, a frontend's fixture — is one grep away.
    pub fn set_status(&mut self, phase: LspLoadPhase) {
        self.status = phase;
    }

    /// Whether there is a worker to send requests to at all.
    ///
    /// Demo mode has none, and neither does a project whose loader has not
    /// handed its channels over yet — in both, every sender declines.
    pub fn has_worker(&self) -> bool {
        self.request_tx.is_some()
    }

    /// How many requests are outstanding.
    ///
    /// The warmup reads this as its in-flight ceiling and `CoreApp` reads it
    /// as "is the graph still arriving"; see [`LspBridge::pending_summary`]
    /// for what they are when it stops moving.
    pub fn pending_count(&self) -> usize {
        self.pending_requests.len()
    }

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

    // ---- Test scaffolding ----
    //
    // `pending_requests` is private so that [`LspBridge::dispatch`] is the
    // only thing that can fill it. A test that wants a reply handled has to
    // put the entry there some other way, and these three are that way:
    // `pub(crate)` and `#[cfg(test)]`, so they exist for `core`'s own unit
    // tests and for nothing that ships.

    /// Pretend a request went out under `request_id`.
    #[cfg(test)]
    pub(crate) fn insert_pending(
        &mut self,
        request_id: impl Into<String>,
        pending: PendingRequest,
    ) {
        self.pending_requests.insert(request_id.into(), pending);
    }

    /// Pretend everything in flight was answered.
    ///
    /// Not [`LspBridge::clear`], which also drops every `loading_states`
    /// entry: a symbol that reads `NotLoaded` again is one the warmup will
    /// ask about a second time, which is a different situation from the one
    /// these tests are setting up.
    #[cfg(test)]
    pub(crate) fn clear_pending(&mut self) {
        self.pending_requests.clear();
    }

    /// Everything currently in flight.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> impl Iterator<Item = &PendingRequest> {
        self.pending_requests.values()
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
        let outcome =
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Outgoing);
        assert!(matches!(
            outcome,
            SendOutcome::Failed(Some(AppEvent::FailedToSendRequest))
        ));
        assert!(bridge.pending_requests.is_empty(), "prepare");

        let mut bridge = dead_worker();
        let outcome = bridge.send_references(&graph, &id);
        assert!(matches!(
            outcome,
            SendOutcome::Failed(Some(AppEvent::FailedToSendRequest))
        ));
        assert!(bridge.pending_requests.is_empty(), "references");

        let mut bridge = dead_worker();
        let outcome = bridge.send_workspace_symbols();
        assert!(matches!(
            outcome,
            SendOutcome::Failed(Some(AppEvent::FailedToSendRequest))
        ));
        assert!(bridge.pending_requests.is_empty(), "workspace symbols");

        let mut bridge = dead_worker();
        let outcome = bridge.send_follow_up_call(
            &id,
            hierarchy_item(),
            CallDirection::Outgoing,
            RequestOrigin::Warmup,
        );
        // This one reports the failure and has nothing to add to it: the
        // symbol's `LoadingState` is where it lands. See [`SendOutcome`].
        assert!(matches!(outcome, SendOutcome::Failed(None)));
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

        let outcome = bridge.send_follow_up_call(
            &id,
            hierarchy_item(),
            CallDirection::Incoming,
            RequestOrigin::User,
        );

        assert!(matches!(
            outcome,
            SendOutcome::Failed(Some(AppEvent::NoLspChannel))
        ));
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
        assert!(matches!(
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Incoming),
            // Sent, and with nothing to say about it: the caller has already
            // told the user it is fetching. See [`SendOutcome`].
            SendOutcome::Sent(None)
        ));
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
            SendOutcome::Sent(Some(AppEvent::FindingReferences { .. }))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::FindReferencesWithSymbols { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "references");

        let (mut bridge, mut rx) = live_worker();
        assert!(matches!(
            bridge.send_workspace_symbols(),
            SendOutcome::Sent(Some(AppEvent::RefreshingWorkspaceSymbols))
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
            SendOutcome::Sent(Some(AppEvent::LoadingCalls { .. }))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::GetOutgoingCalls { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1, "follow-up");
    }

    /// An item already in hand is the prepare's answer, so the send skips
    /// to the follow-up: one index request, no document opened.
    #[test]
    fn a_symbol_with_an_item_in_hand_is_followed_up_not_prepared() {
        let (graph, id) = one_function();
        let (mut bridge, mut rx) = live_worker();
        bridge.learn_hierarchy_item(id, hierarchy_item());

        let outcome =
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Outgoing);

        assert!(matches!(outcome, SendOutcome::Sent(None)));
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::GetOutgoingCalls { .. })
        ));
        assert_eq!(bridge.pending_requests.len(), 1);
        assert_eq!(
            bridge.documents_in_flight(),
            0,
            "a follow-up is answered from the index and holds no document"
        );
        // The direction still comes off the asker.
        let (mut bridge, mut rx) = live_worker();
        bridge.learn_hierarchy_item(id, hierarchy_item());
        let _ =
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::User, CallDirection::Incoming);
        assert!(matches!(
            rx.try_recv(),
            Ok(LspRequest::GetIncomingCalls { .. })
        ));
    }

    /// A prepare holds its document open until the reply; the warmup's
    /// document budget is a count of exactly these.
    #[test]
    fn a_prepare_holds_a_document_and_the_items_outlive_a_refresh() {
        let (graph, id) = one_function();
        let (mut bridge, _rx) = live_worker();
        let _ =
            bridge.send_call_hierarchy(&graph, &id, RequestOrigin::Warmup, CallDirection::Outgoing);
        assert_eq!(bridge.documents_in_flight(), 1);
        assert_eq!(bridge.warmup_in_flight(), 1);

        bridge.learn_hierarchy_item(id, hierarchy_item());
        let _ = bridge.clear();

        assert_eq!(bridge.documents_in_flight(), 0);
        assert!(
            bridge.has_hierarchy_item(&id),
            "a refresh re-queries the symbols, not the server: the items stay valid"
        );
    }

    /// The id a request is filed under has to be the id it was sent with, or
    /// the reply matches nothing and `take_pending` logs it as unknown.
    #[test]
    fn a_request_is_filed_under_the_id_it_was_sent_with() {
        let (mut bridge, mut rx) = live_worker();
        let _ = bridge.send_workspace_symbols();

        let sent = match rx.try_recv().expect("the request went out") {
            LspRequest::GetWorkspaceSymbols { request_id, .. } => request_id,
            other => panic!("expected a workspace symbol query, got {other:?}"),
        };
        assert!(bridge.take_pending(&sent).is_some());
        assert!(bridge.pending_requests.is_empty());
    }
}
