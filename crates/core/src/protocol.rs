//! Frontend-agnostic wire protocol shared by every out-of-process frontend.
//!
//! Both the web frontend (JSON over WebSocket) and the desktop frontend
//! (JSON over Tauri IPC) speak exactly these message shapes, which is what
//! lets them share a single JavaScript client: only the transport differs.
//!
//! The TUI does not use this module — it calls `CoreApp` directly and needs
//! no serialisation hop.

use std::collections::HashMap;

use model::{CallDirection, CallGraph, LspLoadPhase, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::{compute_layout, LayoutOptions, NodePos};

/// Messages the backend sends to the client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full state: graph structure, node positions, workspaces, selection.
    Snapshot {
        graph: CallGraph,
        layout: HashMap<SymbolId, NodePos>,
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
    Layout {
        layout: HashMap<SymbolId, NodePos>,
    },
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
    let layout = compute_layout(&core.call_graph, layout_root, direction, &layout_options(core));

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

/// Layout options taken from the active workspace.
fn layout_options(core: &CoreApp) -> LayoutOptions<'_> {
    match core.workspaces.current() {
        Some(ws) => LayoutOptions {
            depth: ws.depth,
            collapsed: &ws.collapsed,
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
                AppEvent::LspProgress(LspLoadPhase::Completed) => {
                    if !self.lsp_completed_sent {
                        log::info!("[protocol] LspProgress(Completed) — one-time resnapshot");
                        self.lsp_completed_sent = true;
                        resnapshot = true;
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
    fn lsp_completed_snapshots_once_and_then_latches() {
        let mut router = EventRouter::new();

        let first = router.route(&[AppEvent::LspProgress(LspLoadPhase::Completed)]);
        assert!(first.resnapshot, "the transition to Completed must snapshot");

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
        for field in ["id", "name", "direction", "selected", "depth", "collapsed"] {
            assert!(ws.get(field).is_some(), "workspace field `{field}` is read by the client");
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
            assert_eq!(routed.forward.len(), 1, "{event:?} should still be reported");
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
