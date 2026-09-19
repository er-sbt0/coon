use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;

use grid::{Dag, LayoutConfig, LayoutEngine};
use logic::{ClusterKey, Clustering};
use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

/// Node geometry for the pointer-driven canvas, in **pixels**.
///
/// The `grid` crate's defaults are in terminal cells — 20 characters wide by 3
/// rows tall — which is right for the TUI and wrong here. Laying out in the
/// units the canvas actually draws in means the client can use the positions
/// verbatim instead of scaling them by a magic factor.
pub const NODE_WIDTH: f32 = 150.0;
pub const NODE_HEIGHT: f32 = 32.0;

/// Empty space between one node and the next, horizontally and vertically.
pub const COLUMN_GUTTER: f32 = 72.0;
pub const ROW_GUTTER: f32 = 26.0;

// `grid` treats `level_separation` and `node_separation` as the full centre-to-
// centre *pitch*, not as a gap added to the node's own size. With the crate's
// terminal defaults (node_width 20, level_separation 4) that distinction is
// easy to miss; at pixel scale it is the difference between a clean grid and
// every node overlapping its neighbour. So pitch = size + gutter, explicitly.
pub const LEVEL_SEPARATION: f32 = NODE_WIDTH + COLUMN_GUTTER;
pub const NODE_SEPARATION: f32 = NODE_HEIGHT + ROW_GUTTER;
/// Extra breathing room between distinct subtrees, along the axis siblings
/// spread on — which is the vertical one left-to-right, the horizontal one
/// top-down.
pub const SUBTREE_SEPARATION: f32 = NODE_SEPARATION + ROW_GUTTER;

// Top-down is a transpose of the layout, but *not* of the spacing. A node is
// 150×32: left-to-right the graph advances along the node's long axis and
// spreads along its short one, so the same two gutters give a shape that reads.
// Transposed, levels advance along the 32px axis — reusing `NODE_SEPARATION` as
// the level pitch stacks rows 58px apart while siblings sit 222px apart, and
// the whole graph comes out as a wide, flat band that fits by its width and
// leaves the rows almost touching. Top-down therefore gets its own pair:
// generous between levels, because that axis is now doing the work of showing
// depth, and tighter between siblings, because width is the axis under
// pressure.
pub const LEVEL_GUTTER_TOPDOWN: f32 = 96.0;
pub const SIBLING_GUTTER_TOPDOWN: f32 = 40.0;
pub const LEVEL_SEPARATION_TOPDOWN: f32 = NODE_HEIGHT + LEVEL_GUTTER_TOPDOWN;
pub const NODE_SEPARATION_TOPDOWN: f32 = NODE_WIDTH + SIBLING_GUTTER_TOPDOWN;
pub const SUBTREE_SEPARATION_TOPDOWN: f32 = NODE_SEPARATION_TOPDOWN + SIBLING_GUTTER_TOPDOWN;

/// The bounding box a laid-out graph is pulled towards, as cross-axis extent
/// over level-axis extent. A canvas is landscape, so a graph that is a little
/// wider than it is tall reads well; a 5:1 stripe does not — and top-down is
/// exactly that by construction, because siblings then spread along the node's
/// 150px axis while levels advance along its 32px one.
pub const TARGET_ASPECT: f32 = 1.6;
/// How far `balance_aspect` may stretch the level axis. Spreading levels apart
/// uses the empty half of the canvas, but past a point it only means the user
/// pans further to follow one edge, so the stripe is improved, not abolished.
pub const MAX_LEVEL_STRETCH: f32 = 2.5;

/// How many levels of callees/callers to show by default.
pub const DEFAULT_DEPTH: usize = 3;
pub const MIN_DEPTH: usize = 1;
pub const MAX_DEPTH: usize = 8;

/// Which way the graph flows: root on the left with callees to the right, or
/// root on top with callees below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LayoutOrientation {
    #[default]
    LeftRight,
    TopDown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NodePos {
    pub x: f32,
    pub y: f32,
}

/// What to include in a layout.
#[derive(Debug, Clone)]
pub struct LayoutOptions<'a> {
    /// Levels of children to traverse from the root. 1 = root plus its
    /// immediate children.
    pub depth: usize,
    /// Nodes whose children the user has collapsed. The node itself stays
    /// visible; traversal stops there.
    pub collapsed: &'a HashSet<SymbolId>,
    /// Nodes the user asked to load the calls of. Traversal walks one level
    /// *past* `depth` at these, so the gesture appends the callees it fetched
    /// to the graph already on screen instead of moving the window onto them.
    pub expanded: &'a HashSet<SymbolId>,
    /// Which way levels advance.
    pub orientation: LayoutOrientation,
}

impl Default for LayoutOptions<'_> {
    fn default() -> Self {
        static EMPTY: std::sync::OnceLock<HashSet<SymbolId>> = std::sync::OnceLock::new();
        Self {
            depth: DEFAULT_DEPTH,
            collapsed: EMPTY.get_or_init(HashSet::new),
            expanded: EMPTY.get_or_init(HashSet::new),
            orientation: LayoutOrientation::default(),
        }
    }
}

/// `grid` always advances levels along x and spreads siblings along y; its
/// `TreeOrientation` is not honoured by the Sugiyama pass. A top-down graph is
/// therefore the same layout transposed: the node size and separations go in
/// swapped, and the coordinates come back swapped (see `to_centre`). The
/// separations are the orientation's own, not the left-to-right pair reordered
/// — see the `*_TOPDOWN` constants.
fn layout_config(orientation: LayoutOrientation) -> LayoutConfig {
    let (node_w, node_h, level_sep, node_sep, subtree_sep) = match orientation {
        LayoutOrientation::LeftRight => (
            NODE_WIDTH,
            NODE_HEIGHT,
            LEVEL_SEPARATION,
            NODE_SEPARATION,
            SUBTREE_SEPARATION,
        ),
        LayoutOrientation::TopDown => (
            NODE_HEIGHT,
            NODE_WIDTH,
            LEVEL_SEPARATION_TOPDOWN,
            NODE_SEPARATION_TOPDOWN,
            SUBTREE_SEPARATION_TOPDOWN,
        ),
    };
    LayoutConfig::new()
        .with_node_size(node_w, node_h)
        .with_node_separation(node_sep)
        .with_level_separation(level_sep)
        .with_subtree_separation(subtree_sep)
}

/// Turn one `grid` top-left corner into a canvas centre, undoing the transpose
/// for a top-down layout.
fn to_centre(pos: &grid::Position, orientation: LayoutOrientation) -> NodePos {
    match orientation {
        LayoutOrientation::LeftRight => NodePos {
            x: pos.x + NODE_WIDTH / 2.0,
            y: pos.y + NODE_HEIGHT / 2.0,
        },
        LayoutOrientation::TopDown => NodePos {
            x: pos.y + NODE_WIDTH / 2.0,
            y: pos.x + NODE_HEIGHT / 2.0,
        },
    }
}

/// Spread the levels apart until the graph stops being a stripe.
///
/// `grid` packs each axis as tightly as its separations allow, which is right
/// per-level and wrong overall: a wide fan of siblings gives a bounding box far
/// wider than it is tall (top-down) or taller than wide (left-to-right), and
/// fitting that to a canvas leaves half the screen empty while the levels sit
/// almost on top of each other. Every node on a level shares its level-axis
/// coordinate, so scaling that axis uniformly is exactly "use a bigger pitch" —
/// no node moves relative to its own level, and no two nodes can collide.
///
/// Only ever a stretch, never a squash, and capped: see `MAX_LEVEL_STRETCH`.
///
/// Generic in the key only so the rooted tree and the cluster map can share it;
/// it reads nothing but the positions.
fn balance_aspect<K: Eq + std::hash::Hash>(
    positions: &mut HashMap<K, NodePos>,
    orientation: LayoutOrientation,
) {
    if positions.len() < 2 {
        return;
    }
    // The level axis is the one levels advance along; the cross axis is the one
    // siblings spread on.
    let (level, cross): (Vec<f32>, Vec<f32>) = match orientation {
        LayoutOrientation::LeftRight => (
            positions.values().map(|p| p.x).collect(),
            positions.values().map(|p| p.y).collect(),
        ),
        LayoutOrientation::TopDown => (
            positions.values().map(|p| p.y).collect(),
            positions.values().map(|p| p.x).collect(),
        ),
    };
    let extent = |v: &[f32]| {
        let min = v.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = v.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        (min, max - min)
    };
    let (level_min, level_extent) = extent(&level);
    let (_, cross_extent) = extent(&cross);

    // A single level has nothing to spread apart.
    if level_extent < 1.0 {
        return;
    }
    let aspect = cross_extent / level_extent;
    if aspect <= TARGET_ASPECT {
        return;
    }
    let stretch = (aspect / TARGET_ASPECT).min(MAX_LEVEL_STRETCH);

    for pos in positions.values_mut() {
        let coord = match orientation {
            LayoutOrientation::LeftRight => &mut pos.x,
            LayoutOrientation::TopDown => &mut pos.y,
        };
        *coord = level_min + (*coord - level_min) * stretch;
    }
}

/// Compute pixel positions for the subgraph rooted at `root`.
///
/// Returns an empty map if `root` is None or not found in the graph. Positions
/// are the **centre** of each node, which is what a canvas wants; the `grid`
/// crate reports top-left corners, so they are converted here rather than in
/// every frontend.
pub fn compute_layout(
    graph: &CallGraph,
    root: Option<SymbolId>,
    direction: CallDirection,
    opts: &LayoutOptions<'_>,
) -> HashMap<SymbolId, NodePos> {
    let root = match root {
        Some(r) => r,
        None => return HashMap::new(),
    };

    if graph.get_function(&root).is_none() {
        return HashMap::new();
    }

    let max_depth = opts.depth.clamp(MIN_DEPTH, MAX_DEPTH);

    let mut symbol_to_node: HashMap<SymbolId, usize> = HashMap::new();
    let mut node_to_symbol: HashMap<usize, SymbolId> = HashMap::new();
    let mut dag: Dag<SymbolId> = Dag::new();
    let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::new();

    let root_idx = dag.add_node(root);
    symbol_to_node.insert(root, root_idx);
    node_to_symbol.insert(root_idx, root);
    queue.push_back((root, 0));

    while let Some((symbol, depth)) = queue.pop_front() {
        // The depth limit is how far the layout reaches on its own. A node the
        // user explicitly asked for the calls of is walked one level past it —
        // its children arrive at `depth + 1` and stop there unless they were
        // asked for too — so that gesture *adds* to what is on screen. Moving
        // the window onto the expanded node instead, which is what this used to
        // do, redrew the whole graph around a node the user only wanted more of.
        if depth >= max_depth && !opts.expanded.contains(&symbol) {
            continue;
        }
        // A collapsed node stays on screen; its children simply aren't walked.
        if opts.collapsed.contains(&symbol) {
            continue;
        }
        let parent_idx = symbol_to_node[&symbol];

        let children = match direction {
            CallDirection::Outgoing => graph.callee_ids(&symbol),
            CallDirection::Incoming => graph.caller_ids(&symbol),
        };

        for &child in children {
            let child_idx = if let Some(&idx) = symbol_to_node.get(&child) {
                idx
            } else {
                let idx = dag.add_node(child);
                symbol_to_node.insert(child, idx);
                node_to_symbol.insert(idx, child);
                queue.push_back((child, depth + 1));
                idx
            };
            let _ = dag.add_edge(parent_idx, child_idx);
        }
    }

    let mut engine = match LayoutEngine::with_config(layout_config(opts.orientation)) {
        Ok(e) => e,
        Err(_) => LayoutEngine::new(),
    };
    let layout = match engine.compute_dag(&dag) {
        Ok(l) => l,
        Err(_) => return HashMap::new(),
    };

    let mut positions: HashMap<SymbolId, NodePos> = node_to_symbol
        .iter()
        .filter_map(|(&node_idx, &symbol_id)| {
            layout
                .position(node_idx)
                .map(|pos| (symbol_id, to_centre(&pos, opts.orientation)))
        })
        .collect();

    balance_aspect(&mut positions, opts.orientation);
    positions
}
// ---------------------------------------------------------------------------
// The cluster map
// ---------------------------------------------------------------------------

/// One element of the cluster map: a whole cluster, standing in for the
/// functions inside it.
///
/// It serialises as a string rather than as serde's usual tagged enum, because
/// it is a map key on the wire and JSON has no keys but strings. That costs
/// nothing and buys something: cytoscape needs a stable string element id for
/// every node it draws, and this is one.
///
/// One variant, because a map draws boxes and nothing else — see
/// [`GraphLayout`] for why that is the whole design. It stays an enum so that
/// a map which one day drew, say, the isolated functions no cluster claims
/// would add a variant and a prefix beside `cluster:`, rather than change the
/// type every position on the wire is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutNodeId {
    Cluster(ClusterKey),
}

impl fmt::Display for LayoutNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutNodeId::Cluster(key) => write!(f, "cluster:{}:{}", key.level, key.anchor.0),
        }
    }
}

/// The order a `LayoutNodeId` sorts in is its `Display` order, and that is not
/// an accident worth losing: the string is what the client keys its elements
/// by, so a sort done here and a sort done on the wire agree. It is spelled out
/// rather than derived because a derive gets the level wrong: it is rendered in
/// decimal, so it is compared as digits and not as a number — `cluster:10:`
/// lands before `cluster:1:`.
///
/// This exists because the callers used to sort on `to_string()`, which formats
/// a uuid into a fresh `String` twice per comparison, four times for a full
/// compare, O(n log n) times over.
impl Ord for LayoutNodeId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (LayoutNodeId::Cluster(mine), LayoutNodeId::Cluster(theirs)) => {
                level_cmp(mine.level, theirs.level)
                    .then_with(|| mine.anchor.0.cmp(&theirs.anchor.0))
            }
        }
    }
}

impl PartialOrd for LayoutNodeId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Two cluster levels compared the way their `Display` strings compare: the
/// decimal digits, then the `:` that closes the field. `':'` (0x3a) is above
/// every digit, so a level whose digits are a prefix of another's sorts
/// *after* it — `cluster:1:` after `cluster:10:`. On the stack, because the
/// point of having an `Ord` at all is to stop allocating per comparison.
fn level_cmp(mine: usize, theirs: usize) -> Ordering {
    let (mut mine_buf, mut theirs_buf) = ([0u8; LEVEL_FIELD], [0u8; LEVEL_FIELD]);
    level_field(mine, &mut mine_buf).cmp(level_field(theirs, &mut theirs_buf))
}

/// Twenty digits is a 64-bit `usize` at its widest, and one more for the `:`.
const LEVEL_FIELD: usize = 21;

/// `level` in decimal followed by its `:`, written into `buf`.
fn level_field(level: usize, buf: &mut [u8; LEVEL_FIELD]) -> &[u8] {
    let mut rest = level;
    let mut len = 0;
    loop {
        buf[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    buf[..len].reverse();
    buf[len] = b':';
    &buf[..len + 1]
}

impl std::str::FromStr for LayoutNodeId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let uuid = |raw: &str| {
            uuid::Uuid::parse_str(raw).map_err(|e| format!("`{text}` holds no uuid: {e}"))
        };
        if let Some(rest) = text.strip_prefix("cluster:") {
            let (level, anchor) = rest
                .split_once(':')
                .ok_or_else(|| format!("`{text}` is missing the cluster level"))?;
            let level = level
                .parse::<usize>()
                .map_err(|e| format!("`{text}` holds no level: {e}"))?;
            return Ok(LayoutNodeId::Cluster(ClusterKey {
                level,
                anchor: SymbolId(uuid(anchor)?),
            }));
        }
        Err(format!("`{text}` names no cluster"))
    }
}

impl Serialize for LayoutNodeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LayoutNodeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// One cluster on the map, drawn as a single node standing in for everything
/// inside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaNode {
    pub key: ClusterKey,
    pub label: String,
    /// How many leaf functions the node stands in for — the `+N` the client
    /// draws on it.
    pub count: usize,
    pub level: usize,
}

/// One aggregated call relation between two clusters on the map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutEdge {
    pub source: LayoutNodeId,
    pub target: LayoutNodeId,
    /// How many individual calls this one line stands for.
    ///
    /// The client scaled the line width by it until 8408323, when the map went
    /// back to drawing every edge the way the rooted view draws a call, so
    /// nothing on the canvas reads it now. It stays on the wire as the only
    /// record of what a line aggregates, which is what `aggregate_calls`'s
    /// tests assert on.
    pub weight: usize,
}

/// A laid-out cluster map: one node per cluster at the level being shown, what
/// to write on each, and what calls between them.
///
/// Flat, deliberately. The ladder `logic::cluster` builds is a zoom, not a tree
/// to open: every level partitions the same functions, so a level *is* a whole
/// map and the client shows exactly one at a time. A nested presentation would
/// have to invent which of the ~11 rungs an opened box expands into, and with
/// pair merges the answer is always "two more boxes" — a lot of clicking to
/// travel a ladder that one control walks in a step.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphLayout {
    pub nodes: HashMap<LayoutNodeId, NodePos>,
    pub metas: Vec<MetaNode>,
    pub edges: Vec<LayoutEdge>,
}

/// Where one function sits when the whole call graph is laid out at once: the
/// layer it lands in, and how far along that layer it sits.
///
/// Unit-free on purpose. `cross` counts node slots from the middle of its
/// layer, not pixels, so one frame serves both orientations — the map turns a
/// seat into a position using whichever node size and pitch the orientation
/// being drawn calls for.
#[derive(Debug, Clone, Copy)]
struct Seat {
    layer: u32,
    cross: f32,
}

/// A layout of every function in the graph, kept as seats rather than pixels —
/// the frame the cluster map borrows its geometry from.
///
/// The map used to lay its own quotient out from scratch, which made every rung
/// a different picture of the same program: the boxes a zoom step merged landed
/// wherever crossing minimisation put them, so zooming out shuffled the canvas
/// instead of coarsening it, and a box holding `main` had no reason to sit
/// anywhere near where `main` sits in the call tree. A frame fixes both at once,
/// because every rung is placed by where its members already are.
///
/// Computed over the whole graph and not over the window on screen: the map is
/// a partition of the whole program, so a rooted, depth-limited walk would seat
/// only the handful of functions the tree happens to reach.
#[derive(Debug, Clone, Default)]
pub struct MapFrame {
    seats: HashMap<SymbolId, Seat>,
}

impl MapFrame {
    pub fn is_empty(&self) -> bool {
        self.seats.is_empty()
    }

    pub fn len(&self) -> usize {
        self.seats.len()
    }

    /// Where a whole cluster sits: the average of its members' seats.
    ///
    /// The mean rather than the root-most member, because a box is the whole of
    /// what it holds — a module whose entry point is shallow and whose helpers
    /// are deep belongs between them, not on top of its entry point. `None`
    /// when nothing in the cluster has a seat, which is what a frame one
    /// generation behind the graph says about a cluster made of functions that
    /// arrived after it.
    fn seat_of(&self, members: &[SymbolId]) -> Option<Seat> {
        let mut layer = 0.0f64;
        let mut cross = 0.0f64;
        let mut seated = 0u32;
        for member in members {
            if let Some(seat) = self.seats.get(member) {
                layer += f64::from(seat.layer);
                cross += f64::from(seat.cross);
                seated += 1;
            }
        }
        if seated == 0 {
            return None;
        }
        let n = f64::from(seated);
        Some(Seat {
            layer: (layer / n).round() as u32,
            cross: (cross / n) as f32,
        })
    }
}

/// Lay every function in the graph out at once, and keep where each one landed.
///
/// Rootless, unlike [`compute_layout`]: there is no one function a whole
/// program hangs off, and the map does not have one either. `grid` layers a
/// DAG from its sources, so the frame comes out reading the way the call tree
/// does — callers before what they call — which is the whole point of borrowing
/// it.
///
/// Isolated functions are left out. `logic::cluster` drops them too, so a seat
/// for one could only ever go unused, and each would otherwise take a slot in
/// the first layer and push the graph that does have calls off to one side.
///
/// Not cheap — a few hundred milliseconds over a project's worth of functions,
/// where a snapshot has to be built in a fraction of that — so it is computed
/// on the worker thread that computes a clustering, once per settled graph, and
/// every rung of every map is then placed from the one frame. See
/// `CoreApp::sync_clusterings`.
pub fn compute_map_frame(graph: &CallGraph) -> MapFrame {
    // `function_ids` is a HashMap iteration and `Dag::add_node` hands out
    // indices in call order, so the graph has to be walked in a fixed order or
    // the frame — and every map placed from it — comes out different run to
    // run.
    let mut functions: Vec<SymbolId> = graph
        .function_ids()
        .copied()
        .filter(|id| !graph.callee_ids(id).is_empty() || !graph.caller_ids(id).is_empty())
        .collect();
    functions.sort_unstable_by_key(|id| id.0);
    if functions.is_empty() {
        return MapFrame::default();
    }

    let mut dag: Dag<SymbolId> = Dag::new();
    let index: HashMap<SymbolId, usize> =
        functions.iter().map(|&id| (id, dag.add_node(id))).collect();
    // `callee_ids` reports one entry per call *site*, which is what
    // `aggregate_calls` wants and what a layout does not: the same pair added
    // twelve times is twelve parallel edges for the crossing pass to chew on.
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for caller in &functions {
        let from = index[caller];
        for callee in graph.callee_ids(caller) {
            let Some(&to) = index.get(callee) else {
                continue;
            };
            if seen.insert((from, to)) {
                let _ = dag.add_edge(from, to);
            }
        }
    }

    let mut engine = match LayoutEngine::with_config(layout_config(LayoutOrientation::LeftRight)) {
        Ok(engine) => engine,
        Err(_) => LayoutEngine::new(),
    };
    let Ok(layout) = engine.compute_dag(&dag) else {
        return MapFrame::default();
    };

    // The frame is read back in the units `grid` laid it out in: a layer is at
    // `layer_index * level_separation`, and a node sits `pos_in_layer` slots of
    // `node_separation` from the middle of its layer. Dividing them back out is
    // what makes the frame unit-free — and it is why `balance_aspect` is not
    // run here, since stretching the level axis would leave the layer index no
    // longer recoverable from the coordinate.
    let seats = functions
        .iter()
        .filter_map(|id| {
            let pos = layout.position(index[id])?;
            Some((
                *id,
                Seat {
                    layer: (pos.x / LEVEL_SEPARATION).round().max(0.0) as u32,
                    cross: pos.y / NODE_SEPARATION,
                },
            ))
        })
        .collect();

    MapFrame { seats }
}

/// Lay out one rung of the coarsening ladder: every cluster at `level` as a
/// single node, with the calls between them aggregated onto the lines.
///
/// This is a second path alongside [`compute_layout`], not a replacement. A
/// rooted call tree starts from one function and walks outwards; the map draws
/// a partition of the whole graph. The two share their geometry
/// (`layout_config`, `balance_aspect`) and the frame the map is placed in
/// ([`compute_map_frame`]), which is a `compute_layout`-shaped walk of its own.
///
/// A `level` the ladder does not have draws nothing, which is the right answer
/// while a clustering is still being computed and the frontend is holding a
/// level from the last one. Functions the clustering leaves out — the isolated
/// ones, see [`logic::cluster`] — are in no cluster and therefore on no map.
pub fn compute_cluster_layout(
    graph: &CallGraph,
    clustering: &Clustering,
    frame: &MapFrame,
    level: usize,
    direction: CallDirection,
    orientation: LayoutOrientation,
) -> GraphLayout {
    let ids = clustering.level(level);
    if ids.is_empty() {
        return GraphLayout::default();
    }

    let mut display: Vec<LayoutNodeId> = Vec::with_capacity(ids.len());
    let mut metas: Vec<MetaNode> = Vec::with_capacity(ids.len());
    let mut owner: HashMap<SymbolId, LayoutNodeId> = HashMap::new();
    let mut seats: Vec<Option<Seat>> = Vec::with_capacity(ids.len());

    for &id in ids {
        let Some(cluster) = clustering.cluster(id) else {
            continue;
        };
        let me = LayoutNodeId::Cluster(cluster.key);
        display.push(me);
        metas.push(MetaNode {
            key: cluster.key,
            label: cluster.label.clone(),
            count: cluster.members.len(),
            level: cluster.level,
        });
        for member in &cluster.members {
            owner.insert(*member, me);
        }
        seats.push(frame.seat_of(&cluster.members));
    }

    let edges = aggregate_calls(graph, &owner, direction);
    let mut nodes = place_boxes(&display, &seats, direction, orientation);
    balance_aspect(&mut nodes, orientation);

    GraphLayout {
        nodes,
        metas,
        edges,
    }
}

/// Put each box where its functions already are.
///
/// Three steps, and each one gives up as little of the frame as it has to:
///
/// 1. **A column per layer that is used.** A box's layer is the average of its
///    members', so the layers a map lands on are a subset of the frame's with
///    gaps in it; the gaps are closed rather than drawn, because an empty
///    column is a stretch of blank canvas saying nothing. Reading the map
///    incoming turns the columns round, which is what reversing every edge
///    does to a layering.
/// 2. **A shared cross axis, scaled to fit.** The frame spreads a project over
///    thousands of slots and a rung holds a few dozen boxes, so the cross
///    coordinates are scaled onto the span the busiest column actually needs.
///    It is one scale for the whole map, not one per column: what makes the map
///    readable as the same picture is that a box and the box it calls line up
///    across columns.
/// 3. **Pushed apart only where they collide.** Averaging lands boxes on top of
///    one another; within a column they are separated in the order the frame
///    put them, by the least that clears a node, and the column is then slid
///    back so its middle has not moved. Order is the part of the frame worth
///    defending to the last — that is what a reader recognises — and absolute
///    position is what gives way.
///
/// A box no member of which has a seat cannot be placed by any of this. Those
/// go in a lane of their own past the last column, in the clustering's order:
/// the frame is one generation behind a graph that just grew, so this is a box
/// of functions that arrived after it, and stacking them where they cannot be
/// confused for placed boxes says so better than dropping them.
fn place_boxes(
    display: &[LayoutNodeId],
    seats: &[Option<Seat>],
    direction: CallDirection,
    orientation: LayoutOrientation,
) -> HashMap<LayoutNodeId, NodePos> {
    // 1. Columns. A BTreeSet is what makes "the layers in use, in order" fall
    //    out; the map from layer to column is then just the rank.
    let used: std::collections::BTreeSet<u32> = seats.iter().flatten().map(|s| s.layer).collect();
    let column_of: BTreeMap<u32, usize> = used
        .iter()
        .enumerate()
        .map(|(rank, &layer)| {
            let column = match direction {
                CallDirection::Outgoing => rank,
                CallDirection::Incoming => used.len() - 1 - rank,
            };
            (layer, column)
        })
        .collect();
    let unseated_column = used.len();

    // 2. The cross axis, scaled onto what the busiest column needs. Boxes with
    //    no seat are placed, not scaled — they have no position to preserve —
    //    so they are kept out of both the extent and the scale.
    let mut per_column: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (slot, seat) in seats.iter().enumerate() {
        let column = match seat {
            Some(seat) => column_of[&seat.layer],
            None => unseated_column,
        };
        per_column.entry(column).or_default().push(slot);
    }
    let crowd = per_column
        .iter()
        .filter(|(&column, _)| column != unseated_column)
        .map(|(_, boxes)| boxes.len())
        .max()
        .unwrap_or(0);
    let (low, high) = seats
        .iter()
        .flatten()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), seat| {
            (low.min(seat.cross), high.max(seat.cross))
        });
    let extent = high - low;
    let scale = if extent > f32::EPSILON && crowd > 1 {
        (crowd - 1) as f32 / extent
    } else {
        0.0
    };

    // 3. Within a column: in the frame's order, pushed apart to clear a node,
    //    then slid back onto the middle the column asked for.
    let mut slots: Vec<f32> = vec![0.0; display.len()];
    for (&column, boxes) in &mut per_column {
        let mut wanted: Vec<(usize, f32)> = boxes
            .iter()
            .enumerate()
            .map(|(nth, &slot)| {
                let cross = match seats[slot] {
                    Some(seat) => (seat.cross - low) * scale,
                    // An unseated box has no cross coordinate at all; its lane
                    // is ordered by the clustering, which `display` is already in.
                    None => nth as f32,
                };
                (slot, cross)
            })
            .collect();
        // Ties are broken on the node id so that two boxes whose members
        // average to the same spot do not swap places between runs. On the id
        // itself, whose `Ord` is its `Display` order: rendering the string here
        // instead would format two uuids for every comparison of every box.
        wanted.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| display[a.0].cmp(&display[b.0]))
        });

        let before: f32 = wanted.iter().map(|(_, cross)| *cross).sum::<f32>();
        let mut last = f32::NEG_INFINITY;
        let mut after = 0.0;
        for (slot, cross) in &mut wanted {
            let placed = cross.max(last + 1.0);
            last = placed;
            after += placed;
            slots[*slot] = placed;
        }
        // Spreading a column only ever pushes downwards, so a crowded one
        // drifts off the end of the map it belongs to unless it is recentred.
        // Unseated boxes have no middle worth keeping — they are a lane, not a
        // position — so they are left where the spread put them.
        if column != unseated_column && !wanted.is_empty() {
            let drift = (after - before) / wanted.len() as f32;
            for (slot, _) in &wanted {
                slots[*slot] -= drift;
            }
        }
    }

    // Pixels, at last. Levels advance along the axis the orientation names, and
    // a slot is that orientation's own sibling pitch — a node is 150x32, so the
    // two axes are not interchangeable and reusing one pair for both would give
    // a top-down map a flat stripe, exactly as it would the rooted tree.
    let (level_pitch, cross_pitch) = match orientation {
        LayoutOrientation::LeftRight => (LEVEL_SEPARATION, NODE_SEPARATION),
        LayoutOrientation::TopDown => (LEVEL_SEPARATION_TOPDOWN, NODE_SEPARATION_TOPDOWN),
    };
    display
        .iter()
        .enumerate()
        .map(|(slot, node)| {
            let column = match seats[slot] {
                Some(seat) => column_of[&seat.layer],
                None => unseated_column,
            };
            let level = column as f32 * level_pitch;
            let cross = slots[slot] * cross_pitch;
            let pos = match orientation {
                LayoutOrientation::LeftRight => NodePos { x: level, y: cross },
                LayoutOrientation::TopDown => NodePos { x: cross, y: level },
            };
            (*node, pos)
        })
        .collect()
}

/// Roll every call up to the pair of clusters it runs between.
///
/// [`CallGraph::edges`] would report the same calls one entry per call *site*,
/// but the walk goes the way `logic::cluster` walks them — `callee_ids` per
/// function — for what it costs: `owner` holds only the functions the rung has
/// boxed, which for a map scoped to one neighbourhood is a few dozen of a
/// project's thousands, and a caller with no box is skipped before its calls
/// are looked at. `edges()` would read every call site in the project to find
/// the ones crossing between two boxes. Either way the duplicates in the slice
/// are the multiplicity, counted for free.
///
/// Calls whose two ends land in the same cluster are dropped. Those are the
/// ones internal to a box whose whole purpose is to not show its insides.
///
/// One line per ordered pair, and never both orders: a rung's quotient is a
/// DAG, which `logic::cluster` guarantees rather than this function checking.
fn aggregate_calls(
    graph: &CallGraph,
    owner: &HashMap<SymbolId, LayoutNodeId>,
    direction: CallDirection,
) -> Vec<LayoutEdge> {
    let mut weights: HashMap<(LayoutNodeId, LayoutNodeId), usize> = HashMap::new();

    for caller in graph.function_ids() {
        let Some(&from) = owner.get(caller) else {
            continue;
        };
        for callee in graph.callee_ids(caller) {
            let Some(&to) = owner.get(callee) else {
                continue;
            };
            if from == to {
                continue;
            }
            // `direction` says which way the user is reading the graph, so an
            // incoming map is the same aggregation with every arrow turned
            // round — exactly what `compute_layout` does with caller_ids.
            let pair = match direction {
                CallDirection::Outgoing => (from, to),
                CallDirection::Incoming => (to, from),
            };
            *weights.entry(pair).or_insert(0) += 1;
        }
    }

    let mut edges: Vec<LayoutEdge> = weights
        .iter()
        .map(|(&(source, target), &weight)| LayoutEdge {
            source,
            target,
            weight,
        })
        .collect();

    // `graph.function_ids()` is a HashMap iteration, so the aggregate comes out
    // in a different order every run. These edges go straight to the wire and
    // the client draws them as given, so sorting them is what makes one graph
    // ship one way — and an export of the map diff against the last one. The
    // order is the ids' `Display` order, which their `Ord` is, without a uuid
    // formatted into a fresh `String` twice per comparison.
    edges.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.target.cmp(&b.target))
    });
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    fn graph_with_chain(names: &[&str]) -> (CallGraph, Vec<SymbolId>) {
        let mut graph = CallGraph::new();
        let ids: Vec<SymbolId> = names
            .iter()
            .map(|n| {
                let f = FunctionNode::new(
                    n.to_string(),
                    n.to_string(),
                    Location::new("src/x.c".to_string(), 1, 0),
                );
                let id = f.id;
                graph.add_function(f);
                id
            })
            .collect();
        for pair in ids.windows(2) {
            graph.add_call(pair[0], pair[1], Location::new("src/x.c".to_string(), 1, 0));
        }
        (graph, ids)
    }

    /// A spread of node ids chosen so that their `Display` order disagrees
    /// with the cheaper one: a level is rendered in decimal, so `cluster:10:`
    /// lands before both `cluster:1:` (`'0'` is under the `':'` that ends the
    /// field) and `cluster:9:`.
    fn awkward_ids(anchors: &[SymbolId]) -> Vec<LayoutNodeId> {
        anchors
            .iter()
            .zip([0, 1, 9, 10, 2, 11])
            .map(|(&anchor, level)| LayoutNodeId::Cluster(ClusterKey { level, anchor }))
            .collect()
    }

    #[test]
    fn depth_limits_how_far_the_layout_walks() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c", "d", "e"]);

        for depth in [1usize, 2, 3] {
            let collapsed = HashSet::new();
            let opts = LayoutOptions {
                depth,
                collapsed: &collapsed,
                ..Default::default()
            };
            let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);
            assert_eq!(
                layout.len(),
                depth + 1,
                "depth {depth} should show the root plus {depth} level(s)"
            );
        }
    }

    #[test]
    fn collapsing_a_node_keeps_it_but_drops_its_children() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c", "d"]);
        let mut collapsed = HashSet::new();
        collapsed.insert(ids[1]);

        let opts = LayoutOptions {
            depth: MAX_DEPTH,
            collapsed: &collapsed,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        assert!(layout.contains_key(&ids[0]), "root stays");
        assert!(
            layout.contains_key(&ids[1]),
            "the collapsed node itself stays"
        );
        assert!(!layout.contains_key(&ids[2]), "its children are dropped");
        assert!(!layout.contains_key(&ids[3]));
    }

    #[test]
    fn expanding_a_node_appends_one_level_and_keeps_the_rest() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c", "d", "e"]);
        let collapsed = HashSet::new();
        let mut expanded = HashSet::new();
        expanded.insert(ids[2]);

        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            expanded: &expanded,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        // Everything the depth limit already showed is still there — expanding
        // adds, it does not re-root.
        for (i, id) in ids.iter().take(3).enumerate() {
            assert!(
                layout.contains_key(id),
                "{} is still on screen",
                ["a", "b", "c"][i]
            );
        }
        assert!(
            layout.contains_key(&ids[3]),
            "the expanded node's callee joins it"
        );
        assert!(
            !layout.contains_key(&ids[4]),
            "one level only: the callee's own callees wait to be asked for"
        );
    }

    #[test]
    fn expanding_a_node_off_the_root_path_leaves_the_root_tree_alone() {
        // main → a → b, and main → z. Expanding `a` used to re-root the BFS
        // there, which dropped `z` off the canvas; it has nothing to do with
        // the node the user expanded and should not move.
        let mut graph = CallGraph::new();
        let ids: Vec<SymbolId> = ["main", "a", "b", "z"]
            .iter()
            .map(|name| {
                let node = FunctionNode::new(
                    name.to_string(),
                    name.to_string(),
                    Location::new("src/x.c".to_string(), 1, 0),
                );
                let id = node.id;
                graph.add_function(node);
                id
            })
            .collect();
        let call = |g: &mut CallGraph, from: SymbolId, to: SymbolId| {
            g.add_call(from, to, Location::new("src/x.c".to_string(), 1, 0));
        };
        call(&mut graph, ids[0], ids[1]);
        call(&mut graph, ids[1], ids[2]);
        call(&mut graph, ids[0], ids[3]);

        let collapsed = HashSet::new();
        let mut expanded = HashSet::new();
        expanded.insert(ids[1]);

        let opts = LayoutOptions {
            depth: 1,
            collapsed: &collapsed,
            expanded: &expanded,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        assert!(layout.contains_key(&ids[0]), "the root is still the root");
        assert!(layout.contains_key(&ids[3]), "the sibling branch stays put");
        assert!(layout.contains_key(&ids[2]), "and `a` gained its callee");
    }

    #[test]
    fn collapsing_wins_over_expanding() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c"]);
        let mut collapsed = HashSet::new();
        collapsed.insert(ids[1]);
        let mut expanded = HashSet::new();
        expanded.insert(ids[1]);

        let opts = LayoutOptions {
            depth: MAX_DEPTH,
            collapsed: &collapsed,
            expanded: &expanded,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        assert!(
            !layout.contains_key(&ids[2]),
            "a node collapsed after it was expanded is collapsed"
        );
    }

    #[test]
    fn positions_are_node_centres_in_pixels() {
        let (graph, ids) = graph_with_chain(&["a", "b"]);
        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        let a = layout[&ids[0]];
        let b = layout[&ids[1]];

        // Columns must clear a full node width, or nodes overlap their
        // neighbours — which is exactly what the crate's terminal-unit defaults
        // produced on a pixel canvas.
        let pitch = (b.x - a.x).abs();
        assert!(
            pitch >= NODE_WIDTH + COLUMN_GUTTER,
            "column pitch {pitch} must clear a {NODE_WIDTH}px node plus its gutter"
        );
    }

    #[test]
    fn sibling_rows_clear_the_node_height() {
        let mut graph = CallGraph::new();
        let parent = FunctionNode::new(
            "p".to_string(),
            "p".to_string(),
            Location::new("src/x.c".to_string(), 1, 0),
        );
        let parent_id = parent.id;
        graph.add_function(parent);

        let mut kids = Vec::new();
        for name in ["k1", "k2", "k3"] {
            let f = FunctionNode::new(
                name.to_string(),
                name.to_string(),
                Location::new("src/x.c".to_string(), 1, 0),
            );
            let id = f.id;
            graph.add_function(f);
            graph.add_call(parent_id, id, Location::new("src/x.c".to_string(), 1, 0));
            kids.push(id);
        }

        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(parent_id), CallDirection::Outgoing, &opts);

        let mut ys: Vec<f32> = kids.iter().map(|k| layout[k].y).collect();
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in ys.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= NODE_HEIGHT + ROW_GUTTER,
                "sibling row pitch {gap} must clear a {NODE_HEIGHT}px node plus its gutter"
            );
        }
    }

    #[test]
    fn top_down_advances_levels_downwards_and_spreads_siblings_across() {
        let mut graph = CallGraph::new();
        let parent = FunctionNode::new(
            "p".to_string(),
            "p".to_string(),
            Location::new("src/x.c".to_string(), 1, 0),
        );
        let parent_id = parent.id;
        graph.add_function(parent);

        let mut kids = Vec::new();
        for name in ["k1", "k2"] {
            let f = FunctionNode::new(
                name.to_string(),
                name.to_string(),
                Location::new("src/x.c".to_string(), 1, 0),
            );
            let id = f.id;
            graph.add_function(f);
            graph.add_call(parent_id, id, Location::new("src/x.c".to_string(), 1, 0));
            kids.push(id);
        }

        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            orientation: LayoutOrientation::TopDown,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(parent_id), CallDirection::Outgoing, &opts);

        let p = layout[&parent_id];
        let a = layout[&kids[0]];
        let b = layout[&kids[1]];

        // Levels now advance down, by the top-down level pitch — which is
        // deliberately much more than the node's 32px height, or the rows stack
        // into an unreadable band.
        assert!(a.y > p.y && b.y > p.y, "children sit below the root");
        assert!(
            (a.y - p.y).abs() >= LEVEL_SEPARATION_TOPDOWN,
            "level pitch {} must clear a {NODE_HEIGHT}px node plus its gutter",
            a.y - p.y
        );
        // Siblings spread sideways, clearing a full node width — the axis whose
        // pitch is easy to get wrong once the layout is transposed.
        assert!(
            (b.x - a.x).abs() >= NODE_SEPARATION_TOPDOWN,
            "sibling pitch {} must clear a {NODE_WIDTH}px node plus its gutter",
            b.x - a.x
        );
        assert!((a.y - b.y).abs() < 1.0, "siblings share a row");
    }

    /// A parent with `n` children, rooted at the parent.
    fn fan(n: usize) -> (CallGraph, SymbolId) {
        let mut graph = CallGraph::new();
        let parent = FunctionNode::new(
            "p".to_string(),
            "p".to_string(),
            Location::new("src/x.c".to_string(), 1, 0),
        );
        let parent_id = parent.id;
        graph.add_function(parent);
        for i in 0..n {
            let f = FunctionNode::new(
                format!("k{i}"),
                format!("k{i}"),
                Location::new("src/x.c".to_string(), 1, 0),
            );
            let id = f.id;
            graph.add_function(f);
            graph.add_call(parent_id, id, Location::new("src/x.c".to_string(), 1, 0));
        }
        (graph, parent_id)
    }

    fn extents(layout: &HashMap<SymbolId, NodePos>) -> (f32, f32) {
        let span = |v: Vec<f32>| {
            v.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                - v.iter().cloned().fold(f32::INFINITY, f32::min)
        };
        (
            span(layout.values().map(|p| p.x).collect()),
            span(layout.values().map(|p| p.y).collect()),
        )
    }

    #[test]
    fn a_wide_top_down_fan_gets_its_levels_pushed_apart() {
        let (graph, root) = fan(3);
        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            orientation: LayoutOrientation::TopDown,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(root), CallDirection::Outgoing, &opts);
        let (w, h) = extents(&layout);

        // Siblings side by side are a stripe at the base pitch: the level axis
        // stretches until the box is the shape a canvas actually is.
        assert!(
            h > LEVEL_SEPARATION_TOPDOWN,
            "level pitch {h} should exceed the base {LEVEL_SEPARATION_TOPDOWN}"
        );
        assert!(
            (w / h - TARGET_ASPECT).abs() < 0.05,
            "aspect {} should land on the target {TARGET_ASPECT}",
            w / h
        );
    }

    #[test]
    fn the_stretch_stops_at_the_cap() {
        let (graph, root) = fan(20);
        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: 2,
            collapsed: &collapsed,
            orientation: LayoutOrientation::TopDown,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(root), CallDirection::Outgoing, &opts);
        let (_, h) = extents(&layout);

        // Twenty siblings in a row can't be balanced without putting a screenful
        // between one level and the next, so the stripe is improved, not cured.
        assert!(
            (h - LEVEL_SEPARATION_TOPDOWN * MAX_LEVEL_STRETCH).abs() < 1.0,
            "level pitch {h} should be the base pitch times the {MAX_LEVEL_STRETCH}× cap"
        );
    }

    #[test]
    fn a_graph_that_already_reads_well_is_left_alone() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c", "d"]);
        let collapsed = HashSet::new();
        let opts = LayoutOptions {
            depth: MAX_DEPTH,
            collapsed: &collapsed,
            ..Default::default()
        };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);
        let (w, _) = extents(&layout);

        // A chain is all level axis and no cross axis: nothing to balance, so
        // the pitch must come back exactly as configured.
        assert!(
            (w - LEVEL_SEPARATION * 3.0).abs() < 1.0,
            "a straight chain keeps its {LEVEL_SEPARATION}px pitch, got {w}"
        );
    }

    #[test]
    fn unknown_or_missing_root_yields_nothing() {
        let (graph, _) = graph_with_chain(&["a"]);
        let opts = LayoutOptions::default();
        assert!(compute_layout(&graph, None, CallDirection::Outgoing, &opts).is_empty());
    }

    // -----------------------------------------------------------------------
    // The cluster map
    // -----------------------------------------------------------------------

    use crate::demo::create_demo_call_graph;
    use logic::{ClusterMethod, Clustering};

    fn demo() -> (CallGraph, Clustering) {
        let graph = create_demo_call_graph();
        let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
        (graph, clustering)
    }

    fn map_at(graph: &CallGraph, clustering: &Clustering, level: usize) -> GraphLayout {
        compute_cluster_layout(
            graph,
            clustering,
            &compute_map_frame(graph),
            level,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        )
    }

    fn keys_of(layout: &GraphLayout) -> HashSet<ClusterKey> {
        layout
            .nodes
            .keys()
            .map(|LayoutNodeId::Cluster(key)| *key)
            .collect()
    }

    /// A rung is a whole map: every cluster at that level, and nothing else.
    #[test]
    fn a_map_is_one_node_per_cluster_at_the_rung() {
        let (graph, clustering) = demo();

        for level in 0..clustering.level_count() {
            let layout = map_at(&graph, &clustering, level);
            let expected: HashSet<ClusterKey> = clustering
                .level(level)
                .iter()
                .filter_map(|id| clustering.cluster(*id))
                .map(|c| c.key)
                .collect();

            assert_eq!(keys_of(&layout), expected, "rung {level}");
            assert_eq!(
                layout.nodes.len(),
                expected.len(),
                "rung {level} draws nothing but its clusters"
            );
            assert_eq!(layout.metas.len(), expected.len(), "each one is named");
        }
    }

    /// Zooming changes the grain, never the coverage — that is what makes the
    /// ladder a zoom rather than a tree to open.
    #[test]
    fn every_rung_covers_the_same_functions() {
        let (graph, clustering) = demo();

        let members = |level: usize| -> HashSet<SymbolId> {
            map_at(&graph, &clustering, level)
                .metas
                .iter()
                .flat_map(|meta| {
                    clustering
                        .by_key(meta.key)
                        .map(|c| c.members.clone())
                        .unwrap_or_default()
                })
                .collect()
        };

        let coarsest = members(clustering.level_count() - 1);
        assert!(!coarsest.is_empty());
        for level in 0..clustering.level_count() {
            assert_eq!(members(level), coarsest, "rung {level}");
        }
    }

    #[test]
    fn a_finer_rung_is_a_wider_map() {
        let (graph, clustering) = demo();
        assert!(
            clustering.level_count() > 1,
            "the demo graph takes more than one round"
        );

        let widths: Vec<usize> = (0..clustering.level_count())
            .map(|level| map_at(&graph, &clustering, level).nodes.len())
            .collect();
        for pair in widths.windows(2) {
            assert!(pair[1] < pair[0], "a coarser rung is narrower: {widths:?}");
        }
    }

    /// A level the ladder does not have draws nothing — which is what a client
    /// holding a level from a taller ladder asks for.
    #[test]
    fn a_rung_that_does_not_exist_lays_out_nothing() {
        let (graph, clustering) = demo();
        let layout = map_at(&graph, &clustering, clustering.level_count() + 5);
        assert!(layout.nodes.is_empty());
        assert!(layout.metas.is_empty());
        assert!(layout.edges.is_empty());
    }

    #[test]
    fn an_empty_clustering_lays_out_nothing() {
        let layout = map_at(&CallGraph::new(), &Clustering::default(), 0);
        assert!(layout.nodes.is_empty());
        assert!(layout.metas.is_empty());
        assert!(layout.edges.is_empty());
    }

    #[test]
    fn a_meta_node_says_what_it_hides_and_what_to_call_it() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;
        let layout = map_at(&graph, &clustering, top);

        for meta in &layout.metas {
            let cluster = clustering.by_key(meta.key).expect("a key from the map");
            assert_eq!(meta.count, cluster.members.len());
            assert_eq!(meta.label, cluster.label);
            assert_eq!(meta.level, top);
            assert!(!meta.label.trim().is_empty(), "every box carries a name");
        }
    }

    // -- edges --------------------------------------------------------------

    /// Two heavy pairs and a one-way chain `a -> b -> c` that would cross
    /// between them in both directions if both pairs merged — the shape
    /// modularity alone draws as two boxes calling each other, and the one
    /// `logic::cluster` refuses. The ten spare functions are there only to put
    /// the graph over `TARGET_TOP_CLUSTERS`, so that a round of merging
    /// happens at all.
    fn crossing_graph() -> (CallGraph, Clustering) {
        let mut graph = CallGraph::new();
        let add = |graph: &mut CallGraph, name: &str, line: u32| {
            graph.add_function(FunctionNode::new(
                name.to_string(),
                name.to_string(),
                Location::new("src/x.c".to_string(), line, 0),
            ))
        };
        let call = |graph: &mut CallGraph, from: SymbolId, to: SymbolId, line: u32| {
            graph.add_call(from, to, Location::new("src/x.c".to_string(), line, 0));
        };

        let a = add(&mut graph, "a", 1);
        let b = add(&mut graph, "b", 2);
        let c = add(&mut graph, "c", 3);
        let d = add(&mut graph, "d", 4);
        // Heavy inside each pair, so that is what pays to merge.
        for line in 100..105 {
            call(&mut graph, a, c, line);
            call(&mut graph, b, d, line + 50);
        }
        // Light between them, once each way.
        call(&mut graph, a, b, 200);
        call(&mut graph, b, c, 201);

        for i in 0..5 {
            let left = add(&mut graph, &format!("spare{i}l"), 300 + i * 2);
            let right = add(&mut graph, &format!("spare{i}r"), 301 + i * 2);
            for line in 0..5 {
                call(&mut graph, left, right, 400 + i * 10 + line);
            }
        }

        let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
        (graph, clustering)
    }

    #[test]
    fn edge_weights_count_the_calls_that_actually_cross() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;
        let layout = map_at(&graph, &clustering, top);

        let owner: HashMap<SymbolId, ClusterKey> = layout
            .metas
            .iter()
            .flat_map(|meta| {
                let members = clustering
                    .by_key(meta.key)
                    .map(|c| c.members.clone())
                    .unwrap_or_default();
                members.into_iter().map(move |m| (m, meta.key))
            })
            .collect();

        let mut expected: HashMap<(ClusterKey, ClusterKey), usize> = HashMap::new();
        for caller in graph.function_ids() {
            let Some(&from) = owner.get(caller) else {
                continue;
            };
            for callee in graph.get_callees(caller) {
                let Some(&to) = owner.get(&callee.id) else {
                    continue;
                };
                if from != to {
                    *expected.entry((from, to)).or_insert(0) += 1;
                }
            }
        }

        let mut drawn = 0usize;
        for edge in &layout.edges {
            let (LayoutNodeId::Cluster(source), LayoutNodeId::Cluster(target)) =
                (edge.source, edge.target);
            assert_eq!(
                Some(edge.weight),
                expected.get(&(source, target)).copied(),
                "a line stands for every call across its boundary"
            );
            drawn += 1;
        }
        assert_eq!(drawn, expected.len(), "one line per ordered pair");
    }

    /// The aggregate is walked out of a `HashMap`, so its order is imposed —
    /// and it is the node ids' `Display` order, the string the client keys its
    /// elements by. Nothing downstream re-sorts it, so this is the wire order.
    #[test]
    fn the_aggregate_ships_in_the_node_ids_display_order() {
        let (mut graph, ids) = graph_with_chain(&["a", "b", "c", "d", "e", "f"]);
        // Every function calls every later one, so a source repeats and the
        // target is what breaks the tie.
        for (nth, &from) in ids.iter().enumerate() {
            for &to in &ids[nth + 1..] {
                graph.add_call(from, to, Location::new("src/x.c".to_string(), 2, 0));
            }
        }
        let display = awkward_ids(&ids);
        let owner: HashMap<SymbolId, LayoutNodeId> =
            ids.iter().copied().zip(display.iter().copied()).collect();

        let shipped: Vec<(String, String)> =
            aggregate_calls(&graph, &owner, CallDirection::Outgoing)
                .iter()
                .map(|edge| (edge.source.to_string(), edge.target.to_string()))
                .collect();

        assert_eq!(shipped.len(), 15, "every ordered pair crosses a boundary");
        let mut expected = shipped.clone();
        expected.sort();
        assert_eq!(
            shipped, expected,
            "sorted on the ids as the client sees them"
        );
        assert!(
            shipped.windows(2).any(|pair| pair[0].0 == pair[1].0),
            "and a repeated source, or the target tie-break went untested"
        );
    }

    /// The map never draws a boundary crossed both ways: `logic::cluster`
    /// refuses the grouping, so there is nothing here to fold.
    #[test]
    fn no_boundary_on_the_map_is_crossed_both_ways() {
        let (graph, clustering) = crossing_graph();
        for level in 0..clustering.level_count() {
            let layout = map_at(&graph, &clustering, level);
            let pairs: HashSet<(String, String)> = layout
                .edges
                .iter()
                .map(|e| (e.source.to_string(), e.target.to_string()))
                .collect();
            for (source, target) in &pairs {
                assert!(
                    !pairs.contains(&(target.clone(), source.clone())),
                    "rung {level} draws {source} -> {target} both ways"
                );
            }
        }
    }

    #[test]
    fn reading_the_map_incoming_turns_every_arrow_round() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;

        let out = compute_cluster_layout(
            &graph,
            &clustering,
            &compute_map_frame(&graph),
            top,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        let incoming = compute_cluster_layout(
            &graph,
            &clustering,
            &compute_map_frame(&graph),
            top,
            CallDirection::Incoming,
            LayoutOrientation::LeftRight,
        );

        let pairs = |layout: &GraphLayout| -> HashSet<(String, String)> {
            layout
                .edges
                .iter()
                .map(|e| (e.source.to_string(), e.target.to_string()))
                .collect()
        };
        let reversed: HashSet<(String, String)> = pairs(&out)
            .into_iter()
            .map(|(source, target)| (target, source))
            .collect();
        assert_eq!(pairs(&incoming), reversed);
    }

    #[test]
    fn the_map_transposes_for_a_top_down_orientation() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;

        let left_right = compute_cluster_layout(
            &graph,
            &clustering,
            &compute_map_frame(&graph),
            top,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        let top_down = compute_cluster_layout(
            &graph,
            &clustering,
            &compute_map_frame(&graph),
            top,
            CallDirection::Outgoing,
            LayoutOrientation::TopDown,
        );

        assert_eq!(
            left_right.nodes.keys().collect::<HashSet<_>>(),
            top_down.nodes.keys().collect::<HashSet<_>>(),
            "the same map, laid out along the other axis"
        );

        let spread = |layout: &GraphLayout, axis: fn(&NodePos) -> f32| {
            let values: Vec<f32> = layout.nodes.values().map(axis).collect();
            let min = values.iter().cloned().fold(f32::INFINITY, f32::min);
            let max = values.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            max - min
        };
        assert!(
            spread(&top_down, |p| p.y) > 0.0 && spread(&left_right, |p| p.x) > 0.0,
            "levels advance along the axis the orientation names"
        );
    }

    #[test]
    fn two_runs_lay_the_same_map_out_identically() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;
        let first = map_at(&graph, &clustering, top);
        let second = map_at(&graph, &clustering, top);

        assert_eq!(
            serde_json::to_string(&first.edges).unwrap(),
            serde_json::to_string(&second.edges).unwrap(),
            "a HashMap iteration must not decide what the file says"
        );
        for (node, pos) in &first.nodes {
            let other = second.nodes.get(node).expect("the same nodes");
            assert_eq!((pos.x, pos.y), (other.x, other.y));
        }
    }

    // -- the frame -----------------------------------------------------------

    /// The frame reads the way the call tree does: a caller before what it
    /// calls, which is the whole reason the map is worth placing in it.
    #[test]
    fn the_frame_seats_a_caller_ahead_of_what_it_calls() {
        let (graph, chain) = graph_with_chain(&["a", "b", "c", "d"]);
        let frame = compute_map_frame(&graph);

        assert_eq!(
            frame.len(),
            chain.len(),
            "every function in the chain is seated"
        );
        let layers: Vec<u32> = chain
            .iter()
            .map(|id| frame.seats.get(id).expect("a seat").layer)
            .collect();
        assert_eq!(layers, vec![0, 1, 2, 3], "one layer per step of the chain");
    }

    /// A function nothing calls and that calls nothing is in no cluster, so a
    /// seat for it could only take a slot in the first layer and push the graph
    /// that does have calls sideways.
    #[test]
    fn the_frame_leaves_out_what_has_no_calls() {
        let (mut graph, chain) = graph_with_chain(&["a", "b"]);
        let alone = graph.add_function(FunctionNode::new(
            "alone".to_string(),
            "alone".to_string(),
            Location::new("alone.c".to_string(), 1, 1),
        ));

        let frame = compute_map_frame(&graph);
        assert_eq!(frame.len(), chain.len());
        assert!(!frame.seats.contains_key(&alone));
    }

    /// A box is the whole of what it holds, so it sits at the average of its
    /// members rather than on top of whichever one is nearest the front.
    #[test]
    fn a_box_sits_at_the_average_of_its_members() {
        let (graph, chain) = graph_with_chain(&["a", "b", "c", "d", "e"]);
        let frame = compute_map_frame(&graph);

        let seat = frame.seat_of(&chain[0..5]).expect("the chain is seated");
        assert_eq!(seat.layer, 2, "layers 0..4 average to the middle one");
        assert!(
            frame.seat_of(&[]).is_none(),
            "an empty box has nowhere to be"
        );
    }

    // -- placement ------------------------------------------------------------

    fn boxes(n: usize) -> Vec<LayoutNodeId> {
        (0..n)
            .map(|i| {
                LayoutNodeId::Cluster(ClusterKey {
                    level: 0,
                    anchor: SymbolId::from_content(&format!("b{i}"), "b.c", i as u32),
                })
            })
            .collect()
    }

    fn placed(
        display: &[LayoutNodeId],
        seats: &[Option<Seat>],
        direction: CallDirection,
    ) -> Vec<NodePos> {
        let nodes = place_boxes(display, seats, direction, LayoutOrientation::LeftRight);
        display.iter().map(|node| nodes[node]).collect()
    }

    /// A layer with no box in it is closed up rather than drawn: an empty
    /// column is a stretch of blank canvas saying nothing.
    #[test]
    fn a_column_per_layer_that_is_used() {
        let display = boxes(3);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 0.0,
            }),
            Some(Seat {
                layer: 2,
                cross: 0.0,
            }),
            Some(Seat {
                layer: 9,
                cross: 0.0,
            }),
        ];

        let out = placed(&display, &seats, CallDirection::Outgoing);
        assert_eq!(
            out.iter().map(|p| p.x).collect::<Vec<_>>(),
            vec![0.0, LEVEL_SEPARATION, 2.0 * LEVEL_SEPARATION]
        );
    }

    /// Reading the map the other way round is what reversing every call does to
    /// a layering: the callers end up on the far side.
    #[test]
    fn reading_the_map_incoming_turns_the_columns_round() {
        let display = boxes(3);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 0.0,
            }),
            Some(Seat {
                layer: 1,
                cross: 0.0,
            }),
            Some(Seat {
                layer: 2,
                cross: 0.0,
            }),
        ];

        let out = placed(&display, &seats, CallDirection::Incoming);
        assert_eq!(
            out.iter().map(|p| p.x).collect::<Vec<_>>(),
            vec![2.0 * LEVEL_SEPARATION, LEVEL_SEPARATION, 0.0]
        );
    }

    /// Averaging lands boxes on top of one another. They are pushed apart by
    /// the least that clears a node, and the column is slid back so that
    /// spreading it has not moved where the column as a whole sits.
    #[test]
    fn boxes_in_one_column_clear_each_other_without_moving_the_column() {
        let display = boxes(3);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 4.0
            });
            3
        ];

        let mut out: Vec<f32> = placed(&display, &seats, CallDirection::Outgoing)
            .iter()
            .map(|p| p.y)
            .collect();
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in out.windows(2) {
            assert!(
                pair[1] - pair[0] >= NODE_SEPARATION - 0.01,
                "a box has to clear the one above it: {out:?}"
            );
        }
        let middle = out.iter().sum::<f32>() / out.len() as f32;
        assert!(
            middle.abs() < 0.01,
            "three boxes that all wanted the same spot end up centred on it: {middle}"
        );
    }

    /// Boxes whose members average to one spot have nothing to order them but
    /// their ids, and two runs that disagree would swap them on the canvas. The
    /// order is the ids' `Display` order, the same one the aggregate ships in.
    #[test]
    fn boxes_wanting_one_spot_are_ordered_by_node_id() {
        let anchors: Vec<SymbolId> = (0..6)
            .map(|i| SymbolId::from_content(&format!("b{i}"), "b.c", i))
            .collect();
        let display = awkward_ids(&anchors);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 4.0
            });
            6
        ];

        let out = place_boxes(
            &display,
            &seats,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        let mut down_the_column: Vec<(String, f32)> = display
            .iter()
            .map(|node| (node.to_string(), out[node].y))
            .collect();
        down_the_column.sort_by(|a, b| a.1.partial_cmp(&b.1).expect("no box is at NaN"));

        let placed: Vec<String> = down_the_column.into_iter().map(|(id, _)| id).collect();
        let mut expected = placed.clone();
        expected.sort();
        assert_eq!(placed, expected);
    }

    /// Order is the part of the frame worth defending to the last: it is what a
    /// reader recognises from the call tree, and absolute position is what
    /// gives way when the boxes will not fit.
    #[test]
    fn a_crowded_column_keeps_the_order_the_frame_put_it_in() {
        let display = boxes(4);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 30.0,
            }),
            Some(Seat {
                layer: 0,
                cross: 10.0,
            }),
            Some(Seat {
                layer: 0,
                cross: 20.0,
            }),
            Some(Seat {
                layer: 0,
                cross: 0.0,
            }),
        ];

        let out = placed(&display, &seats, CallDirection::Outgoing);
        let order: Vec<usize> = {
            let mut slots: Vec<usize> = (0..display.len()).collect();
            slots.sort_by(|a, b| out[*a].y.partial_cmp(&out[*b].y).unwrap());
            slots
        };
        assert_eq!(
            order,
            vec![3, 1, 2, 0],
            "top to bottom, as the frame had them"
        );
    }

    /// The frame is one generation behind a graph that just grew, so a box of
    /// functions that arrived after it cannot be placed by any of this. Stacked
    /// past the last column, where they cannot be read as a position.
    #[test]
    fn a_box_with_no_seat_waits_past_the_last_column() {
        let display = boxes(3);
        let seats = vec![
            Some(Seat {
                layer: 0,
                cross: 0.0,
            }),
            Some(Seat {
                layer: 1,
                cross: 0.0,
            }),
            None,
        ];

        let out = placed(&display, &seats, CallDirection::Outgoing);
        assert_eq!(out[2].x, 2.0 * LEVEL_SEPARATION);
        assert!(out[2].x > out[0].x && out[2].x > out[1].x);
    }

    /// The point of the whole thing, end to end: a box holding the entry point
    /// reads before a box holding the allocator everything bottoms out in, at
    /// every rung of the ladder — which is where they are in the call tree, and
    /// where they were at the rung before.
    #[test]
    fn a_box_lands_where_its_functions_are_at_every_rung() {
        let (graph, clustering) = demo();
        let holder = |layout: &GraphLayout, name: &str| -> NodePos {
            let symbol = graph
                .function_ids()
                .find(|id| graph.get_function(id).is_some_and(|f| f.name == name))
                .copied()
                .expect("the demo graph has it");
            let key = clustering
                .cluster(
                    clustering
                        .cluster_of_at(&symbol, layout.metas[0].level)
                        .expect("a cluster at this rung"),
                )
                .expect("a cluster")
                .key;
            layout.nodes[&LayoutNodeId::Cluster(key)]
        };

        for level in 0..clustering.level_count() {
            let layout = map_at(&graph, &clustering, level);
            assert!(
                holder(&layout, "main").x < holder(&layout, "xmalloc").x,
                "rung {level}: the entry point reads before the allocator"
            );
        }
    }

    /// The whole of `Ord`'s claim: sorting by it is sorting by the string, on
    /// the case a derive would have got wrong — a two-digit level read as
    /// digits rather than as a number.
    #[test]
    fn a_node_id_sorts_exactly_as_its_display_string_does() {
        let anchors: Vec<SymbolId> = (0..4)
            .map(|i| SymbolId::from_content(&format!("a{i}"), "a.c", i))
            .collect();
        let mut ids: Vec<LayoutNodeId> = Vec::new();
        for level in [0, 1, 2, 9, 10, 11, 19, 100, 101] {
            ids.extend(
                anchors
                    .iter()
                    .map(|&anchor| LayoutNodeId::Cluster(ClusterKey { level, anchor })),
            );
        }

        let mut by_ord = ids.clone();
        by_ord.sort();
        let mut by_string: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        by_string.sort();

        assert_eq!(
            by_ord.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
            by_string
        );
        let ten = by_string.iter().position(|s| s.starts_with("cluster:10:"));
        let one = by_string.iter().position(|s| s.starts_with("cluster:1:"));
        assert!(
            ten < one,
            "the level is digits, not a number: {ten:?} {one:?}"
        );
    }

    /// A `LayoutNodeId` is a JSON *map key*, and JSON has no keys but strings —
    /// so its string form is the wire contract, not an implementation detail.
    #[test]
    fn a_layout_node_id_is_a_string_on_the_wire() {
        let anchor = SymbolId::from_content("a", "a.c", 1);
        let node = LayoutNodeId::Cluster(ClusterKey { level: 2, anchor });

        let json = serde_json::to_value(node).expect("serialises");
        assert!(json.is_string(), "{json} has to be usable as a map key");
        assert_eq!(
            serde_json::from_value::<LayoutNodeId>(json).expect("round-trips"),
            node
        );

        let mut nodes = HashMap::new();
        nodes.insert(node, NodePos { x: 1.0, y: 2.0 });
        let layout = GraphLayout {
            nodes,
            metas: Vec::new(),
            edges: Vec::new(),
        };
        let json = serde_json::to_value(&layout).expect("a whole layout serialises");
        assert!(json["nodes"][format!("cluster:2:{}", anchor.0)].is_object());
    }
}
