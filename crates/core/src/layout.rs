use std::collections::{HashMap, HashSet, VecDeque};

use grid::{Dag, LayoutConfig, LayoutEngine};
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
    /// Which way levels advance.
    pub orientation: LayoutOrientation,
}

impl Default for LayoutOptions<'_> {
    fn default() -> Self {
        static EMPTY: std::sync::OnceLock<HashSet<SymbolId>> = std::sync::OnceLock::new();
        Self {
            depth: DEFAULT_DEPTH,
            collapsed: EMPTY.get_or_init(HashSet::new),
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
fn balance_aspect(positions: &mut HashMap<SymbolId, NodePos>, orientation: LayoutOrientation) {
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
        if depth >= max_depth {
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
            graph.add_call(
                pair[0],
                pair[1],
                Location::new("src/x.c".to_string(), 1, 0),
            );
        }
        (graph, ids)
    }

    #[test]
    fn depth_limits_how_far_the_layout_walks() {
        let (graph, ids) = graph_with_chain(&["a", "b", "c", "d", "e"]);

        for depth in [1usize, 2, 3] {
            let collapsed = HashSet::new();
            let opts = LayoutOptions { depth, collapsed: &collapsed, ..Default::default() };
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

        let opts = LayoutOptions { depth: MAX_DEPTH, collapsed: &collapsed, ..Default::default() };
        let layout = compute_layout(&graph, Some(ids[0]), CallDirection::Outgoing, &opts);

        assert!(layout.contains_key(&ids[0]), "root stays");
        assert!(layout.contains_key(&ids[1]), "the collapsed node itself stays");
        assert!(!layout.contains_key(&ids[2]), "its children are dropped");
        assert!(!layout.contains_key(&ids[3]));
    }

    #[test]
    fn positions_are_node_centres_in_pixels() {
        let (graph, ids) = graph_with_chain(&["a", "b"]);
        let collapsed = HashSet::new();
        let opts = LayoutOptions { depth: 2, collapsed: &collapsed, ..Default::default() };
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
        let opts = LayoutOptions { depth: 2, collapsed: &collapsed, ..Default::default() };
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
        let opts = LayoutOptions { depth: MAX_DEPTH, collapsed: &collapsed, ..Default::default() };
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
}

