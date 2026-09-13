use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use logic::{ClusterKey, Clustering, GraphTraversal};
use lsp::LspResponse;
use model::{CallDirection, CallGraph, FunctionNode, Location, LspLoadPhase, SymbolId};

use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::LayoutOrientation;
use crate::lsp_bridge::{LoadingState, LspBridge};
use crate::workspace::{CoreWorkspace, CoreWorkspaceManager, ViewMode};

/// How long the call graph must go without gaining nodes or edges before the
/// clustering is recomputed.
///
/// clangd does not hand over a call graph, it dribbles one: workspace symbols
/// arrive in batches and each call hierarchy lands as its own response. A
/// recompute per mutation would spend the whole load running Louvain over a
/// graph that is about to change again, and — worse than the CPU — would
/// reshuffle the map under the user several times a second. Waiting for quiet
/// collapses a burst of arrivals into one recompute.
///
/// Three hundred milliseconds is three of the 100ms ticks the web and desktop
/// backends poll on: long enough that a batch arriving in the next breath still
/// counts as part of the same burst, short enough that the map appears while
/// the user is still looking at the place it belongs.
const CLUSTERING_SETTLE: Duration = Duration::from_millis(300);

/// How many call-hierarchy requests to keep in flight while warming up edges.
///
/// clangd serves these one at a time, so the ceiling is about not parking two
/// hundred requests on a 30 s timeout behind a server that answers them at its
/// own pace — not about parallelism. Eight keeps the pipe busy and leaves room
/// for a request the user makes while the warmup is still running.
const WARMUP_IN_FLIGHT: usize = 8;

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
    /// Communities derived from the **whole** graph, cached.
    ///
    /// Deriving them from the visible BFS window instead would be cheaper and
    /// wrong: the window changes every time the user expands a node, so the
    /// boxes would regroup and rename themselves on every click. The clustering
    /// is a property of the program, not of what is currently on screen.
    clustering: Clustering,
    /// Set whenever the graph gains nodes or edges; cleared by the recompute.
    clustering_dirty: bool,
    /// The communities of one function's neighbourhood, for each scope a
    /// workspace currently has its map pointed at.
    ///
    /// Cached here rather than on the workspace because two tabs scoped to the
    /// same function in the same direction are asking for the same clustering,
    /// and because this is where the recompute that keeps it current already
    /// lives. Kept in step with the workspaces by `sync_scoped_clusterings`, so
    /// that reading one — which every snapshot does — needs no `&mut`.
    scoped: HashMap<(SymbolId, CallDirection), Clustering>,
    /// Set when `scoped` is out of date with the graph: the entries are still
    /// worth keeping as seeds, so this marks them stale rather than dropping
    /// them and losing the grouping stability seeding buys.
    scoped_dirty: bool,
    /// When the graph last changed. The settling period is measured from here.
    graph_changed_at: Option<Instant>,
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
        // Cluster up front rather than leaving it to the first tick. In demo
        // mode the graph handed in here is the entire graph and no loader will
        // ever arrive to mark the cache dirty, so a clustering that only
        // happens on a settling timer would never happen at all. In LSP mode
        // the graph is empty at this point and this costs nothing.
        let clustering = Clustering::compute(&call_graph);
        Self {
            call_graph,
            selected_function: None,
            lsp,
            workspaces: CoreWorkspaceManager::new(),
            clustering,
            clustering_dirty: false,
            scoped: HashMap::new(),
            scoped_dirty: false,
            graph_changed_at: None,
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

    /// The cached communities of the whole call graph.
    pub fn clustering(&self) -> &Clustering {
        &self.clustering
    }

    /// The communities the given workspace's map is drawn from: one function's
    /// neighbourhood where it has been scoped to one, the whole program
    /// otherwise.
    ///
    /// Falls back to the whole-program clustering if the scoped one is missing,
    /// which `sync_scoped_clusterings` is there to make impossible — but the
    /// honest answer to "which communities" is never a panic.
    pub fn clustering_for(&self, ws: &CoreWorkspace) -> &Clustering {
        ws.map_scope
            .and_then(|scope| self.scoped.get(&(scope, ws.direction)))
            .unwrap_or(&self.clustering)
    }

    /// The communities of the active workspace's map.
    pub fn active_clustering(&self) -> &Clustering {
        match self.workspaces.current() {
            Some(ws) => self.clustering_for(ws),
            None => &self.clustering,
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
            Command::ToggleClusterExpanded(key) => self.cmd_toggle_cluster_expanded(key),
            Command::DrillIntoCluster(key) => self.cmd_drill_into_cluster(key),
            Command::ZoomOutToClusters => self.cmd_zoom_out_to_clusters(),
            Command::NewWorkspace => self.cmd_new_workspace(),
            Command::CloseWorkspace(index) => self.cmd_close_workspace(index),
            Command::NextWorkspace => self.cmd_next_workspace(),
            Command::PreviousWorkspace => self.cmd_previous_workspace(),
            Command::SwitchWorkspace(index) => self.cmd_switch_workspace(index),
            Command::RenameWorkspace(index, name) => self.cmd_rename_workspace(index, name),
            Command::StartGraphFromFunction(id) => self.cmd_start_graph_from_function(id),
            Command::SearchSelectFunction(id) => self.cmd_search_select_function(id),
            Command::ExpandFunction(id) => self.cmd_expand_function(id),
        };
        // Any command can be the one that changes which map is wanted — a tab
        // switch, a direction toggle, a new scope — so the cache is squared up
        // once, here, rather than in each handler that might have moved it.
        self.sync_scoped_clusterings();
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
        events.push(AppEvent::LspProgress(self.lsp.status.clone()));

        // Drain pending LSP responses
        let (responses, timeout_events) = self.lsp.drain_responses();
        events.extend(timeout_events);

        for response in responses {
            events.extend(self.handle_lsp_response(response));
        }

        // Before reclustering: a warmup request sent now is what stops the
        // recompute below from running against an edgeless graph.
        events.extend(self.pump_edge_warmup());

        // Last, so a batch ingested on this very tick is counted before the
        // settling period is judged.
        events.extend(self.maybe_recluster());
        // And after it, because that is what marks the scoped clusterings stale.
        self.sync_scoped_clusterings();

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
                        self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                        log::info!(
                            "CoreApp: OutgoingCalls loaded for {:?}: {} callee(s)",
                            symbol_id,
                            count
                        );
                        if count == 0 {
                            events.push(AppEvent::FunctionHasNoCallees);
                        }
                        events.push(AppEvent::OutgoingCallsLoaded { symbol_id });
                        events.push(AppEvent::LayoutDirty);
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
                        self.lsp.set_loading_state(symbol_id, LoadingState::Loaded);
                        log::info!(
                            "CoreApp: IncomingCalls loaded for {:?}: {} caller(s)",
                            symbol_id,
                            count
                        );
                        if count == 0 {
                            events.push(AppEvent::FunctionHasNoCallees);
                        }
                        events.push(AppEvent::IncomingCallsLoaded { symbol_id });
                        events.push(AppEvent::LayoutDirty);
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
            LspResponse::CallHierarchyPrepared { request_id, items } => {
                if let Some(pending) = self.lsp.take_pending(&request_id) {
                    if let Some(symbol_id) = pending.symbol_id {
                        if !items.is_empty() {
                            let first_item = items[0].clone();
                            let direction = self
                                .workspaces
                                .current()
                                .map(|w| w.direction)
                                .unwrap_or(CallDirection::Outgoing);
                            let outgoing = matches!(direction, CallDirection::Outgoing);
                            if let Some(ev) = self
                                .lsp
                                .send_follow_up_call(&symbol_id, first_item, outgoing)
                            {
                                events.push(ev);
                            }
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
        // Kick off call hierarchy loading if not already done
        if !self.lsp.is_function_loaded(&id) && !self.lsp.is_function_loading(&id) {
            self.lsp.set_loading_state(id, LoadingState::Loading);
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                return vec![ev, AppEvent::LayoutDirty];
            }
        }
        vec![AppEvent::LayoutDirty]
    }

    fn cmd_expand_or_collapse(&mut self) -> Vec<AppEvent> {
        // The selected node comes from the frontend (TUI passes it via SelectFunction).
        // Here we just request call hierarchy for the currently selected function.
        if let Some(id) = self.selected_function {
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                return vec![ev, AppEvent::LayoutDirty];
            }
            return vec![AppEvent::LayoutDirty];
        }
        vec![]
    }

    fn cmd_find_references(&mut self) -> Vec<AppEvent> {
        let symbol_id = self.workspaces.current().and_then(|w| w.root_symbol);
        if let Some(id) = symbol_id {
            let mut events = Vec::new();
            if let Some(ev) = self.lsp.send_references(&self.call_graph, &id) {
                events.push(ev);
            }
            events
        } else {
            vec![]
        }
    }

    fn cmd_refresh(&mut self) -> Vec<AppEvent> {
        self.lsp.clear();
        let mut events = Vec::new();
        if let Some(ev) = self.lsp.send_workspace_symbols() {
            events.push(ev);
        }
        if let Some(id) = self.selected_function {
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                events.push(ev);
            }
        }
        events
    }

    fn cmd_toggle_call_direction(&mut self) -> Vec<AppEvent> {
        if let Some(ws) = self.workspaces.current_mut() {
            ws.direction = match ws.direction {
                CallDirection::Outgoing => CallDirection::Incoming,
                CallDirection::Incoming => CallDirection::Outgoing,
            };
            // Reversing the arrows is a different tree; the nodes walked past
            // the depth limit in the old one have nothing to say about it.
            ws.expanded.clear();
            log::info!(
                "CoreApp: direction toggled to {:?}, expansions cleared",
                ws.direction
            );
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
        let direction = ws.direction;
        let clustering = self.cluster_within(symbol, direction, None);
        if clustering.is_empty() {
            log::info!("CoreApp: map scope {symbol:?} declined — nothing to group");
            return vec![AppEvent::FunctionHasNoCommunities];
        }
        self.scoped.insert((symbol, direction), clustering);

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

    fn cmd_toggle_cluster_expanded(&mut self, key: ClusterKey) -> Vec<AppEvent> {
        match self.workspaces.current_mut() {
            Some(ws) => {
                ws.toggle_cluster_expanded(key);
                vec![AppEvent::LayoutDirty]
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
        let lsp_event = self.lsp.send_call_hierarchy(&self.call_graph, &id);
        let mut events = vec![ws_event, AppEvent::LayoutDirty];
        if let Some(ev) = lsp_event {
            events.push(ev);
        }
        events
    }

    fn cmd_search_select_function(&mut self, id: SymbolId) -> Vec<AppEvent> {
        self.cmd_select_function(id)
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

        if !self.lsp.is_function_loaded(&id) && !self.lsp.is_function_loading(&id) {
            self.lsp.set_loading_state(id, LoadingState::Loading);
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                return vec![ev, AppEvent::LayoutDirty];
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
    /// The timestamp restarts on every change rather than being stamped once
    /// when the cache goes dirty: the settling period is about the stream
    /// having gone quiet, not about how long ago it started talking.
    fn mark_graph_changed(&mut self) {
        self.clustering_dirty = true;
        self.graph_changed_at = Some(Instant::now());
    }

    /// Bring the scoped clusterings in step with what the workspaces are asking
    /// for: compute the ones that are missing or stale, drop the ones no tab
    /// points at any more.
    ///
    /// Doing it here, once per dispatch and once per tick, is what lets
    /// `clustering_for` be a lookup on `&self` — and so lets a snapshot, which
    /// only ever holds `&CoreApp`, draw a scoped map at all.
    fn sync_scoped_clusterings(&mut self) {
        let wanted: HashSet<(SymbolId, CallDirection)> = self
            .workspaces
            .workspaces
            .iter()
            .filter_map(|ws| ws.map_scope.map(|scope| (scope, ws.direction)))
            .collect();

        // The overwhelmingly common case: no tab has scoped its map, and there
        // is nothing to recompute however dirty the graph has got.
        if wanted.is_empty() && self.scoped.is_empty() {
            self.scoped_dirty = false;
            return;
        }

        self.scoped.retain(|key, _| wanted.contains(key));

        let stale = std::mem::take(&mut self.scoped_dirty);
        for key in wanted {
            if !stale && self.scoped.contains_key(&key) {
                continue;
            }
            // Seeded from what this scope held before, for the reason the
            // whole-program recompute is: a map that regroups itself under the
            // user reads as the view breaking, not as new information.
            let previous = self.scoped.remove(&key);
            let clustering = self.cluster_within(key.0, key.1, previous.as_ref());
            log::info!(
                "CoreApp: clustered the neighbourhood of {:?} ({:?}): {} top-level cluster(s)",
                key.0,
                key.1,
                clustering.top_level().len()
            );
            self.scoped.insert(key, clustering);
        }
    }

    /// Cluster everything `scope` reaches, reading the graph the way the
    /// workspace does.
    fn cluster_within(
        &self,
        scope: SymbolId,
        direction: CallDirection,
        previous: Option<&Clustering>,
    ) -> Clustering {
        let traversal = GraphTraversal::new(&self.call_graph);
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

        Clustering::compute_within(&self.call_graph, &reachable, previous)
    }

    /// Whether the initial edge warmup is still running.
    fn warming_up(&self) -> bool {
        self.warmup_seeded
            && (!self.edge_warmup.is_empty() || !self.lsp.pending_requests.is_empty())
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
            && matches!(self.lsp.status, LspLoadPhase::Completed)
            && self.lsp.request_tx.is_some()
        {
            self.warmup_seeded = true;
            self.edge_warmup = self.call_graph.function_ids().copied().collect();
            self.warmup_total = self.edge_warmup.len();
            self.warmup_last_moved = Some(Instant::now());
            self.warmup_started_at = Some(Instant::now());
            log::info!(
                "CoreApp: warming up call hierarchy for {} symbol(s), {} in flight, {} node(s) in the graph",
                self.edge_warmup.len(),
                self.lsp.pending_requests.len(),
                self.call_graph.node_count()
            );
        }

        let mut events = Vec::new();
        let mut sent = 0usize;
        let mut skipped = 0usize;
        while self.lsp.pending_requests.len() < WARMUP_IN_FLIGHT {
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
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                events.push(ev);
            }
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
            self.lsp.pending_requests.len(),
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

    /// Recompute the cached clustering if the graph has changed and then gone
    /// quiet — or if clangd has already told us nothing more is coming.
    fn maybe_recluster(&mut self) -> Vec<AppEvent> {
        if !self.clustering_dirty {
            return vec![];
        }

        let settled = self
            .graph_changed_at
            .is_some_and(|at| at.elapsed() >= CLUSTERING_SETTLE);
        // Once the load is complete the graph only grows because the user asked
        // it to, and then they are waiting on the answer: one recompute per
        // action is both affordable and the thing they just requested. It also
        // rescues a project whose symbols arrive in an unbroken stream, where
        // waiting for quiet would mean waiting until the very end anyway.
        // The warmup below makes the graph grow on its own after the load
        // reports Completed, so the "nothing more is coming" shortcut would fire
        // on every tick for as long as it runs — a Louvain pass per 100 ms, each
        // one free to move the map under the user. While it runs, fall back to
        // the ordinary quiet rule; the shortcut returns once it drains.
        let load_finished =
            matches!(self.lsp.status, LspLoadPhase::Completed) && !self.warming_up();
        if !settled && !load_finished {
            return vec![];
        }

        // Seeded, always. Louvain is greedy, so a cold run over a graph that
        // gained a single edge can regroup communities nothing touched; seeding
        // from what is already on screen keeps the map still unless the new
        // edges genuinely argue for moving something.
        let next = Clustering::compute_seeded(&self.call_graph, Some(&self.clustering));
        self.clustering_dirty = false;
        self.graph_changed_at = None;
        // Any scoped map is derived from the same graph and is now just as out
        // of date — including when the grouping below turns out not to have
        // moved, since a new edge can leave the whole program's communities
        // alone and still redraw one function's neighbourhood.
        self.scoped_dirty = true;

        if same_grouping(&self.clustering, &next) {
            return vec![];
        }

        let clusters = next.top_level().len();
        self.clustering = next;
        log::info!(
            "CoreApp: clustering recomputed over {} node(s): {} top-level cluster(s)",
            self.call_graph.node_count(),
            clusters
        );
        vec![AppEvent::ClusteringUpdated { clusters }]
    }

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
        self.mark_graph_changed();
    }
}

/// Whether two clusterings describe the same grouping.
///
/// `Cluster` is deliberately not `PartialEq` and does not need to be: the only
/// question here is whether anything a frontend would draw has moved, so this
/// compares the fields a frontend draws. It is a linear pass next to the
/// Louvain run that produced the thing, and it is what keeps a recompute that
/// confirms the status quo from announcing itself as news.
fn same_grouping(before: &Clustering, after: &Clustering) -> bool {
    before.clusters().count() == after.clusters().count()
        && before.clusters().zip(after.clusters()).all(|(a, b)| {
            a.id == b.id
                && a.level == b.level
                && a.parent == b.parent
                && a.children == b.children
                && a.entry_point == b.entry_point
                && a.label == b.label
                && a.members == b.members
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp_bridge::PendingRequest;
    use model::Location;
    use tokio::sync::mpsc;

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
        let id = app.clustering().cluster_of(&symbol).expect("clustered");
        app.clustering().cluster(id).expect("a known id").key
    }

    /// The laid-out nodes of the current snapshot — what the canvas would draw.
    fn placed(app: &CoreApp) -> Vec<SymbolId> {
        match crate::protocol::snapshot_of(app) {
            crate::protocol::ServerMsg::Snapshot { layout, .. } => layout.keys().copied().collect(),
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
            app.workspaces.current().and_then(|ws| ws.root_symbol),
            Some(root),
            "expanding does not re-root the workspace"
        );
        for id in &before {
            assert!(after.contains(id), "everything on screen stayed on screen");
        }
        assert!(after.contains(&leaf), "and the fetched callee was appended");
    }

    /// Pretend the settling period has already elapsed, rather than sleeping
    /// through it for real in a unit test.
    fn backdate(app: &mut CoreApp) {
        app.graph_changed_at = Some(Instant::now() - CLUSTERING_SETTLE);
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
        // batch to start the settling clock — the clustering has to be there
        // from the moment the graph is handed over.
        assert!(!app.clustering().is_empty());
        let left_cluster = app.clustering().cluster_of(&left[0]).expect("clustered");
        let right_cluster = app.clustering().cluster_of(&right[0]).expect("clustered");
        assert_ne!(left_cluster, right_cluster);
    }

    #[test]
    fn a_changed_graph_is_not_reclustered_until_the_stream_goes_quiet() {
        let mut app = CoreApp::new(CallGraph::new());
        assert!(app.clustering().is_empty());

        let (graph, left, _) = two_triangles();
        app.call_graph = graph;
        app.mark_graph_changed();

        // A batch that has only just landed may be one of several; recomputing
        // now would be work thrown away and a map that jumps.
        let events = app.tick();
        assert!(
            app.clustering().is_empty(),
            "a change still arriving must not trigger a recompute"
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, AppEvent::ClusteringUpdated { .. })));

        backdate(&mut app);
        let events = app.tick();
        assert!(app.clustering().cluster_of(&left[0]).is_some());
        let reported = events.iter().find_map(|e| match e {
            AppEvent::ClusteringUpdated { clusters } => Some(*clusters),
            _ => None,
        });
        assert_eq!(reported, Some(app.clustering().top_level().len()));

        // Nothing changed since, so the next tick has nothing to say.
        assert!(!app
            .tick()
            .iter()
            .any(|e| matches!(e, AppEvent::ClusteringUpdated { .. })));
    }

    #[test]
    fn a_completed_load_reclusters_without_waiting_for_quiet() {
        let mut app = CoreApp::new(CallGraph::new());
        let (graph, left, _) = two_triangles();
        app.call_graph = graph;
        app.mark_graph_changed();

        // clangd says there is nothing more coming, so there is nothing left to
        // settle and no reason to make the user watch an empty map for it.
        app.lsp.status = LspLoadPhase::Completed;
        let events = app.tick();

        assert!(app.clustering().cluster_of(&left[0]).is_some());
        assert!(events
            .iter()
            .any(|e| matches!(e, AppEvent::ClusteringUpdated { .. })));
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
        let (tx, rx) = mpsc::unbounded_channel();
        app.lsp.request_tx = Some(tx);
        app.lsp.status = LspLoadPhase::Completed;
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
        app.lsp.pending_requests.clear();
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
        app.lsp.pending_requests.clear();
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
        app.lsp.pending_requests.clear();

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
        // `Completed` normally means "recluster now, nothing more is coming".
        // The warmup makes that false: the graph keeps growing on its own, and
        // honouring the shortcut would run Louvain on every 100ms tick.
        let (mut app, _rx) = loaded_app(2);
        app.tick();
        assert!(app.warming_up());

        app.clustering_dirty = true;
        app.graph_changed_at = Some(Instant::now());
        assert!(
            app.maybe_recluster().is_empty(),
            "no recompute while edges are still arriving"
        );
        assert!(app.clustering_dirty, "and the work is still owed");

        // Drained: the shortcut comes back.
        app.edge_warmup.clear();
        app.lsp.pending_requests.clear();
        assert!(!app.warming_up());
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
        assert!(app.lsp.pending_requests.is_empty());

        let events = app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "issued-by-the-loader".to_string(),
            symbols: vec![FunctionNode::new(
                "alpha".to_string(),
                "alpha".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });

        assert_eq!(app.call_graph.function_ids().count(), 1);
        assert!(app.clustering_dirty);
        assert!(events
            .iter()
            .any(|e| matches!(e, AppEvent::GraphSymbolsUpdated { count: 1 })));
    }

    #[test]
    fn every_way_the_graph_grows_marks_the_cache_dirty() {
        let mut app = CoreApp::new(CallGraph::new());
        assert!(!app.clustering_dirty);

        app.lsp.pending_requests.insert(
            "symbols".to_string(),
            PendingRequest::new(None, "workspace symbols"),
        );
        app.handle_lsp_response(LspResponse::WorkspaceSymbols {
            request_id: "symbols".to_string(),
            symbols: vec![FunctionNode::new(
                "alpha".to_string(),
                "alpha".to_string(),
                Location::new("test.c".to_string(), 1, 0),
            )],
        });
        assert!(app.clustering_dirty, "new symbols are new graph nodes");

        app.clustering_dirty = false;
        let caller = *app.call_graph.function_ids().next().expect("one symbol");
        app.lsp.pending_requests.insert(
            "calls".to_string(),
            PendingRequest::new(Some(caller), "outgoing calls"),
        );
        app.handle_lsp_response(LspResponse::OutgoingCalls {
            request_id: "calls".to_string(),
            calls: vec![lsp_types::CallHierarchyOutgoingCall {
                to: hierarchy_item("beta", 2),
                from_ranges: vec![hierarchy_item("beta", 2).range],
            }],
        });
        assert!(app.clustering_dirty, "new calls are new graph edges");
    }

    #[test]
    fn drilling_into_a_cluster_roots_it_at_the_entry_point() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetViewMode(ViewMode::ClusterMap));

        let key = key_of(&app, right[0]);
        let entry = app
            .clustering()
            .by_key(key)
            .and_then(|c| c.entry_point)
            .expect("a cluster with members has a front door");

        let events = app.dispatch(Command::DrillIntoCluster(key));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));

        let ws = app.workspaces.current().expect("a workspace");
        assert_eq!(ws.root_symbol, Some(entry));
        assert_eq!(ws.view, ViewMode::Rooted);
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
        assert_eq!(ws.root_symbol, None);
        assert_eq!(ws.view, ViewMode::ClusterMap);
    }

    #[test]
    fn zooming_out_returns_to_the_map() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);

        app.dispatch(Command::DrillIntoCluster(key_of(&app, right[0])));
        assert_eq!(app.workspaces.current().unwrap().view, ViewMode::Rooted);

        let events = app.dispatch(Command::ZoomOutToClusters);
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));
        assert_eq!(app.workspaces.current().unwrap().view, ViewMode::ClusterMap);

        assert!(
            app.dispatch(Command::ZoomOutToClusters).is_empty(),
            "already on the map, so nothing to redraw"
        );
    }

    #[test]
    fn toggling_a_cluster_round_trips_and_a_new_root_clears_it() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        let key = key_of(&app, right[0]);

        let events = app.dispatch(Command::ToggleClusterExpanded(key));
        assert!(events.iter().any(|e| matches!(e, AppEvent::LayoutDirty)));
        assert!(app
            .workspaces
            .current()
            .unwrap()
            .expanded_clusters
            .contains(&key));

        app.dispatch(Command::ToggleClusterExpanded(key));
        assert!(app
            .workspaces
            .current()
            .unwrap()
            .expanded_clusters
            .is_empty());

        app.dispatch(Command::ToggleClusterExpanded(key));
        app.dispatch(Command::SelectFunction(left[0]));
        assert!(
            app.workspaces
                .current()
                .unwrap()
                .expanded_clusters
                .is_empty(),
            "what stood open described the old view"
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
        assert_eq!(ws.map_scope, Some(right[0]));
        assert_eq!(
            ws.view,
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
            app.clustering().cluster_of(&left[0]).is_some(),
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
        assert_eq!(ws.map_scope, None);
        assert_eq!(
            ws.view,
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
            app.workspaces.current().expect("a workspace").map_scope,
            None
        );
        assert!(
            active(&app).cluster_of(&left[0]).is_some(),
            "the whole program is on the map again"
        );
        assert!(
            app.scoped.is_empty(),
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
            app.workspaces.current().expect("a workspace").view,
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
        assert_eq!(ws.view, ViewMode::Rooted);
        assert!(
            ws.root_symbol.is_some_and(|root| right.contains(&root)),
            "rooted at the front door of a community that is on this map"
        );
        assert_eq!(
            ws.map_scope,
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
        backdate(&mut app);
        app.tick();

        assert!(
            active(&app).cluster_of(&extra).is_some(),
            "a symbol the scope now reaches belongs on its map"
        );
    }

    #[test]
    fn only_the_scopes_in_use_are_kept_clustered() {
        let (graph, _, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetMapScope(Some(right[0])));
        assert_eq!(app.scoped.len(), 1);

        // A second tab, on the whole-program map like every new tab.
        app.dispatch(Command::NewWorkspace);
        assert_eq!(app.scoped.len(), 1, "the first tab still wants its map");

        app.dispatch(Command::CloseWorkspace(0));
        assert!(
            app.scoped.is_empty(),
            "closing the only tab that wanted it drops the clustering"
        );
    }
}
