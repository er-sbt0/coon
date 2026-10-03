//! Frontend-agnostic wire protocol shared by every out-of-process frontend.
//!
//! Both the web frontend (JSON over WebSocket) and the desktop frontend
//! (JSON over Tauri IPC) speak exactly these message shapes, which is what
//! lets them share a single JavaScript client: only the transport differs.
//!
//! The TUI does not use this module — it calls `CoreApp` directly and needs
//! no serialisation hop.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use logic::{ClusterMethod, ClusterSettings, SettingSpec};
use model::{CallDirection, CallGraph, LspLoadPhase, ParseQuality, ParseSummary, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::{
    compute_cluster_layout, compute_layout, GraphDepth, GraphLayout, LayoutOptions,
    LayoutOrientation, NodePos,
};
use crate::workspace::ViewMode;

/// While the bootstrap edge warmup runs, at most one snapshot this often.
///
/// Long enough that the client is not rebuilding its canvas and sidebar on a
/// treadmill, short enough that a project whose map does settle mid-load shows
/// it. Loading is reported by the status bar, not by the graph redrawing.
const LOADING_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// The call graph as the client reads it.
///
/// Deliberately not `CallGraph`. The client never touches a node's `references`
/// or `diagnostics`, nor an edge's `call_location`, and shipping them cost more
/// than everything it does read put together: on jemalloc a late-bootstrap
/// snapshot was 5.97 MB of JSON, of which 4.63 MB was call sites it drops on
/// the floor. It borrows rather than owns for the same reason — building one
/// used to mean deep-copying the whole graph on the way to serialising it.
///
/// The field names are the ones the JavaScript already reads. Shortening them
/// would save a little more and cost the client a rewrite.
#[derive(Debug, Serialize)]
pub struct WireGraph<'a> {
    /// Keyed by symbol id, which is how the client indexes `allFunctions[id]`.
    pub nodes: HashMap<&'a SymbolId, WireNode<'a>>,
    /// One entry per caller/callee pair rather than per call site. A function
    /// calling another from five places is five edges in the graph and one line
    /// on the canvas — the client was already deduplicating them, after paying
    /// to receive and parse all five.
    pub edges: Vec<WireEdge<'a>>,
}

/// A function, as much of one as the client draws and searches.
#[derive(Debug, Serialize)]
pub struct WireNode<'a> {
    pub name: &'a str,
    pub qualified_name: &'a str,
    pub definition_location: WireLocation<'a>,
}

/// Where a symbol is defined — the two fields the sidebar reads. `column` and
/// `length` are on the model's `Location` and go nowhere on the client.
#[derive(Debug, Serialize)]
pub struct WireLocation<'a> {
    pub file_path: &'a str,
    pub line: u32,
}

/// A call, as an edge on the canvas: the two ends and nothing else.
#[derive(Debug, Serialize)]
pub struct WireEdge<'a> {
    pub caller: &'a SymbolId,
    pub callee: &'a SymbolId,
}

/// Borrow the graph in the shape the client reads it.
pub fn wire_graph(graph: &CallGraph) -> WireGraph<'_> {
    let nodes = graph
        .functions()
        .map(|f| {
            (
                &f.id,
                WireNode {
                    name: &f.name,
                    qualified_name: &f.qualified_name,
                    definition_location: WireLocation {
                        file_path: &f.definition_location.file_path,
                        line: f.definition_location.line,
                    },
                },
            )
        })
        .collect();

    let mut seen = HashSet::with_capacity(graph.edge_count());
    let edges = graph
        .edges()
        .filter(|e| seen.insert((e.caller, e.callee)))
        .map(|e| WireEdge {
            caller: &e.caller,
            callee: &e.callee,
        })
        .collect();

    WireGraph { nodes, edges }
}

/// Messages the backend sends to the client.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg<'a> {
    /// Full state: graph structure, node positions, workspaces, selection.
    Snapshot {
        graph: WireGraph<'a>,
        /// Where the rooted call tree's nodes sit, present only while the
        /// active workspace is showing that tree. Absent on the cluster map
        /// for the same reason `cluster_layout` is absent off it: a view that
        /// is not on screen is not laid out.
        #[serde(skip_serializing_if = "Option::is_none")]
        layout: Option<HashMap<SymbolId, NodePos>>,
        /// The cluster map, present only while the active workspace is showing
        /// one. It is a second, parallel layout rather than a replacement for
        /// `layout`: the two views draw different things out of the same graph,
        /// and a client that knows nothing about clusters goes on reading
        /// `layout` and never sees this field at all.
        #[serde(skip_serializing_if = "Option::is_none")]
        cluster_layout: Option<GraphLayout>,
        /// The algorithms the map can be built with, in the order a picker
        /// should list them.
        ///
        /// Constant, and sent anyway: the alternative is the client carrying
        /// its own copy of the list, which is how a method added in `logic`
        /// comes to be one the user cannot reach. A handful of bytes per
        /// snapshot against a dropdown that is never out of date.
        cluster_methods: Vec<MethodInfo>,
        /// How wide the coarsest rung may be — the one setting that belongs to
        /// no single method, so it travels beside the list rather than inside
        /// it, on the same terms.
        cluster_top: SettingSpec,
        workspaces: Vec<WorkspaceInfo>,
        /// Index into `workspaces` of the active tab. The workspace commands
        /// (`SwitchWorkspace`, `CloseWorkspace`, `RenameWorkspace`) address
        /// tabs by index, so the client needs the index, not the id.
        active_workspace: usize,
        selected: Option<SymbolId>,
        /// How much of the build was parsed, under a backend that parses it.
        ///
        /// On the snapshot rather than on an event because it is a fact about
        /// the graph on screen, not a step of getting there: a client that
        /// connects or reloads after the load has to be told it too. Absent
        /// under clangd and in demo mode, where the status bar says "Ready".
        #[serde(skip_serializing_if = "Option::is_none")]
        parse: Option<WireParse<'a>>,
        /// What the backend can be asked after the load, so a client can leave
        /// out a command this one cannot answer instead of offering it and
        /// saying nothing. On the snapshot for the same reason `parse` is.
        capabilities: crate::lsp_bridge::BackendCapabilities,
    },
    /// A single `AppEvent` forwarded verbatim for status display.
    Event { event: AppEvent },
}

/// A [`ParseSummary`] with the verdict on it, so the client decides how loudly
/// to show it by reading a word rather than by keeping its own thresholds.
#[derive(Debug, Serialize)]
pub struct WireParse<'a> {
    #[serde(flatten)]
    pub summary: &'a ParseSummary,
    pub quality: ParseQuality,
}

impl<'a> WireParse<'a> {
    pub fn of(summary: &'a ParseSummary) -> Self {
        Self {
            summary,
            quality: summary.quality(),
        }
    }
}

/// Messages the client sends to the backend.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Command { command: Command },
}

/// One entry in the method picker.
#[derive(Debug, Serialize)]
pub struct MethodInfo {
    /// The value to send back in `Command::SetClusterMethod`.
    pub id: ClusterMethod,
    pub label: &'static str,
    pub description: &'static str,
    /// The one knob this method reads — which field it writes, what to call
    /// it, and the range to draw. Shipped rather than written into the client
    /// for the reason the method list itself is: a knob retuned in `logic` is
    /// then retuned everywhere, and a method added there arrives with its own
    /// control already drawn.
    pub grain: SettingSpec,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: usize,
    pub name: String,
    pub direction: CallDirection,
    pub selected: Option<SymbolId>,
    /// Levels of callees/callers currently shown, or `null` when the view is
    /// walked with no bound. A client draws the number it is given and the
    /// absent bound as whatever its stepper calls "all".
    pub depth: GraphDepth,
    /// Nodes the user has collapsed, so the client can mark them.
    pub collapsed: Vec<SymbolId>,
    /// Which way the graph flows on the canvas.
    pub orientation: LayoutOrientation,
    /// Which view this workspace is showing — the cluster map or a rooted tree.
    pub view: ViewMode,
    /// Which rung of the coarsening ladder the map is drawn at, already
    /// resolved against the clustering that exists — so this is the rung the
    /// client is looking at, never the wish the workspace stored.
    pub map_level: usize,
    /// How many rungs there are. With `map_level` this is the zoom control:
    /// 0 is the finest map and `map_levels - 1` the coarsest. Zero means
    /// nothing is clustered yet, and the control has nothing to offer.
    pub map_levels: usize,
    /// How many clusters the rung being shown holds — the number the zoom
    /// control reads out, and the reason a user picks one rung over another.
    pub map_clusters: usize,
    /// How many clusters every rung holds, finest first, so a control can
    /// offer the ladder as the thing it is: a choice between grains, each one
    /// a box count. `map_clusters` is this indexed by `map_level`, kept
    /// because it is what a control with no room for the rest shows.
    pub map_level_sizes: Vec<usize>,
    /// The rung the method itself stands behind — plain modularity at the
    /// chosen γ, or MCL at the chosen inflation. Sent so a control can mark
    /// it: the rungs above it merge modules that did not want to merge, to fit
    /// a box count, and a reader of the map deserves to know which grain is
    /// the method's answer and which is the target's.
    pub map_opening_level: usize,
    /// Which algorithm built the map.
    pub method: ClusterMethod,
    /// And what it was built to — the method again, with the knobs that method
    /// reads. `method` is the same value, kept because most of the client
    /// wants only that much.
    pub cluster: ClusterSettings,
    /// The function this workspace's cluster map is drawn from, or `null` for
    /// the whole program. The client shows it — a map of one function's
    /// neighbourhood and a map of the project look identical otherwise — and
    /// offers the way back out to the whole program.
    pub map_scope: Option<SymbolId>,
}

/// Build a full `ServerMsg::Snapshot` from current `CoreApp` state.
///
/// Borrows the app for as long as the message lives: the message is written
/// straight out to a socket or an IPC channel, and copying a project's worth of
/// symbols to get there was pure cost.
pub fn snapshot_of(core: &CoreApp) -> ServerMsg<'_> {
    let ws = core.workspaces.current();
    let root = ws.and_then(|w| w.root_symbol());
    let direction = ws.map(|w| w.direction()).unwrap_or(CallDirection::Outgoing);
    let layout = layout_of(core);

    log::info!(
        "[protocol] snapshot: nodes={} edges={} layout_nodes={} root={:?} expanded={} direction={:?} selected={:?}",
        core.call_graph.node_count(),
        core.call_graph.edge_count(),
        layout.as_ref().map_or(0, |l| l.len()),
        root,
        ws.map_or(0, |w| w.expanded().len()),
        direction,
        core.selected_function,
    );

    ServerMsg::Snapshot {
        graph: wire_graph(&core.call_graph),
        layout,
        cluster_layout: cluster_layout_of(core),
        cluster_methods: ClusterMethod::ALL
            .iter()
            .map(|&id| MethodInfo {
                id,
                label: id.label(),
                description: id.description(),
                grain: id.grain_spec(),
            })
            .collect(),
        cluster_top: ClusterSettings::TARGET_TOP,
        workspaces: workspace_infos(core),
        active_workspace: core.workspaces.current_index,
        selected: core.selected_function,
        parse: core.lsp.parse_summary().map(WireParse::of),
        capabilities: core.capabilities(),
    }
}

/// The rooted call tree's node positions for the active workspace, or `None`
/// when it is showing the cluster map instead.
///
/// Computing it is not free — a BFS out of the root and then a full Sugiyama
/// pass over what it reached — and on the map the client throws the answer
/// away: `handleSnapshot` hands `layout` to `rebuildGraph` on the rooted
/// branch and nowhere else. A workspace opens on the map ([`ViewMode`]'s
/// default) and goes back to it whenever the user zooms out, so paying for
/// both views on every snapshot was paying for the one nobody asked to see.
///
/// `None` rather than an empty map, exactly as [`cluster_layout_of`] answers:
/// the two say the same thing in the same way, that this view is not the one
/// being drawn. An empty map is a different statement — a rooted workspace
/// with no root yet, which is a real state and still ships as `Some`.
///
/// The BFS origin is the workspace root and only ever that. Expanding a node
/// does not move it — it widens the walk at that node (see
/// `LayoutOptions::expanded`), so the graph grows rather than changing.
fn layout_of(core: &CoreApp) -> Option<HashMap<SymbolId, NodePos>> {
    let ws = core.workspaces.current()?;
    if ws.view() != ViewMode::Rooted {
        return None;
    }
    Some(compute_layout(
        &core.call_graph,
        ws.root_symbol(),
        ws.direction(),
        &layout_options(core),
    ))
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
    if ws.view() != ViewMode::ClusterMap {
        return None;
    }
    Some(compute_cluster_layout(
        &core.call_graph,
        core.clustering_for(ws),
        core.map_frame(),
        core.map_level_for(ws),
        ws.direction(),
        ws.orientation(),
    ))
}

/// Layout options taken from the active workspace.
pub(crate) fn layout_options(core: &CoreApp) -> LayoutOptions<'_> {
    match core.workspaces.current() {
        Some(ws) => LayoutOptions {
            depth: ws.depth(),
            collapsed: ws.collapsed(),
            expanded: ws.expanded(),
            orientation: ws.orientation(),
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
            direction: ws.direction(),
            selected: ws.root_symbol(),
            depth: ws.depth(),
            collapsed: ws.collapsed().iter().copied().collect(),
            orientation: ws.orientation(),
            view: ws.view(),
            map_level: core.map_level_for(ws),
            map_levels: core.clustering_for(ws).level_count(),
            map_clusters: core.clustering_for(ws).level(core.map_level_for(ws)).len(),
            map_level_sizes: {
                let clustering = core.clustering_for(ws);
                (0..clustering.level_count())
                    .map(|level| clustering.level(level).len())
                    .collect()
            },
            map_opening_level: core.clustering_for(ws).opening_level(),
            method: ws.method(),
            cluster: ws.cluster_settings(),
            map_scope: ws.map_scope(),
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
    /// Whether the bootstrap edge warmup is still running, as of the last
    /// `ResolvingCalls`. While it is, snapshots are rate-limited.
    warming_up: bool,
    /// A snapshot that fell due while the rate limit was holding. Never
    /// dropped — the next route past the interval sends it.
    snapshot_owed: bool,
    /// When the last snapshot went out, for that rate limit.
    last_snapshot_at: Option<Instant>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(&mut self, events: &[AppEvent]) -> Routed {
        let mut forward = Vec::new();
        // Kept apart because only one of them is worth making somebody wait
        // for. A user's redraw goes out the instant it is asked for; one the
        // background caused can be held.
        let mut user_resnapshot = false;
        let mut background_resnapshot = false;

        for event in events {
            match event {
                // Consumed, not forwarded: the snapshot that follows carries
                // the new positions along with the graph structure.
                AppEvent::LayoutDirty => {
                    // Only ever user-initiated now — the warmup's replies no
                    // longer emit one. See `RequestOrigin`.
                    user_resnapshot = true;
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
                        background_resnapshot = true;
                        forward.push(event.clone());
                    }
                }
                // Graph is now fully populated — resnapshot so the client can
                // show the function picker with all loaded symbols. Deliberately
                // not also forwarded as an Event: the snapshot supersedes it and
                // a double-send can leave the client inconsistent mid-flight.
                AppEvent::GraphSymbolsUpdated { .. } => {
                    log::info!("[protocol] {:?} — scheduling resnapshot", event);
                    background_resnapshot = true;
                }
                // A regrouping changes what the cluster map is made of — which
                // communities exist, what they are called, how many functions
                // each stands for. None of that is derivable from the graph the
                // client already holds, so it has to come down in a snapshot.
                // Forwarded as well, so the status line can say the map moved
                // rather than leaving it to redraw unannounced.
                AppEvent::ClusteringUpdated { .. } => {
                    log::info!("[protocol] {event:?} — regrouped, resnapshot");
                    background_resnapshot = true;
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
                    user_resnapshot = true;
                    forward.push(event.clone());
                }
                AppEvent::ResolvingCalls { done, total } => {
                    // The one thing that says whether the graph is still
                    // growing on its own, which is what the rate limit below is
                    // for. `LspProgress(Completed)` does not: the warmup only
                    // starts once the symbols are all in.
                    self.warming_up = *total > 0 && *done < *total;
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
            resnapshot: self.gate_snapshot(user_resnapshot, background_resnapshot),
        }
    }

    /// Hold the snapshot rate down while the graph is still filling in.
    ///
    /// A snapshot is the whole call graph, and the client answers one by tearing
    /// the canvas down, rebuilding it, and re-rendering a sidebar with every
    /// symbol in the project in it. During bootstrap that is far more often than
    /// there is anything new to see: the warmup grows the graph continuously,
    /// but the map the user is looking at only moves when a regrouping lands.
    ///
    /// Only what the background caused is ever held: a `LayoutDirty` or a tab
    /// change is somebody waiting on their own click, and making them wait out
    /// an interval to see it would be trading one stutter for another.
    ///
    /// Nothing is dropped. An owed snapshot is held, not discarded, and goes out
    /// on the first route past the interval — which is at most a tick away,
    /// since `CoreApp::tick` always emits at least an `LspProgress`. The warmup
    /// finishing clears `warming_up` and releases it immediately, and so does
    /// any user action in the meantime.
    fn gate_snapshot(&mut self, user: bool, background: bool) -> bool {
        self.snapshot_owed |= background;

        // A user waiting on their own click is never made to wait for the rate
        // limit; it also carries whatever the background owed along with it.
        let due = user
            || !self.warming_up
            || self
                .last_snapshot_at
                .is_none_or(|at| at.elapsed() >= LOADING_SNAPSHOT_INTERVAL);
        if !(user || self.snapshot_owed) || !due {
            return false;
        }

        self.snapshot_owed = false;
        self.last_snapshot_at = Some(Instant::now());
        true
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

    /// The rate limit holds background snapshots down while the warmup runs —
    /// but holds them, never drops them. A client that missed the update for
    /// good would be drawing a graph that no longer exists.
    #[test]
    fn a_snapshot_held_back_during_the_warmup_still_goes_out() {
        let mut router = EventRouter::new();

        // The warmup announces itself, and the first regrouping goes out at once.
        let first = router.route(&[
            AppEvent::ResolvingCalls {
                done: 1,
                total: 100,
            },
            AppEvent::ClusteringUpdated { clusters: 3 },
        ]);
        assert!(first.resnapshot, "nothing to wait behind yet");

        // Everything inside the interval is held.
        for done in 2..20 {
            let routed = router.route(&[
                AppEvent::ResolvingCalls { done, total: 100 },
                AppEvent::ClusteringUpdated { clusters: 3 },
            ]);
            assert!(!routed.resnapshot, "a second snapshot {done} ticks later");
        }
        assert!(router.snapshot_owed, "and it is owed, not forgotten");

        // The warmup finishing releases it on the spot, without waiting out the
        // rest of the interval: the graph has stopped moving, and what the
        // client holds is now out of date for good.
        let done = router.route(&[AppEvent::ResolvingCalls {
            done: 100,
            total: 100,
        }]);
        assert!(done.resnapshot, "the warmup is over; send what is owed");
        assert!(!router.snapshot_owed);
    }

    /// The rate limit is for growth nobody asked for. Somebody waiting on their
    /// own click is not made to wait out an interval to see it.
    #[test]
    fn a_users_own_redraw_is_never_held_back_by_the_rate_limit() {
        let mut router = EventRouter::new();
        router.route(&[
            AppEvent::ResolvingCalls {
                done: 1,
                total: 100,
            },
            // The first goes out at once — nothing to wait behind yet — and
            // starts the interval.
            AppEvent::ClusteringUpdated { clusters: 3 },
        ]);

        // Held: the next regrouping the warmup causes, inside the interval.
        assert!(
            !router
                .route(&[AppEvent::ClusteringUpdated { clusters: 4 }])
                .resnapshot
        );
        assert!(router.snapshot_owed);

        // Not held: expanding a node, mid-load, is an answer to a click.
        assert!(router.route(&[AppEvent::LayoutDirty]).resnapshot);
        assert!(
            !router.snapshot_owed,
            "and it carries the owed background snapshot out with it"
        );

        // Nor is switching tabs.
        assert!(
            router
                .route(&[AppEvent::WorkspaceSwitched {
                    id: 1,
                    name: "Graph 1".to_string(),
                }])
                .resnapshot
        );
    }

    /// Nothing is rate-limited when nothing is loading.
    #[test]
    fn the_rate_limit_only_applies_while_the_graph_is_still_filling_in() {
        let mut router = EventRouter::new();
        for _ in 0..5 {
            assert!(
                router
                    .route(&[AppEvent::ClusteringUpdated { clusters: 3 }])
                    .resnapshot
            );
        }
    }

    /// What the client actually reads off a node and an edge — and, just as
    /// much to the point, what it does not.
    #[test]
    fn the_graph_ships_what_the_client_reads_and_nothing_else() {
        use model::{FunctionNode, Location};

        let mut graph = CallGraph::new();
        let caller = graph.add_function(FunctionNode::new(
            "caller".to_string(),
            "ns::caller".to_string(),
            Location::new("/src/a.c".to_string(), 10, 4),
        ));
        let callee = graph.add_function(FunctionNode::new(
            "callee".to_string(),
            "ns::callee".to_string(),
            Location::new("/src/b.c".to_string(), 20, 4),
        ));
        // The same call from two places: two edges in the graph, one line on
        // the canvas.
        graph.add_call(caller, callee, Location::new("/src/a.c".to_string(), 11, 2));
        graph.add_call(caller, callee, Location::new("/src/a.c".to_string(), 12, 2));
        assert_eq!(graph.edge_count(), 2);

        let core = CoreApp::new(graph);
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        let node = &json["graph"]["nodes"][caller.0.to_string()];

        assert_eq!(node["name"], "caller");
        assert_eq!(node["qualified_name"], "ns::caller");
        assert_eq!(node["definition_location"]["file_path"], "/src/a.c");
        assert_eq!(node["definition_location"]["line"], 10);
        for unread in ["references", "diagnostics"] {
            assert!(
                node.get(unread).is_none(),
                "the client never reads `{unread}`, and on jemalloc these were \
                 most of a 6 MB message"
            );
        }

        let edges = json["graph"]["edges"].as_array().expect("an edge array");
        assert_eq!(
            edges.len(),
            1,
            "one line per caller/callee pair, not per call site"
        );
        assert_eq!(edges[0]["caller"], caller.0.to_string());
        assert_eq!(edges[0]["callee"], callee.0.to_string());
        assert!(
            edges[0].get("call_location").is_none(),
            "a full file path per call site, for an edge drawn by its two ends"
        );
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
            format!("{{\"FocusFunction\":{id}}}"),
            format!("{{\"StartGraphFromFunction\":{id}}}"),
            "{\"CloseWorkspace\":0}".to_string(),
            "{\"SwitchWorkspace\":2}".to_string(),
            "{\"RenameWorkspace\":[0,\"renamed\"]}".to_string(),
            "\"NewWorkspace\"".to_string(),
            "\"NextWorkspace\"".to_string(),
            "\"PreviousWorkspace\"".to_string(),
            "\"ToggleCallDirection\"".to_string(),
            "\"Refresh\"".to_string(),
            // The absolute direction, which is what a restored setting sends.
            "{\"SetCallDirection\":\"Incoming\"}".to_string(),
            "{\"SetCallDirection\":\"Outgoing\"}".to_string(),
            "{\"SetGraphDepth\":4}".to_string(),
            // The absent bound. `null` rather than a large number, so the
            // stepper's "all" position is not a magic value either end has to
            // agree on.
            "{\"SetGraphDepth\":null}".to_string(),
            format!("{{\"ToggleCollapsed\":{id}}}"),
            "{\"SetGraphOrientation\":\"TopDown\"}".to_string(),
            "{\"SetGraphOrientation\":\"LeftRight\"}".to_string(),
            "{\"SetViewMode\":\"ClusterMap\"}".to_string(),
            "{\"SetViewMode\":\"Rooted\"}".to_string(),
            format!("{{\"SetMapScope\":{id}}}"),
            "{\"SetMapScope\":null}".to_string(),
            "{\"SetMapLevel\":0}".to_string(),
            "{\"SetMapLevel\":7}".to_string(),
            "{\"SetClusterMethod\":\"Modularity\"}".to_string(),
            // The settings panel sends all of it at once, and a caller that
            // knows only about the method still sends something that parses.
            "{\"SetClusterSettings\":{\"method\":\"Markov\",\"resolution\":1.0,\"inflation\":2.5,\"target_top\":12}}"
                .to_string(),
            "{\"SetClusterSettings\":{\"method\":\"Leiden\"}}".to_string(),
            format!("{{\"DrillIntoCluster\":{{\"level\":2,\"anchor\":{id}}}}}"),
            "\"ZoomOutToClusters\"".to_string(),
        ];

        for case in cases {
            serde_json::from_str::<Command>(&case)
                .unwrap_or_else(|e| panic!("client sends {case}, which no longer parses: {e}"));
        }
    }

    /// An unbounded depth reaches the client as `null`, not as a number it
    /// would have to recognise as meaning "all".
    #[test]
    fn snapshot_carries_an_absent_depth_as_null() {
        let mut core = CoreApp::new(CallGraph::new());

        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert_eq!(json["workspaces"][0]["depth"], 3, "the default is a number");

        core.dispatch(Command::SetGraphDepth(GraphDepth::Unlimited));
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert!(json["workspaces"][0]["depth"].is_null());
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
        let methods = json["cluster_methods"]
            .as_array()
            .expect("the picker's entries");
        assert!(!methods.is_empty());
        for field in ["id", "label", "description"] {
            assert!(
                methods[0].get(field).is_some(),
                "method field `{field}` is read by the client"
            );
        }
        // Each method carries the knob it reads, and the shared one travels
        // beside the list: the client draws its sliders from these and holds
        // no copy of a range.
        for spec in [&methods[0]["grain"], &json["cluster_top"]] {
            for field in [
                "field",
                "label",
                "description",
                "min",
                "max",
                "step",
                "default",
            ] {
                assert!(
                    spec.get(field).is_some(),
                    "setting field `{field}` is read by the client"
                );
            }
        }

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
            "map_level",
            "map_levels",
            "map_clusters",
            "map_level_sizes",
            "map_opening_level",
            "method",
            "cluster",
            "map_scope",
        ] {
            assert!(
                ws.get(field).is_some(),
                "workspace field `{field}` is read by the client"
            );
        }
    }

    /// The parse summary reaches the client on the snapshot, flattened, with
    /// its verdict beside it — and is absent where no backend sent one, so a
    /// clangd load or the demo shows a plain "Ready".
    #[test]
    fn snapshot_carries_the_parse_summary_when_there_is_one() {
        let mut core = CoreApp::new(CallGraph::new());
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert!(json.get("parse").is_none(), "no loader, no summary");

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        core.lsp = crate::lsp_bridge::LspBridge::new_with_status(rx);
        tx.send(model::LspUiMessage::ParseReport(ParseSummary {
            parsed: 212,
            failed: 5,
            with_errors: 40,
            external_calls: 1234,
            rejected_arguments: vec!["-fconserve-stack".into()],
        }))
        .unwrap();
        core.lsp.poll_loader_messages();

        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        let parse = &json["parse"];
        assert_eq!(parse["parsed"], 212);
        assert_eq!(parse["failed"], 5);
        assert_eq!(parse["with_errors"], 40);
        assert_eq!(parse["external_calls"], 1234);
        assert_eq!(parse["rejected_arguments"][0], "-fconserve-stack");
        assert_eq!(parse["quality"], "partial");
    }

    /// The snapshot says what the backend can be asked, so the client can
    /// leave out what it cannot.
    #[test]
    fn snapshot_carries_the_backend_capabilities() {
        let mut core = CoreApp::new(CallGraph::new());
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert_eq!(json["capabilities"]["call_hierarchy"], false, "the demo is whole");
        assert_eq!(json["capabilities"]["refresh"], false);

        let (_loader_tx, loader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_channels_tx, channels_rx) = tokio::sync::oneshot::channel();
        core.lsp = crate::lsp_bridge::LspBridge::new_with_loader(loader_rx, channels_rx);
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        for capability in ["call_hierarchy", "references", "symbol_query", "refresh"] {
            assert_eq!(json["capabilities"][capability], true, "{capability}");
        }
    }

    /// A knob moved in the panel has to survive the round trip and land on
    /// the workspace, or the slider springs back on the next snapshot.
    #[test]
    fn cluster_settings_make_the_round_trip() {
        let mut core = CoreApp::new(crate::demo::create_demo_call_graph());

        let command: Command = serde_json::from_str(
            "{\"SetClusterSettings\":{\"method\":\"Markov\",\"resolution\":1.0,\"inflation\":2.5,\"target_top\":8}}",
        )
        .expect("the panel's command parses");
        core.dispatch(command);

        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        let settings = &json["workspaces"][0]["cluster"];
        assert_eq!(settings["method"], "Markov");
        assert_eq!(settings["inflation"], 2.5);
        assert_eq!(settings["target_top"], 8);
        assert_eq!(
            json["workspaces"][0]["method"], "Markov",
            "the short form agrees with the long one"
        );
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
            json.get("layout").is_none(),
            "and nothing is spent laying out the tree nobody is looking at"
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
        assert!(
            json.get("layout").is_some(),
            "and the view it is showing is the one that gets laid out"
        );
    }

    /// Each view pays for itself and for nothing else: the rooted layout is
    /// computed for the workspace showing the tree, the map for the workspace
    /// showing the map, and a workspace never ships both.
    ///
    /// A rooted layout is not cheap — a BFS out of the root and a Sugiyama
    /// pass over what it reaches — and the client reads `layout` only on the
    /// branch that draws the tree, so shipping it under the map was work done
    /// to be thrown away.
    #[test]
    fn only_the_view_on_screen_is_laid_out() {
        use crate::command::Command;

        let mut core = CoreApp::new(crate::demo::create_demo_call_graph());
        let main = core
            .call_graph
            .find_function_by_name("main")
            .expect("the demo has a main")
            .id;

        // Rooting the tree is asking for it, so this is the rooted view.
        core.dispatch(Command::SelectFunction(main));
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        let layout = json
            .get("layout")
            .and_then(|l| l.as_object())
            .expect("the rooted view ships its layout");
        assert!(
            !layout.is_empty(),
            "and the tree under `main` has nodes on it"
        );
        assert!(
            json.get("cluster_layout").is_none(),
            "the map it is not showing costs nothing"
        );

        // Zooming back out to the communities keeps the root but stops drawing
        // the tree — which is the case this is about.
        core.dispatch(Command::SetViewMode(ViewMode::ClusterMap));
        assert_eq!(
            core.workspaces.current().and_then(|ws| ws.root_symbol()),
            Some(main),
            "the workspace still has a root; it is simply not what is drawn"
        );
        let json = serde_json::to_value(snapshot_of(&core)).expect("snapshot serialises");
        assert!(
            json.get("layout").is_none(),
            "and the tree that root would grow is not laid out for nobody"
        );
        assert!(
            json.get("cluster_layout").is_some(),
            "the view on screen is"
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
