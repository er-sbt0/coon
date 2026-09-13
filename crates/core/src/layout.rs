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
/// Extra breathing room between distinct subtrees.
pub const SUBTREE_SEPARATION: f32 = NODE_SEPARATION + ROW_GUTTER;

/// How many levels of callees/callers to show by default.
pub const DEFAULT_DEPTH: usize = 3;
pub const MIN_DEPTH: usize = 1;
pub const MAX_DEPTH: usize = 8;

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
}

impl Default for LayoutOptions<'_> {
    fn default() -> Self {
        static EMPTY: std::sync::OnceLock<HashSet<SymbolId>> = std::sync::OnceLock::new();
        Self {
            depth: DEFAULT_DEPTH,
            collapsed: EMPTY.get_or_init(HashSet::new),
        }
    }
}

fn layout_config() -> LayoutConfig {
    LayoutConfig::new()
        .with_node_size(NODE_WIDTH, NODE_HEIGHT)
        .with_node_separation(NODE_SEPARATION)
        .with_level_separation(LEVEL_SEPARATION)
        .with_subtree_separation(SUBTREE_SEPARATION)
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

    let mut engine = match LayoutEngine::with_config(layout_config()) {
        Ok(e) => e,
        Err(_) => LayoutEngine::new(),
    };
    let layout = match engine.compute_dag(&dag) {
        Ok(l) => l,
        Err(_) => return HashMap::new(),
    };

    node_to_symbol
        .iter()
        .filter_map(|(&node_idx, &symbol_id)| {
            layout.position(node_idx).map(|pos| {
                (
                    symbol_id,
                    NodePos {
                        x: pos.x + NODE_WIDTH / 2.0,
                        y: pos.y + NODE_HEIGHT / 2.0,
                    },
                )
            })
        })
        .collect()
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
            let opts = LayoutOptions { depth, collapsed: &collapsed };
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

        let opts = LayoutOptions { depth: MAX_DEPTH, collapsed: &collapsed };
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
        let opts = LayoutOptions { depth: 2, collapsed: &collapsed };
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
        let opts = LayoutOptions { depth: 2, collapsed: &collapsed };
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
    fn unknown_or_missing_root_yields_nothing() {
        let (graph, _) = graph_with_chain(&["a"]);
        let opts = LayoutOptions::default();
        assert!(compute_layout(&graph, None, CallDirection::Outgoing, &opts).is_empty());
    }
}
