use std::collections::{HashMap, HashSet, VecDeque};
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

/// One element of the cluster map: a function, or a whole cluster standing in
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
    /// How many individual calls this one line stands for, so the client can
    /// draw a heavily used relation more heavily than a stray one. For a mutual
    /// pair it is both directions added together, because the line is both.
    pub weight: usize,
    /// Calls run both ways between these two clusters.
    ///
    /// Pair merging puts no constraint on direction — a round can perfectly
    /// well put a cluster on both ends of a one-way path — so the quotient of a
    /// level is freely cyclic even over a call graph that is a clean DAG.
    /// Drawing that as two opposed arrows reads as "these call each other" over
    /// code where nothing does; drawing it as one double-headed line says the
    /// true thing, which is that the boundary is crossed in both directions.
    /// `source` and `target` still name the heavier direction, because the
    /// layout has to place the two somehow and the heavier flow is the better
    /// guess at which comes first.
    pub mutual: bool,
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

/// Lay out one rung of the coarsening ladder: every cluster at `level` as a
/// single node, with the calls between them aggregated onto the lines.
///
/// This is a second path alongside [`compute_layout`], not a replacement. A
/// rooted call tree starts from one function and walks outwards; the map draws
/// a partition of the whole graph. The two share their geometry
/// (`layout_config`, `to_centre`, `balance_aspect`) and nothing else.
///
/// A `level` the ladder does not have draws nothing, which is the right answer
/// while a clustering is still being computed and the frontend is holding a
/// level from the last one. Functions the clustering leaves out — the isolated
/// ones, see [`logic::cluster`] — are in no cluster and therefore on no map.
pub fn compute_cluster_layout(
    graph: &CallGraph,
    clustering: &Clustering,
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
        // A rung's quotient is freely cyclic — see `LayoutEdge::mutual` — and
        // folding the mutual pairs into one line does not make it acyclic, only
        // smaller. `grid` breaks what cycles remain for itself in its cycle
        // pass.
        let _ = dag.add_edge(index[&edge.source], index[&edge.target]);
    }

    let mut engine = match LayoutEngine::with_config(layout_config(orientation)) {
        Ok(e) => e,
        Err(_) => LayoutEngine::new(),
    };
    let layout = match engine.compute_dag(&dag) {
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
        metas,
        edges,
    }
}

/// Roll every call up to the pair of clusters it runs between, folding a pair
/// that calls both ways into a single line.
///
/// `CallGraph` exposes no `edges()`, so the calls have to be walked the way
/// `logic::cluster` walks them: `get_callees` per function, which returns one
/// entry per distinct call *site* and so counts multiplicity for free.
///
/// Calls whose two ends land in the same cluster are dropped. Those are the
/// ones internal to a box whose whole purpose is to not show its insides.
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

    let mut edges: Vec<LayoutEdge> = Vec::with_capacity(weights.len());
    for (&(source, target), &weight) in &weights {
        let back = weights.get(&(target, source)).copied();
        match back {
            // Emitted once, from whichever end carries more calls. Equal
            // weights are broken on the id so the choice is stable rather than
            // whichever way the HashMap happened to be walked.
            Some(back_weight) => {
                let heavier = weight > back_weight
                    || (weight == back_weight && source.to_string() < target.to_string());
                if !heavier {
                    continue;
                }
                edges.push(LayoutEdge {
                    source,
                    target,
                    weight: weight + back_weight,
                    mutual: true,
                });
            }
            None => edges.push(LayoutEdge {
                source,
                target,
                weight,
                mutual: false,
            }),
        }
    }

    // `graph.function_ids()` is a HashMap iteration, so the aggregate has to be
    // sorted before it decides node order in the DAG — otherwise the same graph
    // lays out differently from one run to the next.
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
            level,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        )
    }

    fn keys_of(layout: &GraphLayout) -> HashSet<ClusterKey> {
        layout
            .nodes
            .keys()
            .filter_map(|node| match node {
                LayoutNodeId::Cluster(key) => Some(*key),
                LayoutNodeId::Symbol(_) => None,
            })
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

    /// Two clusters that call each other, in a graph big enough to coarsen.
    ///
    /// `a` and `c` merge, `b` and `d` merge, and the chain `a -> b -> c` then
    /// crosses between the two in both directions — the shape pair merging
    /// produces freely, and the reason a rung's quotient is not acyclic. The
    /// ten spare functions are there only to put the graph over
    /// `TARGET_TOP_CLUSTERS`, so that a round of merging happens at all.
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
                (edge.source, edge.target)
            else {
                panic!("a cluster map draws clusters");
            };
            let forward = expected.get(&(source, target)).copied().unwrap_or(0);
            let back = expected.get(&(target, source)).copied().unwrap_or(0);
            assert_eq!(
                edge.weight,
                forward + back,
                "a line stands for every call it draws, both ways when it is mutual"
            );
            assert_eq!(edge.mutual, forward > 0 && back > 0);
            drawn += 1;
        }
        // Mutual pairs are one line, so the count is pairs, not directions.
        let unordered: HashSet<(String, String)> = expected
            .keys()
            .map(|(a, b)| {
                let (a, b) = (format!("{:?}", a), format!("{:?}", b));
                if a < b {
                    (a, b)
                } else {
                    (b, a)
                }
            })
            .collect();
        assert_eq!(drawn, unordered.len());
    }

    /// Pair merging puts no constraint on direction, so a rung's quotient is
    /// freely cyclic. Two opposed arrows read as "these call each other"; one
    /// double-headed line says the true thing.
    #[test]
    fn a_pair_that_calls_both_ways_is_drawn_once() {
        let (graph, clustering) = crossing_graph();
        let layout = map_at(&graph, &clustering, 0);

        assert_eq!(layout.edges.len(), 1, "one line, not two opposed arrows");
        let edge = &layout.edges[0];
        assert!(edge.mutual, "calls cross this boundary in both directions");
        assert_eq!(edge.weight, 2, "one call out and one back");
    }

    #[test]
    fn a_one_way_crossing_is_not_mutual() {
        let (graph, clustering) = demo();
        let layout = map_at(&graph, &clustering, 0);
        assert!(
            layout.edges.iter().any(|e| !e.mutual),
            "the demo graph is mostly a DAG; some crossing has to run one way"
        );
    }

    #[test]
    fn reading_the_map_incoming_turns_every_arrow_round() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;

        let out = compute_cluster_layout(
            &graph,
            &clustering,
            top,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        let incoming = compute_cluster_layout(
            &graph,
            &clustering,
            top,
            CallDirection::Incoming,
            LayoutOrientation::LeftRight,
        );

        let pairs = |layout: &GraphLayout| -> HashSet<(String, String, bool)> {
            layout
                .edges
                .iter()
                .map(|e| (e.source.to_string(), e.target.to_string(), e.mutual))
                .collect()
        };
        // Compared as unordered pairs. A one-way line plainly flips; a mutual
        // one is already both ways, and which of its two ends the layout put
        // first is decided by the heavier direction — which reversing the map
        // swaps. The claim is that the same boundaries are drawn, carrying the
        // same mutuality, not that the tuples match.
        let unordered = |layout: &GraphLayout| -> HashSet<(String, String, bool)> {
            pairs(layout)
                .into_iter()
                .map(|(source, target, mutual)| {
                    if source < target {
                        (source, target, mutual)
                    } else {
                        (target, source, mutual)
                    }
                })
                .collect()
        };
        assert_eq!(unordered(&incoming), unordered(&out));
    }

    #[test]
    fn the_map_transposes_for_a_top_down_orientation() {
        let (graph, clustering) = demo();
        let top = clustering.level_count() - 1;

        let left_right = compute_cluster_layout(
            &graph,
            &clustering,
            top,
            CallDirection::Outgoing,
            LayoutOrientation::LeftRight,
        );
        let top_down = compute_cluster_layout(
            &graph,
            &clustering,
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
            metas: Vec::new(),
            edges: Vec::new(),
        };
        let json = serde_json::to_value(&layout).expect("a whole layout serialises");
        assert!(json["nodes"][format!("fn:{}", symbol.0)].is_object());
    }
}
