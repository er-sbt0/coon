use std::collections::{HashMap, VecDeque};

use grid::{Dag, LayoutEngine};
use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePos {
    pub x: f32,
    pub y: f32,
}

/// Compute layout positions for the subgraph rooted at `root`.
/// Returns an empty map if `root` is None or not found in the graph.
pub fn compute_layout(
    graph: &CallGraph,
    root: Option<SymbolId>,
    direction: CallDirection,
) -> HashMap<SymbolId, NodePos> {
    let root = match root {
        Some(r) => r,
        None => return HashMap::new(),
    };

    if graph.get_function(&root).is_none() {
        return HashMap::new();
    }

    let mut symbol_to_node: HashMap<SymbolId, usize> = HashMap::new();
    let mut node_to_symbol: HashMap<usize, SymbolId> = HashMap::new();
    let mut dag: Dag<SymbolId> = Dag::new();
    let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::new();

    let root_idx = dag.add_node(root);
    symbol_to_node.insert(root, root_idx);
    node_to_symbol.insert(root_idx, root);
    queue.push_back((root, 0));

    const MAX_DEPTH: usize = 5;

    while let Some((symbol, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
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

    let mut engine = LayoutEngine::new();
    let layout = match engine.compute_dag(&dag) {
        Ok(l) => l,
        Err(_) => return HashMap::new(),
    };

    node_to_symbol
        .iter()
        .filter_map(|(&node_idx, &symbol_id)| {
            layout
                .position(node_idx)
                .map(|pos| (symbol_id, NodePos { x: pos.x, y: pos.y }))
        })
        .collect()
}
