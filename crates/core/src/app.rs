use std::time::{Duration, Instant};

use logic::{ClusterKey, Clustering};
use lsp::LspResponse;
use model::{CallDirection, CallGraph, FunctionNode, Location, LspLoadPhase, SymbolId};

use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::LayoutOrientation;
use crate::lsp_bridge::{LoadingState, LspBridge};
use crate::workspace::{CoreWorkspaceManager, ViewMode};

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

/// Frontend-agnostic application core.
///
/// Owns the call graph, workspace state, and LSP bridge. Emits `AppEvent`
/// values rather than mutating any rendering state. Frontends wrap `CoreApp`
/// and translate the events into their own display concerns.
pub struct CoreApp {
    pub call_graph: CallGraph,
    pub selected_function: Option<SymbolId>,
    /// The node used as the BFS root for layout computation in the web frontend.
    /// Set to `Some(id)` when the user expands a node in-place (so the subgraph
    /// is centred on that node). Cleared whenever the workspace root or direction
    /// changes so the view resets to the normal root-anchored layout.
    pub layout_focus: Option<SymbolId>,
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
    /// When the graph last changed. The settling period is measured from here.
    graph_changed_at: Option<Instant>,
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
            layout_focus: None,
            lsp,
            workspaces: CoreWorkspaceManager::new(),
            clustering,
            clustering_dirty: false,
            graph_changed_at: None,
        }
    }

    /// The cached communities of the whole call graph.
    pub fn clustering(&self) -> &Clustering {
        &self.clustering
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
            self.call_graph.add_function(function);
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

        // Last, so a batch ingested on this very tick is counted before the
        // settling period is judged.
        events.extend(self.maybe_recluster());

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
                if let Some(_pending) = self.lsp.take_pending(&request_id) {
                    let count = symbols.len();
                    for symbol in symbols {
                        self.call_graph.add_function(symbol);
                    }
                    if count > 0 {
                        self.mark_graph_changed();
                    }
                    events.push(AppEvent::GraphSymbolsUpdated { count });
                }
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
        // Changing the root resets the layout focus so the view is anchored to the new root.
        self.layout_focus = None;
        log::info!("CoreApp: root changed to {:?}, layout_focus cleared", id);
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
        // Direction change resets the layout focus to the workspace root.
        self.layout_focus = None;
        if let Some(ws) = self.workspaces.current_mut() {
            ws.direction = match ws.direction {
                CallDirection::Outgoing => CallDirection::Incoming,
                CallDirection::Incoming => CallDirection::Outgoing,
            };
            log::info!(
                "CoreApp: direction toggled to {:?}, layout_focus cleared",
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
        let Some(entry) = self.clustering.by_key(key).and_then(|c| c.entry_point) else {
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
        // Shift the layout BFS window to this node so its children become visible
        // without changing the workspace root_symbol.
        //
        // If the user clicks the same already-loaded node again, layout_focus
        // is unchanged and nothing needs to be done — avoid a redundant snapshot.
        if self.layout_focus == Some(id) && self.lsp.is_function_loaded(&id) {
            log::info!(
                "CoreApp: expand in-place {:?} — already focused and loaded, no-op",
                id
            );
            return vec![];
        }
        self.layout_focus = Some(id);
        log::info!("CoreApp: expand in-place {:?}, layout_focus set", id);
        // Unlike SelectFunction, do NOT change root_symbol — expand in place.
        if !self.lsp.is_function_loaded(&id) && !self.lsp.is_function_loading(&id) {
            self.lsp.set_loading_state(id, LoadingState::Loading);
            if let Some(ev) = self.lsp.send_call_hierarchy(&self.call_graph, &id) {
                return vec![ev, AppEvent::LayoutDirty];
            }
        }
        vec![AppEvent::LayoutDirty]
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
        let load_finished = matches!(self.lsp.status, LspLoadPhase::Completed);
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
        let qualified_name = match other_detail {
            Some(detail) if !detail.is_empty() => format!("{}::{}", detail, other_name),
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

    #[test]
    fn every_way_the_graph_grows_marks_the_cache_dirty() {
        let mut app = CoreApp::new(CallGraph::new());
        assert!(!app.clustering_dirty);

        app.lsp.pending_requests.insert(
            "symbols".to_string(),
            PendingRequest {
                symbol_id: None,
                timestamp: Instant::now(),
            },
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
            PendingRequest {
                symbol_id: Some(caller),
                timestamp: Instant::now(),
            },
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
}
