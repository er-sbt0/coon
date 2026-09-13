use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use grid::{Dag, LayoutConfig, LayoutEngine};
use logic::{ClusterId, ClusterKey, Clustering};
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
            CallDirection::Outgoing => graph.get_callees(&symbol),
            CallDirection::Incoming => graph.get_callers(&symbol),
        };

        for child_func in children {
            let child_idx = if let Some(&idx) = symbol_to_node.get(&child_func.id) {
                idx
            } else {
                let idx = dag.add_node(child_func.id);
                symbol_to_node.insert(child_func.id, idx);
                node_to_symbol.insert(idx, child_func.id);
                queue.push_back((child_func.id, depth + 1));
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

/// One element of the cluster map: a function, or a whole community standing in
/// for the functions inside it.
///
/// It serialises as a string rather than as serde's usual tagged enum, because
/// it is a map key on the wire and JSON has no keys but strings. That costs
/// nothing and buys something: cytoscape needs a stable string element id for
/// every node it draws, and this is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutNodeId {
    Symbol(SymbolId),
    Cluster(ClusterKey),
}

impl fmt::Display for LayoutNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutNodeId::Symbol(symbol) => write!(f, "fn:{}", symbol.0),
            LayoutNodeId::Cluster(key) => write!(f, "cluster:{}:{}", key.level, key.anchor.0),
        }
    }
}

impl std::str::FromStr for LayoutNodeId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let uuid = |raw: &str| {
            uuid::Uuid::parse_str(raw).map_err(|e| format!("`{text}` holds no uuid: {e}"))
        };
        if let Some(rest) = text.strip_prefix("fn:") {
            return Ok(LayoutNodeId::Symbol(SymbolId(uuid(rest)?)));
        }
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
        Err(format!("`{text}` is neither a function nor a cluster"))
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

/// A labelled box around everything one expanded cluster turned into.
///
/// There is deliberately **no rectangle** here. The client draws a group as a
/// cytoscape compound parent, which sizes and places itself around whatever
/// children it is given; working a rect out in Rust would be computing the same
/// geometry twice, and the Rust copy would drift the moment cytoscape's
/// padding, label height or child bounds changed under it. So a group says what
/// is inside it and the renderer decides where that puts its edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupBox {
    pub key: ClusterKey,
    pub label: String,
    /// How many functions the cluster holds altogether, however few of them the
    /// current expansion actually draws.
    pub count: usize,
    pub level: usize,
    /// The elements whose parent this box is: display nodes, plus the key of
    /// any nested group. A `Cluster` id in here names a group exactly when it is
    /// *not* also in [`GraphLayout::nodes`] — a cluster is either expanded, and
    /// so a box listed in [`GraphLayout::groups`], or collapsed, and so a node
    /// described by a [`MetaNode`], and never both.
    pub members: Vec<LayoutNodeId>,
}

/// A community the user has not opened, drawn as a single node standing in for
/// everything inside it.
///
/// The counterpart to [`GroupBox`], and deliberately the same four fields: a
/// cluster that is closed is a node and needs a name on it, a cluster that is
/// open is a box and needs a name on that. Which of the two a `ClusterKey`
/// names is not something the client should have to work out for itself, and
/// the snapshot carries no [`Clustering`] for it to work it out *from* — so the
/// layout says so, once, in whichever list the cluster landed in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaNode {
    pub key: ClusterKey,
    pub label: String,
    /// How many leaf functions the meta-node stands in for — the `+N` the
    /// client draws on it.
    pub count: usize,
    pub level: usize,
}

/// One aggregated call relation between two display nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutEdge {
    pub source: LayoutNodeId,
    pub target: LayoutNodeId,
    /// How many individual calls this one line stands for, so the client can
    /// draw a heavily used relation more heavily than a stray one.
    pub weight: usize,
}

/// A laid-out cluster map: where each element sits, which boxes enclose them,
/// and what calls between them.
///
/// `groups` and `metas` are two halves of one statement about the clustering:
/// every cluster the walk reached is in exactly one of them — `groups` the ones
/// that were opened, `metas` the ones that were not — and the `metas` are
/// precisely the `Cluster` keys that appear in `nodes`. So a client can draw the
/// whole map from this one structure: `nodes` says where things sit, `metas`
/// says what to write on the closed ones, `groups` what to write on the boxes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphLayout {
    pub nodes: HashMap<LayoutNodeId, NodePos>,
    pub groups: Vec<GroupBox>,
    pub metas: Vec<MetaNode>,
    pub edges: Vec<LayoutEdge>,
}

/// Lay out the cluster map: the communities the user has not opened as single
/// meta-nodes, the ones they have as boxes around whatever is a level finer.
///
/// This is a second path alongside [`compute_layout`], not a replacement.
/// A rooted call tree starts from one function and walks outwards; the map
/// starts from the whole graph and walks *inwards* as far as `expanded` says,
/// so the two share their geometry (`layout_config`, `to_centre`,
/// `balance_aspect`) and nothing else.
///
/// Functions the clustering leaves out — the isolated ones, see
/// [`logic::cluster`] — are in no cluster and therefore on no map.
pub fn compute_cluster_layout(
    graph: &CallGraph,
    clustering: &Clustering,
    expanded: &HashSet<ClusterKey>,
    direction: CallDirection,
    orientation: LayoutOrientation,
) -> GraphLayout {
    let mut walk = Walk::default();

    for &id in clustering.top_level() {
        descend(clustering, id, expanded, None, &mut walk);
    }

    let Walk {
        display,
        node_groups,
        owner,
        groups,
        metas,
        ..
    } = walk;

    if display.is_empty() {
        return GraphLayout::default();
    }

    let edges = aggregate_calls(graph, &owner, direction);

    // `Dag::add_node` hands out indices in call order, so a node's index is its
    // position in `display` and no second lookup table is needed.
    let mut dag: Dag<LayoutNodeId> = Dag::new();
    for node in &display {
        dag.add_node(*node);
    }
    let index: HashMap<LayoutNodeId, usize> = display
        .iter()
        .enumerate()
        .map(|(i, node)| (*node, i))
        .collect();
    for edge in &edges {
        // Aggregated calls between communities are freely cyclic — A calls into
        // B and B calls back — which `grid` breaks for itself in its cycle pass.
        let _ = dag.add_edge(index[&edge.source], index[&edge.target]);
    }

    let mut engine = match LayoutEngine::with_config(layout_config(orientation)) {
        Ok(e) => e,
        Err(_) => LayoutEngine::new(),
    };
    // Grouped, not flat: a box is only drawable if the nodes it encloses sit
    // next to each other. Left to the plain barycentre ordering a community's
    // members land wherever their neighbours pull them, and two compound
    // parents drawn around two scattered sets interpenetrate — which reads as a
    // broken canvas rather than as a map. See `LayoutEngine::compute_dag_grouped`.
    let layout = match engine.compute_dag_grouped(&dag, &node_groups) {
        Ok(l) => l,
        Err(_) => return GraphLayout::default(),
    };

    let mut nodes: HashMap<LayoutNodeId, NodePos> = display
        .iter()
        .enumerate()
        .filter_map(|(i, node)| {
            layout
                .position(i)
                .map(|pos| (*node, to_centre(&pos, orientation)))
        })
        .collect();
    balance_aspect(&mut nodes, orientation);

    GraphLayout {
        nodes,
        groups,
        metas,
        edges,
    }
}

/// What one walk of the dendrogram builds up.
///
/// The walk produces four things at once and they have to stay in step — the
/// display list and its group tags are read by index, and `owner` decides which
/// display node a call lands on — so they travel together rather than as six
/// `&mut` parameters threaded through the recursion.
#[derive(Default)]
struct Walk {
    /// Everything that gets a position, in the order it was reached.
    display: Vec<LayoutNodeId>,
    /// Parallel to `display`: the innermost open cluster each node sits in, as
    /// an id `grid` treats as opaque. A node inside nested open clusters takes
    /// the deepest one, because that is the box whose edge it must not be
    /// separated from — hold the innermost runs together and the outer boxes,
    /// built from those runs, hold together too.
    node_groups: Vec<Option<u32>>,
    /// Which display node each clustered function's calls are attributed to.
    owner: HashMap<SymbolId, LayoutNodeId>,
    groups: Vec<GroupBox>,
    metas: Vec<MetaNode>,
    /// Handed out on entering an expanded cluster. Ids need only be distinct,
    /// and the walk is deterministic, so a counter is as good a name as any.
    next_group: u32,
}

/// Turn one cluster into either a meta-node or a box, recursing while the user
/// has opened what we find.
///
/// Always returns the cluster's own id, because that is what its parent box
/// wants to list either way: collapsed it names the meta-node, expanded it
/// names the nested box.
///
/// A cluster that `expanded` names but whose parent is *closed* is never
/// reached, so a stale expansion set costs nothing — the deeper entry simply
/// waits, and takes effect the moment the user opens the cluster above it.
///
/// `enclosing` is the group id of the innermost box we are already inside, and
/// is what anything placed here is tagged with unless this cluster opens a box
/// of its own and claims it.
fn descend(
    clustering: &Clustering,
    id: ClusterId,
    expanded: &HashSet<ClusterKey>,
    enclosing: Option<u32>,
    walk: &mut Walk,
) -> Option<LayoutNodeId> {
    let cluster = clustering.cluster(id)?;
    let me = LayoutNodeId::Cluster(cluster.key);

    if !expanded.contains(&cluster.key) {
        walk.display.push(me);
        // A closed cluster belongs to whatever box we are inside, if any: it is
        // a single node, so there is no run of its own to keep together.
        walk.node_groups.push(enclosing);
        walk.metas.push(MetaNode {
            key: cluster.key,
            label: cluster.label.clone(),
            count: cluster.members.len(),
            level: cluster.level,
        });
        // Every call into or out of any member is a call on the meta-node now.
        for member in &cluster.members {
            walk.owner.insert(*member, me);
        }
        return Some(me);
    }

    // Opening a cluster draws a box, and everything the recursion puts on the
    // canvas from here down is inside it unless a deeper box claims it first.
    let mine = walk.next_group;
    walk.next_group += 1;

    // Level 0 is as fine as the dendrogram goes, so opening one of those boxes
    // is what finally puts functions on the canvas.
    let members: Vec<LayoutNodeId> = if cluster.level == 0 {
        cluster
            .members
            .iter()
            .map(|symbol| {
                let node = LayoutNodeId::Symbol(*symbol);
                walk.display.push(node);
                walk.node_groups.push(Some(mine));
                walk.owner.insert(*symbol, node);
                node
            })
            .collect()
    } else {
        cluster
            .children
            .iter()
            .filter_map(|&child| descend(clustering, child, expanded, Some(mine), walk))
            .collect()
    };

    walk.groups.push(GroupBox {
        key: cluster.key,
        label: cluster.label.clone(),
        count: cluster.members.len(),
        level: cluster.level,
        members,
    });
    Some(me)
}

/// Roll every call up to the pair of display nodes it runs between.
///
/// `CallGraph` exposes no `edges()`, so the calls have to be walked the way
/// `logic::cluster` walks them: `get_callees` per function, which returns one
/// entry per distinct call *site* and so counts multiplicity for free.
///
/// Calls whose two ends land on the same display node are dropped. Those are
/// the ones internal to a collapsed community, and drawing them would be a
/// self-loop on a box whose whole purpose is to not show its insides.
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
        for callee in graph.get_callees(caller) {
            let Some(&to) = owner.get(&callee.id) else {
                continue;
            };
            if from == to {
                continue;
            }
            // `direction` says which way the user is reading the graph, so an
            // incoming map is the same aggregation with every arrow turned
            // round — exactly what `compute_layout` does with get_callers.
            let pair = match direction {
                CallDirection::Outgoing => (from, to),
                CallDirection::Incoming => (to, from),
            };
            *weights.entry(pair).or_insert(0) += 1;
        }
    }

    // `graph.function_ids()` is a HashMap iteration, so the aggregate has to be
    // sorted before it decides node order in the DAG — otherwise the same graph
    // lays out differently from one run to the next.
    let mut edges: Vec<LayoutEdge> = weights
        .into_iter()
        .map(|((source, target), weight)| LayoutEdge {
            source,
            target,
            weight,
        })
        .collect();
    edges.sort_by(|a, b| {
        a.source
            .to_string()
            .cmp(&b.source.to_string())
            .then_with(|| a.target.to_string().cmp(&b.target.to_string()))
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
    use logic::Clustering;

    fn demo() -> (CallGraph, Clustering) {
        let graph = create_demo_call_graph();
        let clustering = Clustering::compute(&graph);
        (graph, clustering)
    }

    /// Every cluster in the clustering, top level first, keyed the way the
    /// workspace's expansion set keys them.
    fn all_keys(clustering: &Clustering) -> HashSet<ClusterKey> {
        clustering.clusters().map(|c| c.key).collect()
    }

    fn clusters_of(layout: &GraphLayout) -> Vec<ClusterKey> {
        layout
            .nodes
            .keys()
            .filter_map(|node| match node {
                LayoutNodeId::Cluster(key) => Some(*key),
                LayoutNodeId::Symbol(_) => None,
            })
            .collect()
    }

    fn symbols_of(layout: &GraphLayout) -> Vec<SymbolId> {
        layout
            .nodes
            .keys()
            .filter_map(|node| match node {
                LayoutNodeId::Symbol(symbol) => Some(*symbol),
                LayoutNodeId::Cluster(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_closed_map_is_one_node_per_top_level_community() {
        let (graph, clustering) = demo();
        let expanded = HashSet::new();
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        let top: HashSet<ClusterKey> = clustering
            .top_level()
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().key)
            .collect();
        assert!(top.len() > 1, "the demo graph is meant to have communities");

        assert_eq!(
            clusters_of(&layout).into_iter().collect::<HashSet<_>>(),
            top,
            "a map nobody has opened shows the top level and nothing else"
        );
        assert!(
            symbols_of(&layout).is_empty(),
            "no functions until one opens"
        );
        assert_eq!(
            layout.nodes.len(),
            top.len(),
            "and every one of them is placed"
        );
        assert!(
            layout.groups.is_empty(),
            "a box needs an open cluster to exist"
        );
    }

    #[test]
    fn opening_a_cluster_swaps_it_for_its_children_and_adds_one_box() {
        let (graph, clustering) = demo();
        let closed = HashSet::new();
        let before = compute_cluster_layout(
            &graph,
            &clustering,
            &closed,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        // A top-level cluster with sub-clusters under it, so opening it is a
        // step down the dendrogram rather than straight to functions.
        let opened = clustering
            .top_level()
            .iter()
            .map(|id| clustering.cluster(*id).unwrap())
            .find(|c| c.level > 0 && c.children.len() > 1)
            .expect("the demo graph is meant to produce a hierarchy");
        let children: HashSet<ClusterKey> = opened
            .children
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().key)
            .collect();

        let expanded = HashSet::from([opened.key]);
        let after = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        let shown: HashSet<ClusterKey> = clusters_of(&after).into_iter().collect();
        assert!(
            !shown.contains(&opened.key),
            "the cluster itself is now a box, not a node"
        );
        assert!(children.is_subset(&shown), "its children took its place");
        assert_eq!(after.nodes.len(), before.nodes.len() - 1 + children.len());

        assert_eq!(
            after.groups.len(),
            1,
            "exactly one box for one open cluster"
        );
        let group = &after.groups[0];
        assert_eq!(group.key, opened.key);
        assert_eq!(group.label, opened.label);
        assert_eq!(
            group.count,
            opened.members.len(),
            "the box counts functions"
        );
        assert_eq!(group.level, opened.level);
        assert_eq!(
            group.members.iter().copied().collect::<HashSet<_>>(),
            children.iter().map(|k| LayoutNodeId::Cluster(*k)).collect(),
        );
    }

    #[test]
    fn opening_everything_reaches_the_functions_themselves() {
        let (graph, clustering) = demo();
        let expanded = all_keys(&clustering);
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        assert!(
            clusters_of(&layout).is_empty(),
            "with every box open there is no community left to stand in for anything"
        );

        let clustered: HashSet<SymbolId> = graph
            .function_ids()
            .copied()
            .filter(|id| clustering.cluster_of(id).is_some())
            .collect();
        assert_eq!(
            symbols_of(&layout).into_iter().collect::<HashSet<_>>(),
            clustered
        );
        assert_eq!(layout.nodes.len(), clustered.len(), "all of them placed");

        // Every cluster at every level is open, so every one is a box, and the
        // level-0 boxes are the ones holding functions.
        assert_eq!(layout.groups.len(), clustering.clusters().count());
        for group in &layout.groups {
            assert!(!group.members.is_empty());
            if group.level == 0 {
                assert!(group
                    .members
                    .iter()
                    .all(|m| matches!(m, LayoutNodeId::Symbol(_))));
            }
        }
    }

    /// The display node a symbol is drawn as, worked out through
    /// `Clustering::ancestry` rather than through the walk `compute_cluster_layout`
    /// does, so the two have to agree for a reason.
    fn drawn_as(clustering: &Clustering, symbol: &SymbolId) -> Option<LayoutNodeId> {
        let top = clustering.ancestry(symbol).last().copied()?;
        Some(LayoutNodeId::Cluster(clustering.cluster(top)?.key))
    }

    #[test]
    fn edge_weights_count_the_calls_that_actually_cross() {
        let (graph, clustering) = demo();
        let expanded = HashSet::new();
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        let mut expected: HashMap<(LayoutNodeId, LayoutNodeId), usize> = HashMap::new();
        for caller in graph.function_ids() {
            let Some(from) = drawn_as(&clustering, caller) else {
                continue;
            };
            for callee in graph.get_callees(caller) {
                let Some(to) = drawn_as(&clustering, &callee.id) else {
                    continue;
                };
                if from == to {
                    continue;
                }
                *expected.entry((from, to)).or_insert(0) += 1;
            }
        }
        assert!(!expected.is_empty(), "the communities are meant to talk");

        let actual: HashMap<(LayoutNodeId, LayoutNodeId), usize> = layout
            .edges
            .iter()
            .map(|e| ((e.source, e.target), e.weight))
            .collect();
        assert_eq!(
            actual.len(),
            layout.edges.len(),
            "one edge per ordered pair"
        );
        assert_eq!(actual, expected);

        for edge in &layout.edges {
            assert_ne!(
                edge.source, edge.target,
                "a call inside a closed community is that community's own business"
            );
            assert!(layout.nodes.contains_key(&edge.source));
            assert!(layout.nodes.contains_key(&edge.target));
        }
    }

    #[test]
    fn reading_the_map_incoming_turns_every_arrow_round() {
        let (graph, clustering) = demo();
        let expanded = HashSet::new();
        let of = |direction| {
            compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                direction,
                LayoutOrientation::LeftRight,
            )
        };

        let out = of(CallDirection::Outgoing);
        let incoming = of(CallDirection::Incoming);

        let reversed: HashMap<(LayoutNodeId, LayoutNodeId), usize> = out
            .edges
            .iter()
            .map(|e| ((e.target, e.source), e.weight))
            .collect();
        let actual: HashMap<(LayoutNodeId, LayoutNodeId), usize> = incoming
            .edges
            .iter()
            .map(|e| ((e.source, e.target), e.weight))
            .collect();
        assert_eq!(actual, reversed);
    }

    /// Two triangles joined by a single call: two communities, one relation
    /// between them, which is the smallest map that has a direction at all.
    fn two_communities() -> (CallGraph, Clustering) {
        let mut graph = CallGraph::new();
        let triangle = |graph: &mut CallGraph, prefix: &str, base: u32| {
            let mut ids = Vec::new();
            for (offset, suffix) in ["a", "b", "c"].iter().enumerate() {
                let name = format!("{prefix}_{suffix}");
                let f = FunctionNode::new(
                    name.clone(),
                    name,
                    Location::new("src/x.c".to_string(), base + offset as u32, 0),
                );
                let id = f.id;
                graph.add_function(f);
                ids.push(id);
            }
            for (offset, (from, to)) in [(0, 1), (1, 2), (0, 2)].into_iter().enumerate() {
                graph.add_call(
                    ids[from],
                    ids[to],
                    Location::new("src/x.c".to_string(), base + 100 + offset as u32, 0),
                );
            }
            ids
        };
        let left = triangle(&mut graph, "left", 10);
        let right = triangle(&mut graph, "right", 40);
        graph.add_call(
            left[2],
            right[0],
            Location::new("src/x.c".to_string(), 500, 0),
        );

        let clustering = Clustering::compute(&graph);
        (graph, clustering)
    }

    #[test]
    fn the_map_transposes_for_a_top_down_orientation() {
        let (graph, clustering) = two_communities();
        assert_eq!(clustering.top_level().len(), 2, "two communities, one edge");

        let expanded = HashSet::new();
        let of = |orientation| {
            compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                CallDirection::Outgoing,
                orientation,
            )
        };

        let side_by_side = of(LayoutOrientation::LeftRight);
        let stacked = of(LayoutOrientation::TopDown);
        assert_eq!(side_by_side.nodes.len(), 2);
        assert_eq!(stacked.nodes.len(), 2);

        let pair = |layout: &GraphLayout| {
            let mut positions: Vec<NodePos> = layout.nodes.values().copied().collect();
            positions.sort_by(|a, b| (a.x, a.y).partial_cmp(&(b.x, b.y)).unwrap());
            (positions[0], positions[1])
        };

        // Left-to-right, the caller's community sits a level to the left of the
        // callee's and the two share a row.
        let (a, b) = pair(&side_by_side);
        assert!(
            (b.x - a.x).abs() >= LEVEL_SEPARATION,
            "level pitch {} must clear a node plus its gutter",
            b.x - a.x
        );
        assert!((a.y - b.y).abs() < 1.0, "one level apart, same row");

        // Transposed, the same relation runs down the canvas instead — the
        // levels advance along y and the two share a column.
        let (a, b) = pair(&stacked);
        assert!(
            (b.y - a.y).abs() >= LEVEL_SEPARATION_TOPDOWN,
            "level pitch {} must clear a node plus its top-down gutter",
            b.y - a.y
        );
        assert!((a.x - b.x).abs() < 1.0, "one level apart, same column");
    }

    /// A stale expansion set can name a cluster whose parent is shut — the
    /// user opened both, closed the parent, and the child's key stayed behind.
    #[test]
    fn a_cluster_opened_under_a_closed_one_waits_its_turn() {
        let (graph, clustering) = demo();
        let parent = clustering
            .top_level()
            .iter()
            .map(|id| clustering.cluster(*id).unwrap())
            .find(|c| c.level > 0 && !c.children.is_empty())
            .expect("a hierarchy");
        let child = clustering.cluster(parent.children[0]).unwrap();

        let closed = HashSet::new();
        let baseline = compute_cluster_layout(
            &graph,
            &clustering,
            &closed,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        // Only the child is open, and nothing reaches it.
        let stale = HashSet::from([child.key]);
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &stale,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        assert!(
            layout.groups.is_empty(),
            "an expansion under a closed cluster draws no box"
        );
        assert_eq!(
            clusters_of(&layout).into_iter().collect::<HashSet<_>>(),
            clusters_of(&baseline).into_iter().collect::<HashSet<_>>(),
            "and leaves the map exactly as it was"
        );

        // Open the parent and the entry that was waiting takes effect.
        let both = HashSet::from([parent.key, child.key]);
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &both,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        assert_eq!(layout.groups.len(), 2, "the parent's box and the child's");
        assert!(layout.groups.iter().any(|g| g.key == child.key));
    }

    #[test]
    fn an_empty_clustering_lays_out_nothing() {
        let graph = CallGraph::new();
        let clustering = Clustering::compute(&graph);
        assert!(clustering.is_empty());

        let expanded = HashSet::new();
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );

        assert!(layout.nodes.is_empty());
        assert!(layout.groups.is_empty());
        assert!(layout.edges.is_empty());
    }

    // -----------------------------------------------------------------------
    // Meta-nodes, and keeping a box's contents together
    // -----------------------------------------------------------------------

    /// The leaf functions under a cluster, counted by walking the dendrogram
    /// down to level 0 rather than by trusting `Cluster::members` — the field
    /// `MetaNode::count` is copied from is the one under test.
    fn leaves_under(clustering: &Clustering, id: ClusterId) -> HashSet<SymbolId> {
        let cluster = clustering.cluster(id).expect("a live cluster id");
        if cluster.level == 0 {
            return cluster.members.iter().copied().collect();
        }
        cluster
            .children
            .iter()
            .flat_map(|&child| leaves_under(clustering, child))
            .collect()
    }

    /// The first top-level cluster with enough children to be worth opening.
    fn openable(clustering: &Clustering) -> &logic::Cluster {
        clustering
            .top_level()
            .iter()
            .map(|id| clustering.cluster(*id).unwrap())
            .find(|c| c.level > 0 && c.children.len() > 1)
            .expect("the demo graph is meant to produce a hierarchy")
    }

    /// Every display node inside one box, descending through nested boxes. A
    /// `Cluster` member is a box exactly when it is not itself a placed node.
    fn drawn_members(layout: &GraphLayout, key: ClusterKey) -> HashSet<LayoutNodeId> {
        let group = layout
            .groups
            .iter()
            .find(|g| g.key == key)
            .expect("an open cluster has a box");
        let mut inside = HashSet::new();
        for member in &group.members {
            if layout.nodes.contains_key(member) {
                inside.insert(*member);
            } else if let LayoutNodeId::Cluster(nested) = member {
                inside.extend(drawn_members(layout, *nested));
            }
        }
        inside
    }

    /// The placed nodes bucketed into layers and ordered along the axis siblings
    /// spread on — which is y left-to-right and x top-down, the same transpose
    /// `to_centre` undoes. Contiguity is a property *within* a layer: two nodes
    /// a level apart say nothing about each other's neighbours.
    ///
    /// Every node on a layer is given the same level-axis coordinate and
    /// `balance_aspect` scales that axis uniformly, so bucketing on the exact
    /// bits is sound and no tolerance has to be invented.
    fn layers(
        positions: &HashMap<LayoutNodeId, NodePos>,
        orientation: LayoutOrientation,
    ) -> Vec<Vec<LayoutNodeId>> {
        let axes = |p: &NodePos| match orientation {
            LayoutOrientation::LeftRight => (p.x, p.y),
            LayoutOrientation::TopDown => (p.y, p.x),
        };
        let mut by_level: HashMap<u32, Vec<(f32, LayoutNodeId)>> = HashMap::new();
        for (node, pos) in positions {
            let (level, cross) = axes(pos);
            by_level
                .entry(level.to_bits())
                .or_default()
                .push((cross, *node));
        }
        let mut levels: Vec<(u32, Vec<(f32, LayoutNodeId)>)> = by_level.into_iter().collect();
        levels.sort_by_key(|(level, _)| *level);
        levels
            .into_iter()
            .map(|(_, mut row)| {
                row.sort_by(|a, b| {
                    a.0.partial_cmp(&b.0)
                        .unwrap()
                        .then_with(|| a.1.to_string().cmp(&b.1.to_string()))
                });
                row.into_iter().map(|(_, node)| node).collect()
            })
            .collect()
    }

    /// Where `members` sit in one layer's order.
    fn slots(layer: &[LayoutNodeId], members: &HashSet<LayoutNodeId>) -> Vec<usize> {
        layer
            .iter()
            .enumerate()
            .filter(|(_, node)| members.contains(node))
            .map(|(i, _)| i)
            .collect()
    }

    /// True when nothing outside `members` sits between two of them.
    fn contiguous(layer: &[LayoutNodeId], members: &HashSet<LayoutNodeId>) -> bool {
        slots(layer, members)
            .windows(2)
            .all(|pair| pair[1] == pair[0] + 1)
    }

    /// A layer worth testing: two of the group's members, and at least one
    /// outsider that could have come to rest between them.
    fn could_be_split(layer: &[LayoutNodeId], members: &HashSet<LayoutNodeId>) -> bool {
        let inside = slots(layer, members).len();
        inside > 1 && inside < layer.len()
    }

    /// The order `compute_cluster_layout` reaches display nodes in, rebuilt from
    /// the clustering alone.
    ///
    /// `grid` knows its nodes by index and the index is the position in that
    /// list, so a reference layout that added its nodes in any other order would
    /// be a different layout and would prove nothing.
    fn display_order(clustering: &Clustering, expanded: &HashSet<ClusterKey>) -> Vec<LayoutNodeId> {
        fn walk(
            clustering: &Clustering,
            id: ClusterId,
            expanded: &HashSet<ClusterKey>,
            out: &mut Vec<LayoutNodeId>,
        ) {
            let cluster = clustering.cluster(id).expect("a live cluster id");
            if !expanded.contains(&cluster.key) {
                out.push(LayoutNodeId::Cluster(cluster.key));
            } else if cluster.level == 0 {
                out.extend(cluster.members.iter().map(|s| LayoutNodeId::Symbol(*s)));
            } else {
                for &child in &cluster.children {
                    walk(clustering, child, expanded, out);
                }
            }
        }
        let mut out = Vec::new();
        for &id in clustering.top_level() {
            walk(clustering, id, expanded, &mut out);
        }
        out
    }

    /// The same map laid out the way it was before grouping: one flat DAG
    /// through `compute_dag`, with no idea which nodes belong together.
    ///
    /// This is the baseline for both of the things worth knowing about the
    /// switch — that it changed nothing when nothing is open, and that it
    /// changed the right thing when something is.
    fn flat_layout(
        clustering: &Clustering,
        expanded: &HashSet<ClusterKey>,
        edges: &[LayoutEdge],
        orientation: LayoutOrientation,
    ) -> HashMap<LayoutNodeId, NodePos> {
        let display = display_order(clustering, expanded);
        let index: HashMap<LayoutNodeId, usize> = display
            .iter()
            .enumerate()
            .map(|(i, node)| (*node, i))
            .collect();
        let mut dag: Dag<LayoutNodeId> = Dag::new();
        for node in &display {
            dag.add_node(*node);
        }
        for edge in edges {
            let _ = dag.add_edge(index[&edge.source], index[&edge.target]);
        }
        let mut engine =
            LayoutEngine::with_config(layout_config(orientation)).expect("a valid config");
        let laid_out = engine.compute_dag(&dag).expect("a layout");
        let mut positions: HashMap<LayoutNodeId, NodePos> = display
            .iter()
            .enumerate()
            .filter_map(|(i, node)| {
                laid_out
                    .position(i)
                    .map(|pos| (*node, to_centre(&pos, orientation)))
            })
            .collect();
        balance_aspect(&mut positions, orientation);
        positions
    }

    /// A graph whose communities the ungrouped layout pulls apart.
    ///
    /// The demo graph is no use for this: open one of its communities and the
    /// members land on separate levels, where contiguity means nothing and any
    /// assertion about it passes for free. What is needed is a community whose
    /// members share a level with a stranger, and whose callers sit far enough
    /// apart that the barycentre heuristic wants to put the stranger between
    /// them. So: a star community `a*`, two of whose points are called from
    /// elsewhere, plus a handful of triangles that cluster on their own and give
    /// the layer something to interleave with.
    ///
    /// The exact communities are Louvain's to decide and are not asserted here;
    /// what the test needs is only that *some* box comes out split without
    /// grouping, and it checks that before it checks anything else.
    fn interleaving_graph() -> (CallGraph, Clustering, SymbolId) {
        let mut graph = CallGraph::new();
        let mut ids: HashMap<&str, SymbolId> = HashMap::new();
        let mut line = 1u32;
        for name in [
            "a0", "a1", "a2", "a3", "p1", "p2", "p3", "q1", "q2", "q3", "r1", "r2", "r3", "b1",
            "b2", "b3",
        ] {
            let f = FunctionNode::new(
                name.to_string(),
                name.to_string(),
                Location::new("src/x.c".to_string(), line, 0),
            );
            line += 1;
            ids.insert(name, f.id);
            graph.add_function(f);
        }
        for (from, to) in [
            // The star, with `a3` called three times so that the two callers it
            // picks up below cannot tempt Louvain into moving it out.
            ("a0", "a1"),
            ("a0", "a2"),
            ("a0", "a3"),
            ("a0", "a3"),
            ("a0", "a3"),
            // Four triangles, each tight enough to be a community.
            ("p1", "p2"),
            ("p2", "p3"),
            ("p1", "p3"),
            ("q1", "q2"),
            ("q2", "q3"),
            ("q1", "q3"),
            ("r1", "r2"),
            ("r2", "r3"),
            ("r1", "r3"),
            ("b1", "b2"),
            ("b2", "b3"),
            ("b1", "b3"),
            // The calls that spread the star's points across the level.
            ("p1", "a1"),
            ("q1", "a3"),
            ("r1", "a3"),
            ("p2", "b1"),
        ] {
            graph.add_call(
                ids[from],
                ids[to],
                Location::new("src/x.c".to_string(), line, 0),
            );
            line += 1;
        }
        let clustering = Clustering::compute(&graph);
        (graph, clustering, ids["a0"])
    }

    /// Opening a community must put its members next to each other, because the
    /// client draws a box round them and a box round a scattered set swallows
    /// whatever it happens to span.
    ///
    /// The first assertion is the one that keeps this honest: the same map laid
    /// out flat, the way it was before `compute_dag_grouped`, has to come out
    /// split. Without that this test would pass on a layout that never had a
    /// problem.
    #[test]
    fn an_open_clusters_members_land_side_by_side() {
        let (graph, clustering, anchor) = interleaving_graph();
        let community = clustering
            .cluster_of(&anchor)
            .expect("a clustered function");
        let opened = clustering.cluster(community).expect("its community");
        let expanded = HashSet::from([opened.key]);

        for orientation in [LayoutOrientation::LeftRight, LayoutOrientation::TopDown] {
            let layout = compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                CallDirection::Outgoing,
                orientation,
            );
            let members = drawn_members(&layout, opened.key);
            assert!(members.len() > 1, "a box with one thing in it cannot split");

            let flat = flat_layout(&clustering, &expanded, &layout.edges, orientation);
            assert!(
                layers(&flat, orientation)
                    .iter()
                    .any(|layer| !contiguous(layer, &members)),
                "{orientation:?}: the ungrouped layout has to scatter this box, \
                 or holding it together is untested"
            );

            let mut tested = 0;
            for layer in layers(&layout.nodes, orientation) {
                assert!(
                    contiguous(&layer, &members),
                    "{orientation:?}: an outsider sits inside the box, layer {layer:?}"
                );
                if could_be_split(&layer, &members) {
                    tested += 1;
                }
            }
            assert!(
                tested > 0,
                "{orientation:?}: no layer holds two members and an outsider that \
                 could have come between them"
            );
        }
    }

    /// The same guarantee one level in: a box inside a box is still a box.
    ///
    /// The tag a node carries is its *innermost* open cluster, so it is the
    /// inner runs that `grid` is asked to keep whole. An outer box is built from
    /// those runs and from its own meta-nodes, and holding it together as well
    /// is a further constraint this does not yet make — which is why the claim
    /// here is about the inner box, and about it staying inside its parent's
    /// run rather than wandering off among strangers.
    #[test]
    fn a_nested_box_stays_whole_inside_the_one_around_it() {
        let (graph, clustering) = demo();
        // Everything open: every level-0 box is then nested in the level-1 box
        // that owns it, which is the case worth checking and the one a user
        // reaches by opening twice.
        let expanded = all_keys(&clustering);
        let nested: Vec<(ClusterKey, ClusterKey)> = clustering
            .clusters()
            .filter(|c| c.level == 0)
            .map(|c| {
                let parent = clustering
                    .cluster(c.parent.expect("a level-0 cluster has a parent here"))
                    .unwrap();
                (c.key, parent.key)
            })
            .collect();
        assert!(nested.len() > 1, "the demo graph is meant to nest");

        for orientation in [LayoutOrientation::LeftRight, LayoutOrientation::TopDown] {
            let layout = compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                CallDirection::Outgoing,
                orientation,
            );
            let flat = flat_layout(&clustering, &expanded, &layout.edges, orientation);
            let (mut tested, mut would_have_split) = (0, 0);

            for (inner, outer) in &nested {
                let inside = drawn_members(&layout, *inner);
                let around = drawn_members(&layout, *outer);
                assert!(inside.is_subset(&around), "the inner box is in the outer");
                // A community that cannot join anything without putting a cycle
                // back into the map ends up as its parent's only child. That is
                // a box drawn round one box — nothing nests inside it, so there
                // is nothing here to check.
                if around.len() == inside.len() {
                    continue;
                }

                if layers(&flat, orientation)
                    .iter()
                    .any(|layer| !contiguous(layer, &inside))
                {
                    would_have_split += 1;
                }
                for layer in layers(&layout.nodes, orientation) {
                    assert!(
                        contiguous(&layer, &inside),
                        "{orientation:?}: the inner box is split, layer {layer:?}"
                    );
                    // Two of the inner box's members, and a sibling of the box
                    // itself on the same level to come between them: this is the
                    // level where the nesting is actually load-bearing.
                    if could_be_split(&layer, &inside) && !slots(&layer, &around).is_empty() {
                        tested += 1;
                    }
                }
            }
            assert!(
                tested > 0,
                "{orientation:?}: no level puts two of an inner box's members \
                 beside anything that could split them"
            );
            assert!(
                would_have_split > 0,
                "{orientation:?}: the ungrouped layout has to scatter one of \
                 these boxes, or nesting them is untested"
            );
        }
    }

    #[test]
    fn every_closed_cluster_is_named_and_no_open_one_is() {
        let (graph, clustering) = demo();
        let opened = openable(&clustering);
        let nested = clustering.cluster(opened.children[0]).unwrap();

        for expanded in [
            HashSet::new(),
            HashSet::from([opened.key]),
            HashSet::from([opened.key, nested.key]),
            all_keys(&clustering),
        ] {
            let layout = compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                CallDirection::Outgoing,
                LayoutOrientation::LeftRight,
            );

            let named: HashSet<ClusterKey> = layout.metas.iter().map(|m| m.key).collect();
            assert_eq!(
                named.len(),
                layout.metas.len(),
                "a meta-node gets described once, not twice"
            );
            assert_eq!(
                named,
                clusters_of(&layout).into_iter().collect::<HashSet<_>>(),
                "`metas` is exactly the cluster ids `nodes` places"
            );
            for group in &layout.groups {
                assert!(
                    !named.contains(&group.key),
                    "an expanded cluster is a box, and a box is not a meta-node"
                );
            }
        }
    }

    #[test]
    fn a_meta_node_says_what_it_hides_and_what_to_call_it() {
        let (graph, clustering) = demo();
        let opened = openable(&clustering);
        let expanded = HashSet::from([opened.key]);
        let layout = compute_cluster_layout(
            &graph,
            &clustering,
            &expanded,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        assert!(layout.metas.len() > 1, "several communities stay shut");

        for meta in &layout.metas {
            let cluster = clustering
                .by_key(meta.key)
                .expect("a meta-node names a live cluster");
            assert_eq!(meta.label, cluster.label, "the clustering names its own");
            assert_eq!(meta.level, cluster.level);
            assert_eq!(
                meta.count,
                leaves_under(&clustering, cluster.id).len(),
                "`+N` counts the functions underneath, at whatever depth"
            );
            assert!(meta.count > 0, "an empty community is not a community");
        }
    }

    /// The regression guard: with nothing open there are no groups, so the
    /// grouped engine has to return what the flat one returned — not close, the
    /// same bits. A map nobody has opened is the first thing anyone sees, and it
    /// must not have moved.
    #[test]
    fn a_map_with_nothing_open_is_laid_out_exactly_as_before() {
        let (graph, clustering) = demo();
        let expanded = HashSet::new();

        for orientation in [LayoutOrientation::LeftRight, LayoutOrientation::TopDown] {
            let layout = compute_cluster_layout(
                &graph,
                &clustering,
                &expanded,
                CallDirection::Outgoing,
                orientation,
            );
            let flat = flat_layout(&clustering, &expanded, &layout.edges, orientation);

            assert_eq!(layout.nodes.len(), flat.len());
            assert!(!flat.is_empty());
            for (node, before) in &flat {
                let after = layout.nodes.get(node).expect("the same nodes");
                assert_eq!(after.x.to_bits(), before.x.to_bits(), "{node} moved in x");
                assert_eq!(after.y.to_bits(), before.y.to_bits(), "{node} moved in y");
            }
        }
    }

    /// A `LayoutNodeId` is a JSON *map key*, and JSON has no keys but strings —
    /// so its string form is the wire contract, not an implementation detail.
    #[test]
    fn a_layout_node_id_is_a_string_on_the_wire() {
        let symbol = SymbolId::from_content("a", "a.c", 1);
        for node in [
            LayoutNodeId::Symbol(symbol),
            LayoutNodeId::Cluster(ClusterKey {
                level: 2,
                anchor: symbol,
            }),
        ] {
            let json = serde_json::to_value(node).expect("serialises");
            assert!(json.is_string(), "{json} has to be usable as a map key");
            assert_eq!(
                serde_json::from_value::<LayoutNodeId>(json).expect("round-trips"),
                node
            );
        }

        let mut nodes = HashMap::new();
        nodes.insert(LayoutNodeId::Symbol(symbol), NodePos { x: 1.0, y: 2.0 });
        let layout = GraphLayout {
            nodes,
            groups: Vec::new(),
            metas: Vec::new(),
            edges: Vec::new(),
        };
        let json = serde_json::to_value(&layout).expect("a whole layout serialises");
        assert!(json["nodes"][format!("fn:{}", symbol.0)].is_object());
    }
}
