use lsp::LspResponse;
use model::{CallDirection, CallGraph, FunctionNode, Location, SymbolId};

use crate::command::Command;
use crate::events::AppEvent;
use crate::lsp_bridge::{LoadingState, LspBridge};
use crate::workspace::CoreWorkspaceManager;

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
}

impl CoreApp {
    /// Create a `CoreApp` in demo/offline mode (no LSP).
    pub fn new(call_graph: CallGraph) -> Self {
        Self {
            call_graph,
            selected_function: None,
            layout_focus: None,
            lsp: LspBridge::new(),
            workspaces: CoreWorkspaceManager::new(),
        }
    }

    /// Create a `CoreApp` with an active LSP bridge.
    pub fn new_with_lsp(call_graph: CallGraph, lsp: LspBridge) -> Self {
        Self {
            call_graph,
            selected_function: None,
            layout_focus: None,
            lsp,
            workspaces: CoreWorkspaceManager::new(),
        }
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
    }
}
