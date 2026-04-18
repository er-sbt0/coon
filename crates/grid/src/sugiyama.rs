use std::collections::BTreeMap;

use petgraph::algo::toposort;
use petgraph::graph::DiGraph;

use crate::config::LayoutConfig;
use crate::dag::Dag;
use crate::edge_router::compute_orthogonal_routing;
use crate::error::LayoutError;
use crate::result::LayoutResult;
use crate::types::{EdgePath, LayoutBounds, Position};

/// Expanded graph used internally during the four Sugiyama phases.
///
/// Real nodes occupy indices `0..dag_len`; dummy nodes occupy `dag_len..`.
/// This convention makes `all_positions[..dag_len]` the slice of real-node
/// coordinates that goes into `LayoutResult`.
struct SugiyamaGraph {
    /// `layers[layer_idx]` = ordered list of node indices in that layer
    layers: Vec<Vec<usize>>,
    /// `positions[node_idx]` = position of node within its layer (for barycenter)
    positions: Vec<Option<usize>>,
    /// `is_dummy[node_idx]` — true for inserted dummy nodes (used in Phase 4 rendering)
    #[allow(dead_code)]
    is_dummy: Vec<bool>,
    /// All edges (real + dummy-chain segments)
    edges: Vec<(usize, usize)>,
    /// Parallel to `edges`: original real (u, v) this segment belongs to
    original_edge: Vec<(usize, usize)>,
}

/// Entry point called from `LayoutEngine::compute_dag`.
pub(crate) fn compute_sugiyama<T>(
    dag: &Dag<T>,
    config: &LayoutConfig,
) -> Result<LayoutResult, LayoutError> {
    if dag.is_empty() {
        return Err(LayoutError::EmptyTree);
    }

    let all_dag_edges: Vec<(usize, usize)> = dag.edges().collect();
    let (forward_edges, back_edges) = remove_cycles(dag.len(), &all_dag_edges);
    let layers = assign_layers(dag.len(), &forward_edges);
    let mut sg = insert_dummy_nodes(dag.len(), &layers, &forward_edges);

    let passes = config.crossing_minimization_passes.unwrap_or(2);
    minimize_crossings(&mut sg, passes);

    let all_positions = assign_coordinates(&sg, config);

    // Real-node positions only (index space 0..dag_len)
    let node_positions = all_positions[..dag.len()].to_vec();
    let bounds = bounds_from_positions(&node_positions, config.node_width, config.node_height);

    let mut result = LayoutResult::new(node_positions, bounds, dag.version());

    let nw = config.node_width;
    let nh = config.node_height;

    for path in route_edges(&sg, &all_positions, dag.len(), nw, nh) {
        result.push_edge(path);
    }

    for &(u, v) in &back_edges {
        if u < all_positions.len() && v < all_positions.len() {
            // Back-edge: exit from bottom-center of u, enter top-center of v for visual clarity
            let from = Position::new(all_positions[u].x + nw / 2.0, all_positions[u].y + nh);
            let to = Position::new(all_positions[v].x + nw / 2.0, all_positions[v].y);
            let path = compute_orthogonal_routing(u, v, from, to);
            result.push_cross_edge(path);
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Phase 1: Cycle removal
// ---------------------------------------------------------------------------

/// Classify edges into forward edges (DAG) and back-edges (cycle-forming) using
/// iterative DFS. Back-edges are the minimum set needed to make the graph acyclic.
fn remove_cycles(n: usize, edges: &[(usize, usize)]) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for &(from, to) in edges {
        if from < n && to < n {
            adj[from].push(to);
        }
    }

    let mut visited = vec![false; n];
    let mut in_stack = vec![false; n];
    let mut back_set = std::collections::HashSet::new();

    for start in 0..n {
        if visited[start] {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)]; // (node, child_cursor)
        visited[start] = true;
        in_stack[start] = true;

        while let Some(top) = stack.last_mut() {
            let (u, ci) = *top;
            if ci < adj[u].len() {
                top.1 += 1;
                let v = adj[u][ci];
                if in_stack[v] {
                    back_set.insert((u, v));
                } else if !visited[v] {
                    visited[v] = true;
                    in_stack[v] = true;
                    stack.push((v, 0));
                }
            } else {
                in_stack[u] = false;
                stack.pop();
            }
        }
    }

    let mut forward = Vec::new();
    let mut back = Vec::new();
    for &e in edges {
        if back_set.contains(&e) {
            back.push(e);
        } else {
            forward.push(e);
        }
    }
    (forward, back)
}

// ---------------------------------------------------------------------------
// Phase 2: Layer assignment (longest-path ranking)
// ---------------------------------------------------------------------------

fn assign_layers(n: usize, forward_edges: &[(usize, usize)]) -> Vec<usize> {
    if n == 0 {
        return vec![];
    }

    // Use petgraph toposort to get a valid processing order
    let mut pg: DiGraph<(), ()> = DiGraph::with_capacity(n, forward_edges.len());
    let pg_nodes: Vec<_> = (0..n).map(|_| pg.add_node(())).collect();
    for &(from, to) in forward_edges {
        pg.add_edge(pg_nodes[from], pg_nodes[to], ());
    }

    let topo = match toposort(&pg, None) {
        Ok(order) => order,
        // Shouldn't happen after remove_cycles, but fall back gracefully
        Err(_) => (0..n).map(petgraph::graph::NodeIndex::new).collect(),
    };

    // Longest-path rank: layer[u] = max(layer[pred] + 1) for all predecessors
    let mut preds: Vec<Vec<usize>> = vec![vec![]; n];
    for &(from, to) in forward_edges {
        preds[to].push(from);
    }

    let mut layer = vec![0usize; n];
    for node_idx in &topo {
        let u = node_idx.index();
        for &pred in &preds[u] {
            layer[u] = layer[u].max(layer[pred] + 1);
        }
    }

    layer
}

// ---------------------------------------------------------------------------
// Phase 3a: Dummy node insertion
// ---------------------------------------------------------------------------

fn insert_dummy_nodes(
    dag_len: usize,
    layers: &[usize],
    forward_edges: &[(usize, usize)],
) -> SugiyamaGraph {
    if dag_len == 0 {
        return SugiyamaGraph {
            layers: vec![],
            positions: vec![],
            is_dummy: vec![],
            edges: vec![],
            original_edge: vec![],
        };
    }

    let max_layer = layers.iter().copied().max().unwrap_or(0);
    let num_layers = max_layer + 1;

    let mut layer_nodes: Vec<Vec<usize>> = vec![vec![]; num_layers];
    for i in 0..dag_len {
        layer_nodes[layers[i]].push(i);
    }

    let mut is_dummy = vec![false; dag_len];
    let mut next_id = dag_len;
    let mut all_edges: Vec<(usize, usize)> = Vec::new();
    let mut original_edge: Vec<(usize, usize)> = Vec::new();

    for &(u, v) in forward_edges {
        let lu = layers[u];
        let lv = layers[v];

        if lv == lu + 1 {
            // Adjacent layers: direct edge, no dummy needed
            all_edges.push((u, v));
            original_edge.push((u, v));
        } else if lv > lu + 1 {
            // Long-span edge: insert dummy chain through intermediate layers
            let mut prev = u;
            for l in (lu + 1)..lv {
                let dummy = next_id;
                next_id += 1;
                is_dummy.push(true);
                layer_nodes[l].push(dummy);
                all_edges.push((prev, dummy));
                original_edge.push((u, v));
                prev = dummy;
            }
            all_edges.push((prev, v));
            original_edge.push((u, v));
        } else {
            // Same or earlier layer (shouldn't happen after assign_layers, but be safe)
            all_edges.push((u, v));
            original_edge.push((u, v));
        }
    }

    let total = next_id;
    let mut positions = vec![None::<usize>; total];
    for nodes in &layer_nodes {
        for (pos, &node) in nodes.iter().enumerate() {
            positions[node] = Some(pos);
        }
    }

    SugiyamaGraph {
        layers: layer_nodes,
        positions,
        is_dummy,
        edges: all_edges,
        original_edge,
    }
}

// ---------------------------------------------------------------------------
// Phase 3b: Crossing minimization (barycenter heuristic)
// ---------------------------------------------------------------------------

fn minimize_crossings(g: &mut SugiyamaGraph, passes: usize) {
    if g.layers.len() <= 1 || passes == 0 {
        return;
    }

    let total = g.positions.len();

    // Build adjacency from the expanded edge list
    let mut preds: Vec<Vec<usize>> = vec![vec![]; total];
    let mut succs: Vec<Vec<usize>> = vec![vec![]; total];
    for &(from, to) in &g.edges {
        if from < total && to < total {
            succs[from].push(to);
            preds[to].push(from);
        }
    }

    for _ in 0..passes {
        // Top-down: sort each layer by mean predecessor position in the layer above
        for l in 1..g.layers.len() {
            let sorted = sort_by_barycenter(&g.layers[l], &preds, &g.positions);
            for (pos, &u) in sorted.iter().enumerate() {
                g.positions[u] = Some(pos);
            }
            g.layers[l] = sorted;
        }

        // Bottom-up: sort each layer by mean successor position in the layer below
        for l in (0..g.layers.len().saturating_sub(1)).rev() {
            let sorted = sort_by_barycenter(&g.layers[l], &succs, &g.positions);
            for (pos, &u) in sorted.iter().enumerate() {
                g.positions[u] = Some(pos);
            }
            g.layers[l] = sorted;
        }
    }
}

fn sort_by_barycenter(
    layer: &[usize],
    neighbors: &[Vec<usize>],
    positions: &[Option<usize>],
) -> Vec<usize> {
    let mut with_bc: Vec<(usize, f32)> = layer
        .iter()
        .map(|&u| {
            let neighbor_pos: Vec<f32> = neighbors
                .get(u)
                .into_iter()
                .flatten()
                .filter_map(|&n| positions.get(n).copied().flatten())
                .map(|p| p as f32)
                .collect();

            let bc = if neighbor_pos.is_empty() {
                positions.get(u).copied().flatten().unwrap_or(0) as f32
            } else {
                neighbor_pos.iter().sum::<f32>() / neighbor_pos.len() as f32
            };
            (u, bc)
        })
        .collect();

    with_bc.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    with_bc.into_iter().map(|(u, _)| u).collect()
}

// ---------------------------------------------------------------------------
// Phase 4: Coordinate assignment
// ---------------------------------------------------------------------------

fn assign_coordinates(g: &SugiyamaGraph, config: &LayoutConfig) -> Vec<Position> {
    let total = g.positions.len();
    if total == 0 {
        return vec![];
    }

    let mut coords = vec![Position::default(); total];

    for (layer_idx, nodes) in g.layers.iter().enumerate() {
        let x = layer_idx as f32 * config.level_separation;
        let count = nodes.len();
        let total_span = (count as f32 - 1.0) * config.node_separation;
        let start_y = -total_span / 2.0;

        for (pos_in_layer, &node) in nodes.iter().enumerate() {
            let y = start_y + pos_in_layer as f32 * config.node_separation;
            coords[node] = Position::new(x, y);
        }
    }

    coords
}

// ---------------------------------------------------------------------------
// Edge routing
// ---------------------------------------------------------------------------

/// Build one `EdgePath` per original (u, v) pair by routing through dummy waypoints.
/// `dag_len` separates real nodes (< dag_len) from dummies (>= dag_len).
/// `node_width`/`node_height` are used to compute port positions on node borders.
fn route_edges(
    g: &SugiyamaGraph,
    all_positions: &[Position],
    _dag_len: usize,
    node_width: f32,
    node_height: f32,
) -> Vec<EdgePath> {
    // Group edge segments by their original (u, v) in a sorted map for determinism
    let mut chains: BTreeMap<(usize, usize), Vec<(usize, usize)>> = BTreeMap::new();
    for (edge, orig) in g.edges.iter().zip(g.original_edge.iter()) {
        chains.entry(*orig).or_default().push(*edge);
    }

    let mut result = Vec::new();

    for ((u, v), mut segments) in chains {
        if u >= all_positions.len() || v >= all_positions.len() {
            continue;
        }

        // Sort segments in ascending layer order (= ascending x)
        segments.sort_by(|a, b| {
            let ax = all_positions.get(a.0).map(|p| p.x).unwrap_or(0.0);
            let bx = all_positions.get(b.0).map(|p| p.x).unwrap_or(0.0);
            ax.partial_cmp(&bx).unwrap_or(std::cmp::Ordering::Equal)
        });

        // Collect ordered waypoints: [from of first seg, to of each seg]
        let mut waypoints: Vec<Position> = Vec::with_capacity(segments.len() + 1);
        if let Some(&(first_from, _)) = segments.first() {
            if first_from < all_positions.len() {
                waypoints.push(all_positions[first_from]);
            }
        }
        for &(_, to) in &segments {
            if to < all_positions.len() {
                waypoints.push(all_positions[to]);
            }
        }

        // Adjust waypoints to use node-border connection ports:
        // - first (parent): exit from right edge, vertically centered
        // - last (child):   enter from left edge, vertically centered
        // - dummies:        vertically centered pass-through
        if waypoints.len() >= 2 {
            let last_idx = waypoints.len() - 1;
            // Parent right-exit port
            waypoints[0].x += node_width - 1.0;
            waypoints[0].y += node_height / 2.0;
            // Child left-entry port
            waypoints[last_idx].y += node_height / 2.0;
            // Dummy pass-through: vertically center only
            // (dummy x is already between the two real nodes)
            for wp in &mut waypoints[1..last_idx] {
                wp.y += node_height / 2.0;
            }
        } else if waypoints.len() == 1 {
            // Self-loop or degenerate: just center vertically
            waypoints[0].y += node_height / 2.0;
        }

        // Route each consecutive hop; combine all segments into a single EdgePath
        let mut edge_segments = Vec::new();
        for w in waypoints.windows(2) {
            let hop = compute_orthogonal_routing(u, v, w[0], w[1]);
            edge_segments.extend(hop.segments);
        }

        if !edge_segments.is_empty() {
            result.push(EdgePath {
                parent_id: u,
                child_id: v,
                segments: edge_segments,
            });
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bounds_from_positions(
    positions: &[Position],
    node_width: f32,
    node_height: f32,
) -> LayoutBounds {
    if positions.is_empty() {
        return LayoutBounds::default();
    }

    let hw = node_width / 2.0;
    let hh = node_height / 2.0;

    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for p in positions {
        min_x = min_x.min(p.x - hw);
        max_x = max_x.max(p.x + hw);
        min_y = min_y.min(p.y - hh);
        max_y = max_y.max(p.y + hh);
    }

    LayoutBounds::new(min_x, max_x, min_y, max_y)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Dag;

    #[test]
    fn remove_cycles_linear_chain_no_back_edges() {
        // 0 → 1 → 2: no cycles
        let edges = vec![(0, 1), (1, 2)];
        let (fwd, back) = remove_cycles(3, &edges);
        assert_eq!(fwd.len(), 2);
        assert!(back.is_empty());
    }

    #[test]
    fn remove_cycles_simple_cycle() {
        // 0 → 1 → 0: one back-edge
        let edges = vec![(0, 1), (1, 0)];
        let (fwd, back) = remove_cycles(2, &edges);
        assert_eq!(fwd.len() + back.len(), 2);
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn assign_layers_diamond() {
        // root(0) → A(1) → C(3)
        // root(0) → B(2) → C(3)
        let fwd = vec![(0, 1), (0, 2), (1, 3), (2, 3)];
        let layers = assign_layers(4, &fwd);
        assert_eq!(layers[0], 0);
        assert_eq!(layers[1], 1);
        assert_eq!(layers[2], 1);
        assert_eq!(layers[3], 2);
    }

    #[test]
    fn assign_layers_long_path() {
        // 0 → 1, 0 → 3, 1 → 2, 2 → 3
        // Longest path to 3: 0→1→2→3 = layer 3
        let fwd = vec![(0, 1), (0, 3), (1, 2), (2, 3)];
        let layers = assign_layers(4, &fwd);
        assert_eq!(layers[0], 0);
        assert_eq!(layers[1], 1);
        assert_eq!(layers[2], 2);
        assert_eq!(layers[3], 3);
    }

    #[test]
    fn insert_dummy_nodes_long_span_creates_dummies() {
        // 0 at layer 0, 1 at layer 2 → span = 2, needs 1 dummy
        let fwd = vec![(0, 1)];
        let layers = vec![0, 2];
        let sg = insert_dummy_nodes(2, &layers, &fwd);

        // 3 nodes total: 0, 1, dummy
        assert_eq!(sg.positions.len(), 3);
        assert!(sg.is_dummy[2]);

        // 2 edges: 0→dummy, dummy→1 (both with original_edge (0,1))
        assert_eq!(sg.edges.len(), 2);
        assert_eq!(sg.original_edge[0], (0, 1));
        assert_eq!(sg.original_edge[1], (0, 1));
    }

    #[test]
    fn compute_sugiyama_single_node() {
        let mut dag: Dag<&str> = Dag::new();
        dag.add_node("root");

        let config = LayoutConfig::new();
        let result = compute_sugiyama(&dag, &config).unwrap();

        assert_eq!(result.positions().len(), 1);
        assert!(result.edges().is_empty());
        assert!(result.cross_edges().is_empty());
    }

    #[test]
    fn compute_sugiyama_diamond_no_duplicate_positions() {
        let mut dag: Dag<u32> = Dag::new();
        let root = dag.add_node(0);
        let a = dag.add_node(1);
        let b = dag.add_node(2);
        let c = dag.add_node(3);
        dag.add_edge(root, a).unwrap();
        dag.add_edge(root, b).unwrap();
        dag.add_edge(a, c).unwrap();
        dag.add_edge(b, c).unwrap();

        let config = LayoutConfig::new();
        let result = compute_sugiyama(&dag, &config).unwrap();

        // 4 unique node positions (C appears exactly once)
        assert_eq!(result.positions().len(), 4);

        // C position is unique
        let c_pos = result.position(c).unwrap();
        let pos_at_c: Vec<_> = result.positions().iter().filter(|&&p| p == c_pos).collect();
        assert_eq!(pos_at_c.len(), 1);
    }

    #[test]
    fn compute_sugiyama_diamond_layer_positions() {
        let mut dag: Dag<u32> = Dag::new();
        let root = dag.add_node(0);
        let a = dag.add_node(1);
        let b = dag.add_node(2);
        let c = dag.add_node(3);
        dag.add_edge(root, a).unwrap();
        dag.add_edge(root, b).unwrap();
        dag.add_edge(a, c).unwrap();
        dag.add_edge(b, c).unwrap();

        let config = LayoutConfig::new();
        let result = compute_sugiyama(&dag, &config).unwrap();
        let level_sep = config.level_separation;

        // root at layer 0 (x=0), A and B at layer 1 (x=level_sep), C at layer 2 (x=2*level_sep)
        assert_eq!(result.position(root).unwrap().x, 0.0);
        assert_eq!(result.position(a).unwrap().x, level_sep);
        assert_eq!(result.position(b).unwrap().x, level_sep);
        assert_eq!(result.position(c).unwrap().x, 2.0 * level_sep);
    }

    #[test]
    fn compute_sugiyama_cycle_no_panic() {
        let mut dag: Dag<u32> = Dag::new();
        let a = dag.add_node(0);
        let b = dag.add_node(1);
        dag.add_edge(a, b).unwrap();
        dag.add_edge(b, a).unwrap();

        let config = LayoutConfig::new();
        let result = compute_sugiyama(&dag, &config).unwrap();

        assert_eq!(result.positions().len(), 2);
        // One of the two edges is a back-edge
        assert_eq!(result.cross_edges().len(), 1);
    }

    #[test]
    fn compute_sugiyama_empty_returns_error() {
        let dag: Dag<u32> = Dag::new();
        let config = LayoutConfig::new();
        assert!(matches!(
            compute_sugiyama(&dag, &config),
            Err(LayoutError::EmptyTree)
        ));
    }

    #[test]
    fn crossing_minimization_does_not_increase_crossings() {
        // Fixed-topology graph where crossing minimization should help or maintain.
        // Layer 0: [0]
        // Layer 1: [1, 2, 3]
        // Layer 2: [4]
        // Edges: 0→1, 0→2, 0→3, 1→4, 2→4, 3→4
        // With crossing minimization, positions should be stable.
        let mut dag: Dag<u32> = Dag::new();
        for i in 0..5 {
            dag.add_node(i);
        }
        dag.add_edge(0, 1).unwrap();
        dag.add_edge(0, 2).unwrap();
        dag.add_edge(0, 3).unwrap();
        dag.add_edge(1, 4).unwrap();
        dag.add_edge(2, 4).unwrap();
        dag.add_edge(3, 4).unwrap();

        let config = LayoutConfig::new();
        let result = compute_sugiyama(&dag, &config).unwrap();

        // All 5 nodes have positions
        assert_eq!(result.positions().len(), 5);
        // Node 0 at layer 0, nodes 1/2/3 at layer 1, node 4 at layer 2
        let level_sep = config.level_separation;
        assert_eq!(result.position(0).unwrap().x, 0.0);
        assert_eq!(result.position(4).unwrap().x, 2.0 * level_sep);
        for &mid in &[1usize, 2, 3] {
            assert_eq!(result.position(mid).unwrap().x, level_sep);
        }
    }
}
