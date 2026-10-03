use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use logic::{ClusterKey, ClusterMethod, ClusterSettings, Clustering, GraphTraversal};
use lsp::LspResponse;
use model::{CallDirection, CallGraph, FunctionNode, Location, LspLoadPhase, SymbolId};

use crate::command::Command;
use crate::events::AppEvent;
use crate::layout::{compute_map_frame, GraphDepth, LayoutOrientation, MapFrame};
use crate::lsp_bridge::{
    BackendCapabilities, Capability, LoadingState, LspBridge, PendingRequest, RequestOrigin,
    SendOutcome,
};
use crate::warmup::WarmupQueue;
use crate::workspace::{CoreWorkspace, CoreWorkspaceManager, ViewMode};

/// How many prepares — requests that open a document — the warmup keeps in
/// flight.
///
/// A `prepareCallHierarchy` runs against its file's AST: clangd opens the
/// file, builds its preamble and parses it, and holds all of that in memory
/// for as long as the document stays open. So this is a budget on clangd's
/// memory and on how many of its AST workers the warmup occupies, not on the
/// pipe. The follow-ups that fetch a symbol's calls are budgeted separately,
/// in [`WARMUP_INDEX_IN_FLIGHT`], because they cost none of that.
///
/// It is bounded from above by [`lsp::OPEN_DOCUMENT_LIMIT`], and the `const`
/// assertion below is what enforces that rather than describing it.
const WARMUP_IN_FLIGHT: usize = 8;

/// How many index-only requests the warmup keeps in flight on top.
///
/// The follow-up for a symbol whose item is in hand —
/// `callHierarchy/outgoingCalls` — is answered from clangd's index in a few
/// milliseconds and opens nothing, so the only cost of keeping more of them
/// outstanding is the 30 s timeout each one is parked on. Sixteen is a pipe
/// depth: enough that clangd's worker pool has the next one waiting when it
/// finishes this one, while the reply crosses two channels and a tick. It is
/// not parallelism — clangd runs at most `-j` of them — and past the pool's
/// size a deeper pipe only queues.
const WARMUP_INDEX_IN_FLIGHT: usize = 16;

/// How many files the warmup prepares from at once.
///
/// Prepares are grouped by file so that a document is opened once for all
/// of its symbols, and this is how many such groups are worked in parallel.
/// The number is set by clangd's AST cache, not by its thread count: clangd
/// keeps the ASTs of three idle files (`ASTRetentionPolicy::MaxRetainedASTs`,
/// with no flag to change it) and rebuilds any other from its preamble on
/// every request, at ~80 ms on jemalloc. Reading from three files at a time
/// means the second prepare into a file finds the AST the first one built;
/// reading from eight means every prepare rebuilds it. Measured on jemalloc:
/// a prepare into an already open file had a median of 82 ms under the old
/// queue, which read from eight files at once; it is 3–6 ms at three lanes
/// and 29 ms at eight, and the warmup at eight lanes is a quarter slower
/// despite sending fewer prepares. Two lanes measure the same as three.
const WARMUP_LANES: usize = 3;

/// How many files the warmup opens ahead of its lanes.
///
/// The first prepare into a file waits for clangd to build the file's
/// preamble: a quarter of a second on jemalloc, three times what the rest of
/// the file's prepares cost together. Opening the next files while the
/// current ones are being read puts that build on another of clangd's
/// threads, in parallel with the reading, instead of at the head of every
/// file's queue. Deeper than the lanes, so there is always a built file to
/// move to; not much deeper, because each one is a live preamble in clangd's
/// memory until the LRU lets it go, and because the file a lane takes next
/// is chosen by a ranking that moves as replies land, so what was opened
/// ahead is not always what is read next. Measured on jemalloc: the first
/// prepare into a file costs 240 ms when the prepare opens the file itself,
/// 55 ms when the open led it by 100–300 ms, and 15 ms past 300 ms; eight
/// ahead instead of four measures the same wall time as four.
const WARMUP_PREFETCH: usize = 4;

/// The warmup may not fill the set of documents clangd is holding open.
///
/// A document with a request outstanding against it is never evicted — the
/// worker pins it (`lsp::service::document`) until clangd answers — so the
/// warmup cannot lose a symbol's edges to the LRU whatever its rate. What it
/// can do is *overfill* the set: a pinned document that would have been
/// evicted stays, and the set runs over its limit by the number of pins. Its
/// pins are at most one per prepare in flight, and it opens
/// [`WARMUP_PREFETCH`] more ahead of them; keeping both together under half
/// the limit leaves the other half for the user's own expands and lookups
/// and for the files the lanes have just finished with, which is what makes
/// the limit a ceiling on clangd's memory rather than a floor.
///
/// The limit is runtime — `COON_OPEN_DOCUMENT_LIMIT` may lower it — so the
/// invariant is enforced by clamping here rather than by a `const` assertion:
/// the budgets are capped at a quarter of the limit each (and never below
/// one), which keeps their sum under half of it for any limit of four or
/// more. Below four the warmup runs one prepare at a time and opens nothing
/// ahead — slow, correct, and within the memory it was given.
fn warmup_in_flight() -> usize {
    WARMUP_IN_FLIGHT.min(lsp::open_document_limit() / 4).max(1)
}

fn warmup_prefetch() -> usize {
    if lsp::open_document_limit() < 4 {
        return 0;
    }
    WARMUP_PREFETCH.min(lsp::open_document_limit() / 4).max(1)
}

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

/// One function's neighbourhood: where a scoped map is drawn from, which way
/// it reaches, and how far.
///
/// The depth is the workspace's own — the number the toolbar's stepper shows —
/// so a map scoped to a function groups the same functions the call tree of
/// that function was showing. It belongs *inside* the scope rather than beside
/// it in [`ClusterTarget`]: the whole-program map has no origin to walk from,
/// so a depth on it would be a key that splits the one cache entry every tab
/// falls back to, and recompute it on every press of a stepper that does not
/// govern it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MapScope {
    symbol: SymbolId,
    direction: CallDirection,
    depth: GraphDepth,
}

/// Which clustering a workspace is asking for — and so what a cache entry and
/// a background job are each about.
///
/// Every part of it comes off the workspace, because every part of it changes
/// what comes back: the settings are the algorithm and the knobs it reads, the
/// scope is the subgraph, the direction decides which way a scope reaches, and
/// the depth decides how far. Two tabs agreeing on all of it are asking the
/// same question and share the answer — and two that agree on the method but
/// not on its grain are not, which is why the whole of [`ClusterSettings`] is
/// in the key rather than the method alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClusterTarget {
    /// One function's neighbourhood, or `None` for the whole program.
    scope: Option<MapScope>,
    settings: ClusterSettings,
}

impl ClusterTarget {
    fn of(ws: &CoreWorkspace) -> Self {
        Self {
            scope: ws.map_scope().map(|symbol| MapScope {
                symbol,
                direction: ws.direction(),
                depth: ws.depth(),
            }),
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
    fn order(&self) -> (bool, u128, bool, Option<usize>, usize, u64, u64, usize) {
        let (scope, incoming, depth) = match self.scope {
            Some(scope) => (
                scope.symbol.0.as_u128(),
                matches!(scope.direction, CallDirection::Incoming),
                scope.depth.levels(),
            ),
            None => (0, false, None),
        };
        (
            self.scope.is_some(),
            scope,
            incoming,
            depth,
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

/// The functions of `old` that are not in `new` under their id but are there
/// under another: same qualified name, same file, a different line. Each maps
/// to the candidate nearest its old line, ties to the earlier one, and a
/// candidate that was itself in `old` is not one — it is a different
/// function that already had that name, an overload or a `static` twin, and
/// is carried across under its own id.
fn moved_symbols(old: &CallGraph, new: &CallGraph) -> HashMap<SymbolId, SymbolId> {
    let mut arrived: HashMap<(&str, &str), Vec<(u32, SymbolId)>> = HashMap::new();
    for function in new.functions() {
        if old.get_function(&function.id).is_none() {
            arrived
                .entry((
                    function.qualified_name.as_str(),
                    function.definition_location.file_path.as_str(),
                ))
                .or_default()
                .push((function.definition_location.line, function.id));
        }
    }
    old.functions()
        .filter(|function| new.get_function(&function.id).is_none())
        .filter_map(|function| {
            let line = function.definition_location.line;
            let candidates = arrived.get(&(
                function.qualified_name.as_str(),
                function.definition_location.file_path.as_str(),
            ))?;
            let (_, id) = candidates
                .iter()
                .min_by_key(|(at, _)| (at.abs_diff(line), *at))?;
            Some((function.id, *id))
        })
        .collect()
}

/// Where `id` is referenced, read off a graph that holds every edge: its
/// definition, then every call site of it, in source order.
///
/// Narrower than clangd's answer, which is `textDocument/references` with
/// `includeDeclaration` and so also names every declaration — a prototype in
/// a header, a forward declaration — and any reference outside a function
/// body, such as a function pointer in a static table's initialiser. What the
/// graph holds is the references *inside* the functions it knows, which
/// includes a function passed as a callback (libclang records references, not
/// only calls), each with the function it sits in as the referencing symbol.
fn references_in_graph(graph: &CallGraph, id: &SymbolId) -> Vec<model::Reference> {
    let Some(function) = graph.get_function(id) else {
        return Vec::new();
    };
    let from = |caller: &FunctionNode| model::ReferencingSymbol {
        name: caller.name.clone(),
        qualified_name: caller.qualified_name.clone(),
        kind: model::ReferenceSymbolKind::Function,
    };
    let mut call_sites: Vec<model::Reference> = graph
        .edges()
        .filter(|edge| edge.callee == *id)
        .map(|edge| model::Reference {
            location: edge.call_location.clone(),
            referencing_symbol: graph.get_function(&edge.caller).map(from),
        })
        .collect();
    call_sites.sort_by(|a, b| {
        let (a, b) = (&a.location, &b.location);
        (&a.file_path, a.line, a.column).cmp(&(&b.file_path, b.line, b.column))
    });
    // The definition first, as the function itself referencing it: that is
    // what clangd's containing-symbol lookup names for a definition, too.
    let mut references = vec![model::Reference {
        location: function.definition_location.clone(),
        referencing_symbol: Some(from(function)),
    }];
    references.extend(call_sites);
    references
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
    /// Derived from the whole graph, or — when a tab has scoped its map to one
    /// function — from what that function reaches within the tab's depth, which
    /// is the same walk the call tree it was scoped from was drawing. What is
    /// deliberately *not* in it is the rest of the window: collapse and expand
    /// change every time the user clicks a node, and boxes that regrouped and
    /// renamed themselves on every click would be describing the clicking
    /// rather than the program.
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
    ///
    /// The queue decides the *form* each question takes as well as its
    /// order — see [`WarmupQueue`]: a symbol some reply has already named
    /// goes out as its follow-up alone, and the rest are prepared file by
    /// file.
    edge_warmup: WarmupQueue,
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
            edge_warmup: WarmupQueue::default(),
            warmup_seeded: false,
            warmup_total: 0,
            warmup_finish_reported: false,
            warmup_last_done: 0,
            warmup_last_moved: None,
            warmup_started_at: None,
            warmup_last_logged: None,
        }
    }

    /// What the backend behind this app can be asked. See
    /// [`BackendCapabilities`].
    pub fn capabilities(&self) -> BackendCapabilities {
        self.lsp.capabilities()
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
            Command::SetCallDirection(direction) => self.cmd_set_call_direction(direction),
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

        // Take whole graphs from a backend that reads the build up front. They
        // arrive with their edges, and nothing is marked on the way in: that
        // every node is as loaded as it will ever be follows from the backend
        // having no call hierarchy to ask (see `BackendCapabilities`), which
        // is also what keeps the warmup and a click from asking it. Only the
        // last one counts, since each is the whole of it.
        if let Some(graph) = self.lsp.take_graphs().pop() {
            self.replace_graph(graph);
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
                        events.extend(self.record_references(symbol_id, references));
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
            LspResponse::PreloadComplete { request_id, .. } => {
                // The warmup's open-ahead, acknowledged. The loader's own
                // preload never reaches here — it waits for that reply itself
                // before handing the channels over — so this is only ever
                // ours, and there is nothing to do with it but retire the
                // entry `send_preload` recorded.
                self.lsp.take_pending(&request_id);
            }
            LspResponse::WorkspaceSymbolsProgress { request_id, found } => {
                // A discovery sweep still moving: a refresh the bridge sent,
                // or the tail of the loader's own. Keeps the request off the
                // timeout while it reports — see `PendingRequest::heard_at` —
                // and nothing else, since the functions arrive together in the
                // `WorkspaceSymbols` that ends it.
                log::debug!("CoreApp: discovery {request_id} has found {found} function(s)");
                self.lsp.heard_from(&request_id);
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
                            // Kept: an expand the other way round, or a
                            // requeue after a refresh, spends it instead of
                            // preparing again. See `LspBridge::hierarchy_items`.
                            self.lsp.learn_hierarchy_item(symbol_id, first_item.clone());
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
        let Some(id) = symbol_id else {
            return vec![];
        };
        // Nothing on the canvas changes, so there is no `LayoutDirty` here.
        // Where the answer comes from is the capability's to say: a whole
        // graph holds every call site already, and is answered on the spot;
        // anything else is asked of clangd, and the answer arrives as its own
        // event later — or is declined as unsupported, by the sender.
        let capabilities = self.lsp.capabilities();
        if capabilities.references && capabilities.graph_is_whole() {
            let references = references_in_graph(&self.call_graph, &id);
            return self.record_references(id, references).into_iter().collect();
        }
        self.lsp
            .send_references(&self.call_graph, &id)
            .event()
            .into_iter()
            .collect()
    }

    /// Store the references found for `symbol_id` on its node, and say how
    /// many there were — the one place both sources of an answer land, so a
    /// frontend hears the same thing whichever backend found them.
    fn record_references(
        &mut self,
        symbol_id: SymbolId,
        references: Vec<model::Reference>,
    ) -> Option<AppEvent> {
        let function = self.call_graph.get_function_mut(&symbol_id)?;
        let count = references.len();
        function.references = references;
        Some(if count == 0 {
            AppEvent::NoReferencesFound { symbol_id }
        } else {
            AppEvent::ReferencesFound { symbol_id, count }
        })
    }

    fn cmd_refresh(&mut self) -> Vec<AppEvent> {
        // Decided before anything is cleared: a backend that cannot reload has
        // nothing to clear for, and its graph is the one it will always have.
        let capabilities = self.lsp.capabilities();
        if !capabilities.refresh {
            return self
                .lsp
                .unsupported(Capability::Refresh)
                .event()
                .into_iter()
                .collect();
        }
        // A whole graph is refreshed by loading it again, not by re-asking for
        // symbols: there is nothing to clear here either, because the graph
        // on screen stays until its replacement arrives whole. A refresh
        // while a load is running is not a second parse — the one running
        // already reads the build as it stands — and the user is told so,
        // since a key that does nothing looks broken.
        if capabilities.graph_is_whole() {
            if self.lsp.load_in_progress() {
                log::info!("CoreApp: refresh while a load is running; not starting another");
                return vec![AppEvent::AlreadyLoading];
            }
            return self.lsp.send_reload().event().into_iter().collect();
        }
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

    /// Read the graph in the named direction.
    ///
    /// Only redraws on a real change, the way the orientation command does: a
    /// control — or a setting being restored — may name the direction already
    /// in force, and the answer to that is nothing, not a rebuild of the same
    /// tree with the expansions thrown away.
    fn cmd_set_call_direction(&mut self, direction: CallDirection) -> Vec<AppEvent> {
        let changed = self
            .workspaces
            .current_mut()
            .is_some_and(|ws| ws.set_direction(direction));
        if changed {
            log::info!("CoreApp: direction set to {direction:?}, expansions cleared");
            vec![AppEvent::LayoutDirty]
        } else {
            vec![]
        }
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

    fn cmd_set_graph_depth(&mut self, depth: GraphDepth) -> Vec<AppEvent> {
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
        let scope = MapScope {
            symbol,
            direction: ws.direction(),
            depth: ws.depth(),
        };
        let target = ClusterTarget {
            scope: Some(scope),
            settings: ws.cluster_settings(),
        };
        let clustering = cluster_within(&self.call_graph, scope, target.settings);
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
            log::info!(
                "CoreApp: cluster map scoped to {symbol:?} ({:?}, depth {})",
                scope.direction,
                scope.depth
            );
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
    /// Swap the graph for one a whole-graph backend just delivered.
    ///
    /// Replaced, not merged. Merging is right for clangd, whose replies each
    /// add to a graph nobody claims is finished; it is wrong for a backend
    /// whose every delivery is the whole program, because a union never
    /// forgets — a function deleted from the source, or a call taken out of
    /// one, would stay on the map after the refresh that was meant to show it
    /// gone. The first delivery goes through here as well: replacing an empty
    /// graph is the merge it used to be.
    ///
    /// The workspaces are carried across by symbol id. Ids are derived from
    /// the qualified name, the file and the line, so a function no edit
    /// touched keeps its id and everything pointing at it stays put. One
    /// whose line moved — anything below an edit in the same file — has a new
    /// id, and is found again by name and file, nearest line first, so a root
    /// does not vanish because a comment was added above it. What is truly
    /// gone falls back as [`CoreWorkspace::carry_over`] describes, and the
    /// selection with it.
    ///
    /// Every cached map goes stale with the graph and is recomputed once the
    /// load reports `Completed`, as after the first load; a stale map stays
    /// drawable meanwhile, and a box naming a function that is gone draws
    /// nothing for it.
    fn replace_graph(&mut self, graph: CallGraph) {
        let moved = moved_symbols(&self.call_graph, &graph);
        let translate = |id: SymbolId| {
            if graph.get_function(&id).is_some() {
                Some(id)
            } else {
                moved.get(&id).copied()
            }
        };
        for ws in &mut self.workspaces.workspaces {
            ws.carry_over(translate);
        }
        self.selected_function = self.selected_function.and_then(translate);
        log::info!(
            "CoreApp: graph replaced: {} function(s) and {} edge(s), was {} and {}; {} moved",
            graph.node_count(),
            graph.edge_count(),
            self.call_graph.node_count(),
            self.call_graph.edge_count(),
            moved.len()
        );
        self.call_graph = graph;
        self.mark_graph_changed();
    }

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
    ///
    /// Counts the warmup's own requests in flight, not everyone's. It used to
    /// read `pending_count`, so a single click after the warmup had drained —
    /// one `RequestOrigin::User` prepare — put the app back into this state,
    /// re-announced the finish when it landed, and had the whole program
    /// clustered a second time over a graph that had gained one edge.
    fn warming_up(&self) -> bool {
        self.warmup_seeded && (!self.edge_warmup.is_empty() || self.lsp.warmup_in_flight() > 0)
    }

    /// Put `id` on the warmup queue, in the pool its state calls for.
    ///
    /// Returns whether it was new to the queue. The file is what the queue
    /// groups prepares by; an item already learned for the symbol — from a
    /// reply that named it before it was ever queued — means it needs no
    /// prepare at all.
    fn queue_for_warmup(&mut self, id: SymbolId) -> bool {
        let Some((file, line)) = self.call_graph.get_function(&id).map(|f| {
            (
                f.definition_location.file_path.clone(),
                f.definition_location.line,
            )
        }) else {
            return false;
        };
        let queued = self.edge_warmup.push(id, file, line);
        if queued && self.lsp.has_hierarchy_item(&id) {
            self.edge_warmup.promote(id);
        }
        queued
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
        if self.warmup_seeded && !known && self.queue_for_warmup(id) {
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
            if self.queue_for_warmup(id) {
                requeued += 1;
            }
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
    /// follow up for calls, or the follow-up alone when the item is already in
    /// hand — so every response lands in the handlers that already know how to
    /// turn it into edges.
    ///
    /// Two budgets, filled in order. Symbols whose item some reply has
    /// already carried go out first, as index-only follow-ups, up to
    /// [`WARMUP_INDEX_IN_FLIGHT`]: each is a few milliseconds of clangd's
    /// time, and each reply names more callees — so the cascade from one
    /// prepare runs down through everything it calls before another prepare
    /// is spent on any of them. Then prepares, up to [`WARMUP_IN_FLIGHT`],
    /// from at most [`WARMUP_LANES`] files at a time, with the next
    /// [`WARMUP_PREFETCH`] files opened ahead. The two budgets are separate
    /// rather than one because they are spent on different things: a prepare
    /// occupies an AST worker and a document slot, a follow-up neither, and
    /// one ceiling on both either starved the index of requests it could
    /// answer at once or let the prepares fill clangd's memory.
    fn pump_edge_warmup(&mut self) -> Vec<AppEvent> {
        // Open the queue once the symbol stream is done and the bridge is
        // live: before that there is nothing to send on. Everything already in
        // the graph goes in here; everything arriving after queues itself
        // through `ingest_symbol`.
        // Never on a backend with no call hierarchy: its graph arrived with
        // every edge, and there is nothing to warm. Asked of the capability
        // rather than inferred from the missing worker, which a clangd bridge
        // also lacks until its channels are handed over.
        if !self.warmup_seeded
            && self.lsp.capabilities().call_hierarchy
            && matches!(self.lsp.status(), LspLoadPhase::Completed)
            && self.lsp.has_worker()
        {
            self.warmup_seeded = true;
            self.edge_warmup.clear();
            let ids: Vec<SymbolId> = self.call_graph.function_ids().copied().collect();
            for id in ids {
                self.queue_for_warmup(id);
            }
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
        let mut followed_up = 0usize;
        let mut prepared = 0usize;
        let mut skipped = 0usize;
        loop {
            let documents = self.lsp.documents_in_flight();
            let index_only = self.lsp.pending_count().saturating_sub(documents);
            let next = if index_only < WARMUP_INDEX_IN_FLIGHT {
                self.edge_warmup.next_ready()
            } else if self.edge_warmup.has_ready() {
                // Follow-ups waiting behind a full pipe: the cascade is
                // still running, and each of those replies names more of
                // what the lanes would otherwise prepare. A prepare now is
                // the one most likely to be wasted, so the lanes wait for
                // the pipe rather than spend one.
                break;
            } else {
                None
            };
            let next = match next {
                Some(id) => Some(id),
                None if documents < warmup_in_flight() => self.edge_warmup.next_prepare(WARMUP_LANES),
                None => None,
            };
            let Some(id) = next else {
                break;
            };
            // The user may have expanded this one while the queue drained.
            if self.lsp.is_function_loaded(&id) || self.lsp.is_function_loading(&id) {
                skipped += 1;
                continue;
            }
            self.lsp.set_loading_state(id, LoadingState::Loading);
            // Whether this goes out as a prepare or as the follow-up alone is
            // the bridge's call, from the items it holds — which is also why
            // a lane's symbol can turn out to need no prepare: its item
            // arrived after the queue's pools were sorted, and the send is
            // what reads the final state.
            if self.lsp.has_hierarchy_item(&id) {
                followed_up += 1;
            } else {
                prepared += 1;
            }
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

        // The files the lanes reach next, opened now so their preambles are
        // built by the time they are read. See `LspBridge::send_preload`.
        if self.warmup_seeded {
            let ahead: Vec<lsp_types::Url> = self
                .edge_warmup
                .files_to_prefetch(warmup_prefetch())
                .iter()
                .filter_map(|file| lsp_types::Url::from_file_path(file).ok())
                .collect();
            if !ahead.is_empty() {
                log::debug!(
                    "CoreApp: warmup opening {} file(s) ahead of its lanes",
                    ahead.len()
                );
                events.extend(self.lsp.send_preload(ahead).event());
            }
        }

        if followed_up > 0 || prepared > 0 || skipped > 0 {
            log::debug!(
                "CoreApp: warmup pump sent {} prepare(s) and {} follow-up(s) from items in hand, skipped {} already-loaded, {} queued",
                prepared,
                followed_up,
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
        let documents = self.lsp.documents_in_flight();
        let summary = format!(
            "CoreApp: edge warmup {}/{} — {} queued, {}/{} prepare(s) and {}/{} index request(s) in flight, unchanged for {:.1}s; waiting on: {}",
            done,
            self.warmup_total,
            self.edge_warmup.len(),
            documents,
            warmup_in_flight(),
            self.lsp.pending_count().saturating_sub(documents),
            WARMUP_INDEX_IN_FLIGHT,
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
                Some(scope) => cluster_within(&graph, scope, target.settings),
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
        // The item names the other end, and is its ticket for a calls request
        // of its own: if that symbol is still waiting on the warmup queue, it
        // no longer needs a prepare. See `LspBridge::hierarchy_items`.
        self.lsp.learn_hierarchy_item(other_id, item.clone());
        self.edge_warmup.promote(other_id);
        // `fromRanges` are positions in the *caller's* file. On an incoming
        // reply the caller is `item`; on an outgoing one it is the symbol the
        // request was about, whose file is the one on the item it was asked
        // with — not the callee's, which filed every cross-file call under the
        // file of the function it called.
        let site_file = if is_outgoing {
            self.lsp
                .hierarchy_item(&symbol_id)
                .map(|asked| asked.uri.path().to_string())
                .or_else(|| {
                    self.call_graph
                        .get_function(&symbol_id)
                        .map(|caller| caller.definition_location.file_path.clone())
                })
                .unwrap_or(file_path)
        } else {
            file_path
        };
        for from_range in from_ranges {
            let call_location = Location::new(
                site_file.clone(),
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
/// Bounded by the workspace's depth, which is the same walk the call tree the
/// user came from was drawing: a map of "what `f` reaches" that reaches further
/// than the view it was asked for from is a map of a different question, and
/// the boxes it draws hold functions the user cannot see. The number is the
/// only part of the view the map takes — collapse and expand are not, because
/// they describe one tree's shape rather than how much program is in view, and
/// a map that regrouped and renamed its boxes on every collapse click would be
/// describing the clicking rather than the program.
///
/// [`GraphDepth::Unlimited`] is the whole neighbourhood, as it was before the
/// depth reached here at all.
///
/// A free function rather than a method because a worker thread is what mostly
/// runs it, and a thread cannot borrow the app.
fn cluster_within(graph: &CallGraph, scope: MapScope, settings: ClusterSettings) -> Clustering {
    let traversal = GraphTraversal::new(graph);
    let levels = scope.depth.clamped().levels();
    let reachable: HashSet<SymbolId> = match scope.direction {
        CallDirection::Outgoing => traversal.find_reachable_within(&scope.symbol, levels),
        CallDirection::Incoming => traversal.find_can_reach_within(&scope.symbol, levels),
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
        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(1)));

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
        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(1)));
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

    /// A `CoreApp` whose finished load found `per_file` symbols in each of
    /// the named files, all absolute. Symbol `n` of a file sits on line
    /// `n + 1`, so [`prepared_lines`] still names them.
    fn loaded_app_in_files(
        files: &[(&str, usize)],
    ) -> (CoreApp, mpsc::UnboundedReceiver<lsp::LspRequest>) {
        let mut graph = CallGraph::new();
        for (file, symbols) in files {
            for i in 0..*symbols {
                let name = format!("{}_{i}", file.trim_start_matches('/').replace('/', "_"));
                graph.add_function(FunctionNode::new(
                    name.clone(),
                    name,
                    Location::new(file.to_string(), i as u32 + 1, 1),
                ));
            }
        }
        let mut app = CoreApp::new(graph);
        let rx = wire_worker(&mut app);
        app.lsp.set_status(LspLoadPhase::Completed);
        (app, rx)
    }

    /// The requests waiting on `rx`, sorted into prepares (by the file they
    /// open), follow-ups (by the item's name) and files opened ahead.
    struct Sent {
        prepares: Vec<(String, u32)>,
        follow_ups: Vec<String>,
        opened_ahead: Vec<String>,
    }

    fn drain_sent(rx: &mut mpsc::UnboundedReceiver<lsp::LspRequest>) -> Sent {
        let mut out = Sent {
            prepares: vec![],
            follow_ups: vec![],
            opened_ahead: vec![],
        };
        while let Ok(req) = rx.try_recv() {
            match req {
                lsp::LspRequest::PrepareCallHierarchy {
                    document_uri,
                    position,
                    ..
                } => out
                    .prepares
                    .push((document_uri.path().to_string(), position.line + 1)),
                lsp::LspRequest::GetOutgoingCalls {
                    call_hierarchy_item,
                    ..
                } => out.follow_ups.push(call_hierarchy_item.name),
                lsp::LspRequest::PreloadDocuments { document_uris, .. } => out
                    .opened_ahead
                    .extend(document_uris.iter().map(|u| u.path().to_string())),
                other => panic!("the warmup sent {other:?}"),
            }
        }
        out
    }

    /// A `loaded_app` whose first `with_items` symbols a reply has already
    /// named, so they go out as follow-ups.
    fn loaded_app_with_items(
        with_items: usize,
        without: usize,
    ) -> (CoreApp, mpsc::UnboundedReceiver<lsp::LspRequest>) {
        let (mut app, rx) = loaded_app(with_items + without);
        let named: Vec<SymbolId> = app
            .call_graph
            .functions()
            .filter(|f| line_of(&app, &f.id) <= with_items as u32)
            .map(|f| f.id)
            .collect();
        for id in &named {
            let line = line_of(&app, id);
            app.lsp
                .learn_hierarchy_item(*id, call_item("named", Some("named"), "/src/test.c", line));
        }
        (app, rx)
    }

    /// Prepares open documents and follow-ups do not, so the warmup budgets
    /// them apart: every symbol with an item in hand goes out as a follow-up
    /// up to its own ceiling, and the prepares fill theirs beside them. One
    /// ceiling on both either starved the index or filled clangd's memory.
    #[test]
    fn follow_ups_and_prepares_are_budgeted_apart() {
        let with_items = WARMUP_INDEX_IN_FLIGHT;
        let (mut app, mut rx) = loaded_app_with_items(with_items, WARMUP_IN_FLIGHT * 2);

        app.tick();

        let sent = drain_sent(&mut rx);
        assert_eq!(
            sent.follow_ups.len(),
            WARMUP_INDEX_IN_FLIGHT,
            "the index is kept fed, and not flooded"
        );
        assert_eq!(
            sent.prepares.len(),
            WARMUP_IN_FLIGHT,
            "and the prepares go out beside them, not behind them"
        );
        for (_, line) in &sent.prepares {
            assert!(
                *line > with_items as u32,
                "line {line} had an item in hand and must not be prepared"
            );
        }
        assert_eq!(
            app.lsp.documents_in_flight(),
            WARMUP_IN_FLIGHT,
            "only the prepares hold a document"
        );
    }

    /// Follow-ups waiting behind a full pipe mean the cascade is still
    /// running, and each reply names more of what the lanes would prepare —
    /// so while they wait, the lanes do too. Preparing beside them spent a
    /// parse of a file on symbols the next reply was about to hand over.
    #[test]
    fn the_lanes_wait_while_follow_ups_are_backed_up_behind_the_pipe() {
        let backlog = WARMUP_INDEX_IN_FLIGHT * 2;
        let (mut app, mut rx) = loaded_app_with_items(backlog, WARMUP_IN_FLIGHT);

        app.tick();

        let sent = drain_sent(&mut rx);
        assert_eq!(sent.follow_ups.len(), WARMUP_INDEX_IN_FLIGHT);
        assert!(
            sent.prepares.is_empty(),
            "no prepare while follow-ups are waiting on the pipe"
        );

        // The pipe drains: the rest of the backlog goes, and the lanes start.
        app.lsp.clear_pending();
        app.tick();
        let next = drain_sent(&mut rx);
        assert_eq!(next.follow_ups.len(), WARMUP_INDEX_IN_FLIGHT);
        assert_eq!(next.prepares.len(), WARMUP_IN_FLIGHT);
    }

    /// A follow-up reply names every callee, with the item clangd would mint
    /// by preparing it. A callee still waiting its turn on the queue is
    /// therefore asked about with that item — one index request — and never
    /// prepared at all.
    #[test]
    fn a_symbol_a_reply_has_named_is_followed_up_without_a_prepare() {
        let (mut app, mut rx) = loaded_app(WARMUP_IN_FLIGHT * 2);
        app.tick();
        let first = drain_sent(&mut rx);
        assert_eq!(first.prepares.len(), WARMUP_IN_FLIGHT);
        let waiting = *app.edge_warmup.iter().next().expect("symbols still queued");
        let waiting_line = line_of(&app, &waiting);
        let in_flight = *app
            .lsp
            .pending()
            .find_map(|p| p.symbol_id.as_ref())
            .expect("a prepare in flight");

        // One of the in-flight symbols turns out to call the waiting one.
        app.lsp.insert_pending(
            "follow-up",
            PendingRequest::new(Some(in_flight), "outgoing calls", RequestOrigin::Warmup),
        );
        app.handle_lsp_response(LspResponse::OutgoingCalls {
            request_id: "follow-up".to_string(),
            calls: vec![lsp_types::CallHierarchyOutgoingCall {
                to: call_item(
                    &format!("f{}", waiting_line - 1),
                    Some(&format!("f{}", waiting_line - 1)),
                    "/src/test.c",
                    waiting_line,
                ),
                from_ranges: vec![],
            }],
        });
        assert!(
            app.lsp.has_hierarchy_item(&waiting),
            "the item the reply carried is kept"
        );

        app.tick();

        let next = drain_sent(&mut rx);
        assert_eq!(
            next.follow_ups,
            vec![format!("f{}", waiting_line - 1)],
            "the named symbol goes out as its follow-up alone"
        );
        assert!(
            next.prepares.iter().all(|(_, line)| *line != waiting_line),
            "and is not prepared as well"
        );
        assert!(!app.edge_warmup.contains(&waiting));
    }

    /// Prepares are grouped by file, a few files at a time, and the files
    /// after those are opened ahead: a document opened once serves every
    /// symbol in it, where hash order reopened each of jemalloc's files nine
    /// times over and paid for a preamble each time.
    #[test]
    fn prepares_are_grouped_by_file_with_the_next_files_opened_ahead() {
        let files: Vec<String> = (0..WARMUP_LANES + WARMUP_PREFETCH + 2)
            .map(|i| format!("/src/file_{i}.c"))
            .collect();
        let per_file: Vec<(&str, usize)> = files.iter().map(|f| (f.as_str(), 3)).collect();
        let (mut app, mut rx) = loaded_app_in_files(&per_file);

        app.tick();

        let sent = drain_sent(&mut rx);
        assert_eq!(sent.prepares.len(), WARMUP_IN_FLIGHT);
        let touched: HashSet<&String> = sent.prepares.iter().map(|(file, _)| file).collect();
        assert_eq!(
            touched.len(),
            WARMUP_LANES,
            "eight prepares into three files, not eight files: {:?}",
            sent.prepares
        );
        assert_eq!(
            sent.opened_ahead.len(),
            WARMUP_PREFETCH,
            "and the next files are opened while those are read"
        );
        for file in &sent.opened_ahead {
            assert!(
                !touched.contains(file),
                "{file} is being prepared from, not merely opened ahead"
            );
        }
        // The open-ahead is acknowledged by the worker at once; here nobody
        // answers, so retire it by hand as the reply arm would.
        let ahead_ids: Vec<String> = app
            .lsp
            .pending()
            .filter(|p| p.symbol_id.is_none())
            .map(|p| p.label.clone())
            .collect();
        assert_eq!(
            ahead_ids.len(),
            1,
            "one open-ahead request, for all of them"
        );

        // The next pump opens nothing it already opened.
        app.lsp.clear_pending();
        app.tick();
        let next = drain_sent(&mut rx);
        for file in &next.opened_ahead {
            assert!(
                !sent.opened_ahead.contains(file),
                "{file} was opened ahead twice"
            );
        }
    }

    /// The end of the warmup is the end: a click afterwards is one request
    /// the user made, not the warmup starting over. Counting every pending
    /// request re-announced the finish on each click and had the whole
    /// program clustered again over a graph one edge bigger.
    #[test]
    fn a_click_after_the_warmup_has_drained_does_not_restart_it() {
        let (mut app, _rx) = loaded_app(1);
        app.tick();
        app.lsp.clear_pending();
        app.tick();
        assert!(!app.warming_up());
        assert!(app.warmup_finish_reported);
        let id = *app.call_graph.function_ids().next().expect("the symbol");

        app.lsp.insert_pending(
            "click",
            PendingRequest::new(Some(id), "prepare f0", RequestOrigin::User),
        );

        assert!(!app.warming_up(), "a user's request is not the warmup's");
        assert!(!app.loading());
        assert!(
            !app.tick()
                .iter()
                .any(|e| matches!(e, AppEvent::ResolvingCalls { .. })),
            "and nothing is re-announced when it lands"
        );
    }

    /// The item shortcut is the bridge's, not the warmup's: a node the user
    /// expands is asked about the same way, so a symbol any reply has named
    /// opens on one index request rather than a parse of its file.
    #[test]
    fn expanding_a_symbol_a_reply_has_named_needs_no_prepare() {
        let (graph, left, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        let mut rx = wire_worker(&mut app);
        let target = left[1];
        app.lsp
            .learn_hierarchy_item(target, hierarchy_item("named", 0));

        app.dispatch(Command::ExpandFunction(target));

        let sent = drain_sent(&mut rx);
        assert_eq!(sent.follow_ups, vec!["named".to_string()]);
        assert!(sent.prepares.is_empty());
        assert!(app.lsp.is_function_loading(&target));
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

    /// A call site is in the caller's file, whichever way the reply came.
    /// `fromRanges` on an outgoing reply are positions in the function asked
    /// about; filing them under the callee's file put a call written in
    /// `app.c` at `config.c:74`, which on a small C project was 356 of its
    /// 548 call sites.
    #[test]
    fn a_call_site_is_filed_under_the_callers_file() {
        let site = lsp_types::Range {
            start: lsp_types::Position {
                line: 73,
                character: 8,
            },
            end: lsp_types::Position {
                line: 73,
                character: 18,
            },
        };

        // Outgoing: we asked about `app` in app.c, clangd names `config_set`.
        let mut app = CoreApp::new(CallGraph::new());
        let asked = call_item("app", Some("app"), "/src/app.c", 60);
        let caller = app.call_graph.add_function(FunctionNode::new(
            "app".to_string(),
            "app".to_string(),
            Location::new("/src/app.c".to_string(), 60, 5),
        ));
        app.lsp.learn_hierarchy_item(caller, asked);
        app.process_call_entry(
            caller,
            &call_item("config_set", Some("config_set"), "/src/config.c", 12),
            &[site],
            true,
        );

        // Incoming: we asked about `config_set`, clangd names `app` as caller.
        let mut other = CoreApp::new(CallGraph::new());
        let callee = other.call_graph.add_function(FunctionNode::new(
            "config_set".to_string(),
            "config_set".to_string(),
            Location::new("/src/config.c".to_string(), 12, 5),
        ));
        other.process_call_entry(
            callee,
            &call_item("app", Some("app"), "/src/app.c", 60),
            &[site],
            false,
        );

        for (way, graph) in [("outgoing", &app.call_graph), ("incoming", &other.call_graph)] {
            let edges: Vec<_> = graph.edges().collect();
            assert_eq!(edges.len(), 1, "{way}: one call site");
            assert_eq!(
                edges[0].call_location.file_path, "/src/app.c",
                "{way}: the call is written in app.c"
            );
            assert_eq!(edges[0].call_location.line, 74);
        }
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
        for id in app.edge_warmup.iter() {
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
    fn the_scoped_map_stops_where_the_depth_limit_does() {
        // left_a → left_b, left_c → right_a → right_b, right_c: one level of
        // calls short of the far triangle, two levels into it.
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(1)));

        app.dispatch(Command::SetMapScope(Some(left[0])));

        for symbol in left {
            assert!(
                active(&app).cluster_of(&symbol).is_some(),
                "one level out is the triangle this function is in"
            );
        }
        for symbol in right {
            assert!(
                active(&app).cluster_of(&symbol).is_none(),
                "and the next triangle is past where the view stops"
            );
        }
    }

    #[test]
    fn deepening_the_view_widens_the_scoped_map_with_it() {
        let (graph, left, right) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(1)));
        app.dispatch(Command::SetMapScope(Some(left[0])));

        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(3)));
        tick_until_settled(&mut app);

        assert!(
            right.iter().all(|s| active(&app).cluster_of(s).is_some()),
            "three levels out reaches the far triangle, so the map holds it"
        );
        assert!(
            app.clusterings.keys().any(|target| matches!(
                target.scope,
                Some(scope) if scope.depth == GraphDepth::Levels(3)
            )),
            "and the map that is showing is the one built to the depth on screen"
        );
    }

    #[test]
    fn the_whole_program_map_is_not_bounded_by_the_depth() {
        // Depth is where a walk *from somewhere* stops, and the whole-program
        // map starts nowhere in particular — so the stepper must neither
        // narrow it nor split its cache entry per number.
        let (graph, left, _) = two_triangles();
        let mut app = CoreApp::new(graph);
        app.dispatch(Command::SetGraphDepth(GraphDepth::Levels(1)));
        tick_until_settled(&mut app);

        let whole = ClusterTarget {
            scope: None,
            settings: ClusterSettings::default(),
        };
        assert!(
            app.clusterings
                .get(&whole)
                .expect("the whole-program map is keyed without a depth")
                .cluster_of(&left[0])
                .is_some(),
            "the whole program is on it whatever the stepper says"
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

    /// A libclang-shaped app: a bridge that hears from a loader and has nothing
    /// to ask, handed one graph of two functions and one call, loaded.
    fn whole_graph_app() -> (CoreApp, SymbolId, SymbolId) {
        let mut graph = CallGraph::new();
        let caller = add(&mut graph, "caller", 1);
        let callee = add(&mut graph, "callee", 2);
        graph.add_call(caller, callee, Location::new("test.c".to_string(), 1, 0));

        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = CoreApp::new_with_lsp(CallGraph::new(), LspBridge::new_with_status(rx));
        tx.send(model::LspUiMessage::AddGraph(Box::new(graph))).unwrap();
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::Completed))
            .unwrap();
        app.tick();
        (app, caller, callee)
    }

    /// A backend that hands the graph over whole has every node loaded because
    /// it has no call hierarchy, not because each node was marked on the way
    /// in — and so nothing is ever fetched: no warmup, no loading state from a
    /// click, nothing pending.
    #[test]
    fn a_whole_graph_backend_never_fetches_edges() {
        let (mut app, caller, callee) = whole_graph_app();
        assert_eq!(app.capabilities(), BackendCapabilities::WHOLE_GRAPH);
        assert_eq!(app.call_graph.edge_count(), 1);
        assert!(app.lsp.is_function_loaded(&caller));
        assert_eq!(app.lsp.get_loading_state(&callee), &LoadingState::Loaded);

        app.dispatch(Command::SelectFunction(caller));
        app.dispatch(Command::ExpandFunction(callee));
        app.dispatch(Command::StartGraphFromFunction(callee));
        tick_until_settled(&mut app);

        assert!(!app.lsp.is_function_loading(&caller));
        assert!(!app.lsp.is_function_loading(&callee));
        assert_eq!(app.lsp.pending_count(), 0, "nothing was asked");
        assert!(!app.warmup_seeded, "there is nothing to warm");
        assert!(!app.loading(), "the load is over once the graph is in");
    }

    /// What a whole-graph backend cannot answer is answered once, as
    /// unsupported, and says nothing to the frontend — and a refresh does not
    /// clear what it cannot bring back.
    #[test]
    fn a_whole_graph_backend_answers_unsupported_and_keeps_its_graph() {
        let (mut app, caller, _) = whole_graph_app();
        app.dispatch(Command::SelectFunction(caller));

        assert!(app.dispatch(Command::Refresh).is_empty());
        assert_eq!(app.call_graph.node_count(), 2, "a refresh cleared nothing");

        assert!(matches!(
            app.lsp.send_reload(),
            SendOutcome::Unsupported(Capability::Refresh)
        ));
        assert!(matches!(
            app.lsp.send_workspace_symbols(),
            SendOutcome::Unsupported(Capability::SymbolQuery)
        ));
        assert!(matches!(
            app.lsp.send_call_hierarchy(
                &app.call_graph,
                &caller,
                RequestOrigin::User,
                CallDirection::Outgoing
            ),
            SendOutcome::Unsupported(Capability::CallHierarchy)
        ));
    }

    /// A whole graph answers find-references itself, on the spot, from the
    /// call sites it holds: the definition first, then every call in source
    /// order, each named by the function it sits in. Nothing is asked.
    #[test]
    fn a_whole_graph_answers_references_from_its_call_sites() {
        let mut graph = CallGraph::new();
        let target = add(&mut graph, "target", 1);
        let first = add(&mut graph, "first", 10);
        let second = add(&mut graph, "second", 20);
        call(&mut graph, second, target, 22);
        call(&mut graph, first, target, 12);
        call(&mut graph, first, target, 11);
        call(&mut graph, first, second, 13);

        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = CoreApp::new_with_lsp(CallGraph::new(), LspBridge::new_with_status(rx));
        tx.send(model::LspUiMessage::AddGraph(Box::new(graph))).unwrap();
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::Completed))
            .unwrap();
        app.tick();
        assert!(app.capabilities().references);

        app.dispatch(Command::StartGraphFromFunction(target));
        let events = app.dispatch(Command::FindReferences);
        assert!(
            matches!(
                events.as_slice(),
                [AppEvent::ReferencesFound { symbol_id, count: 4 }] if *symbol_id == target
            ),
            "{events:?}"
        );
        assert_eq!(app.lsp.pending_count(), 0, "answered here, not asked");

        let found: Vec<(u32, &str)> = app.call_graph.get_function(&target).unwrap()
            .references
            .iter()
            .map(|r| {
                let from = r.referencing_symbol.as_ref().expect("every one has a function");
                (r.location.line, from.name.as_str())
            })
            .collect();
        assert_eq!(
            found,
            [(1, "target"), (11, "first"), (12, "first"), (22, "second")]
        );
    }

    /// A libclang-shaped app whose loader can be asked to load again: the
    /// graph handed over, the load finished, the map settled. Hands back the
    /// loader's two ends — what it says, and what it is asked.
    fn reloadable_app(
        graph: CallGraph,
    ) -> (
        CoreApp,
        mpsc::UnboundedSender<model::LspUiMessage>,
        mpsc::UnboundedReceiver<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (reload_tx, reload_rx) = mpsc::unbounded_channel();
        let mut app = CoreApp::new_with_lsp(
            CallGraph::new(),
            LspBridge::new_with_reloader(rx, reload_tx),
        );
        deliver(&tx, graph);
        tick_until_settled(&mut app);
        (app, tx, reload_rx)
    }

    /// What the libclang loader sends at the end of every load.
    fn deliver(tx: &mpsc::UnboundedSender<model::LspUiMessage>, graph: CallGraph) {
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::IndexingProject {
            done: 1,
            total: 1,
        }))
        .unwrap();
        tx.send(model::LspUiMessage::AddGraph(Box::new(graph))).unwrap();
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::Completed))
            .unwrap();
    }

    /// A refresh under libclang asks the loader for the whole load again, and
    /// what comes back replaces the graph rather than joining it: a function
    /// that is gone from the source is gone from the graph and from the map,
    /// and the root that survived is still the root.
    #[test]
    fn a_refresh_replaces_a_whole_graph_and_keeps_what_survived() {
        let (graph, left, right) = two_triangles();
        let (mut app, tx, mut reload_rx) = reloadable_app(graph);
        assert!(app.capabilities().refresh);
        assert!(active(&app).cluster_of(&right[0]).is_some());
        app.dispatch(Command::StartGraphFromFunction(left[0]));
        app.dispatch(Command::SelectFunction(left[0]));

        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["LspProgress"]);
        assert!(reload_rx.try_recv().is_ok(), "the loader was asked");
        assert!(app.loading(), "loading from the moment it is asked");

        // The left triangle alone, at the same lines: the same ids.
        let mut edited = CallGraph::new();
        triangle(&mut edited, "left", 10);
        deliver(&tx, edited);
        tick_until_settled(&mut app);

        assert_eq!(app.call_graph.node_count(), 3);
        assert_eq!(app.call_graph.edge_count(), 3, "the call across is gone too");
        assert!(app.call_graph.get_function(&right[0]).is_none());
        let ws = app.workspaces.current().unwrap();
        assert_eq!(ws.root_symbol(), Some(left[0]));
        assert_eq!(ws.view(), ViewMode::Rooted);
        assert_eq!(app.selected_function, Some(left[0]));
        assert!(
            active(&app).cluster_of(&right[0]).is_none(),
            "the map was recomputed over the new graph"
        );
        assert!(!app.loading());
        assert!(
            serde_json::to_value(crate::protocol::snapshot_of(&app)).is_ok(),
            "the snapshot draws"
        );
    }

    /// A function whose line moved has a new id, and is found again by name
    /// and file; a root that is gone outright lets its tab fall back to the
    /// map rather than a tree grown from nothing.
    #[test]
    fn a_refresh_follows_a_moved_root_and_lets_go_of_a_vanished_one() {
        let (graph, left, right) = two_triangles();
        let (mut app, tx, _reload_rx) = reloadable_app(graph);
        // Each opens a tab of its own, beside the one the app opened on.
        app.dispatch(Command::StartGraphFromFunction(left[0]));
        app.dispatch(Command::StartGraphFromFunction(right[0]));
        app.dispatch(Command::SelectFunction(right[0]));
        app.dispatch(Command::Refresh);

        // Two lines added above the left triangle, and the right one deleted.
        let mut edited = CallGraph::new();
        let moved = triangle(&mut edited, "left", 12);
        assert_ne!(moved[0], left[0], "a moved line is a new id");
        deliver(&tx, edited);
        tick_until_settled(&mut app);

        let [_, kept, dropped] = &app.workspaces.workspaces[..] else {
            panic!("three workspaces");
        };
        assert_eq!(kept.root_symbol(), Some(moved[0]), "found again by name");
        assert_eq!(kept.view(), ViewMode::Rooted);
        assert_eq!(dropped.root_symbol(), None);
        assert_eq!(dropped.view(), ViewMode::ClusterMap, "back to the map");
        assert_eq!(app.selected_function, None);
    }

    /// A refresh while a load is running starts no second one, during the
    /// first load and during a reload alike, and says so.
    #[test]
    fn a_refresh_during_a_load_starts_nothing() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (reload_tx, mut reload_rx) = mpsc::unbounded_channel();
        let mut app = CoreApp::new_with_lsp(
            CallGraph::new(),
            LspBridge::new_with_reloader(rx, reload_tx),
        );
        app.tick();
        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["AlreadyLoading"]);

        let (graph, _, _) = two_triangles();
        deliver(&tx, graph);
        tick_until_settled(&mut app);
        assert!(reload_rx.try_recv().is_err(), "nothing was asked during the load");

        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["LspProgress"]);
        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["AlreadyLoading"]);
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::IndexingProject {
            done: 0,
            total: 1,
        }))
        .unwrap();
        app.tick();
        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["AlreadyLoading"]);
        assert!(reload_rx.try_recv().is_ok());
        assert!(reload_rx.try_recv().is_err(), "one parse, however often asked");

        // A failed reload is over too, and can be retried.
        tx.send(model::LspUiMessage::Progress(LspLoadPhase::Failed("x".into())))
            .unwrap();
        app.tick();
        assert_eq!(app.call_graph.node_count(), 6, "a failure keeps the graph");
        assert_eq!(shapes(&app.dispatch(Command::Refresh)), ["LspProgress"]);
    }

    /// A clangd bridge is capable before it is wired: the worker arriving late
    /// is a different question from what the backend can do.
    #[test]
    fn a_clangd_bridge_is_capable_before_its_worker_arrives() {
        let (_loader_tx, loader_rx) = mpsc::unbounded_channel();
        let (_channels_tx, channels_rx) = tokio::sync::oneshot::channel();
        let bridge = LspBridge::new_with_loader(loader_rx, channels_rx);
        assert_eq!(bridge.capabilities(), BackendCapabilities::CLANGD);
        assert!(!bridge.has_worker());

        let (mut app, _rx) = loaded_app(1);
        assert_eq!(app.capabilities(), BackendCapabilities::CLANGD);
        let id = only_symbol(&app);
        assert!(!app.lsp.is_function_loaded(&id), "clangd edges are fetched");
        app.dispatch(Command::SelectFunction(id));
        assert!(app.lsp.is_function_loading(&id));
    }
}
