//! Frontend-agnostic wire protocol shared by every out-of-process frontend.
//!
//! Both the web frontend (JSON over WebSocket) and the desktop frontend
//! (JSON over Tauri IPC) speak exactly these message shapes, which is what
//! lets them share a single JavaScript client: only the transport differs.
//!
//! The TUI does not use this module — it calls `CoreApp` directly and needs
//! no serialisation hop.

use std::collections::HashMap;

use logic::ClusterKey;
use model::{CallDirection, CallGraph, LspLoadPhase, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::{
    compute_cluster_layout, compute_layout, GraphLayout, LayoutOptions, LayoutOrientation, NodePos,
};
use crate::workspace::ViewMode;

/// Messages the backend sends to the client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full state: graph structure, node positions, workspaces, selection.
    Snapshot {
        graph: CallGraph,
        layout: HashMap<SymbolId, NodePos>,
        /// The cluster map, present only while the active workspace is showing
        /// one. It is a second, parallel layout rather than a replacement for
        /// `layout`: the two views draw different things out of the same graph,
        /// and a client that knows nothing about clusters goes on reading
        /// `layout` and never sees this field at all.
        #[serde(skip_serializing_if = "Option::is_none")]
        cluster_layout: Option<GraphLayout>,
        workspaces: Vec<WorkspaceInfo>,
        /// Index into `workspaces` of the active tab. The workspace commands
        /// (`SwitchWorkspace`, `CloseWorkspace`, `RenameWorkspace`) address
        /// tabs by index, so the client needs the index, not the id.
        active_workspace: usize,
        selected: Option<SymbolId>,
    },
    /// A single `AppEvent` forwarded verbatim for status display.
    Event { event: AppEvent },
    /// New positions only, reusing the client's cached graph structure.
    Layout { layout: HashMap<SymbolId, NodePos> },
}

/// Messages the client sends to the backend.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Command { command: Command },
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: usize,
    pub name: String,
    pub direction: CallDirection,
    pub selected: Option<SymbolId>,
    /// Levels of callees/callers currently shown.
    pub depth: usize,
    /// Nodes the user has collapsed, so the client can mark them.
    pub collapsed: Vec<SymbolId>,
    /// Which way the graph flows on the canvas.
    pub orientation: LayoutOrientation,
    /// Which view this workspace is showing — the cluster map or a rooted tree.
    pub view: ViewMode,
    /// Clusters the user has opened on the map, so the client can mark them.
    /// These are the keys the client sends back, so they have to survive a
    /// recompute the client did not see.
    pub expanded_clusters: Vec<ClusterKey>,
    /// The function this workspace's cluster map is drawn from, or `null` for
    /// the whole program. The client shows it — a map of one function's
    /// neighbourhood and a map of the project look identical otherwise — and
    /// offers the way back out to the whole program.
    pub map_scope: Option<SymbolId>,
}

/// Build a full `ServerMsg::Snapshot` from current `CoreApp` state.
pub fn snapshot_of(core: &CoreApp) -> ServerMsg {
    let ws = core.workspaces.current();
    let root = ws.and_then(|w| w.root_symbol);
    let direction = ws.map(|w| w.direction).unwrap_or(CallDirection::Outgoing);
    // Use layout_focus (set by ExpandFunction) as the BFS origin when present,
    // so the layout window shifts to the expanded node rather than staying
    // anchored to the workspace root.
    let layout_root = core.layout_focus.or(root);
    let layout = compute_layout(
        &core.call_graph,
        layout_root,
        direction,
        &layout_options(core),
    );

    log::info!(
        "[protocol] snapshot: nodes={} edges={} layout_nodes={} root={:?} layout_focus={:?} direction={:?} selected={:?}",
        core.call_graph.node_count(),
        core.call_graph.edge_count(),
        layout.len(),
        root,
        core.layout_focus,
        direction,
        core.selected_function,
    );

    ServerMsg::Snapshot {
        graph: core.call_graph.clone(),
        layout,
        cluster_layout: cluster_layout_of(core),
        workspaces: workspace_infos(core),
        active_workspace: core.workspaces.current_index,
        selected: core.selected_function,
    }
}

/// Build a positions-only `ServerMsg::Layout` from current `CoreApp` state.
pub fn layout_of(core: &CoreApp) -> ServerMsg {
    let ws = core.workspaces.current();
    let root = ws.and_then(|w| w.root_symbol);
    let direction = ws.map(|w| w.direction).unwrap_or(CallDirection::Outgoing);
    ServerMsg::Layout {
        layout: compute_layout(&core.call_graph, root, direction, &layout_options(core)),
    }
}

/// The cluster map for the active workspace, or `None` when it is showing the
/// rooted call tree instead.
///
/// Computing it is not free — it walks every call in the graph — so it is done
/// only for the view that is actually on screen. A workspace the user has
/// drilled out of is showing a call tree and pays nothing for the map.
///
/// Which communities it draws is the workspace's own business: a map scoped to
/// a function has its own clustering, and `clustering_for` is what knows which.
fn cluster_layout_of(core: &CoreApp) -> Option<GraphLayout> {
    let ws = core.workspaces.current()?;
    if ws.view != ViewMode::ClusterMap {
        return None;
    }
    Some(compute_cluster_layout(
        &core.call_graph,
        core.clustering_for(ws),
        &ws.expanded_clusters,
        ws.direction,
        ws.orientation,
    ))
}

/// Layout options taken from the active workspace.
fn layout_options(core: &CoreApp) -> LayoutOptions<'_> {
    match core.workspaces.current() {
        Some(ws) => LayoutOptions {
            depth: ws.depth,
            collapsed: &ws.collapsed,
            orientation: ws.orientation,
        },
        None => LayoutOptions::default(),
    }
}

pub fn workspace_infos(core: &CoreApp) -> Vec<WorkspaceInfo> {
    core.workspaces
        .workspaces
        .iter()
        .map(|ws| WorkspaceInfo {
            id: ws.id,
            name: ws.name.clone(),
            direction: ws.direction,
            selected: ws.root_symbol,
            depth: ws.depth,
            collapsed: ws.collapsed.iter().copied().collect(),
            orientation: ws.orientation,
            view: ws.view,
            expanded_clusters: ws.expanded_clusters.iter().copied().collect(),
            map_scope: ws.map_scope,
        })
        .collect()
}

/// What a batch of `AppEvent`s means for the client.
pub struct Routed {
    /// Events to forward individually as `ServerMsg::Event`.
    pub forward: Vec<AppEvent>,
    /// Whether a fresh full snapshot should follow.
    pub resnapshot: bool,
}

/// Translates `AppEvent` batches into client-facing messages.
///
/// Stateful because `LspProgress(Completed)` latches — the phase stays
/// `Completed` forever once loading finishes, so without a gate every tick
/// would trigger a resnapshot indefinitely.
#[derive(Debug, Default)]
pub struct EventRouter {
    lsp_completed_sent: bool,
    /// The last warmup fraction handed to the client.
    ///
    /// Kept only so the log can record what the status bar was actually told:
    /// `ResolvingCalls` goes out on every tick, so without this the log would
    /// either say nothing or say it ten times a second — and the question a
    /// stuck "Resolving calls 15/19" raises is precisely whether the client was
    /// ever sent anything past 15.
    last_resolving: Option<(usize, usize)>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(&mut self, events: &[AppEvent]) -> Routed {
        let mut forward = Vec::new();
        let mut resnapshot = false;

        for event in events {
            match event {
                // Consumed, not forwarded: the snapshot that follows carries
                // the new positions along with the graph structure.
                AppEvent::LayoutDirty => {
                    resnapshot = true;
                }
                // Forwarded exactly once, and that once matters: the client
                // treats every phase before `Completed` as "stage 1 is still
                // running", so a client that never sees it keeps a loading
                // stage open forever — and the status line goes on painting the
                // last fraction it was given long after the load has finished.
                // Once, not every tick, because the phase latches: re-sending it
                // ten times a second would stamp "Ready" over anything else the
                // status line had to say.
                AppEvent::LspProgress(LspLoadPhase::Completed) => {
                    if !self.lsp_completed_sent {
                        log::info!(
                            "[protocol] LspProgress(Completed) — forwarded once, with a resnapshot"
                        );
                        self.lsp_completed_sent = true;
                        resnapshot = true;
                        forward.push(event.clone());
                    }
                }
                // Graph is now fully populated — resnapshot so the client can
                // show the function picker with all loaded symbols. Deliberately
                // not also forwarded as an Event: the snapshot supersedes it and
                // a double-send can leave the client inconsistent mid-flight.
                AppEvent::GraphSymbolsUpdated { .. } => {
                    log::info!("[protocol] {:?} — scheduling resnapshot", event);
                    resnapshot = true;
                }
                // A regrouping changes what the cluster map is made of — which
                // communities exist, what they are called, how many functions
                // each stands for. None of that is derivable from the graph the
                // client already holds, so it has to come down in a snapshot.
                // Forwarded as well, so the status line can say the map moved
                // rather than leaving it to redraw unannounced.
                AppEvent::ClusteringUpdated { .. } => {
                    log::info!("[protocol] {event:?} — regrouped, resnapshot");
                    resnapshot = true;
                    forward.push(event.clone());
                }
                // Switching, creating or closing a tab changes the root the
                // layout is anchored to, but `CoreApp` emits no LayoutDirty for
                // it. Resnapshot so the client re-lays out, and forward the
                // event too so the status line can report it.
                AppEvent::WorkspaceCreated { .. }
                | AppEvent::WorkspaceSwitched { .. }
                | AppEvent::WorkspaceClosed
                | AppEvent::WorkspaceRenamed { .. } => {
                    log::info!("[protocol] {:?} — workspace change, resnapshot", event);
                    resnapshot = true;
                    forward.push(event.clone());
                }
                AppEvent::ResolvingCalls { done, total } => {
                    if self.last_resolving != Some((*done, *total)) {
                        self.last_resolving = Some((*done, *total));
                        log::info!("[protocol] ResolvingCalls {done}/{total} — sent to client");
                    }
                    forward.push(event.clone());
                }
                _ => forward.push(event.clone()),
            }
        }

        Routed {
            forward,
            resnapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{CallGraph, SymbolId};

    fn sym() -> SymbolId {
        SymbolId::from_content("test::fn", "src/test.cpp", 1)
    }

    #[test]
    fn ordinary_events_are_forwarded_verbatim() {
        let mut router = EventRouter::new();
        let routed = router.route(&[
            AppEvent::FunctionHasNoCallees,
            AppEvent::ReferencesFound {
                symbol_id: sym(),
                count: 3,
            },
        ]);

        assert_eq!(routed.forward.len(), 2);
        assert!(!routed.resnapshot);
    }

    #[test]
    fn layout_dirty_triggers_a_snapshot_instead_of_being_forwarded() {
        let mut router = EventRouter::new();
        let routed = router.route(&[AppEvent::LayoutDirty]);

        assert!(routed.forward.is_empty(), "LayoutDirty must be consumed");
        assert!(routed.resnapshot);
    }

    #[test]
    fn graph_symbols_updated_snapshots_without_double_sending() {
        let mut router = EventRouter::new();
        let routed = router.route(&[AppEvent::GraphSymbolsUpdated { count: 42 }]);

        assert!(
            routed.forward.is_empty(),
            "the snapshot supersedes the event; sending both can leave the client inconsistent"
        );
        assert!(routed.resnapshot);
    }

    #[test]
    fn a_regrouping_snapshots_and_says_so() {
        let mut router = EventRouter::new();
        let routed = router.route(&[AppEvent::ClusteringUpdated { clusters: 6 }]);

        assert!(
            routed.resnapshot,
            "which communities exist is not derivable from the graph the client holds"
        );
        assert_eq!(
            routed.forward.len(),
            1,
            "unlike a symbol refresh, the map moving under the user is worth reporting"
        );
    }

    #[test]
    fn lsp_completed_snapshots_once_and_then_latches() {
        let mut router = EventRouter::new();

        let first = router.route(&[AppEvent::LspProgress(LspLoadPhase::Completed)]);
        assert!(
            first.resnapshot,
            "the transition to Completed must snapshot"
        );
        assert_eq!(
            first.forward.len(),
            1,
            "the client needs this one to close its loading stage — without it \
             the status line never leaves the last fraction it was given"
        );

        // The phase stays Completed forever, so every later tick repeats the
        // event. Without the gate this would resnapshot at 10 Hz indefinitely.
        for _ in 0..5 {
            let later = router.route(&[AppEvent::LspProgress(LspLoadPhase::Completed)]);
            assert!(!later.resnapshot, "Completed must only snapshot once");
            assert!(later.forward.is_empty());
        }
    }

    #[test]
    fn in_progress_lsp_phases_are_forwarded_every_time() {
        let mut router = EventRouter::new();
        let routed = router.route(&[
            AppEvent::LspProgress(LspLoadPhase::LoadingWorkspaceSymbols { loaded: 10 }),
            AppEvent::LspProgress(LspLoadPhase::LoadingWorkspaceSymbols { loaded: 20 }),
        ]);

        assert_eq!(routed.forward.len(), 2, "progress drives the status line");
        assert!(!routed.resnapshot);
    }

    /// Every command shape the JavaScript client sends, verbatim.
    ///
    /// The client builds these by hand as JSON, so serde's enum representation
    /// is a real contract between the two. A rename of a `Command` variant
    /// compiles fine and silently breaks the UI; this catches that.
    #[test]
    fn client_command_shapes_deserialize() {
        let id = format!("\"{}\"", sym().0);
        let cases = [
            format!("{{\"SelectFunction\":{id}}}"),
            format!("{{\"ExpandFunction\":{id}}}"),
            format!("{{\"StartGraphFromFunction\":{id}}}"),
            "{\"CloseWorkspace\":0}".to_string(),
            "{\"SwitchWorkspace\":2}".to_string(),
            "{\"RenameWorkspace\":[0,\"renamed\"]}".to_string(),
            "\"NewWorkspace\"".to_string(),
            "\"NextWorkspace\"".to_string(),
            "\"PreviousWorkspace\"".to_string(),
            "\"ToggleCallDirection\"".to_string(),
            "\"Refresh\"".to_string(),
            "{\"SetGraphDepth\":4}".to_string(),
            format!("{{\"ToggleCollapsed\":{id}}}"),
            "{\"SetGraphOrientation\":\"TopDown\"}".to_string(),
            "{\"SetGraphOrientation\":\"LeftRight\"}".to_string(),
            "{\"SetViewMode\":\"ClusterMap\"}".to_string(),
            "{\"SetViewMode\":\"Rooted\"}".to_string(),
            format!("{{\"SetMapScope\":{id}}}"),
            "{\"SetMapScope\":null}".to_string(),
            format!("{{\"ToggleClusterExpanded\":{{\"level\":0,\"anchor\":{id}}}}}"),
            format!("{{\"DrillIntoCluster\":{{\"level\":2,\"anchor\":{id}}}}}"),
            "\"ZoomOutToClusters\"".to_string(),
        ];

        for case in cases {
            serde_json::from_str::<Command>(&case)
                .unwrap_or_else(|e| panic!("client sends {case}, which no longer parses: {e}"));
        }
    }

    /// The client reads `msg.type`, `msg.active_workspace` and `msg.workspaces`
    /// off the snapshot; pin those names.
    #[test]
    fn snapshot_wire_shape_is_stable() {
        let core = CoreApp::new(CallGraph::new());
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");

        assert_eq!(json["type"], "snapshot");
        assert_eq!(json["active_workspace"], 0);
        assert!(json["workspaces"].is_array());
        assert!(json.get("graph").is_some());
        assert!(json.get("layout").is_some());

        let ws = &json["workspaces"][0];
        for field in [
            "id",
            "name",
            "direction",
            "selected",
            "depth",
            "collapsed",
            "orientation",
            "view",
            "expanded_clusters",
            "map_scope",
        ] {
            assert!(
                ws.get(field).is_some(),
                "workspace field `{field}` is read by the client"
            );
        }
    }

    /// The cluster map rides along on the snapshot rather than replacing the
    /// rooted layout, and only for the view that is actually showing.
    #[test]
    fn the_cluster_map_rides_along_only_for_the_view_showing_it() {
        let mut core = CoreApp::new(crate::demo::create_demo_call_graph());

        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        let map = json
            .get("cluster_layout")
            .expect("a fresh workspace opens on the map");
        assert!(
            map["nodes"].as_object().is_some_and(|n| !n.is_empty()),
            "and the map has the communities on it"
        );
        assert!(
            json.get("layout").is_some(),
            "the rooted layout still ships"
        );

        core.workspaces
            .current_mut()
            .expect("a default workspace")
            .set_view(ViewMode::Rooted);

        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert!(
            json.get("cluster_layout").is_none(),
            "a rooted workspace must send exactly what it sent before"
        );
    }

    /// A scoped map ships the same way the whole-program one does, drawn from
    /// the clustering that workspace asked for.
    #[test]
    fn a_scoped_map_ships_only_the_neighbourhood_it_was_scoped_to() {
        use crate::command::Command;

        let mut core = CoreApp::new(crate::demo::create_demo_call_graph());
        let scope = core
            .call_graph
            .find_function_by_name("handle_client")
            .expect("the demo serves clients")
            .id;
        let reachable: std::collections::HashSet<SymbolId> =
            logic::GraphTraversal::new(&core.call_graph)
                .find_reachable_from(&scope)
                .into_iter()
                .collect();
        assert!(
            reachable.len() < core.call_graph.node_count(),
            "a scope worth testing leaves something out"
        );

        core.dispatch(Command::SetMapScope(Some(scope)));
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");

        assert_eq!(
            json["workspaces"][0]["map_scope"],
            serde_json::to_value(scope).expect("a symbol id serialises"),
            "the client is told which map this is"
        );

        let map = json
            .get("cluster_layout")
            .expect("scoping the map is asking for the map");
        let drawn = map["nodes"].as_object().expect("nodes is an object");
        assert!(
            !drawn.is_empty(),
            "and the neighbourhood has communities in it"
        );
        for id in drawn.keys() {
            let Some(symbol) = id.strip_prefix("fn:") else {
                continue; // a closed community, named by its key
            };
            let symbol = SymbolId(symbol.parse().expect("a uuid"));
            assert!(
                reachable.contains(&symbol),
                "{symbol:?} is not reachable from the scope and has no place on its map"
            );
        }
    }

    #[test]
    fn workspace_changes_resnapshot_and_forward() {
        for event in [
            AppEvent::WorkspaceCreated {
                id: 2,
                name: "Graph 2".to_string(),
            },
            AppEvent::WorkspaceSwitched {
                id: 1,
                name: "Graph 1".to_string(),
            },
            AppEvent::WorkspaceClosed,
            AppEvent::WorkspaceRenamed {
                name: "renamed".to_string(),
            },
        ] {
            let mut router = EventRouter::new();
            let routed = router.route(std::slice::from_ref(&event));
            assert!(
                routed.resnapshot,
                "{event:?} changes the displayed graph and must resnapshot"
            );
            assert_eq!(
                routed.forward.len(),
                1,
                "{event:?} should still be reported"
            );
        }
    }

    #[test]
    fn a_mixed_batch_forwards_and_snapshots_together() {
        let mut router = EventRouter::new();
        let routed = router.route(&[
            AppEvent::LoadingCalls {
                direction: "Outgoing".to_string(),
            },
            AppEvent::LayoutDirty,
            AppEvent::FunctionHasNoCallees,
        ]);

        assert_eq!(routed.forward.len(), 2);
        assert!(routed.resnapshot);
    }
}
