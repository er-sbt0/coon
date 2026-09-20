use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use logic::{ClusterKey, ClusterMethod, ClusterSettings, Clustering, GraphTraversal};
use lsp::LspResponse;
use model::{CallDirection, CallGraph, FunctionNode, Location, LspLoadPhase, SymbolId};

use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::{compute_map_frame, LayoutOrientation, MapFrame};
use crate::lsp_bridge::{LoadingState, LspBridge, PendingRequest, RequestOrigin, SendOutcome};
use crate::workspace::{CoreWorkspace, CoreWorkspaceManager, ViewMode};

/// How many call-hierarchy requests to keep in flight while warming up edges.
///
/// clangd serves these one at a time, so the ceiling is about not parking two
/// hundred requests on a 30 s timeout behind a server that answers them at its
/// own pace — not about parallelism. Eight keeps the pipe busy and leaves room
/// for a request the user makes while the warmup is still running.
///
/// It is also bounded from above by [`lsp::OPEN_DOCUMENT_LIMIT`], and the
/// `const` assertion below is what enforces that rather than describing it.
const WARMUP_IN_FLIGHT: usize = 8;

/// The warmup may not outrun the set of documents clangd is holding open.
///
/// A call hierarchy is two round trips: the prepare opens the symbol's file
/// and puts it at the recent end of the LRU, and the follow-up
/// (`GetOutgoingCalls`) neither reopens it nor touches it again — so the file
/// has to survive both legs on the position the prepare gave it. Every other
/// request dispatched in that window can open a file of its own, and a slot
/// freed by a reply is refilled at once, so under in-order service up to twice
/// this concurrency of fresh documents lands on top of it: the hard floor is
/// `2 * WARMUP_IN_FLIGHT + 1`. Replies are not in fact ordered — a slow file
/// leaves its document sitting while the other slots cycle — and the user's
/// own expands and find-references draw on the same budget and open files of
/// their own, neither of which has a bound worth quoting. So the enforced
/// margin is four times over, which is what the shipping pair already is.
///
/// Violating it is silent at runtime: clangd answers the follow-up with
/// "trying to get AST for non-added document", the symbol keeps no edges, and
/// the graph merely comes out smaller than it should. Hence a build failure.
/// Raising the concurrency is allowed — it just has to be paid for in
/// [`lsp::OPEN_DOCUMENT_LIMIT`], which is clangd's memory, and saying so here
/// is the point.
const _: () = assert!(
    WARMUP_IN_FLIGHT * 4 <= lsp::OPEN_DOCUMENT_LIMIT,
    "WARMUP_IN_FLIGHT must stay at or below a quarter of lsp::OPEN_DOCUMENT_LIMIT: \
     a call hierarchy needs its document open across two round trips, and evicting \
     it mid-flight loses the symbol's edges silently. Raise OPEN_DOCUMENT_LIMIT too."
);

/// How often the edge warmup writes its state to the log while it runs.
///
/// The status bar shows a fraction and nothing else, so a warmup that has
/// stopped moving looks exactly like one that is merely slow. This is the log's
/// answer to "stuck on what?" — often enough to catch a stall as it happens,
/// rare enough not to bury the log under ten lines a second.
const WARMUP_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// How long the warmup fraction may sit still before the log says so out loud.
///
/// Longer than any single call-hierarchy round trip on a warm index, shorter
/// than the 30s request timeout in `LspBridge` — so a request clangd is never
/// going to answer is named here before the timeout quietly retires it.
const WARMUP_STALL_AFTER: Duration = Duration::from_secs(10);

/// Which clustering a workspace is asking for — and so what a cache entry and
/// a background job are each about.
///
/// Every part of it comes off the workspace, because every part of it changes
/// what comes back: the settings are the algorithm and the knobs it reads, the
/// scope is the subgraph, and the direction decides which way a scope reaches.
/// Two tabs agreeing on all of it are asking the same question and share the
/// answer — and two that agree on the method but not on its grain are not, which
/// is why the whole of [`ClusterSettings`] is in the key rather than the method
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClusterTarget {
    /// One function's neighbourhood, or `None` for the whole program.
    scope: Option<(SymbolId, CallDirection)>,
    settings: ClusterSettings,
}

impl ClusterTarget {
    fn of(ws: &CoreWorkspace) -> Self {
        Self {
            scope: ws.map_scope().map(|scope| (scope, ws.direction())),
            settings: ws.cluster_settings(),
        }
    }

    /// The whole-program map built to the same settings — what a new tab opens
    /// on, and what widening a scope goes back to.
    fn unscoped(&self) -> Self {
        Self {
            scope: None,
            ..*self
        }
    }

    /// A total order, so which of several owed maps is computed first does not
    /// depend on hash iteration. Settings sort by method and then by the knobs
    /// as they go out on the wire, which is arbitrary but total, and that is
    /// all this promises.
    fn order(&self) -> (bool, u128, bool, usize, u64, u64, usize) {
        let (scope, incoming) = match self.scope {
            Some((symbol, direction)) => (
                symbol.0.as_u128(),
                matches!(direction, CallDirection::Incoming),
            ),
            None => (0, false),
        };
        (
            self.scope.is_some(),
            scope,
            incoming,
            self.settings.method() as usize,
            self.settings.resolution().to_bits(),
            self.settings.inflation().to_bits(),
            self.settings.target_top(),
        )
    }
}

/// A clustering being computed off the tick loop.
///
/// A bare `std::thread` rather than `tokio::spawn_blocking` on purpose. `core`
/// is reached by three frontends through three different runtimes — the TUI
/// from `#[tokio::main]` inside `block_in_place`, the web backend from axum,
/// the desktop one from `tauri::async_runtime` — and by a test suite that has
/// no runtime at all. A thread needs none of them to be true, and this crate
/// stays something you can call without one.
struct ClusteringJob {
    target: ClusterTarget,
    result: mpsc::Receiver<Clustered>,
    started_at: Instant,
}

/// What a worker hands back: the grouping it was asked for, and — when the app
/// had no frame in step with the graph when the job went out — the frame the
/// maps are placed in.
///
/// They travel together because they are the same errand. Both are computed
/// from the graph as it stands when the job starts, both take long enough that
/// doing them under the app's lock froze the tick loop, and the frame is per
/// *graph* rather than per target, so pinning it to its own job would mean a
/// second worker and a second queue for a tenth of the work.
struct Clustered {
    clustering: Clustering,
    frame: Option<MapFrame>,
}

/// Frontend-agnostic application core.
///
/// Owns the call graph, workspace state, and LSP bridge. Emits `AppEvent`
/// values rather than mutating any rendering state. Frontends wrap `CoreApp`
/// and translate the events into their own display concerns.
pub struct CoreApp {
    pub call_graph: CallGraph,
    pub selected_function: Option<SymbolId>,
    pub lsp: LspBridge,
    pub workspaces: CoreWorkspaceManager,
    /// The cluster map behind each question a workspace is asking, cached.
    ///
    /// Keyed by [`ClusterTarget`] rather than held as one whole-program
    /// clustering plus a table of scoped ones: with the method now part of what
    /// is being asked, "the" clustering stopped being a single thing, and the
    /// whole-program map is simply the target whose scope is `None`. One table
    /// then means one staleness rule, one job queue and one eviction pass
    /// instead of two of each.
    ///
    /// Derived from the whole graph (or the whole scope), never from the BFS
    /// window on screen: the window changes every time the user expands a node,
    /// and boxes that regrouped and renamed themselves on every click would be
    /// describing the view rather than the program.
    clusterings: HashMap<ClusterTarget, Clustering>,
    /// Targets whose cached map is out of date with the graph. The entries are
    /// still worth keeping — a stale map is drawable, and stands in while the
    /// replacement is computed — so this marks them rather than dropping them.
    ///
    /// Per target rather than one flag covering all of them because only one
    /// clustering is computed at a time: a single bool cannot say "A has been
    /// redone, B is still owed", so clearing it when the first job went out
    /// would strand every other map on a stale grouping for good.
    stale: HashSet<ClusterTarget>,
    /// The clustering currently being computed on a worker thread, if any.
    ///
    /// Coarsening a large project runs into seconds, and `tick()` is called
    /// with the app's own lock held by the web and desktop backends — so
    /// computing inline froze every command, every snapshot and the tick loop
    /// itself for the duration. Only one job runs at a time: the recompute *is*
    /// the expensive thing, so a second alongside it would compete for a core
    /// rather than add one.
    clustering_job: Option<ClusteringJob>,
    /// Where every function sits when the whole graph is laid out at once — the
    /// frame each rung of each map is placed in, so that a box lands where the
    /// functions it holds already are. See [`crate::layout::MapFrame`].
    ///
    /// One per graph, not one per target: a scoped map draws a neighbourhood of
    /// the same program and belongs in the same picture of it, so widening a
    /// scope moves nothing that stayed.
    map_frame: MapFrame,
    /// Whether that frame describes a graph that has since grown.
    ///
    /// A stale frame is still the right thing to draw with — it seats every
    /// function that was there a moment ago, and `MapFrame::seat_of` says so
    /// for the handful that arrived since — so this marks it rather than
    /// dropping it, exactly as `stale` does for a clustering.
    frame_stale: bool,
    /// Targets whose worker died without answering, so they are not sent
    /// straight back out.
    ///
    /// A panic in the clustering is deterministic given the same graph, so a
    /// retry against that graph panics again — and since what is owed is worked
    /// out from the cache being empty, a death would otherwise spawn a fresh
    /// thread every tick for as long as the app runs. Cleared by
    /// `mark_graph_changed`: a different graph is a different question.
    clustering_failed: HashSet<ClusterTarget>,
    /// Functions still awaiting their first call-hierarchy fetch.
    ///
    /// clangd's workspace symbols carry no edges, so a freshly loaded project is
    /// a heap of isolated nodes: Louvain drops isolated symbols, the clustering
    /// comes out empty, and the cluster map the app opens on has nothing to
    /// draw. Asking for each symbol's callees up front turns that heap into a
    /// graph.
    ///
    /// Fed by every symbol the graph gains from a *symbol query* — the loader's
    /// stream, a discovery reply that lands after it, a refresh — and
    /// deliberately not by the callees a call-hierarchy reply discovers:
    /// following those would walk the whole program.
    edge_warmup: VecDeque<SymbolId>,
    /// Whether the queue has opened. Until it has, arriving symbols are left to
    /// the seeding sweep; after it, each one queues itself as it lands.
    warmup_seeded: bool,
    /// How many symbols the warmup has queued in total, so progress can be
    /// reported as a fraction rather than a countdown from an unknown start.
    ///
    /// Grows when symbols arrive late. The denominator moving is the honest
    /// reading — the work really did get bigger — and beats the alternative of
    /// resolving those symbols silently, outside the number the status bar is
    /// showing.
    ///
    /// The one invariant it has to keep is `warmup_total >= edge_warmup.len()`,
    /// since `done` is the difference: work put *back* on the queue by
    /// `requeue_discarded` therefore lowers `done` rather than raising this,
    /// because those places were counted when they were first queued.
    warmup_total: usize,
    /// Whether the closing `ResolvingCalls { done: total, .. }` has gone out.
    /// Without it the end would be announced on every tick thereafter, since
    /// "the queue is empty" stays true forever.
    warmup_finish_reported: bool,
    /// The fraction the warmup last reported, and when it last changed.
    ///
    /// Only here for the log: a stall is not a state the app acts on, it is a
    /// thing the user notices in the status bar and then has to explain, so what
    /// the log needs is the moment progress stopped and what was outstanding
    /// then.
    warmup_last_done: usize,
    warmup_last_moved: Option<Instant>,
    /// When the warmup opened, so the log can say how long the whole sweep took.
    warmup_started_at: Option<Instant>,
    /// When the warmup last wrote its state out, for throttling.
    warmup_last_logged: Option<Instant>,
}

impl CoreApp {
    /// Create a `CoreApp` in demo/offline mode (no LSP).
    pub fn new(call_graph: CallGraph) -> Self {
        Self::with_bridge(call_graph, LspBridge::new())
    }

    /// Create a `CoreApp` with an active LSP bridge.
    pub fn new_with_lsp(call_graph: CallGraph, lsp: LspBridge) -> Self {
        Self::with_bridge(call_graph, lsp)
    }

    fn with_bridge(call_graph: CallGraph, lsp: LspBridge) -> Self {
        // Cluster up front rather than leaving it to the first tick, and only
        // here: a graph that arrives already populated is demo mode, where this
        // is the whole of it and no loader will ever arrive to say so. In LSP
        // mode the graph is empty at this point and this costs nothing, which
        // is exactly why it can be unconditional — `build_base_graph` over no
        // symbols returns before it allocates.
        let opening = ClusterTarget {
            scope: None,
            settings: ClusterSettings::default(),
        };
        let clusterings =
            HashMap::from([(opening, Clustering::compute(&call_graph, opening.settings))]);
        // The frame the map is placed in, for the same reason and on the same
        // terms: over an empty graph it returns before it allocates, and over a
        // demo graph it is the frame every rung is drawn in from the first
        // snapshot on.
        let map_frame = compute_map_frame(&call_graph);
        Self {
            call_graph,
            selected_function: None,
            lsp,
            workspaces: CoreWorkspaceManager::new(),
            clusterings,
            stale: HashSet::new(),
            clustering_job: None,
            map_frame,
            frame_stale: false,
            clustering_failed: HashSet::new(),
            edge_warmup: VecDeque::new(),
            warmup_seeded: false,
            warmup_total: 0,
            warmup_finish_reported: false,
            warmup_last_done: 0,
            warmup_last_moved: None,
            warmup_started_at: None,
            warmup_last_logged: None,
        }
    }

    /// The cluster map this workspace is looking at: its method, over its
    /// scope, in its direction.
    ///
    /// A target with nothing computed yet reads as empty, not as some other
    /// map. There is a real window where that happens — a project still
    /// loading, or a tab that has just toggled its direction and wants a
    /// neighbourhood nothing has walked — and answering with the nearest thing
    /// to hand would draw every function in the project under a breadcrumb
    /// reading "map of `f`", which says the scope silently failed rather than
    /// that it is not ready. An empty map says the second, which is the true
    /// one.
    pub fn clustering_for(&self, ws: &CoreWorkspace) -> &Clustering {
        static PENDING: std::sync::OnceLock<Clustering> = std::sync::OnceLock::new();
        self.clusterings
            .get(&ClusterTarget::of(ws))
            .unwrap_or_else(|| PENDING.get_or_init(Clustering::default))
    }

    /// The frame every cluster map is placed in: where each function sits when
    /// the whole graph is laid out at once.
    ///
    /// Empty until the first worker has been round, which is what a map drawn
    /// before then is placed against — and there is nothing to draw then
    /// anyway, since the clustering that would fill it comes from the same job.
    pub fn map_frame(&self) -> &MapFrame {
        &self.map_frame
    }

    /// Which rung of the ladder this workspace's map is drawn at.
    ///
    /// The workspace stores a wish — `None` for "wherever this clustering says
    /// to open", a number the user zoomed to otherwise — and this is where it
    /// meets the ladder that actually exists. Clamping here rather than when
    /// the level is set is what lets the wish outlive a recompute: a ladder
    /// that grew a rung as the project loaded gives the user the rung they
    /// asked for, and one that lost rungs gives them the coarsest rather than a
    /// blank canvas.
    ///
    /// The default is [`Clustering::opening_level`], not the top. Coarsening
    /// stops at a handful of boxes, which is where a user zooms out *to* and
    /// not where they should land.
    pub fn map_level_for(&self, ws: &CoreWorkspace) -> usize {
        let clustering = self.clustering_for(ws);
        let top = clustering.top_level_index().unwrap_or_default();
        ws.map_level()
            .unwrap_or_else(|| clustering.opening_level())
            .min(top)
    }

    /// The communities of the active workspace's map.
    pub fn active_clustering(&self) -> &Clustering {
        static PENDING: std::sync::OnceLock<Clustering> = std::sync::OnceLock::new();
        match self.workspaces.current() {
            Some(ws) => self.clustering_for(ws),
            None => PENDING.get_or_init(Clustering::default),
        }
    }

    /// How far the initial edge warmup has got, or `None` when it is not
    /// running. For frontends that read state directly instead of following
    /// `AppEvent::ResolvingCalls`.
    pub fn warmup_progress(&self) -> Option<(usize, usize)> {
        self.warming_up().then(|| {
            (
                self.warmup_total.saturating_sub(self.edge_warmup.len()),
                self.warmup_total,
            )
        })
    }

    // -------------------------------------------------------------------------
    // Public API
    // -------------------------------------------------------------------------

    /// The direction the tab in front is reading the graph in, right now.
    ///
    /// Every user-initiated call-hierarchy fetch reads this at *send* time and
    /// carries the answer on the request. There is no workspace at all only in
    /// tests and the moment before the first one exists, and outgoing is what a
    /// fresh workspace starts on, so that is the fallback.
    pub fn current_direction(&self) -> CallDirection {
        self.workspaces
            .current()
            .map(|w| w.direction())
            .unwrap_or(CallDirection::Outgoing)
    }

    /// Dispatch a domain command and return events describing what changed.
    pub fn dispatch(&mut self, cmd: Command) -> Vec<AppEvent> {
        log::info!("CoreApp::dispatch {:?}", cmd);
        let events = match cmd {
            Command::SelectFunction(id) => self.cmd_select_function(id),
            Command::ExpandOrCollapse => self.cmd_expand_or_collapse(),
            Command::FindReferences => self.cmd_find_references(),
            Command::Refresh => self.cmd_refresh(),
            Command::ToggleCallDirection => self.cmd_toggle_call_direction(),
            Command::SetGraphDepth(depth) => self.cmd_set_graph_depth(depth),
            Command::ToggleCollapsed(id) => self.cmd_toggle_collapsed(id),
            Command::SetGraphOrientation(o) => self.cmd_set_graph_orientation(o),
            Command::SetViewMode(view) => self.cmd_set_view_mode(view),
            Command::SetMapScope(scope) => self.cmd_set_map_scope(scope),
            Command::SetMapLevel(level) => self.cmd_set_map_level(level),
            Command::SetClusterMethod(method) => self.cmd_set_cluster_method(method),
            Command::SetClusterSettings(settings) => self.cmd_set_cluster_settings(settings),
            Command::DrillIntoCluster(key) => self.cmd_drill_into_cluster(key),
            Command::ZoomOutToClusters => self.cmd_zoom_out_to_clusters(),
            Command::NewWorkspace => self.cmd_new_workspace(),
            Command::CloseWorkspace(index) => self.cmd_close_workspace(index),
            Command::NextWorkspace => self.cmd_next_workspace(),
            Command::PreviousWorkspace => self.cmd_previous_workspace(),
            Command::SwitchWorkspace(index) => self.cmd_switch_workspace(index),
            Command::RenameWorkspace(index, name) => self.cmd_rename_workspace(index, name),
            Command::StartGraphFromFunction(id) => self.cmd_start_graph_from_function(id),
            Command::ExpandFunction(id) => self.cmd_expand_function(id),
            Command::FocusFunction(id) => self.cmd_focus_function(id),
            Command::ExportGraph(format) => self.cmd_export_graph(format),
        };
        // Any command can be the one that changes which map is wanted — a tab
        // switch, a direction toggle, a new scope, a new method — so the cache
        // is squared up once, here, rather than in each handler that might have
        // moved it.
        self.sync_clusterings();
        log::info!(
            "CoreApp::dispatch result: {} event(s): {:?}",
            events.len(),
            events
        );
        events
    }

    /// Poll LSP channels and loader; return events for anything that arrived.
    ///
    /// Must be called every tick so the LSP state stays current.
    pub fn tick(&mut self) -> Vec<AppEvent> {
        let mut events = Vec::new();

        // Make sure channels are wired once the loader delivers them
        self.lsp.poll_channels();

        // Ingest new symbols from the background loader
        let new_symbols = self.lsp.poll_loader_messages();
        let symbol_count = new_symbols.len();
        for symbol in new_symbols {
            let function = FunctionNode::new(
                symbol.name.clone(),
                symbol.qualified_name.clone(),
                symbol.location.clone(),
            );
            self.ingest_symbol(function);
        }
        if symbol_count > 0 {
            self.mark_graph_changed();
            log::info!(
                "CoreApp::tick ingested {} new symbol(s), graph now has {} nodes",
                symbol_count,
                self.call_graph.node_count()
            );
        }

        // Emit current LSP phase as a progress event
        events.push(AppEvent::LspProgress(self.lsp.status().clone()));

        // Drain pending LSP responses
        let (responses, timeout_events) = self.lsp.drain_responses();
        events.extend(timeout_events);

        for response in responses {
            events.extend(self.handle_lsp_response(response));
        }

        // Before reclustering: a warmup request sent now is what stops the
        // recompute below from running against an edgeless graph.
        events.extend(self.pump_edge_warmup());

        // Take delivery of a finished map before deciding what is owed, so a
        // job that landed on this tick frees the slot for the next one.
        events.extend(self.collect_finished_clustering());
        self.sync_clusterings();

        events
    }

    // -------------------------------------------------------------------------
    // LSP response handler
    // -------------------------------------------------------------------------

    fn handle_lsp_response(&mut self, response: LspResponse) -> Vec<AppEvent> {
        log::debug!("CoreApp: handling LSP response {:?}", response);
        let mut events = Vec::new();

        match response {
            LspResponse::OutgoingCalls { request_id, calls } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        let count = calls.len();
                        for call in calls {
                            self.process_call_entry(symbol_id, &call.to, &call.from_ranges, true);
                        }
                        if count > 0 {
                            self.mark_graph_changed();
                        }
                        self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                        log::info!(
                            "CoreApp: OutgoingCalls loaded for {:?}: {} callee(s)",
                            symbol_id,
                            count
                        );
                        events.push(AppEvent::OutgoingCallsLoaded { symbol_id });
                        // Only what somebody asked to see. A warmup reply is
                        // edges for a node the canvas is not drawing, and
                        // redrawing for each of them is what made loading
                        // jemalloc lag; the map still catches up, through the
                        // regrouping the settling timer starts. Nor is "no
                        // callees" news when nobody asked: the warmup would say
                        // it about every leaf in the project.
                        if pending.origin == RequestOrigin::User {
                            if count == 0 {
                                events.push(AppEvent::FunctionHasNoCallees);
                            }
                            events.push(AppEvent::LayoutDirty);
                        }
                    }
                }
            }
            LspResponse::IncomingCalls { request_id, calls } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        let count = calls.len();
                        for call in calls {
                            self.process_call_entry(
                                symbol_id,
                                &call.from,
                                &call.from_ranges,
                                false,
                            );
                        }
                        if count > 0 {
                            self.mark_graph_changed();
                        }
                        self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                        log::info!(
                            "CoreApp: IncomingCalls loaded for {:?}: {} caller(s)",
                            symbol_id,
                            count
                        );
                        events.push(AppEvent::IncomingCallsLoaded { symbol_id });
                        // As above: a background fetch is not a redraw.
                        if pending.origin == RequestOrigin::User {
                            if count == 0 {
                                events.push(AppEvent::FunctionHasNoCallees);
                            }
                            events.push(AppEvent::LayoutDirty);
                        }
                    }
                }
            }
            LspResponse::References {
                request_id,
                locations,
            } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        if let Some(function) = self.call_graph.get_function_mut(&symbol_id) {
                            function.references.clear();
                            for location in &locations {
                                function.add_reference(location.clone());
                            }
                            if locations.is_empty() {
                                events.push(AppEvent::NoReferencesFound { symbol_id });
                            } else {
                                events.push(AppEvent::ReferencesFound {
                                    symbol_id,
                                    count: locations.len(),
                                });
                            }
                        }
                    }
                }
            }
            LspResponse::ReferencesWithSymbols {
                request_id,
                references,
            } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        if let Some(function) = self.call_graph.get_function_mut(&symbol_id) {
                            function.references = references;
                        }
                        self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                    }
                }
            }
            LspResponse::WorkspaceSymbols {
                request_id,
                symbols,
            } => {
                // Ingest whether or not we issued the request. The initial
                // discovery query is sent by the LSP loader under its own id, and
                // if clangd answers after the loader's wait expires the reply is
                // forwarded here instead — gating on a pending entry we never
                // registered would drop the whole workspace on the floor.
                self.lsp.take_pending(&request_id);
                let count = symbols.len();
                for symbol in symbols {
                    self.ingest_symbol(symbol);
                }
                if count > 0 {
                    self.mark_graph_changed();
                }
                events.push(AppEvent::GraphSymbolsUpdated { count });
            }
            LspResponse::DocumentSymbols { request_id, .. } => {
                self.lsp.take_pending(&request_id);
            }
            LspResponse::Error { request_id, error } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        self.lsp
                            .set_loading_state(symbol_id, LoadingState::Failed(error.clone()));
                    }
                }
                events.push(AppEvent::LspError(error));
            }
            LspResponse::PreloadComplete { .. } => {
                // No-op in core; the TUI handled this via a dedicated status message.
                // Future: emit a dedicated AppEvent if needed.
            }
            LspResponse::IndexProgress(progress) => {
                // The loader waits on this one before it ever hands the bridge
                // over, and re-queries on it if that wait ran out. What reaches
                // here is the tail of an index that outlived the wait — worth a
                // log line, but not a status change: `self.lsp.status` is
                // already `Completed`, and the warmup's seeding and the
                // clustering's settling both read that field.
                log::info!("CoreApp: clangd background index {:?}", progress);
            }
            LspResponse::CallHierarchyPrepared { request_id, items } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        if !items.is_empty() {
                            let first_item = items[0].clone();
                            // The direction comes off the request, not off
                            // whichever tab is in front now. This arm runs
                            // whenever clangd gets round to answering, which
                            // may be long after the click: reading
                            // `workspaces.current()` here asked for the callers
                            // of a node someone had expanded in an outgoing
                            // view simply because they hit `t` in the meantime,
                            // and since the reply marks the symbol `Loaded`
                            // either way, `cmd_expand_function`'s guard then
                            // refused to ever ask again.
                            let direction = pending.direction.unwrap_or(CallDirection::Outgoing);
                            events.extend(
                                self.lsp
                                    .send_follow_up_call(
                                        &symbol_id,
                                        first_item,
                                        direction,
                                        pending.origin,
                                    )
                                    .event(),
                            );
                        } else {
                            self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                            events.push(AppEvent::FunctionHasNoCallees);
                        }
                    }
                }
            }
        }

        events
    }

    // -------------------------------------------------------------------------
    // Command implementations
    // -------------------------------------------------------------------------

    fn cmd_select_function(&mut self, id: SymbolId) -> Vec<AppEvent> {
        self.selected_function = Some(id);
        log::info!("CoreApp: root changed to {id:?}");
        // Also update the current workspace root
        if let Some(ws) = self.workspaces.current_mut() {
            ws.set_root(id);
        }
        let direction = self.current_direction();
        // Kick off call hierarchy loading if not already done. The redraw is
        // unconditional — the root moved — and whatever the fetch has to say
        // leads, so the status line is written before the canvas is.
        let mut events = Vec::new();
        if !self.lsp.is_function_loaded(&id) && !self.lsp.is_function_loading(&id) {
            self.lsp.set_loading_state(id, LoadingState::Loading);
            events.extend(
                self.lsp
                    .send_call_hierarchy(&self.call_graph, &id, RequestOrigin::User, direction)
                    .event(),
            );
        }
        events.push(AppEvent::LayoutDirty);
        events
    }

    fn cmd_expand_or_collapse(&mut self) -> Vec<AppEvent> {
        // The selected node comes from the frontend (TUI passes it via SelectFunction).
        // Here we just request call hierarchy for the currently selected function.
        let direction = self.current_direction();
        if let Some(id) = self.selected_function {
            let mut events: Vec<AppEvent> = self
                .lsp
                .send_call_hierarchy(&self.call_graph, &id, RequestOrigin::User, direction)
                .event()
                .into_iter()
                .collect();
            events.push(AppEvent::LayoutDirty);
            return events;
        }
        vec![]
    }

    fn cmd_find_references(&mut self) -> Vec<AppEvent> {
        let symbol_id = self.workspaces.current().and_then(|w| w.root_symbol());
        if let Some(id) = symbol_id {
            // Nothing on the canvas changes, so there is no `LayoutDirty`
            // here: the answer arrives as its own event later.
            self.lsp
                .send_references(&self.call_graph, &id)
                .event()
                .into_iter()
                .collect()
        } else {
            vec![]
        }
    }

    fn cmd_refresh(&mut self) -> Vec<AppEvent> {
        let direction = self.current_direction();
        // Everything in flight is about to become unanswerable — see
        // `LspBridge::clear` — so the symbols those requests named go back on
        // the warmup queue before anything else happens.
        let discarded = self.lsp.clear();
        self.requeue_discarded(discarded);
        let mut events = Vec::new();
        events.extend(self.lsp.send_workspace_symbols().event());
        if let Some(id) = self.selected_function {
            events.extend(
                self.lsp
                    .send_call_hierarchy(&self.call_graph, &id, RequestOrigin::User, direction)
                    .event(),
            );
        }
        events
    }

    fn cmd_toggle_call_direction(&mut self) -> Vec<AppEvent> {
        if let Some(ws) = self.workspaces.current_mut() {
            // Reversing the arrows is a different tree, so the nodes walked
            // past the depth limit in the old one are dropped with it — by
            // `toggle_direction`, which owns both halves. Clearing `expanded`
            // here instead is what let the two drift apart.
            let direction = ws.toggle_direction();
            log::info!("CoreApp: direction toggled to {direction:?}, expansions cleared");
        }
        vec![AppEvent::LayoutDirty]
    }

    fn cmd_set_graph_orientation(&mut self, orientation: LayoutOrientation) -> Vec<AppEvent> {
        // Only redraw on a real change; the segmented control can re-send the
        // orientation already on screen.
        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.set_orientation(orientation));
        if changed {
            log::info!("CoreApp: graph orientation set to {orientation:?}");
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
    }

    fn cmd_set_graph_depth(&mut self, depth: usize) -> Vec<AppEvent> {
        // Only redraw when the value actually moved; the client sends this from
        // a stepper that can be held at either end of the range.
        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.set_depth(depth));
        if changed {
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
    }

    fn cmd_toggle_collapsed(&mut self, id: SymbolId) -> Vec<AppEvent> {
        match self.workspaces.current_mut() {
            Some(ws) => {
                ws.toggle_collapsed(id);
                vec![AppEvent::LayoutDirty]
            }
            None => vec![],
        }
    }

    fn cmd_set_view_mode(&mut self, view: ViewMode) -> Vec<AppEvent> {
        // Only redraw on a real change; like the orientation control, whatever
        // names this view can re-send the view already on screen.
        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.set_view(view));
        if changed {
            log::info!("CoreApp: view mode set to {view:?}");
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
    }

    fn cmd_set_map_scope(&mut self, scope: Option<SymbolId>) -> Vec<AppEvent> {
        // Widening back out to the whole program always has a map to land on —
        // the same one every workspace starts at, empty or not.
        let Some(symbol) = scope else {
            let changed = self
                .workspaces
                .current_mut()
                .is_some_and(|ws| ws.set_map_scope(None));
            return if changed {
                vec![AppEvent::LayoutDirty]
            } else {
                vec![]
            };
        };

        let Some(ws) = self.workspaces.current() else {
            return vec![];
        };
        if self.call_graph.get_function(&symbol).is_none() {
            log::info!("CoreApp: map scope {symbol:?} ignored — no such function");
            return vec![];
        }

        // Cluster first, switch second. A function with nothing around it —
        // one whose callees are all leaves, or whose calls have not been
        // resolved yet — has no communities to show, and landing the user on a
        // blank map they then have to find their way off is worse than saying
        // so and leaving them where they are.
        //
        // The one clustering still computed inline, and deliberately: the
        // answer decides whether this command does anything at all, so there is
        // nothing to return until it is known. It is also a deliberate gesture
        // rather than something the load does on its own — once per Ctrl+Shift+M,
        // by a user already waiting on the map they just asked for — where the
        // recompute this replaced fired on a timer for as long as the graph
        // kept growing.
        let direction = ws.direction();
        let target = ClusterTarget {
            scope: Some((symbol, direction)),
            settings: ws.cluster_settings(),
        };
        let clustering = cluster_within(&self.call_graph, symbol, direction, target.settings);
        // And the frame it will be placed in, if no worker has been round yet —
        // which is the case while the project is still arriving, since nothing
        // is clustered until it settles. Inline for the same reason the
        // clustering above is: the user is already waiting on this gesture, and
        // a map placed against no frame is every box in one lane.
        if self.map_frame.is_empty() {
            self.map_frame = compute_map_frame(&self.call_graph);
        }
        if clustering.is_empty() {
            log::info!("CoreApp: map scope {symbol:?} declined — nothing to group");
            return vec![AppEvent::FunctionHasNoCommunities];
        }
        self.clusterings.insert(target, clustering);
        // Freshly computed, so whatever marked it stale no longer applies —
        // without this the sync below would send the same work straight back
        // out to a thread.
        self.stale.remove(&target);

        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.set_map_scope(Some(symbol)));
        if changed {
            log::info!("CoreApp: cluster map scoped to {symbol:?} ({direction:?})");
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
    }

    /// Draw the map at one rung of the ladder, clamped to the rungs there are.
    ///
    /// Clamped rather than refused: a frontend sending a level is working from
    /// the snapshot it last received, and a ladder that has since lost a rung
    /// should land the user on the coarsest map rather than leave the zoom
    /// control looking broken.
    fn cmd_set_map_level(&mut self, level: usize) -> Vec<AppEvent> {
        let Some(ws) = self.workspaces.current() else {
            return vec![];
        };
        let top = self
            .clustering_for(ws)
            .top_level_index()
            .unwrap_or_default();
        let level = level.min(top);
        match self.workspaces.current_mut() {
            Some(ws) => {
                if ws.set_map_level(level) {
                    vec![AppEvent::LayoutDirty]
                } else {
                    vec![]
                }
            }
            None => vec![],
        }
    }

    /// Rebuild the map with a different algorithm.
    ///
    /// Emits nothing but `LayoutDirty`, and that draws the *old* map until the
    /// new one lands: switching method asks a question no cache has an answer
    /// to, so `sync_clusterings` starts a job and the map arrives with the
    /// `ClusteringUpdated` that follows. Blanking the canvas in the meantime
    /// would be honest about the wait and useless to look at.
    fn cmd_set_cluster_method(&mut self, method: ClusterMethod) -> Vec<AppEvent> {
        match self.workspaces.current_mut() {
            Some(ws) => {
                if ws.set_method(method) {
                    log::info!("CoreApp: cluster map switched to {}", method.label());
                    vec![AppEvent::LayoutDirty]
                } else {
                    vec![]
                }
            }
            None => vec![],
        }
    }

    /// Rebuild the map to different settings — the method, its grain, or how
    /// far the ladder coarsens.
    ///
    /// The same answer as [`Self::cmd_set_cluster_method`] and for the same
    /// reason: a knob that moves asks a question no cache holds, so this only
    /// says the layout is dirty and the old map stays on screen until
    /// `sync_clusterings` has a worker bring back the new one.
    ///
    /// What arrives is already sanitised — `ClusterSettings` clamps and snaps
    /// on the way in from the wire — so there is nothing to validate here and
    /// no way for a frontend to ask for a γ the ladder has no meaning for.
    fn cmd_set_cluster_settings(&mut self, settings: ClusterSettings) -> Vec<AppEvent> {
        match self.workspaces.current_mut() {
            Some(ws) => {
                if ws.set_cluster_settings(settings) {
                    log::info!(
                        "CoreApp: cluster map set to {} (grain {}, top {})",
                        settings.method().label(),
                        settings.grain(),
                        settings.target_top()
                    );
                    vec![AppEvent::LayoutDirty]
                } else {
                    vec![]
                }
            }
            None => vec![],
        }
    }

    fn cmd_drill_into_cluster(&mut self, key: ClusterKey) -> Vec<AppEvent> {
        // A cluster with no entry point has nothing to root a tree at, and a key
        // that resolves to nothing is a click against a clustering whose anchor
        // has since moved. Either way the honest answer is to leave the view
        // where the user left it rather than root it somewhere arbitrary.
        let Some(entry) = self
            .active_clustering()
            .by_key(key)
            .and_then(|c| c.entry_point)
        else {
            log::info!("CoreApp: drill into {key:?} ignored — no entry point to root at");
            return vec![];
        };

        // Rooting at the entry point is exactly what selecting it does — new
        // root, cleared collapse and expansion state, call hierarchy requested
        // if it is not loaded yet — so go through the same path rather than a
        // second, subtly different one.
        let events = self.cmd_select_function(entry);
        if let Some(ws) = self.workspaces.current_mut() {
            ws.set_view(ViewMode::Rooted);
        }
        events
    }

    fn cmd_zoom_out_to_clusters(&mut self) -> Vec<AppEvent> {
        self.cmd_set_view_mode(ViewMode::ClusterMap)
    }

    fn cmd_new_workspace(&mut self) -> Vec<AppEvent> {
        let (_, event) = self
            .workspaces
            .create(format!("Graph {}", self.workspaces.next_id));
        vec![event]
    }

    fn cmd_close_workspace(&mut self, index: usize) -> Vec<AppEvent> {
        match self.workspaces.close(index) {
            Ok(ev) | Err(ev) => vec![ev],
        }
    }

    fn cmd_next_workspace(&mut self) -> Vec<AppEvent> {
        self.workspaces
            .next_workspace()
            .map(|e| vec![e])
            .unwrap_or_default()
    }

    fn cmd_previous_workspace(&mut self) -> Vec<AppEvent> {
        self.workspaces
            .previous()
            .map(|e| vec![e])
            .unwrap_or_default()
    }

    fn cmd_switch_workspace(&mut self, index: usize) -> Vec<AppEvent> {
        self.workspaces
            .switch_to(index)
            .map(|e| vec![e])
            .unwrap_or_default()
    }

    fn cmd_rename_workspace(&mut self, index: usize, name: String) -> Vec<AppEvent> {
        self.workspaces
            .rename(index, name)
            .map(|e| vec![e])
            .unwrap_or_default()
    }

    fn cmd_start_graph_from_function(&mut self, id: SymbolId) -> Vec<AppEvent> {
        let name = self
            .call_graph
            .get_function(&id)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| "Graph".to_string());
        let (_, ws_event) = self.workspaces.create_with_function(name, id);
        self.selected_function = Some(id);
        // After the create, not before: the tab that will show this graph is
        // the one just made, and it is that tab's reading the fetch answers.
        let direction = self.current_direction();
        let outcome =
            self.lsp
                .send_call_hierarchy(&self.call_graph, &id, RequestOrigin::User, direction);
        // The tab and its redraw come first here, and anything the fetch has
        // to say trails them: the new tab is the news, and a failure to load
        // it is a footnote on a tab that exists either way.
        let mut events = vec![ws_event, AppEvent::LayoutDirty];
        events.extend(outcome.event());
        events
    }

    /// Render the current view to a file's worth of text and hand it back.
    ///
    /// Changes nothing — no selection, no layout, no fetch — so it emits no
    /// `LayoutDirty` and costs the client no snapshot. The frontend that asked
    /// is the one that knows where a file goes; see `AppEvent::GraphExported`.
    fn cmd_export_graph(&mut self, format: crate::export::ExportFormat) -> Vec<AppEvent> {
        match crate::export::export(self, format) {
            Some(export) => {
                log::info!(
                    "CoreApp: exported {} ({} bytes of {:?})",
                    export.filename,
                    export.contents.len(),
                    format
                );
                vec![AppEvent::GraphExported {
                    filename: export.filename,
                    contents: export.contents,
                }]
            }
            None => {
                log::info!("CoreApp: nothing on screen to export");
                vec![AppEvent::NothingToExport]
            }
        }
    }

    /// Point at a function: no new root, no layout, no fetch.
    ///
    /// Deliberately emits nothing. Every event routes to a fresh snapshot, and
    /// a snapshot makes the client tear the canvas down and build it again —
    /// for a click that changed one node's border. The client draws the
    /// selection itself and sends this so the next snapshot, whenever something
    /// else causes one, agrees with what is on screen.
    fn cmd_focus_function(&mut self, id: SymbolId) -> Vec<AppEvent> {
        self.selected_function = Some(id);
        if let Some(ws) = self.workspaces.current_mut() {
            ws.touch();
        }
        vec![]
    }

    fn cmd_expand_function(&mut self, id: SymbolId) -> Vec<AppEvent> {
        self.selected_function = Some(id);
        // Expanding is purely additive: the workspace root, and so the layout
        // the user is looking at, does not move. All this does is let the BFS
        // walk one level past the depth limit at this node, so its callees are
        // appended to the graph on screen.
        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.expand(id));
        log::info!("CoreApp: expand in-place {id:?}, layout changed: {changed}");

        let direction = self.current_direction();
        if !self.lsp.is_function_loaded(&id) && !self.lsp.is_function_loading(&id) {
            self.lsp.set_loading_state(id, LoadingState::Loading);
            // The one call site that branches on the outcome rather than just
            // forwarding its event, and why [`SendOutcome`] spells the outcome
            // out: a fetch that never went redraws whether or not the
            // expansion moved anything, because the node has just been put
            // into a loading state the canvas is showing and has to come back
            // out of it. A fetch that *did* go leaves the decision below.
            if let SendOutcome::Failed(event) =
                self.lsp
                    .send_call_hierarchy(&self.call_graph, &id, RequestOrigin::User, direction)
            {
                let mut events: Vec<AppEvent> = event.into_iter().collect();
                events.push(AppEvent::LayoutDirty);
                return events;
            }
        }
        // A second click on a node already expanded and already loaded has
        // nothing to redraw.
        if changed {
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
    }

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// Record that the call graph gained nodes or edges.
    ///
    /// This only marks; it never recomputes. What is owed is worked out from
    /// the marks in [`Self::sync_clusterings`], which starts nothing while the
    /// project is still arriving — so the settling a growing graph needs comes
    /// from `loading()` going false, not from a timestamp taken here.
    fn mark_graph_changed(&mut self) {
        // Every map cached describes the graph as it was a moment ago.
        let cached: Vec<ClusterTarget> = self.clusterings.keys().copied().collect();
        self.stale.extend(cached);
        // And so does the frame they are placed in.
        self.frame_stale = true;
        // A new graph is a new question, so whatever failed against the old one
        // is worth asking again.
        self.clustering_failed.clear();
    }

    /// Bring the cached maps in step with what the workspaces are asking for:
    /// start a recompute for one that is missing or stale, drop the ones no tab
    /// points at any more.
    ///
    /// Doing it here, once per dispatch and once per tick, is what lets
    /// [`Self::clustering_for`] be a lookup on `&self` — and so lets a snapshot,
    /// which only ever holds `&CoreApp`, draw a map at all.
    ///
    /// Nothing starts while the project is still arriving. Coarsening is not
    /// incremental — a run over a graph that gained a thousand functions is a
    /// different ladder, not the same one extended — so a map computed mid-load
    /// is a map that will be thrown away, after moving every box under whoever
    /// was reading it. One run when the graph settles, and then one per action
    /// that changes it, is both cheaper and the only version that holds still.
    fn sync_clusterings(&mut self) {
        // Each tab's own map, and the whole-program map for its method
        // alongside it. The second is not speculative: it is what a new tab
        // opens on and what widening a scope goes back to, so evicting it
        // because one tab happens to be scoped right now means that gesture
        // blanks the canvas and waits for a worker. There is one per method,
        // and it is the thing most likely to be wanted next.
        let wanted: HashSet<ClusterTarget> = self
            .workspaces
            .workspaces
            .iter()
            .flat_map(|ws| {
                let target = ClusterTarget::of(ws);
                [target, target.unscoped()]
            })
            .collect();

        self.clusterings.retain(|target, _| wanted.contains(target));
        self.stale.retain(|target| wanted.contains(target));
        self.clustering_failed
            .retain(|target| wanted.contains(target));

        // One job at a time: the recompute *is* the expensive thing, so a
        // second alongside it would compete for a core rather than add one.
        if self.clustering_job.is_some() || self.loading() {
            return;
        }

        let mut owed: Vec<ClusterTarget> = wanted
            .into_iter()
            .filter(|target| self.stale.contains(target) || !self.clusterings.contains_key(target))
            .filter(|target| !self.clustering_failed.contains(target))
            .collect();
        owed.sort_unstable_by_key(ClusterTarget::order);

        // Whichever targets are left waiting keep last time's grouping, which
        // is stale but drawable — better than a tab that blanks while it waits
        // its turn.
        if let Some(&target) = owed.first() {
            self.spawn_clustering(target);
        }
    }

    /// Whether every map a workspace is asking for has been computed against
    /// the graph as it stands.
    #[cfg(test)]
    fn settled(&self) -> bool {
        self.clustering_job.is_none()
            && self.workspaces.workspaces.iter().all(|ws| {
                let target = ClusterTarget::of(ws);
                self.clusterings.contains_key(&target) && !self.stale.contains(&target)
            })
    }

    /// Whether the graph is still arriving from clangd.
    ///
    /// Both halves are needed. `LspProgress(Completed)` says the symbols have
    /// stopped coming, not that the graph has: the edge warmup that follows
    /// adds every call in the project, and a map built from the symbols alone
    /// is a map of isolated functions — which `logic::cluster` drops, leaving
    /// nothing to draw. `NotStarted` is demo mode, where the graph handed to
    /// `CoreApp::new` is the whole of it and nothing is pending at all.
    fn loading(&self) -> bool {
        let phase_done = matches!(
            self.lsp.status(),
            LspLoadPhase::NotStarted | LspLoadPhase::Completed | LspLoadPhase::Failed(_)
        );
        !phase_done || self.warming_up()
    }

    /// Whether the initial edge warmup is still running.
    fn warming_up(&self) -> bool {
        self.warmup_seeded && (!self.edge_warmup.is_empty() || self.lsp.pending_count() > 0)
    }

    /// Add a function that came from a symbol query, and queue its edges.
    ///
    /// The queue used to be a snapshot taken the instant clangd reported
    /// `Completed`, which is not the instant symbols stop arriving. The loader
    /// gives up waiting for its discovery query after `INITIAL_SYMBOL_TIMEOUT`
    /// and a late reply is forwarded here instead; `Command::Refresh` brings a
    /// fresh set whenever the user asks for one. Either way those symbols
    /// landed in the graph after the one sweep that would have resolved them,
    /// so they sat there with no edges and no way to ever get any — isolated,
    /// which is precisely what `logic::cluster` drops.
    ///
    /// Only symbol queries come through here. A callee discovered by a
    /// call-hierarchy reply is added straight to the graph, because queueing
    /// those would cascade the warmup through the whole program — which is why
    /// a loaded project holds far more functions than the warmup ever claims.
    fn ingest_symbol(&mut self, function: FunctionNode) -> SymbolId {
        let known = self.call_graph.get_function(&function.id).is_some();
        let id = self.call_graph.add_function(function);
        // Before the queue opens the seeding sweep takes everything in the
        // graph, so queueing here as well would ask about it twice.
        if self.warmup_seeded && !known {
            self.edge_warmup.push_back(id);
            self.warmup_total += 1;
        }
        id
    }

    /// Put the symbols of discarded in-flight requests back on the warmup
    /// queue.
    ///
    /// A cleared request is work that was started and will never finish: the
    /// reply is dropped as unrecognised and the symbol's loading state goes
    /// with it, so it sits in the graph with no edges, reading `NotLoaded`,
    /// with nothing on its way. Nothing else picks it up — `ingest_symbol`
    /// queues a symbol only the first time the graph sees it, and the refresh
    /// that caused this re-adds the very same functions — so before this the
    /// refresh stranded up to `WARMUP_IN_FLIGHT` symbols permanently, and an
    /// edgeless symbol is what `logic::cluster` drops, so they went missing
    /// from the map rather than showing up wrong.
    ///
    /// Every discarded request that names a symbol is requeued, whoever asked
    /// for it. The queue's question is "does this function still need its
    /// edges fetched", and a cleared request leaves the answer at yes
    /// regardless of which question was outstanding or who wanted it. The
    /// asymmetry decides it: requeuing a [`RequestOrigin::User`] request costs
    /// one round trip and answers it as `Warmup`, so its edges land without
    /// the redraw the user's own answer would have carried — and the user's
    /// affordance survives that, because the cleared loading state puts the
    /// node back to `NotLoaded` and its `+` asks again on the next click, in
    /// the direction the tab is actually reading. Dropping it instead loses
    /// the edges with no second chance at all.
    fn requeue_discarded(&mut self, discarded: Vec<PendingRequest>) {
        // Nothing to put work back on yet. The seeding sweep in
        // `pump_edge_warmup` *replaces* the queue with every function in the
        // graph — which is where these symbols are — so queueing here would be
        // counted and then thrown away by that assignment.
        if !self.warmup_seeded {
            return;
        }

        let mut requeued = 0usize;
        for pending in discarded {
            // A workspace-symbol sweep names no symbol, and the refresh is
            // sending a fresh one of those anyway.
            let Some(id) = pending.symbol_id else {
                continue;
            };
            // Two requests can name one function — a find-references asked for
            // while the warmup's prepare was still out — and a symbol still
            // waiting its turn is on the queue already. Either way the queue
            // holds places, and a second place is a second request and a
            // fraction that never reaches its total.
            if self.edge_warmup.contains(&id) {
                continue;
            }
            self.edge_warmup.push_back(id);
            requeued += 1;
        }
        if requeued == 0 {
            return;
        }

        // `done` is `warmup_total - edge_warmup.len()`, so the total has to
        // stay at least as large as the queue or the fraction saturates at a
        // zero it never leaves. A symbol counted when it was first queued must
        // not be counted twice — its place is coming back, not being added —
        // and that is all of them bar one case: a function the warmup never
        // queued at all, reached as some reply's callee and then expanded by
        // hand. Raising the total to the queue's length covers that without
        // keeping a second record of which symbols have been counted.
        self.warmup_total = self.warmup_total.max(self.edge_warmup.len());
        log::info!(
            "CoreApp: requeued {} in-flight symbol(s) discarded by a refresh, {} queued of {}",
            requeued,
            self.edge_warmup.len(),
            self.warmup_total
        );
    }

    /// Fetch call hierarchies for the symbols the initial load handed us, so the
    /// graph has edges before anything tries to cluster or draw it.
    ///
    /// Reuses the same request path as expanding a node by hand — prepare, then
    /// follow up for calls — so every response lands in the handlers that
    /// already know how to turn it into edges.
    fn pump_edge_warmup(&mut self) -> Vec<AppEvent> {
        // Open the queue once the symbol stream is done and the bridge is
        // live: before that there is nothing to send on. Everything already in
        // the graph goes in here; everything arriving after queues itself
        // through `ingest_symbol`.
        if !self.warmup_seeded
            && matches!(self.lsp.status(), LspLoadPhase::Completed)
            && self.lsp.has_worker()
        {
            self.warmup_seeded = true;
            self.edge_warmup = self.call_graph.function_ids().copied().collect();
            self.warmup_total = self.edge_warmup.len();
            self.warmup_last_moved = Some(Instant::now());
            self.warmup_started_at = Some(Instant::now());
            log::info!(
                "CoreApp: warming up call hierarchy for {} symbol(s), {} in flight, {} node(s) in the graph",
                self.edge_warmup.len(),
                self.lsp.pending_count(),
                self.call_graph.node_count()
            );
        }

        let mut events = Vec::new();
        let mut sent = 0usize;
        let mut skipped = 0usize;
        while self.lsp.pending_count() < WARMUP_IN_FLIGHT {
            let Some(id) = self.edge_warmup.pop_front() else {
                break;
            };
            // The user may have expanded this one while the queue drained.
            if self.lsp.is_function_loaded(&id) || self.lsp.is_function_loading(&id) {
                skipped += 1;
                continue;
            }
            self.lsp.set_loading_state(id, LoadingState::Loading);
            sent += 1;
            // Warmup, not user: nobody is watching for this one symbol's edges,
            // so its reply must not redraw the client. See `RequestOrigin`.
            let outcome = self.lsp.send_call_hierarchy(
                &self.call_graph,
                &id,
                RequestOrigin::Warmup,
                // Outgoing, fixed, and deliberately not the current tab's
                // direction: no tab is waiting on this one. The warmup walks
                // every symbol in the project, and every edge in the project
                // has a project symbol at its caller end, so asking each of
                // them who it calls reaches exactly the same edge set that
                // asking each of them who calls it would — the graph the
                // clustering needs comes out the same either way. Reading the
                // active workspace instead would make the bootstrap depend on
                // which tab happened to be in front, and change direction
                // mid-sweep every time the user hit `t`, for no gain at all.
                CallDirection::Outgoing,
            );
            // A warmup request that goes out is silent by design — see
            // `RequestOrigin` — so this only ever carries a failure.
            events.extend(outcome.event());
        }
        if sent > 0 || skipped > 0 {
            log::debug!(
                "CoreApp: warmup pump sent {} request(s), skipped {} already-loaded, {} queued",
                sent,
                skipped,
                self.edge_warmup.len()
            );
        }

        self.trace_warmup();
        events.extend(self.report_warmup_progress());
        events
    }

    /// Write the warmup's state to the log: how far it has got, what it is
    /// waiting on, and — the point of the whole thing — how long the fraction
    /// has been standing still.
    ///
    /// The warmup only sends while it is under `WARMUP_IN_FLIGHT`, so a
    /// fraction that stops moving means the requests listed here are the ones
    /// nothing has answered. Logging the ages alongside them is what separates
    /// "clangd is slow" from "clangd will never reply to this one".
    fn trace_warmup(&mut self) {
        if !self.warmup_seeded || self.warmup_total == 0 {
            return;
        }

        let done = self.warmup_total.saturating_sub(self.edge_warmup.len());
        if done != self.warmup_last_done {
            self.warmup_last_done = done;
            self.warmup_last_moved = Some(Instant::now());
        }
        if !self.warming_up() {
            return;
        }

        let due = self
            .warmup_last_logged
            .is_none_or(|at| at.elapsed() >= WARMUP_LOG_INTERVAL);
        if !due {
            return;
        }
        self.warmup_last_logged = Some(Instant::now());

        let still = self
            .warmup_last_moved
            .map(|at| at.elapsed())
            .unwrap_or_default();
        let summary = format!(
            "CoreApp: edge warmup {}/{} — {} queued, {}/{} in flight, unchanged for {:.1}s; waiting on: {}",
            done,
            self.warmup_total,
            self.edge_warmup.len(),
            self.lsp.pending_count(),
            WARMUP_IN_FLIGHT,
            still.as_secs_f32(),
            self.lsp.pending_summary()
        );
        if still >= WARMUP_STALL_AFTER {
            log::warn!("{summary}");
        } else {
            log::info!("{summary}");
        }
    }

    /// Report how far the edge warmup has got, so a frontend can say the graph
    /// is still filling in rather than leaving `LspProgress(Completed)` to
    /// claim it is ready.
    ///
    /// Counts symbols taken off the queue, in-flight ones included: they have
    /// been asked for, and the alternative — waiting for each response — makes
    /// the number lag the work by a whole round trip for no gain.
    fn report_warmup_progress(&mut self) -> Vec<AppEvent> {
        if !self.warmup_seeded || self.warmup_total == 0 {
            return vec![];
        }

        if self.warming_up() {
            self.warmup_finish_reported = false;
            let done = self.warmup_total.saturating_sub(self.edge_warmup.len());
            return vec![AppEvent::ResolvingCalls {
                done,
                total: self.warmup_total,
            }];
        }

        if self.warmup_finish_reported {
            return vec![];
        }
        self.warmup_finish_reported = true;
        log::info!(
            "CoreApp: edge warmup drained, {} symbol(s) resolved in {:.1}s — graph has {} node(s), {} edge(s)",
            self.warmup_total,
            self.warmup_started_at
                .map(|at| at.elapsed().as_secs_f32())
                .unwrap_or_default(),
            self.call_graph.node_count(),
            self.call_graph.edge_count(),
        );
        vec![AppEvent::ResolvingCalls {
            done: self.warmup_total,
            total: self.warmup_total,
        }]
    }

    /// Install a clustering a worker thread has finished, if one is ready.
    ///
    /// Never blocks: a job still running leaves everything as it was and is
    /// asked again next tick.
    fn collect_finished_clustering(&mut self) -> Vec<AppEvent> {
        let Some(job) = self.clustering_job.as_ref() else {
            return vec![];
        };
        let (target, started_at) = (job.target, job.started_at);
        let Clustered { clustering, frame } = match job.result.try_recv() {
            Ok(done) => done,
            Err(mpsc::TryRecvError::Empty) => return vec![],
            Err(mpsc::TryRecvError::Disconnected) => {
                // The worker went away without answering, which means it
                // panicked. Recorded rather than retried: a panic that is
                // deterministic would spin here forever, and `mark_graph_changed`
                // is what lets it be asked again.
                log::error!("CoreApp: clustering worker for {target:?} died without a result");
                self.clustering_job = None;
                self.clustering_failed.insert(target);
                return vec![];
            }
        };
        self.clustering_job = None;
        if let Some(frame) = frame {
            log::info!(
                "CoreApp: map frame seats {} of {} function(s)",
                frame.len(),
                self.call_graph.node_count(),
            );
            self.map_frame = frame;
        }

        let clusters = clustering.top_level().len();
        log::info!(
            "CoreApp: clustered {:?} over {} node(s) in {:.1}s: {} cluster(s) at the top of {} rung(s)",
            target,
            self.call_graph.node_count(),
            started_at.elapsed().as_secs_f32(),
            clusters,
            clustering.level_count(),
        );
        self.clusterings.insert(target, clustering);

        // Announced whoever asked for it. A map nobody is looking at right now
        // still belongs to a tab that will be, and the event is what every
        // out-of-process frontend turns into the snapshot carrying it.
        vec![AppEvent::ClusteringUpdated { clusters }]
    }

    /// Hand a clustering off to a worker thread.
    ///
    /// The graph is copied rather than shared. Coarsening wants it for seconds,
    /// so an `Arc` would mean either holding a lock across all of that or making
    /// every `&mut CallGraph` in the app copy-on-write. At 10,000 functions the
    /// copy is a few milliseconds — which is the whole point, because this much
    /// *does* run with the app's lock held.
    fn spawn_clustering(&mut self, target: ClusterTarget) {
        let graph = self.call_graph.clone();
        // The frame rides along with whichever job goes out first after the
        // graph moved, and only that one: it is per graph, not per target, so
        // the jobs behind this one would each recompute the same picture.
        let frame_wanted = self.frame_stale || self.map_frame.is_empty();
        self.frame_stale = false;
        // Marked clean here rather than when the result lands. The job computes
        // against a copy of the graph as it stands at this instant, so anything
        // arriving while it runs is work the job will not cover; clearing the
        // mark on completion would swallow exactly those changes. Clearing it
        // now means `mark_graph_changed` sets it again and the next sync picks
        // them up — one generation late, never lost.
        self.stale.remove(&target);

        let (tx, result) = mpsc::channel();
        std::thread::spawn(move || {
            let clustering = match target.scope {
                None => Clustering::compute(&graph, target.settings),
                Some((scope, direction)) => {
                    cluster_within(&graph, scope, direction, target.settings)
                }
            };
            let frame = frame_wanted.then(|| compute_map_frame(&graph));
            // A gone receiver means the app was dropped while this ran, which
            // is an ordinary shutdown and not worth a word.
            let _ = tx.send(Clustered { clustering, frame });
        });

        log::info!(
            "CoreApp: clustering {:?} over {} node(s) on a worker thread",
            target,
            self.call_graph.node_count()
        );
        self.clustering_job = Some(ClusteringJob {
            target,
            result,
            started_at: Instant::now(),
        });
    }

    /// Add one entry of a call-hierarchy reply: the function on the other end,
    /// and an edge per call site.
    ///
    /// Marking the graph changed is the caller's job, once for the whole reply.
    /// A reply carrying fifty callees is one change to the graph, not fifty,
    /// and doing it here made the warmup rebuild the stale set per call in the
    /// project.
    fn process_call_entry(
        &mut self,
        symbol_id: SymbolId,
        item: &lsp_types::CallHierarchyItem,
        from_ranges: &[lsp_types::Range],
        is_outgoing: bool,
    ) {
        let other_name = item.name.clone();
        let other_detail = item.detail.as_deref();
        let other_uri = &item.uri;
        let other_range = item.range;
        let file_path = other_uri.path().to_string();
        let location = Location::new(
            file_path.clone(),
            other_range.start.line + 1,
            other_range.start.character + 1,
        );
        // clangd's `detail` on a call-hierarchy item is the symbol's own
        // qualified name — "outer::inner::Widget::draw", not the scope around
        // it — so it is the qualified name, not a prefix for one. Building
        // "…::draw::draw" here gave the same function a different name from
        // the one the workspace-symbol path produced, and since `SymbolId`
        // hashes the qualified name it arrived in the graph as a second node:
        // one function, listed twice in the sidebar.
        let qualified_name = match other_detail {
            Some(detail) if !detail.is_empty() => detail.to_string(),
            _ => other_name.clone(),
        };
        let other_id =
            self.call_graph
                .add_function(FunctionNode::new(other_name, qualified_name, location));
        for from_range in from_ranges {
            let call_location = Location::new(
                file_path.clone(),
                from_range.start.line + 1,
                from_range.start.character + 1,
            );
            let (caller, callee) = if is_outgoing {
                (symbol_id, other_id)
            } else {
                (other_id, symbol_id)
            };
            self.call_graph.add_call(caller, callee, call_location);
        }
    }
}

/// Cluster everything `scope` reaches, reading the graph the way the workspace
/// that asked for it does.
///
/// A free function rather than a method because a worker thread is what mostly
/// runs it, and a thread cannot borrow the app.
fn cluster_within(
    graph: &CallGraph,
    scope: SymbolId,
    direction: CallDirection,
    settings: ClusterSettings,
) -> Clustering {
    let traversal = GraphTraversal::new(graph);
    // Unbounded, unlike the rooted tree's BFS: depth is how much of a tree
    // fits on a canvas, and the map exists precisely to stand in for more
    // than fits. Cutting the neighbourhood at three levels would group the
    // near half of a subsystem and call it the subsystem.
    let reachable: HashSet<SymbolId> = match direction {
        CallDirection::Outgoing => traversal.find_reachable_from(&scope),
        CallDirection::Incoming => traversal.find_can_reach(&scope),
    }
    .into_iter()
    .collect();

    Clustering::compute_within(graph, &reachable, settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::Location;
    use tokio::sync::mpsc;

    /// Wire the bridge to a worker that is listening, and hand back the
    /// requests it sends.
    ///
    /// Both halves go in together, which is what `LspBridge::set_channels`
    /// takes: a bridge that can ask and never hear is a state no loader
    /// produces. Nothing here answers over the wire — these tests hand
    /// responses to `handle_lsp_response` directly — so the reply channel is
    /// left to close behind us.
    fn wire_worker(app: &mut CoreApp) -> mpsc::UnboundedReceiver<lsp::LspRequest> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (_replies_tx, replies_rx) = mpsc::unbounded_channel();
        app.lsp.set_channels(replies_rx, tx);
        rx
    }

    /// Distinct lines keep the generated `SymbolId`s distinct.
    fn add(graph: &mut CallGraph, name: &str, line: u32) -> SymbolId {
        graph.add_function(FunctionNode::new(
            name.to_string(),
            name.to_string(),
            Location::new("test.c".to_string(), line, 0),
        ))
    }

    /// Call sites are deduplicated by location, so each call needs its own line.
    fn call(graph: &mut CallGraph, from: SymbolId, to: SymbolId, line: u32) {
        graph.add_call(from, to, Location::new("test.c".to_string(), line, 0));
    }

    fn triangle(graph: &mut CallGraph, prefix: &str, base_line: u32) -> [SymbolId; 3] {
        let a = add(graph, &format!("{prefix}_a"), base_line);
        let b = add(graph, &format!("{prefix}_b"), base_line + 1);
        let c = add(graph, &format!("{prefix}_c"), base_line + 2);
        call(graph, a, b, base_line + 100);
        call(graph, b, c, base_line + 101);
        call(graph, a, c, base_line + 102);
        [a, b, c]
    }

    /// Two triangles joined by a single call, which Louvain reliably reads as
    /// two communities.
    fn two_triangles() -> (CallGraph, [SymbolId; 3], [SymbolId; 3]) {
        let mut graph = CallGraph::new();
        let left = triangle(&mut graph, "left", 10);
        let right = triangle(&mut graph, "right", 40);
        call(&mut graph, left[2], right[0], 500);
        (graph, left, right)
    }

    /// The key naming the cluster that holds `symbol` — what the cluster
    /// commands take, since they come back from a frontend whose snapshot may
    /// predate a recompute.
    fn key_of(app: &CoreApp, symbol: SymbolId) -> ClusterKey {
        let clustering = active(app);
        let id = clustering.cluster_of(&symbol).expect("clustered");
        clustering.cluster(id).expect("a known id").key
    }

    /// The laid-out nodes of the current snapshot — what the canvas would draw.
    ///
    /// Only the rooted view has a layout at all: on the cluster map the
    /// snapshot ships none, so a caller that has not rooted the workspace is
    /// asking about a canvas that is drawing something else.
    fn placed(app: &CoreApp) -> Vec<SymbolId> {
        match crate::protocol::snapshot_of(app) {
            crate::protocol::ServerMsg::Snapshot { layout, .. } => layout
                .expect("the workspace is showing the rooted view")
                .keys()
                .copied()
                .collect(),
            other => panic!("snapshot_of returned {other:?}"),
        }
    }

    #[test]
    fn expanding_a_node_adds_to_the_graph_without_moving_the_root() {
        let mut graph = CallGraph::new();
        let root = add(&mut graph, "root", 1);
        let mid = add(&mut graph, "mid", 2);
        let leaf = add(&mut graph, "leaf", 3);
        let sibling = add(&mut graph, "sibling", 4);
        call(&mut graph, root, mid, 100);
        call(&mut graph, mid, leaf, 101);
        call(&mut graph, root, sibling, 102);

        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SelectFunction(root));
        app.dispatch(Command::SetGraphDepth(1));

        let before = placed(&app);
        assert!(before.contains(&root) && before.contains(&mid) && before.contains(&sibling));
        assert!(!before.contains(&leaf), "the depth limit stops at `mid`");

        app.dispatch(Command::ExpandFunction(mid));

        let after = placed(&app);
        assert_eq!(
            app.workspaces.current().and_then(|ws| ws.root_symbol()),
            Some(root),
            "expanding does not re-root the workspace"
        );
        for id in &before {
            assert!(after.contains(id), "everything on screen stayed on screen");
        }
        assert!(after.contains(&leaf), "and the fetched callee was appended");
    }

    #[test]
    fn focusing_a_node_moves_the_selection_and_nothing_else() {
        let mut graph = CallGraph::new();
        let root = add(&mut graph, "root", 1);
        let mid = add(&mut graph, "mid", 2);
        let leaf = add(&mut graph, "leaf", 3);
        call(&mut graph, root, mid, 100);
        call(&mut graph, mid, leaf, 101);

        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SelectFunction(root));
        app.dispatch(Command::SetGraphDepth(1));
        let before = placed(&app);

        let events = app.dispatch(Command::FocusFunction(mid));

        assert!(
            events.is_empty(),
            "a click must not cost a snapshot: the client draws the border itself"
        );
        assert_eq!(app.selected_function, Some(mid), "the selection moved");
        assert_eq!(
            app.workspaces.current().and_then(|ws| ws.root_symbol()),
            Some(root),
            "the root did not"
        );
        let after = placed(&app);
        assert_eq!(
            before.len(),
            after.len(),
            "and the graph is the one that was already on screen"
        );
        assert!(
            !after.contains(&leaf),
            "pointing at a node does not load it"
        );
    }

    /// Say the load has finished, which is what lets a recompute start at all.
    fn loaded(app: &mut CoreApp) {
        app.lsp.set_status(LspLoadPhase::Completed);
        app.edge_warmup.clear();
        app.lsp.clear_pending();
    }

    /// Tick until every map the app owes has been computed and installed.
    ///
    /// The recompute runs on a worker thread, so a single `tick()` only starts
    /// one — the result lands on some later tick, and a second target waits its
    /// turn behind the first. This is what the frontends do anyway at 100ms
    /// intervals; it just does it faster, and gives up loudly rather than
    /// hanging if the result never comes.
    fn tick_until_settled(app: &mut CoreApp) -> Vec<AppEvent> {
        let mut events = Vec::new();
        for _ in 0..500 {
            events.extend(app.tick());
            if app.settled() {
                return events;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "clustering never settled: job in flight={}, stale={}, cached={}",
            app.clustering_job.is_some(),
            app.stale.len(),
            app.clusterings.len(),
        );
    }

    fn hierarchy_item(name: &str, line: u32) -> lsp_types::CallHierarchyItem {
        let position = lsp_types::Position { line, character: 0 };
        let range = lsp_types::Range {
            start: position,
            end: position,
        };
        lsp_types::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: lsp_types::Url::parse("file:///test.c").expect("a valid file url"),
            range,
            selection_range: range,
            data: None,
        }
    }

    #[test]
    fn demo_mode_is_clustered_before_anything_ticks() {
        let (graph, left, right) = two_triangles();
        let app = CoreApp::new(graph);

        // Nothing has polled, and in demo mode nothing ever will deliver a
        // batch to say the graph is complete — the map has to be there from the
        // moment the graph is handed over.
        let clustering = active(&app);
        assert!(!clustering.is_empty());
        let left_cluster = clustering.cluster_of(&left[0]).expect("clustered");
        let right_cluster = clustering.cluster_of(&right[0]).expect("clustered");
        assert_ne!(left_cluster, right_cluster);
    }

    #[test]
    fn a_project_still_loading_is_not_clustered_until_it_lands() {
        let mut app = CoreApp::new(CallGraph::new());
        assert!(active(&app).is_empty());

        // Symbols are still arriving. Coarsening is not incremental, so a map
        // built now is one that will be thrown away — after moving every box
        // under whoever was reading it.
        app.lsp
            .set_status(LspLoadPhase::LoadingWorkspaceSymbols { loaded: 3 });
        let (graph, left, _) = two_triangles();
        app.call_graph = graph;
        app.mark_graph_changed();

        let events = app.tick();
        assert!(
            app.clustering_job.is_none(),
            "a project still arriving must not be clustered"
        );
        assert!(active(&app).is_empty(), "and nothing has been drawn");
        assert!(!events
            .iter()
            .any(|e| matches!(e, AppEvent::ClusteringUpdated { .. })));

        // clangd says there is nothing more coming, and the warmup has drained.
        loaded(&mut app);
        let mut events = app.tick();
        assert!(
            app.clustering_job.is_some(),
            "the recompute goes out the moment the graph stops moving"
        );

        events.extend(tick_until_settled(&mut app));
        assert!(active(&app).cluster_of(&left[0]).is_some());
        let reported = events.iter().find_map(|e| match e {
            AppEvent::ClusteringUpdated { clusters } => Some(*clusters),
            _ => None,
        });
        assert_eq!(reported, Some(active(&app).top_level().len()));

        // Nothing changed since, so the next tick has nothing to say.
        assert!(!app
            .tick()
            .iter()
            .any(|e| matches!(e, AppEvent::ClusteringUpdated { .. })));
    }

    #[test]
    fn a_graph_that_grows_mid_recompute_is_clustered_again_rather_than_lost() {
        // The worker holds a copy of the graph as it stood when it started, so
        // anything arriving while it runs is invisible to it. Clearing the
        // staleness at spawn rather than on completion is what makes the next
        // sync pick those up instead of marking them already handled.
        let mut app = CoreApp::new(CallGraph::new());
        let (graph, left, _) = two_triangles();
        app.call_graph = graph;
        app.mark_graph_changed();
        loaded(&mut app);

        app.tick();
        assert!(
            app.clustering_job.is_some(),
            "the recompute went out to a worker"
        );
        let target = ClusterTarget::of(app.workspaces.current().expect("a workspace"));
        assert!(
            !app.stale.contains(&target),
            "and the graph it covers is no longer owed"
        );

        // A batch lands while that worker is still going.
        let extra = add(&mut app.call_graph, "extra", 900);
        call(&mut app.call_graph, left[0], extra, 901);
        app.mark_graph_changed();
        assert!(
            app.stale.contains(&target),
            "which puts the cache straight back in debt"
        );

        tick_until_settled(&mut app);
        assert!(
            active(&app).cluster_of(&extra).is_some(),
            "the node that arrived mid-flight is on the map"
        );
    }

    #[test]
    fn a_recompute_in_flight_does_not_hold_up_a_command() {
        // Not a timing claim — `dispatch` never touches the job slot at all,
        // which is the property that matters: there is no path from a command
        // to waiting on the clustering thread.
        let mut app = CoreApp::new(CallGraph::new());
        let (graph, left, _) = two_triangles();
        app.call_graph = graph;
        app.mark_graph_changed();
        loaded(&mut app);
        app.tick();
        assert!(app.clustering_job.is_some());

        let events = app.dispatch(Command::SelectFunction(left[0]));

        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));
        assert_eq!(app.selected_function, Some(left[0]));
        assert!(
            app.clustering_job.is_some(),
            "the command came and went without collecting or joining the job"
        );
    }

    #[test]
    fn a_map_built_to_other_settings_is_not_this_tab_s_map() {
        // The cache is keyed by the whole of `ClusterSettings`, not by the
        // method: the same method at another grain is a different ladder, and
        // handing one over because the algorithm matched would draw a map the
        // user's knobs do not describe.
        let mut app = CoreApp::new(crate::demo::create_demo_call_graph());
        tick_until_settled(&mut app);
        let before = ClusterTarget::of(app.workspaces.current().expect("a workspace"));
        assert!(
            app.clusterings.contains_key(&before),
            "demo mode clusters up front"
        );

        let settings = app
            .workspaces
            .current()
            .expect("a workspace")
            .cluster_settings()
            .with_grain(2.0);
        let events = app.dispatch(Command::SetClusterSettings(settings));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));

        let ws = app.workspaces.current().expect("a workspace");
        let after = ClusterTarget::of(ws);
        assert_ne!(before, after, "a moved knob is a different question");
        assert!(
            !app.clusterings.contains_key(&before),
            "and the answer to the old question is dropped, since no tab asks it any more"
        );
        assert!(
            app.clustering_for(ws).is_empty(),
            "a tab whose settings have no map yet reads as not ready"
        );

        // It arrives on its own, and it is the ladder the new γ builds.
        tick_until_settled(&mut app);
        let ws = app.workspaces.current().expect("a workspace");
        assert_eq!(app.clustering_for(ws).settings(), settings);
        assert_eq!(
            ws.map_level(),
            None,
            "and the zoom went back to wherever the new ladder opens"
        );
    }

    #[test]
    fn a_clustering_worker_that_dies_is_not_sent_straight_back_out() {
        // A panic in the clustering is a function of the graph, so retrying it
        // against the same graph panics again — and since what is owed is
        // worked out afresh every tick from the cache being empty, one
        // repeatable panic would mean a new thread ten times a second for as
        // long as the app runs.
        let (graph, left, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(left[0])));
        let target = ClusterTarget::of(app.workspaces.current().expect("a workspace"));
        assert!(
            app.clusterings.contains_key(&target),
            "the tab has a scoped map"
        );
        assert!(app.clustering_job.is_none());

        // That scope's worker dies without answering.
        app.stale.insert(target);
        app.clustering_failed.insert(target);

        app.sync_clusterings();
        assert!(
            app.clustering_job.is_none(),
            "the target that just killed a worker is not handed straight to another"
        );

        // A changed graph is a different question, so it is worth asking again.
        app.mark_graph_changed();
        assert!(app.clustering_failed.is_empty());
        app.sync_clusterings();
        assert!(
            app.clustering_job.is_some(),
            "and the next graph gets a fresh attempt"
        );
    }

    #[test]
    fn a_scope_whose_clustering_is_still_coming_draws_nothing_not_everything() {
        // Turning a scoped tab around asks for a pairing nothing has computed:
        // the cache is keyed by (scope, direction, method), so the new
        // direction is a target with no entry and `sync_clusterings` starts it
        // on a worker. Until it lands there is no map for this tab, and the
        // whole program's is not a stand-in for one — the breadcrumb still
        // says these are the communities under one function.
        // `left[2]` has both callers and callees, so its neighbourhood is
        // non-empty whichever way the arrows are read — `left[0]` is a source,
        // and turning it around leaves one isolated node and so, correctly, no
        // communities at all.
        let (graph, left, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(left[2])));
        let ws = app.workspaces.current().expect("a workspace");
        assert!(
            !app.clustering_for(ws).is_empty(),
            "the scope the user asked for is computed before the command returns"
        );

        app.dispatch(Command::ToggleCallDirection);

        let ws = app.workspaces.current().expect("a workspace");
        let whole = app
            .clusterings
            .get(&ClusterTarget {
                scope: None,
                settings: ClusterSettings::default(),
            })
            .expect("demo mode clustered the whole graph up front");
        assert!(
            !whole.is_empty(),
            "the whole program's map is sitting right there to be mistakenly handed over"
        );
        assert!(
            app.clustering_for(ws).is_empty(),
            "a scoped tab reads as not ready rather than being handed the whole program"
        );

        // And the real one arrives without anyone having blocked on it.
        tick_until_settled(&mut app);
        let ws = app.workspaces.current().expect("a workspace");
        assert!(
            !app.clustering_for(ws).is_empty(),
            "the turned-around map lands under its own steam"
        );
    }

    /// A `CoreApp` whose bridge looks like a finished load: symbols in the
    /// graph, status `Completed`, and a live request channel. Returns the
    /// receiving end so a test can see what the warmup sent.
    fn loaded_app(symbols: usize) -> (CoreApp, mpsc::UnboundedReceiver<lsp::LspRequest>) {
        let mut graph = CallGraph::new();
        for i in 0..symbols {
            // Absolute: `Url::from_file_path` rejects a relative one, and every
            // location clangd hands back is absolute.
            graph.add_function(FunctionNode::new(
                format!("f{i}"),
                format!("f{i}"),
                Location::new("/src/test.c".to_string(), i as u32 + 1, 1),
            ));
        }
        let mut app = CoreApp::new(graph);
        let rx = wire_worker(&mut app);
        app.lsp.set_status(LspLoadPhase::Completed);
        (app, rx)
    }

    #[test]
    fn a_finished_load_asks_for_the_edges_its_symbols_arrived_without() {
        // clangd's workspace symbols carry no edges, so without this the graph
        // is a heap of isolated nodes and the cluster map has nothing to draw.
        let (mut app, mut rx) = loaded_app(3);
        assert_eq!(app.call_graph.edge_count(), 0);

        app.tick();

        assert!(app.warmup_seeded);
        let mut sent = 0;
        while let Ok(req) = rx.try_recv() {
            assert!(matches!(req, lsp::LspRequest::PrepareCallHierarchy { .. }));
            sent += 1;
        }
        assert_eq!(sent, 3, "every loaded symbol is asked about");
    }

    #[test]
    fn the_warmup_keeps_a_bounded_number_of_requests_in_flight() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 3);

        app.tick();

        let first_pass = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(first_pass, WARMUP_IN_FLIGHT, "clangd is not flooded");
        assert!(!app.edge_warmup.is_empty(), "the rest waits its turn");

        // Answering them frees the slots for the next batch.
        app.lsp.clear_pending();
        app.tick();
        assert_eq!(
            std::iter::from_fn(|| rx.try_recv().ok()).count(),
            WARMUP_IN_FLIGHT
        );
    }

    /// The symbol stream does not stop when clangd says `Completed`.
    ///
    /// The loader gives up waiting for its discovery query after 30s and the
    /// late reply arrives here instead. Seeding the queue once left those
    /// symbols in the graph with no edges for good: isolated, so dropped by
    /// the clustering, so missing from the map that is the first thing the
    /// app shows.
    #[test]
    fn a_symbol_that_lands_after_the_queue_opens_is_asked_about_too() {
        let (mut app, mut rx) = loaded_app(1);
        app.tick();
        assert_eq!(
            std::iter::from_fn(|| rx.try_recv().ok()).count(),
            1,
            "the symbol the load started with"
        );
        assert_eq!(app.warmup_total, 1);

        // A discovery reply landing after the loader stopped waiting for it.
        app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "late".to_string(),
            symbols: vec![FunctionNode::new(
                "late".to_string(),
                "late".to_string(),
                Location::new("/src/late.c".to_string(), 1, 1),
            )],
        });

        assert_eq!(app.warmup_total, 2, "the work grew, and says so");
        app.lsp.clear_pending();
        app.tick();
        assert_eq!(
            std::iter::from_fn(|| rx.try_recv().ok()).count(),
            1,
            "the late symbol is asked about as well"
        );
        assert!(app.edge_warmup.is_empty());
    }

    /// clangd answers overlapping symbol queries with overlapping results, so
    /// the same function arrives repeatedly. Queueing it each time would inflate
    /// the total the status bar is reporting against.
    #[test]
    fn a_symbol_that_arrives_again_is_not_queued_again() {
        let (mut app, _rx) = loaded_app(1);
        app.tick();
        assert_eq!(app.warmup_total, 1);

        let repeat = FunctionNode::new(
            "f0".to_string(),
            "f0".to_string(),
            Location::new("/src/test.c".to_string(), 1, 1),
        );
        for _ in 0..3 {
            app.handle_lsp_response(LspResponse::WorkspaceSymbols {
                request_id: "again".to_string(),
                symbols: vec![repeat.clone()],
            });
        }

        assert_eq!(app.warmup_total, 1, "one symbol, one place in the queue");
        assert!(app.edge_warmup.is_empty());
    }

    /// The warmup resolves the symbols clangd listed. It must not follow what
    /// their replies turn up: that is the whole program, one round trip at a
    /// time.
    #[test]
    fn the_callees_a_reply_discovers_are_not_queued() {
        let (mut app, _rx) = loaded_app(1);
        app.tick();
        assert!(app.edge_warmup.is_empty(), "the one symbol went out");
        let caller = *app
            .call_graph
            .function_ids()
            .next()
            .expect("the loaded symbol");

        app.process_call_entry(
            caller,
            &call_item("callee", Some("callee"), "/src/other.c", 7),
            &[],
            true,
        );

        assert_eq!(
            app.call_graph.node_count(),
            2,
            "the callee joined the graph"
        );
        assert!(
            app.edge_warmup.is_empty(),
            "but the warmup does not chase it"
        );
        assert_eq!(app.warmup_total, 1);
    }

    /// A call-hierarchy item shaped the way clangd actually sends them.
    fn call_item(
        name: &str,
        detail: Option<&str>,
        file: &str,
        line: u32,
    ) -> lsp_types::CallHierarchyItem {
        let pos = lsp_types::Position {
            line: line - 1,
            character: 5,
        };
        let range = lsp_types::Range {
            start: pos,
            end: pos,
        };
        lsp_types::CallHierarchyItem {
            name: name.to_string(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: detail.map(|d| d.to_string()),
            uri: lsp_types::Url::from_file_path(file).expect("absolute path"),
            range,
            selection_range: range,
            data: None,
        }
    }

    /// The two ways a function reaches the graph must agree on its name.
    ///
    /// Workspace symbols arrive with a container; call-hierarchy items arrive
    /// with clangd's `detail`, which is the symbol's *own* qualified name. When
    /// those disagreed the same function got two `SymbolId`s, so it sat in the
    /// graph twice and the sidebar listed it twice.
    #[test]
    fn a_function_found_both_ways_is_one_node() {
        for (name, container, detail) in [
            // A free function: clangd sends an empty container for it.
            ("globalFn", "", "globalFn"),
            // A method: the container is the scope, the detail is scope + name.
            ("draw", "outer::inner::Widget", "outer::inner::Widget::draw"),
        ] {
            let mut app = CoreApp::new(CallGraph::new());

            // Path 1 — the workspace-symbol load.
            let qualified = lsp::make_qualified_name(&Some(container.to_string()), name);
            let caller = app.call_graph.add_function(FunctionNode::new(
                "caller".to_string(),
                "caller".to_string(),
                Location::new("/src/main.cpp".to_string(), 1, 1),
            ));
            app.call_graph.add_function(FunctionNode::new(
                name.to_string(),
                qualified,
                Location::new("/src/main.cpp".to_string(), 7, 6),
            ));
            assert_eq!(app.call_graph.node_count(), 2);

            // Path 2 — the same function, reached as somebody's callee.
            app.process_call_entry(
                caller,
                &call_item(name, Some(detail), "/src/main.cpp", 7),
                &[],
                true,
            );

            assert_eq!(
                app.call_graph.node_count(),
                2,
                "`{name}` was found twice and must still be one node"
            );
        }
    }

    #[test]
    fn the_warmup_reports_progress_so_completed_does_not_read_as_ready() {
        // `LspProgress(Completed)` fires the moment clangd stops handing over
        // symbols, which on a large project is long before the graph is worth
        // looking at. Without this the UI announces "ready" over a canvas that
        // then grows edges for another minute.
        let (mut app, _rx) = loaded_app(WARMUP_IN_FLIGHT * 2);

        let events = app.tick();
        let progress = events
            .iter()
            .find_map(|e| match e {
                AppEvent::ResolvingCalls { done, total } => Some((*done, *total)),
                _ => None,
            })
            .expect("the warmup reports itself");
        assert_eq!(progress, (WARMUP_IN_FLIGHT, WARMUP_IN_FLIGHT * 2));

        // Drained: the end is announced exactly once, so a frontend can put the
        // indicator away without every later tick re-announcing it.
        app.edge_warmup.clear();
        app.lsp.clear_pending();

        let done_events: Vec<_> = app
            .tick()
            .into_iter()
            .filter_map(|e| match e {
                AppEvent::ResolvingCalls { done, total } => Some((done, total)),
                _ => None,
            })
            .collect();
        let all = WARMUP_IN_FLIGHT * 2;
        assert_eq!(done_events, vec![(all, all)], "done == total marks the end");
        assert!(
            !app.tick()
                .iter()
                .any(|e| matches!(e, AppEvent::ResolvingCalls { .. })),
            "and not repeated forever after"
        );
    }

    #[test]
    fn demo_mode_reports_no_warmup_progress() {
        // Nothing is loading, so nothing may claim to be.
        let mut app = CoreApp::new(crate::demo::create_demo_call_graph());
        assert!(app.warmup_progress().is_none());
        assert!(!app
            .tick()
            .iter()
            .any(|e| matches!(e, AppEvent::ResolvingCalls { .. })));
    }

    #[test]
    fn reclustering_holds_off_until_the_warmup_drains() {
        // `Completed` means the *symbols* have stopped arriving. The warmup
        // makes that not the end of the wait: the graph keeps growing on its
        // own as every function's edges land, and a map built from symbols
        // alone is a map of isolated functions, which the clustering drops.
        let (mut app, _rx) = loaded_app(2);
        app.tick();
        assert!(app.warming_up());
        assert!(app.loading(), "a warmup still running is still loading");

        let target = ClusterTarget::of(app.workspaces.current().expect("a workspace"));
        app.stale.insert(target);
        app.sync_clusterings();
        assert!(
            app.clustering_job.is_none(),
            "no recompute while edges are still arriving"
        );
        assert!(app.stale.contains(&target), "and the work is still owed");

        // Drained: the graph has stopped moving.
        app.edge_warmup.clear();
        app.lsp.clear_pending();
        assert!(!app.warming_up());
        assert!(!app.loading());
        app.sync_clusterings();
        assert!(
            app.clustering_job.is_some(),
            "and the map goes out the moment it has"
        );
    }

    /// A request that could not be sent must not read as one still in flight.
    ///
    /// `warming_up` counts `pending_requests`, so an entry left behind by a
    /// failed send holds `loading()` true and stops `sync_clusterings` starting
    /// any clustering at all — the cluster map stays blank and the status bar
    /// stays on "Resolving calls" until the entry times out 30s later. See
    /// `LspBridge::dispatch`.
    #[test]
    fn a_send_that_fails_does_not_stall_the_warmup() {
        let (mut app, rx) = loaded_app(2);
        // The LSP worker is gone; every send from here on fails.
        drop(rx);

        let events = app.tick();

        assert!(app.warmup_seeded, "the warmup opened its queue");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AppEvent::FailedToSendRequest)),
            "and said so"
        );
        assert!(app.edge_warmup.is_empty(), "every symbol was taken off");
        assert!(
            app.lsp.pending_count() == 0,
            "and nothing is waiting for an answer that was never asked for"
        );
        assert!(!app.warming_up());
        assert!(!app.loading(), "so a map can be computed straight away");

        let target = ClusterTarget::of(app.workspaces.current().expect("a workspace"));
        app.stale.insert(target);
        app.sync_clusterings();
        assert!(app.clustering_job.is_some());
    }

    /// The lines the prepare requests waiting on `rx` name, 1-based as the
    /// definition locations are. `loaded_app` puts `f{i}` on line `i + 1`, so
    /// this is how a test says *which* symbols the warmup just asked about.
    fn prepared_lines(rx: &mut mpsc::UnboundedReceiver<lsp::LspRequest>) -> Vec<u32> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|req| match req {
                lsp::LspRequest::PrepareCallHierarchy { position, .. } => Some(position.line + 1),
                _ => None,
            })
            .collect()
    }

    /// Where a symbol is defined, to match against [`prepared_lines`].
    fn line_of(app: &CoreApp, id: &SymbolId) -> u32 {
        app.call_graph
            .get_function(id)
            .expect("in the graph")
            .definition_location
            .line
    }

    /// A refresh discards everything in flight, and the symbols those requests
    /// named have to come back as work.
    ///
    /// `LspBridge::clear` drops the pending entries, so when clangd answers
    /// them `take_pending` returns `None` and `handle_lsp_response` throws the
    /// reply away; it drops the loading states too, so each of those symbols
    /// reads `NotLoaded` again with nothing on its way. Nothing requeues them
    /// — the refresh's own symbol sweep re-adds functions already in the graph,
    /// which `ingest_symbol` leaves alone — so up to `WARMUP_IN_FLIGHT` of them
    /// stayed in the graph edgeless for the life of the process, and isolated
    /// is exactly what `logic::cluster` drops from the map.
    #[test]
    fn a_refresh_puts_the_symbols_it_had_in_flight_back_on_the_queue() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 2);
        app.tick();
        let in_flight: Vec<SymbolId> = app.lsp.pending().filter_map(|p| p.symbol_id).collect();
        assert_eq!(
            in_flight.len(),
            WARMUP_IN_FLIGHT,
            "the first batch went out"
        );
        let queued_before = app.edge_warmup.len();
        prepared_lines(&mut rx);

        app.dispatch(Command::Refresh);

        for id in &in_flight {
            assert!(
                app.edge_warmup.contains(id),
                "a symbol whose answer the refresh made undeliverable is asked again"
            );
        }
        assert_eq!(
            app.edge_warmup.len(),
            queued_before + in_flight.len(),
            "and the ones that had not been sent yet are still there, once each"
        );

        // Back on the queue is only half of it: the queue has to get round to
        // them. Answer everything each round so the pump keeps drawing.
        let mut asked: HashSet<u32> = HashSet::new();
        for _ in 0..6 {
            app.lsp.clear_pending();
            app.tick();
            asked.extend(prepared_lines(&mut rx));
            if app.edge_warmup.is_empty() {
                break;
            }
        }
        assert!(app.edge_warmup.is_empty(), "the queue drained");
        for id in &in_flight {
            assert!(
                asked.contains(&line_of(&app, id)),
                "and a fresh request went out for every stranded symbol"
            );
        }
    }

    /// The fraction the status bar shows has to survive the refresh.
    ///
    /// `report_warmup_progress` computes `done` as `warmup_total -
    /// edge_warmup.len()`, so putting work back on the queue without keeping
    /// the total in step reads as a fraction that runs backwards past zero and
    /// saturates there. Symbols that go back on the queue were counted when
    /// they were first queued, so the total stays put and `done` — correctly —
    /// falls: nothing they were asked about was ever answered.
    #[test]
    fn the_warmup_fraction_stays_honest_across_a_refresh() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 2);
        app.tick();
        prepared_lines(&mut rx);
        assert_eq!(
            app.warmup_progress(),
            Some((WARMUP_IN_FLIGHT, WARMUP_IN_FLIGHT * 2))
        );

        app.dispatch(Command::Refresh);

        let (done, total) = app.warmup_progress().expect("still warming up");
        assert_eq!(
            total,
            WARMUP_IN_FLIGHT * 2,
            "the same symbols, not a bigger project"
        );
        assert_eq!(done, 0, "and not one of them has been resolved");
        assert!(
            app.warmup_total >= app.edge_warmup.len(),
            "or `done` underflows to a zero it never leaves"
        );

        // And the end is still reachable, at the count it started with.
        for _ in 0..6 {
            app.lsp.clear_pending();
            app.tick();
            prepared_lines(&mut rx);
            if app.edge_warmup.is_empty() {
                break;
            }
        }
        // The last batch is still out; answering it is what ends the warmup.
        app.lsp.clear_pending();
        assert!(!app.warming_up());
        assert_eq!(app.warmup_total, WARMUP_IN_FLIGHT * 2);
    }

    /// Requeuing must not put a symbol on the queue twice: the queue's length
    /// is the denominator's other half, so a duplicate is a fraction that never
    /// reaches its total and a second request nobody needed.
    ///
    /// Two requests can name one symbol — a find-references the user asked for
    /// while the warmup's prepare for the same function was still out — and the
    /// symbols still waiting their turn are on the queue already.
    #[test]
    fn a_refresh_does_not_queue_a_symbol_twice() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 2);
        app.tick();
        prepared_lines(&mut rx);
        let doubled = *app
            .lsp
            .pending()
            .find_map(|p| p.symbol_id.as_ref())
            .expect("a symbol in flight");
        app.lsp.insert_pending(
            "references-for-the-same-symbol",
            PendingRequest::new(Some(doubled), "references", RequestOrigin::User),
        );

        app.dispatch(Command::Refresh);

        let mut seen = HashSet::new();
        for id in &app.edge_warmup {
            assert!(seen.insert(*id), "{id:?} is queued twice");
        }
        assert_eq!(
            seen.len(),
            WARMUP_IN_FLIGHT * 2,
            "every symbol of the project, once each"
        );
    }

    /// With nothing outstanding there is nothing to put back, and a refresh
    /// that shuffles the queue anyway would re-ask for edges already in hand.
    #[test]
    fn a_refresh_with_nothing_in_flight_leaves_the_queue_alone() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 2);
        app.tick();
        prepared_lines(&mut rx);
        // Every request answered.
        app.lsp.clear_pending();
        let queued: Vec<SymbolId> = app.edge_warmup.iter().copied().collect();
        let total = app.warmup_total;

        app.dispatch(Command::Refresh);

        assert_eq!(
            app.edge_warmup.iter().copied().collect::<Vec<_>>(),
            queued,
            "same symbols, same order"
        );
        assert_eq!(app.warmup_total, total);
    }

    /// A refresh before the queue has ever opened has nowhere to put work, and
    /// needs none: the seeding sweep in `pump_edge_warmup` *replaces* the queue
    /// with every function in the graph, which is where these symbols are.
    /// Queueing them here would be counted, then thrown away by that
    /// assignment.
    #[test]
    fn a_refresh_before_the_warmup_opens_queues_nothing() {
        let (mut app, mut rx) = loaded_app(2);
        let id = *app.call_graph.function_ids().next().expect("a symbol");
        app.lsp.insert_pending(
            "in-flight-before-the-warmup",
            PendingRequest::new(Some(id), "prepare f0", RequestOrigin::User),
        );
        assert!(!app.warmup_seeded);

        app.dispatch(Command::Refresh);

        assert!(app.edge_warmup.is_empty(), "nothing to put it on yet");
        assert_eq!(app.warmup_total, 0);

        // And the sweep that does open the queue still accounts for it once.
        app.tick();
        assert_eq!(app.warmup_total, 2);
        assert_eq!(prepared_lines(&mut rx).len(), 2);
    }

    #[test]
    fn demo_mode_never_warms_up() {
        // No loader, so no `Completed`, so nothing to ask and nobody to ask.
        let (graph, _, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.tick();
        assert!(!app.warmup_seeded);
        assert!(app.edge_warmup.is_empty());
    }

    #[test]
    fn workspace_symbols_land_even_when_we_never_asked_for_them() {
        // The loader issues the initial discovery query under its own request id.
        // When clangd answers after the loader's wait expires, the reply is
        // forwarded to us with an id we have no pending entry for — and it still
        // has to populate the graph, or the frontend comes up empty.
        let mut app = CoreApp::new(CallGraph::new());
        assert_eq!(app.lsp.pending_count(), 0);

        let events = app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "issued-by-the-loader".to_string(),
            symbols: vec![FunctionNode::new(
                "alpha".to_string(),
                "alpha".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });

        assert_eq!(app.call_graph.function_ids().count(), 1);
        assert!(!app.stale.is_empty());
        assert!(events
            .iter()
            .any(|e| matches!(e, AppEvent::GraphSymbolsUpdated { count: 1 })));
    }

    #[test]
    fn every_way_the_graph_grows_marks_the_cache_dirty() {
        let mut app = CoreApp::new(CallGraph::new());
        assert!(app.stale.is_empty());

        app.lsp.insert_pending(
            "symbols",
            PendingRequest::new(None, "workspace symbols", RequestOrigin::User),
        );
        app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "symbols".to_string(),
            symbols: vec![FunctionNode::new(
                "alpha".to_string(),
                "alpha".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });
        assert!(!app.stale.is_empty(), "new symbols are new graph nodes");

        app.stale.clear();
        let caller = *app.call_graph.function_ids().next().expect("one symbol");
        app.lsp.insert_pending(
            "calls",
            PendingRequest::new(Some(caller), "outgoing calls", RequestOrigin::User),
        );
        app.handle_lsp_response(LspResponse::OutgoingCalls {
            request_id: "calls".to_string(),
            calls: vec![lsp_types::CallHierarchyOutgoingCall {
                to: hierarchy_item("beta", 2),
                from_ranges: vec![hierarchy_item("beta", 2).range],
            }],
        });
        assert!(!app.stale.is_empty(), "new calls are new graph edges");
    }

    /// One reply is one change to the graph. The mark is hoisted out of the
    /// per-entry loop — a reply with fifty callees used to rebuild the stale
    /// set fifty times — so what has to be pinned is that it still happens,
    /// and still only when the reply carried something.
    #[test]
    fn a_reply_marks_the_graph_changed_once_it_has_carried_anything() {
        for (calls, expected) in [
            (Vec::new(), false),
            (
                vec!["beta", "gamma", "delta"]
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| lsp_types::CallHierarchyOutgoingCall {
                        to: hierarchy_item(name, i as u32 + 2),
                        from_ranges: vec![hierarchy_item(name, i as u32 + 2).range],
                    })
                    .collect(),
                true,
            ),
        ] {
            for outgoing in [true, false] {
                let mut app = CoreApp::new(CallGraph::new());
                let caller = app.call_graph.add_function(FunctionNode::new(
                    "alpha".to_string(),
                    "alpha".to_string(),
                    Location::new("test.c".to_string(), 1, 0),
                ));
                app.stale.clear();
                app.frame_stale = false;
                app.lsp.insert_pending(
                    "calls",
                    PendingRequest::new(Some(caller), "calls", RequestOrigin::Warmup),
                );

                let response = if outgoing {
                    LspResponse::OutgoingCalls {
                        request_id: "calls".to_string(),
                        calls: calls.clone(),
                    }
                } else {
                    LspResponse::IncomingCalls {
                        request_id: "calls".to_string(),
                        calls: calls
                            .iter()
                            .map(|call| lsp_types::CallHierarchyIncomingCall {
                                from: call.to.clone(),
                                from_ranges: call.from_ranges.clone(),
                            })
                            .collect(),
                    }
                };
                app.handle_lsp_response(response);

                let what = if outgoing { "outgoing" } else { "incoming" };
                assert_eq!(
                    !app.stale.is_empty(),
                    expected,
                    "{what} reply of {} call(s): the cached map",
                    calls.len()
                );
                assert_eq!(
                    app.frame_stale,
                    expected,
                    "{what} reply of {} call(s): the frame",
                    calls.len()
                );
            }
        }
    }

    /// The bootstrap warmup asks for every symbol's edges. Answering each of
    /// those with a redraw is what made loading a real project lag: on jemalloc
    /// it was 767 whole-graph snapshots, one of which changed the screen.
    #[test]
    fn a_warmup_reply_fills_in_edges_without_asking_for_a_redraw() {
        for (origin, wants_redraw) in [(RequestOrigin::Warmup, false), (RequestOrigin::User, true)]
        {
            let mut app = CoreApp::new(CallGraph::new());
            app.handle_lsp_response(LspResponse::WorkspaceSymbols {
                request_id: "symbols".to_string(),
                symbols: vec![FunctionNode::new(
                    "alpha".to_string(),
                    "alpha".to_string(),
                    Location::new("test.c".to_string(), 1, 0),
                )],
            });
            let caller = *app.call_graph.function_ids().next().expect("one symbol");
            app.lsp.insert_pending(
                "calls",
                PendingRequest::new(Some(caller), "outgoing calls", origin),
            );

            let events = app.handle_lsp_response(LspResponse::OutgoingCalls {
                request_id: "calls".to_string(),
                calls: vec![lsp_types::CallHierarchyOutgoingCall {
                    to: hierarchy_item("beta", 2),
                    from_ranges: vec![hierarchy_item("beta", 2).range],
                }],
            });

            assert_eq!(
                events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)),
                wants_redraw,
                "{origin:?} reply"
            );
            // Either way the edge is in the graph — the warmup still does its
            // job, it just does it quietly.
            assert_eq!(app.call_graph.edge_count(), 1, "{origin:?} reply");
            assert!(!app.stale.is_empty(), "{origin:?} reply");
        }
    }

    /// "No callees" is a leaf, and the warmup walks over thousands of them.
    #[test]
    fn a_warmup_reply_does_not_announce_every_leaf_in_the_project() {
        let mut app = CoreApp::new(CallGraph::new());
        app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "symbols".to_string(),
            symbols: vec![FunctionNode::new(
                "leaf".to_string(),
                "leaf".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });
        let leaf = *app.call_graph.function_ids().next().expect("one symbol");
        app.lsp.insert_pending(
            "calls",
            PendingRequest::new(Some(leaf), "outgoing calls", RequestOrigin::Warmup),
        );

        let events = app.handle_lsp_response(LspResponse::OutgoingCalls {
            request_id: "calls".to_string(),
            calls: vec![],
        });

        assert!(!events
            .iter()
            .any(|e| matches!(e, AppEvent::FunctionHasNoCallees)));
    }

    /// The prepare and the call fetch are two halves of one question, and the
    /// half that carries the answer is the second one — so the origin has to
    /// survive the hop or the gate above never fires.
    #[test]
    fn the_follow_up_request_is_asked_by_whoever_asked_the_prepare() {
        let mut app = CoreApp::new(CallGraph::new());
        let mut rx = wire_worker(&mut app);
        app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "symbols".to_string(),
            symbols: vec![FunctionNode::new(
                "alpha".to_string(),
                "alpha".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });
        let id = *app.call_graph.function_ids().next().expect("one symbol");
        app.lsp.insert_pending(
            "prepare",
            PendingRequest::new(Some(id), "prepare alpha", RequestOrigin::Warmup),
        );

        app.handle_lsp_response(LspResponse::CallHierarchyPrepared {
            request_id: "prepare".to_string(),
            items: vec![hierarchy_item("alpha", 1)],
        });

        rx.try_recv().expect("the follow-up went out");
        assert!(
            app.lsp.pending().all(|p| p.origin == RequestOrigin::Warmup),
            "the follow-up inherits the prepare's origin"
        );
    }

    /// clangd answers a prepare whenever it gets round to it, and the user can
    /// hit `t` — or move to a tab reading the other way — in the meantime.
    /// Resolving the direction at reply time asked for the callers of a node
    /// that had been expanded in an outgoing view, and since the reply marks
    /// the symbol `Loaded` regardless, `cmd_expand_function`'s guard then meant
    /// `+` on that node never did anything again.
    #[test]
    fn a_follow_up_asks_in_the_direction_its_prepare_was_asked_in() {
        let mut graph = CallGraph::new();
        // An absolute path, unlike the rest of these tests: `send_call_hierarchy`
        // turns the definition location into a `file://` URL, and a relative
        // path fails that conversion before any request is ever sent.
        let alpha = graph.add_function(FunctionNode::new(
            "alpha".to_string(),
            "alpha".to_string(),
            Location::new("/test.c".to_string(), 1, 0),
        ));
        let mut app = CoreApp::new(graph);
        let mut rx = wire_worker(&mut app);
        assert_eq!(app.current_direction(), CallDirection::Outgoing);

        // The click: expand a node while the tab reads outgoing.
        app.dispatch(Command::ExpandFunction(alpha));
        let request_id = match rx.try_recv().expect("the prepare went out") {
            lsp::LspRequest::PrepareCallHierarchy { request_id, .. } => request_id,
            other => panic!("expected a prepare, got {other:?}"),
        };

        // ...and the answer is slow enough that the tab flips first.
        app.dispatch(Command::ToggleCallDirection);
        assert_eq!(app.current_direction(), CallDirection::Incoming);

        app.handle_lsp_response(LspResponse::CallHierarchyPrepared {
            request_id,
            items: vec![hierarchy_item("alpha", 1)],
        });

        match rx.try_recv().expect("the follow-up went out") {
            lsp::LspRequest::GetOutgoingCalls { .. } => {}
            other => panic!("the outgoing view must still get its callees, got {other:?}"),
        }
    }

    #[test]
    fn drilling_into_a_cluster_roots_it_at_the_entry_point() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetViewMode(ViewMode::ClusterMap));

        let key = key_of(&app, right[0]);
        let entry = active(&app)
            .by_key(key)
            .and_then(|c| c.entry_point)
            .expect("a cluster with members has a front door");

        let events = app.dispatch(Command::DrillIntoCluster(key));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));

        let ws = app.workspaces.current().expect("a workspace");
        assert_eq!(ws.root_symbol(), Some(entry));
        assert_eq!(ws.view(), ViewMode::Rooted);
        assert_eq!(app.selected_function, Some(entry));
    }

    #[test]
    fn drilling_into_an_unknown_cluster_changes_nothing() {
        let (graph, _, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetViewMode(ViewMode::ClusterMap));

        // A click against a clustering whose anchor has since moved, or a key
        // that never named anything. Either way there is nothing to root at.
        let events = app.dispatch(Command::DrillIntoCluster(ClusterKey {
            level: 0,
            anchor: SymbolId::from_content("nobody", "nowhere.c", 1),
        }));

        assert!(events.is_empty());
        let ws = app.workspaces.current().expect("a workspace");
        assert_eq!(ws.root_symbol(), None);
        assert_eq!(ws.view(), ViewMode::ClusterMap);
    }

    #[test]
    fn zooming_out_returns_to_the_map() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);

        app.dispatch(Command::DrillIntoCluster(key_of(&app, right[0])));
        assert_eq!(app.workspaces.current().unwrap().view(), ViewMode::Rooted);

        let events = app.dispatch(Command::ZoomOutToClusters);
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));
        assert_eq!(
            app.workspaces.current().unwrap().view(),
            ViewMode::ClusterMap
        );

        assert!(
            app.dispatch(Command::ZoomOutToClusters).is_empty(),
            "already on the map, so nothing to redraw"
        );
    }

    #[test]
    fn zooming_the_map_is_clamped_to_the_ladder_and_reset_by_a_new_root() {
        let (graph, left, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        let rungs = active(&app).level_count();
        assert!(rungs > 0, "the demo pair of triangles is clustered");

        let events = app.dispatch(Command::SetMapLevel(0));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));
        assert_eq!(app.workspaces.current().unwrap().map_level(), Some(0));

        // A level from a taller ladder lands on the coarsest rung rather than
        // on a blank canvas.
        app.dispatch(Command::SetMapLevel(rungs + 10));
        assert_eq!(
            app.workspaces.current().unwrap().map_level(),
            Some(rungs - 1),
            "a level past the top is clamped to the top"
        );
        assert_eq!(
            app.map_level_for(app.workspaces.current().unwrap()),
            rungs - 1
        );

        app.dispatch(Command::SelectFunction(left[0]));
        assert_eq!(
            app.workspaces.current().unwrap().map_level(),
            None,
            "the zoom described the old view"
        );
    }

    #[test]
    fn picking_the_method_already_shown_changes_nothing() {
        let (graph, _, _) = two_triangles();
        let mut app = CoreApp::new(graph);

        assert!(
            app.dispatch(Command::SetClusterMethod(ClusterMethod::Modularity))
                .is_empty(),
            "the map is already built that way"
        );
    }

    // ---- The map of one function's neighbourhood ----

    /// The clustering the active workspace's map is drawn from.
    fn active(app: &CoreApp) -> &Clustering {
        app.clustering_for(app.workspaces.current().expect("a default workspace"))
    }

    #[test]
    fn scoping_the_map_clusters_only_what_the_function_reaches() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);

        let events = app.dispatch(Command::SetMapScope(Some(right[0])));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));

        let ws = app.workspaces.current().expect("a default workspace");
        assert_eq!(ws.map_scope(), Some(right[0]));
        assert_eq!(
            ws.view(),
            ViewMode::ClusterMap,
            "naming what to draw a map from is asking to see that map"
        );

        for symbol in right {
            assert!(
                active(&app).cluster_of(&symbol).is_some(),
                "the right triangle is what this function reaches"
            );
        }
        for symbol in left {
            assert!(
                active(&app).cluster_of(&symbol).is_none(),
                "nothing calls back into the left triangle from here"
            );
        }

        assert!(
            app.clusterings
                .get(&ClusterTarget {
                    scope: None,
                    settings: ClusterSettings::default(),
                })
                .expect("the whole-program map is still cached")
                .cluster_of(&left[0])
                .is_some(),
            "and the whole-program clustering is untouched underneath it"
        );
    }

    #[test]
    fn the_scoped_map_reads_the_graph_the_way_the_workspace_does() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::ToggleCallDirection);

        app.dispatch(Command::SetMapScope(Some(right[0])));

        assert!(
            active(&app).cluster_of(&left[0]).is_some(),
            "reading callers, the neighbourhood is what reaches this function"
        );
        assert!(
            active(&app).cluster_of(&right[2]).is_none(),
            "and not what it reaches"
        );
    }

    #[test]
    fn a_function_with_nothing_around_it_is_declined_rather_than_shown_blank() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        // A leaf: it calls nothing, so its neighbourhood is itself.
        app.dispatch(Command::SelectFunction(right[2]));

        let events = app.dispatch(Command::SetMapScope(Some(right[2])));

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AppEvent::FunctionHasNoCommunities)),
            "the refusal has to be reported, or the click looks broken"
        );
        let ws = app.workspaces.current().expect("a default workspace");
        assert_eq!(ws.map_scope(), None);
        assert_eq!(
            ws.view(),
            ViewMode::Rooted,
            "and the user is left on the view they were already reading"
        );
    }

    #[test]
    fn widening_the_scope_goes_back_to_the_whole_program() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(right[0])));

        app.dispatch(Command::SetMapScope(None));

        assert_eq!(
            app.workspaces.current().expect("a workspace").map_scope(),
            None
        );
        assert!(
            active(&app).cluster_of(&left[0]).is_some(),
            "the whole program is on the map again"
        );
        assert!(
            app.clusterings.keys().all(|target| target.scope.is_none()),
            "and the clustering nothing points at any more is not kept around"
        );
    }

    #[test]
    fn drilling_resolves_against_the_map_that_is_showing() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        // A community that exists only on the whole-program map.
        let elsewhere = key_of(&app, left[0]);

        app.dispatch(Command::SetMapScope(Some(right[0])));
        app.dispatch(Command::DrillIntoCluster(elsewhere));

        assert_eq!(
            app.workspaces.current().expect("a workspace").view(),
            ViewMode::ClusterMap,
            "a key for a community not on this map roots nothing"
        );

        // Whereas a key from the map actually showing does.
        let here = {
            let clustering = active(&app);
            let id = clustering.cluster_of(&right[0]).expect("clustered");
            clustering.cluster(id).expect("a known id").key
        };
        app.dispatch(Command::DrillIntoCluster(here));

        let ws = app.workspaces.current().expect("a workspace");
        assert_eq!(ws.view(), ViewMode::Rooted);
        assert!(
            ws.root_symbol().is_some_and(|root| right.contains(&root)),
            "rooted at the front door of a community that is on this map"
        );
        assert_eq!(
            ws.map_scope(),
            Some(right[0]),
            "and the way back still leads to the map it was entered from"
        );
    }

    #[test]
    fn a_scoped_map_follows_the_graph_as_it_grows() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(right[0])));

        let extra = add(&mut app.call_graph, "extra", 900);
        call(&mut app.call_graph, right[2], extra, 901);
        app.mark_graph_changed();
        tick_until_settled(&mut app);

        assert!(
            active(&app).cluster_of(&extra).is_some(),
            "a symbol the scope now reaches belongs on its map"
        );
    }

    #[test]
    fn only_the_scopes_in_use_are_kept_clustered() {
        let scoped = |app: &CoreApp| {
            app.clusterings
                .keys()
                .filter(|target| target.scope.is_some())
                .count()
        };

        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(right[0])));
        assert_eq!(scoped(&app), 1);

        // A second tab, on the whole-program map like every new tab.
        app.dispatch(Command::NewWorkspace);
        assert_eq!(scoped(&app), 1, "the first tab still wants its map");

        app.dispatch(Command::CloseWorkspace(0));
        assert_eq!(
            scoped(&app),
            0,
            "closing the only tab that wanted it drops the clustering"
        );
    }

    // ---- What a call site emits for each send outcome ---------------------
    //
    // Every `LspBridge::send_*` call site turns one outcome into events, and
    // that translation is the whole of what a frontend sees: a status line, a
    // redraw, or silence. These pin it — which events, in which order, for a
    // request that goes out and for one that cannot — so that reshaping the
    // senders' return type cannot move it without a test saying so.

    /// The variant names of a run of events, in order.
    ///
    /// `AppEvent` has no `PartialEq` and needs none for this: what is under
    /// test is which events came out and in what order, never their payloads,
    /// which no sender touches on the paths below.
    fn shapes(events: &[AppEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                format!("{event:?}")
                    .split(['(', '{', ' '])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn failures(events: &[AppEvent]) -> usize {
        shapes(events)
            .iter()
            .filter(|name| *name == "FailedToSendRequest")
            .count()
    }

    /// The one symbol of a `loaded_app(1)`: in the graph, at an absolute path,
    /// so every sender gets all the way to the channel before deciding.
    fn only_symbol(app: &CoreApp) -> SymbolId {
        *app.call_graph.function_ids().next().expect("one symbol")
    }

    #[test]
    fn selecting_a_root_redraws_and_says_so_only_when_the_fetch_did_not_go() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        assert_eq!(
            shapes(&app.dispatch(Command::SelectFunction(id))),
            ["LayoutDirty"],
            "a fetch on its way is not news"
        );

        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::SelectFunction(id))),
            ["FailedToSendRequest", "LayoutDirty"]
        );
    }

    #[test]
    fn expand_or_collapse_redraws_either_way() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.selected_function = Some(id);
        assert_eq!(
            shapes(&app.dispatch(Command::ExpandOrCollapse)),
            ["LayoutDirty"]
        );

        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.selected_function = Some(id);
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::ExpandOrCollapse)),
            ["FailedToSendRequest", "LayoutDirty"]
        );
    }

    #[test]
    fn find_references_reports_the_search_or_the_failure_and_never_redraws() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.dispatch(Command::SelectFunction(id));
        assert_eq!(
            shapes(&app.dispatch(Command::FindReferences)),
            ["FindingReferences"]
        );

        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.dispatch(Command::SelectFunction(id));
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::FindReferences)),
            ["FailedToSendRequest"]
        );
    }

    #[test]
    fn a_refresh_announces_the_sweep_and_reports_each_send_separately() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.selected_function = Some(id);
        assert_eq!(
            shapes(&app.dispatch(Command::Refresh)),
            ["RefreshingWorkspaceSymbols"],
            "the root's hierarchy goes out silently behind the sweep"
        );

        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.selected_function = Some(id);
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::Refresh)),
            ["FailedToSendRequest", "FailedToSendRequest"],
            "two requests, two failures"
        );
    }

    #[test]
    fn starting_a_graph_trails_a_failed_fetch_behind_the_new_tab() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        assert_eq!(
            shapes(&app.dispatch(Command::StartGraphFromFunction(id))),
            ["WorkspaceCreated", "LayoutDirty"]
        );

        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::StartGraphFromFunction(id))),
            ["WorkspaceCreated", "LayoutDirty", "FailedToSendRequest"],
            "this one appends the failure where the others lead with it"
        );
    }

    #[test]
    fn expanding_redraws_when_the_view_moved_and_whenever_the_fetch_failed() {
        let (mut app, _rx) = loaded_app(1);
        let id = only_symbol(&app);
        assert_eq!(
            shapes(&app.dispatch(Command::ExpandFunction(id))),
            ["LayoutDirty"]
        );
        assert!(
            app.dispatch(Command::ExpandFunction(id)).is_empty(),
            "already expanded and already being fetched: nothing to do or say"
        );

        // The case the convention turns on: the expansion changes nothing, so
        // there is nothing to redraw, and the redraw happens anyway because
        // the fetch failed.
        let (mut app, rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.dispatch(Command::ExpandFunction(id));
        app.lsp.set_loading_state(id, LoadingState::NotLoaded);
        drop(rx);
        assert_eq!(
            shapes(&app.dispatch(Command::ExpandFunction(id))),
            ["FailedToSendRequest", "LayoutDirty"]
        );
    }

    #[test]
    fn the_warmup_is_silent_unless_a_send_fails() {
        let (mut app, _rx) = loaded_app(2);
        assert_eq!(
            failures(&app.tick()),
            0,
            "nobody is waiting on a warmup reply, so nothing is reported"
        );

        let (mut app, rx) = loaded_app(2);
        drop(rx);
        assert_eq!(failures(&app.tick()), 2, "one per symbol it could not ask");
    }

    #[test]
    fn a_prepared_hierarchy_announces_its_follow_up_and_is_quiet_when_it_fails() {
        let prepared = |request_id: String| LspResponse::CallHierarchyPrepared {
            request_id,
            items: vec![hierarchy_item("f0", 0)],
        };
        let sent_prepare = |rx: &mut mpsc::UnboundedReceiver<lsp::LspRequest>| match rx
            .try_recv()
            .expect("the prepare went out")
        {
            lsp::LspRequest::PrepareCallHierarchy { request_id, .. } => request_id,
            other => panic!("expected a prepare, got {other:?}"),
        };

        let (mut app, mut rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.dispatch(Command::SelectFunction(id));
        let request_id = sent_prepare(&mut rx);
        assert_eq!(
            shapes(&app.handle_lsp_response(prepared(request_id))),
            ["LoadingCalls"]
        );

        let (mut app, mut rx) = loaded_app(1);
        let id = only_symbol(&app);
        app.dispatch(Command::SelectFunction(id));
        let request_id = sent_prepare(&mut rx);
        drop(rx);
        assert!(
            app.handle_lsp_response(prepared(request_id)).is_empty(),
            "a follow-up that could not be sent tells the frontend nothing"
        );
    }
}
