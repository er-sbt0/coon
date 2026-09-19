use std::collections::BTreeMap;

use crate::config::LayoutConfig;
use crate::dag::Dag;
use crate::edge_router::{compute_orthogonal_routing, compute_orthogonal_routing_with_mid};
use crate::error::LayoutError;
use crate::result::LayoutResult;
use crate::types::{EdgePath, LayoutBounds, Position};

/// Expanded graph used internally during the four Sugiyama phases.
///
/// Real nodes occupy indices `0..dag_len`; dummy nodes occupy `dag_len..`.
/// This convention makes `all_positions[..dag_len]` the slice of real-node
/// coordinates that goes into `LayoutResult` — and it is the *only* record of
/// which nodes are dummies. There used to be an `is_dummy` flag beside it, but
/// nothing downstream ever asked: routing groups segments by the original
/// `(u, v)` they belong to, so a dummy is never an endpoint it can see, and
/// `split_merged_edges` keys on `child_id`, which is a real node by
/// construction. Two answers to one question is one too many.
struct SugiyamaGraph {
    /// `layers[layer_idx]` = ordered list of node indices in that layer
    layers: Vec<Vec<usize>>,
    /// `positions[node_idx]` = position of node within its layer (for barycenter)
    positions: Vec<Option<usize>>,
    /// `groups[node_idx]` — optional group membership, used in Phase 3b to keep
    /// the members of one group contiguous within a layer. `None` = ungrouped.
    groups: Vec<Option<u32>>,
    /// All edges (real + dummy-chain segments)
    edges: Vec<(usize, usize)>,
    /// Parallel to `edges`: original real (u, v) this segment belongs to
    original_edge: Vec<(usize, usize)>,
}

/// Entry point called from `LayoutEngine::compute_dag` and
/// `LayoutEngine::compute_dag_grouped`.
///
/// `groups[i]` is the group of node `i`; `None`, and any index past the end of
/// the slice, means the node belongs to no group. An empty slice is therefore
/// the ungrouped layout, byte for byte.
pub(crate) fn compute_sugiyama_grouped<T>(
    dag: &Dag<T>,
    config: &LayoutConfig,
    groups: &[Option<u32>],
) -> Result<LayoutResult, LayoutError> {
    if dag.is_empty() {
        return Err(LayoutError::EmptyTree);
    }

    let all_dag_edges: Vec<(usize, usize)> = dag.edges().collect();
    let Acyclic {
        forward: forward_edges,
        back: back_edges,
        topo,
    } = remove_cycles(dag.len(), &all_dag_edges);
    let layers = assign_layers(dag.len(), &forward_edges, &topo);
    let mut sg = insert_dummy_nodes(dag.len(), &layers, &forward_edges, groups);

    let passes = config.crossing_minimization_passes.unwrap_or(2);
    minimize_crossings(&mut sg, passes);

    let all_positions = assign_coordinates(&sg, config);

    // Real-node positions only (index space 0..dag_len)
    let node_positions = all_positions[..dag.len()].to_vec();
    let bounds = bounds_from_positions(&node_positions, config.node_width, config.node_height);

    let mut result = LayoutResult::new(node_positions, bounds, dag.version());

    let nw = config.node_width;
    let nh = config.node_height;

    let routed = route_edges(&sg, &all_positions, dag.len(), nw, nh);
    let (single_edges, merged_edges, merge_trunks) = split_merged_edges(routed, &all_positions, nh);
    for path in single_edges {
        result.push_edge(path);
    }
    for path in merged_edges {
        result.push_merged_edge(path);
    }
    for path in merge_trunks {
        result.push_merge_trunk(path);
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

/// What one DFS over the input graph yields: the edges that survive, the ones
/// that had to go, and an order the survivors respect.
struct Acyclic {
    /// Every edge that is not a back-edge, in the input's order.
    forward: Vec<(usize, usize)>,
    /// The back-edges, drawn separately as cross-edges rather than layered.
    back: Vec<(usize, usize)>,
    /// A topological order of `forward`, covering every node `0..n`.
    topo: Vec<usize>,
}

/// Classify edges into forward edges (DAG) and back-edges (cycle-forming) using
/// iterative DFS. Back-edges are the minimum set needed to make the graph acyclic.
///
/// The same DFS hands back a topological order for free, which is why
/// [`assign_layers`] takes one rather than computing its own. Reverse
/// postorder *is* that order: for any edge `(u, v)` the DFS did not call a
/// back-edge, `v` finishes before `u` — it is either a descendant reached from
/// `u`, or a node already off the stack when `u` looked at it — so ordering by
/// descending finish time puts every tail before its head. The back-edges are
/// exactly the pairs that break that, and they are the ones being removed.
fn remove_cycles(n: usize, edges: &[(usize, usize)]) -> Acyclic {
    let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
    for &(from, to) in edges {
        if from < n && to < n {
            adj[from].push(to);
        }
    }

    let mut visited = vec![false; n];
    let mut in_stack = vec![false; n];
    let mut back_set = std::collections::HashSet::new();
    let mut postorder: Vec<usize> = Vec::with_capacity(n);

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
                // `u` is finished: everything reachable from it that was not
                // behind a back-edge is already in `postorder`.
                postorder.push(u);
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
    // Reverse postorder. The outer loop visits every node, so this names all
    // `n` of them whether or not any edge touches them.
    postorder.reverse();
    Acyclic {
        forward,
        back,
        topo: postorder,
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Layer assignment (longest-path ranking)
// ---------------------------------------------------------------------------

/// Longest-path ranking: `layer[u] = max(layer[pred] + 1)`, over the whole
/// predecessor set.
///
/// `topo` is [`Acyclic::topo`], a topological order of `forward_edges`, and it
/// has to be one: the rank of a node is read off its predecessors, so they
/// must already be final when it is reached. *Which* valid order it is does
/// not matter — the longest path to a node is a property of the graph, not of
/// the walk — so building one here from scratch, as this used to do with a
/// throwaway `petgraph::DiGraph` and `toposort`, allocated a second copy of
/// the graph to recover something the cycle-removal DFS had just computed and
/// dropped.
///
/// There is no fallback for "the order was not topological" any more, and
/// nothing to fall back *to*: the old `Err(_) => (0..n)` arm silently produced
/// a wrong layering for a graph that still had a cycle, which is a layout bug
/// that looks like a layout. The order now comes from the same pass that
/// removed the cycles, so it is correct by construction.
fn assign_layers(n: usize, forward_edges: &[(usize, usize)], topo: &[usize]) -> Vec<usize> {
    if n == 0 {
        return vec![];
    }

    let mut preds: Vec<Vec<usize>> = vec![vec![]; n];
    for &(from, to) in forward_edges {
        preds[to].push(from);
    }

    let mut layer = vec![0usize; n];
    for &u in topo {
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
    groups: &[Option<u32>],
) -> SugiyamaGraph {
    if dag_len == 0 {
        return SugiyamaGraph {
            layers: vec![],
            positions: vec![],
            groups: vec![],
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

    // Group membership for the expanded graph: real nodes take theirs from the
    // caller, dummies inherit below.
    let mut node_groups: Vec<Option<u32>> = (0..dag_len).map(|i| group_of(groups, i)).collect();
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
            // Long-span edge: insert dummy chain through intermediate layers.
            // A dummy joins the group only when the edge stays inside one: a
            // wire routed through a group is fine, a wire merely passing by
            // must not split that group's run.
            let dummy_group = match (group_of(groups, u), group_of(groups, v)) {
                (Some(gu), Some(gv)) if gu == gv => Some(gu),
                _ => None,
            };
            let mut prev = u;
            for l in (lu + 1)..lv {
                let dummy = next_id;
                next_id += 1;
                node_groups.push(dummy_group);
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
        groups: node_groups,
        edges: all_edges,
        original_edge,
    }
}

/// Group of original node `i`, treating a short slice as all-`None`.
fn group_of(groups: &[Option<u32>], i: usize) -> Option<u32> {
    groups.get(i).copied().flatten()
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
            let sorted = sort_by_barycenter(&g.layers[l], &preds, &g.positions, &g.groups);
            for (pos, &u) in sorted.iter().enumerate() {
                g.positions[u] = Some(pos);
            }
            g.layers[l] = sorted;
        }

        // Bottom-up: sort each layer by mean successor position in the layer below
        for l in (0..g.layers.len().saturating_sub(1)).rev() {
            let sorted = sort_by_barycenter(&g.layers[l], &succs, &g.positions, &g.groups);
            for (pos, &u) in sorted.iter().enumerate() {
                g.positions[u] = Some(pos);
            }
            g.layers[l] = sorted;
        }
    }
}

/// Order one layer by barycenter, keeping the members of a group contiguous.
///
/// A node's own barycenter is the mean position of its neighbours in the
/// adjacent layer, as in the ungrouped case. A group's barycenter is the mean
/// of its members' own barycenters *within this layer*, so the group travels as
/// a block. The sort key is `(group barycenter, group id, own barycenter)`:
///
/// * an ungrouped node uses its own barycenter in the group slot, so it
///   competes as a group of one and can come to rest between two groups rather
///   than being pushed to an end;
/// * the group id only breaks exact ties between two groups that landed on the
///   same barycenter — without it their members would interleave. It never
///   orders groups whose barycenters differ, so the opaque id stays opaque.
///
/// With no groups the key collapses to `(own, None, own)`, which makes exactly
/// the comparisons the ungrouped sort made; the sort is stable, so the result
/// is unchanged.
fn sort_by_barycenter(
    layer: &[usize],
    neighbors: &[Vec<usize>],
    positions: &[Option<usize>],
    groups: &[Option<u32>],
) -> Vec<usize> {
    // (node, own barycenter, group)
    let with_bc: Vec<(usize, f32, Option<u32>)> = layer
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
            (u, bc, group_of(groups, u))
        })
        .collect();

    // Group barycenters: summed in layer order, held in a BTreeMap so nothing
    // here depends on hash iteration order.
    let mut group_sums: BTreeMap<u32, (f32, usize)> = BTreeMap::new();
    for &(_, bc, group) in &with_bc {
        if let Some(g) = group {
            let entry = group_sums.entry(g).or_insert((0.0, 0));
            entry.0 += bc;
            entry.1 += 1;
        }
    }

    // Materialise the full key per node so the comparator stays cheap and the
    // group barycenter is computed once.
    let mut keyed: Vec<(f32, Option<u32>, f32, usize)> = with_bc
        .into_iter()
        .map(|(u, bc, group)| {
            let group_bc = match group {
                Some(g) => {
                    let (sum, count) = group_sums[&g];
                    sum / count as f32
                }
                None => bc,
            };
            (group_bc, group, bc, u)
        })
        .collect();

    keyed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    keyed.into_iter().map(|(_, _, _, u)| u).collect()
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

    // -----------------------------------------------------------------------
    // Pre-compute per-edge stagger fractions — two separate fracs:
    //
    // 1. `exit_frac`  (per parent→child rank within the same parent)
    //    Controls the vertical exit port on the parent node so that siblings
    //    leave from different rows.  f = (k+1)/(n+1) where n = sibling count.
    //
    // 2. `mid_frac`   (per edge rank within the same LAYER-PAIR)
    //    Controls the horizontal position of the vertical bar so that edges
    //    from *different parents* that cross the same inter-layer gap also get
    //    distinct columns.  Without this, all single-child parents produce
    //    frac=0.5 and their bars form a shared visual spine that appears to
    //    cut through the bounding boxes of every node whose y falls in range.
    // -----------------------------------------------------------------------

    // Build node → layer index map
    let mut node_to_layer: Vec<usize> = vec![0; all_positions.len().max(g.positions.len())];
    for (layer_idx, nodes) in g.layers.iter().enumerate() {
        for &node in nodes {
            if node < node_to_layer.len() {
                node_to_layer[node] = layer_idx;
            }
        }
    }

    // --- exit_frac: per-parent sibling rank ---
    let mut parent_to_children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &(u, v) in chains.keys() {
        parent_to_children.entry(u).or_default().push(v);
    }
    // Sort each child list by destination Y for stable, visually consistent ordering
    for children in parent_to_children.values_mut() {
        children.sort_by(|&a, &b| {
            let ay = all_positions.get(a).map(|p| p.y).unwrap_or(0.0);
            let by_ = all_positions.get(b).map(|p| p.y).unwrap_or(0.0);
            ay.partial_cmp(&by_).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let exit_frac: BTreeMap<(usize, usize), f32> = parent_to_children
        .iter()
        .flat_map(|(&parent, children)| {
            let n = children.len() as f32;
            children.iter().enumerate().map(move |(k, &child)| {
                // When a parent has multiple children, all wires exit from the
                // same center-right port (0.5).  This ensures they share a single
                // visual exit point, which matters for the reverse-arrow view
                // where these ports become arrowhead targets (multiple staggered
                // arrows into the same node looks wrong).  Individual wires still
                // diverge cleanly at their own mid_x corner positions.
                // For single-child parents the standard (k+1)/(n+1) formula is fine.
                let frac = if n > 1.0 {
                    0.5
                } else {
                    (k as f32 + 1.0) / (n + 1.0)
                };
                ((parent, child), frac)
            })
        })
        .collect();

    // --- mid_frac: per-layer-pair rank (avoids shared spine across parents) ---
    // Group all edges by (src_layer, dst_layer); sort by midpoint-y for
    // deterministic, visually ordered assignment.
    let mut layer_pair_edges: BTreeMap<(usize, usize), Vec<(usize, usize)>> = BTreeMap::new();
    for &(u, v) in chains.keys() {
        let lu = if u < node_to_layer.len() {
            node_to_layer[u]
        } else {
            0
        };
        let lv = if v < node_to_layer.len() {
            node_to_layer[v]
        } else {
            0
        };
        layer_pair_edges.entry((lu, lv)).or_default().push((u, v));
    }
    for edges in layer_pair_edges.values_mut() {
        edges.sort_by(|&(u1, v1), &(u2, v2)| {
            let mid1 = (all_positions.get(u1).map(|p| p.y).unwrap_or(0.0)
                + all_positions.get(v1).map(|p| p.y).unwrap_or(0.0))
                / 2.0;
            let mid2 = (all_positions.get(u2).map(|p| p.y).unwrap_or(0.0)
                + all_positions.get(v2).map(|p| p.y).unwrap_or(0.0))
                / 2.0;
            mid1.partial_cmp(&mid2).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mid_frac: BTreeMap<(usize, usize), f32> = layer_pair_edges
        .iter()
        .flat_map(|(_, edges)| {
            let n = edges.len() as f32;
            edges
                .iter()
                .enumerate()
                .map(move |(k, &(u, v))| ((u, v), (k as f32 + 1.0) / (n + 1.0)))
        })
        .collect();

    // Keep `stagger` as an alias for exit_frac for backward compatibility in
    // the waypoint adjustment code below.
    let stagger = exit_frac;

    let mut result = Vec::new();

    for ((u, v), mut segments) in chains {
        if u >= all_positions.len() || v >= all_positions.len() {
            continue;
        }

        // exit_frac: controls the vertical exit port on the parent node
        let frac = stagger.get(&(u, v)).copied().unwrap_or(0.5);
        // mid_frac: controls the horizontal column of the vertical bar within
        // the inter-layer gap.  Using the layer-pair rank prevents bars from
        // different parents (but the same layer pair) from sharing one column
        // and forming a continuous visual spine.
        let mfrac = mid_frac.get(&(u, v)).copied().unwrap_or(0.5);

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
        // - first (parent): exit from right edge; Y staggered by sibling rank
        // - last (child):   enter from left edge, vertically centered
        // - dummies:        vertically centered pass-through
        if waypoints.len() >= 2 {
            let last_idx = waypoints.len() - 1;
            // Parent right-exit port — stagger exit Y so siblings don't share a port
            waypoints[0].x += node_width - 1.0;
            waypoints[0].y += node_height * frac;
            // Child left-entry port — keep centered (multiple parents are rarer)
            waypoints[last_idx].y += node_height / 2.0;
            // Dummy pass-through: vertically center only
            for wp in &mut waypoints[1..last_idx] {
                wp.y += node_height / 2.0;
            }
        } else if waypoints.len() == 1 {
            waypoints[0].y += node_height / 2.0;
        }

        // Route each consecutive hop using a staggered mid_x so that edges
        // between the same layer pair don't share a vertical bar column.
        let mut edge_segments = Vec::new();
        for w in waypoints.windows(2) {
            let gap = w[1].x - w[0].x;
            // Use layer-pair frac (mfrac) for mid_x so edges from different
            // parents in the same layer pair get distinct vertical columns.
            // Clamp to [0.25, 0.75] so the bar never lands right on a border.
            let clamped = 0.25 + mfrac * 0.5;
            let mid_x = w[0].x + gap * clamped;
            let hop = compute_orthogonal_routing_with_mid(u, v, w[0], w[1], mid_x);
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

/// Post-process routed edges: for any child node that has more than one incoming
/// edge, trim each per-parent path so it ends at a shared "merge column" one cell
/// left of the child node, then emit a single short trunk from that column into
/// the child.  Returns `(single_edges, merged_edges, merge_trunks)`.
///
/// * `single_edges`  — unchanged paths for children with exactly one parent.
/// * `merged_edges`  — trimmed per-parent paths (rendered as lines, no arrowhead).
/// * `merge_trunks`  — one horizontal trunk per multi-parent child (carries the
///                     single shared arrowhead).
fn split_merged_edges(
    paths: Vec<EdgePath>,
    positions: &[Position],
    node_height: f32,
) -> (Vec<EdgePath>, Vec<EdgePath>, Vec<EdgePath>) {
    use std::collections::BTreeMap;

    // Group paths by child_id while preserving order.
    let mut by_child: BTreeMap<usize, Vec<EdgePath>> = BTreeMap::new();
    for path in paths {
        by_child.entry(path.child_id).or_default().push(path);
    }

    let mut single_edges: Vec<EdgePath> = Vec::new();
    let mut merged_edges: Vec<EdgePath> = Vec::new();
    let mut merge_trunks: Vec<EdgePath> = Vec::new();

    for (child_id, group) in by_child {
        if group.len() <= 1 {
            // Single parent — no merging needed.
            single_edges.extend(group);
        } else {
            // Multiple parents share this child.
            let child_pos = match positions.get(child_id) {
                Some(p) => *p,
                None => {
                    single_edges.extend(group);
                    continue;
                }
            };
            let merge_x = child_pos.x - 1.0;
            let child_entry_y = child_pos.y + node_height / 2.0;

            for mut path in group {
                // Find the last Horizontal segment and shorten it to merge_x.
                let last_h_idx = path
                    .segments
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, s)| {
                        matches!(s.segment_type, crate::types::EdgeSegmentType::Horizontal)
                    })
                    .map(|(i, _)| i);

                if let Some(idx) = last_h_idx {
                    let seg = &mut path.segments[idx];
                    if (merge_x - seg.from.x).abs() < 0.05 {
                        // Would become zero-length — remove it entirely.
                        path.segments.remove(idx);
                    } else {
                        seg.to.x = merge_x;
                        seg.to.y = child_entry_y;
                        // Also correct the from.y to match child_entry_y (last H is always flat).
                        seg.from.y = child_entry_y;
                    }
                }

                if !path.segments.is_empty() {
                    merged_edges.push(path);
                }
            }

            // One shared trunk: horizontal from merge_x to child left border.
            if merge_x < child_pos.x {
                use crate::types::{EdgePath, EdgeSegment, EdgeSegmentType, Position};
                merge_trunks.push(EdgePath {
                    parent_id: child_id, // sentinel — same id as child marks it as a trunk
                    child_id,
                    segments: vec![EdgeSegment {
                        from: Position {
                            x: merge_x,
                            y: child_entry_y,
                        },
                        to: Position {
                            x: child_pos.x,
                            y: child_entry_y,
                        },
                        segment_type: EdgeSegmentType::Horizontal,
                    }],
                });
            }
        }
    }

    (single_edges, merged_edges, merge_trunks)
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

    /// The ungrouped layout, as `LayoutEngine::compute_dag` reaches it.
    fn compute_sugiyama<T>(
        dag: &Dag<T>,
        config: &LayoutConfig,
    ) -> Result<LayoutResult, LayoutError> {
        compute_sugiyama_grouped(dag, config, &[])
    }

    /// Whether `topo` lists every tail before its head.
    fn topo_respects(topo: &[usize], edges: &[(usize, usize)]) -> bool {
        let mut rank = vec![usize::MAX; topo.len()];
        for (i, &u) in topo.iter().enumerate() {
            rank[u] = i;
        }
        edges.iter().all(|&(u, v)| rank[u] < rank[v])
    }

    /// The ranking is a property of the graph, not of the walk that reached
    /// it: any topological order gives the same layers. This is what lets the
    /// order come from the cycle-removal DFS instead of a second pass.
    #[test]
    fn assign_layers_is_independent_of_the_topological_order() {
        // A diamond with a long span: 0→1→2→4, 0→3→4, 0→4.
        let fwd = vec![(0, 1), (1, 2), (2, 4), (0, 3), (3, 4), (0, 4)];
        let from_dfs = remove_cycles(5, &fwd).topo;
        assert!(topo_respects(&from_dfs, &fwd));
        let expected = assign_layers(5, &fwd, &from_dfs);
        assert_eq!(expected, vec![0, 1, 2, 1, 3]);

        // Every other valid order of the same five nodes must agree.
        let mut perm = [0usize, 1, 2, 3, 4];
        let mut checked = 0;
        permutations(&mut perm, 0, &mut |order: &[usize]| {
            if topo_respects(order, &fwd) {
                checked += 1;
                assert_eq!(assign_layers(5, &fwd, order), expected, "order {order:?}");
            }
        });
        assert!(checked > 1, "the fixture has only one valid order");
    }

    /// Every permutation of `slice`, handed to `f` one at a time.
    fn permutations(slice: &mut [usize], k: usize, f: &mut impl FnMut(&[usize])) {
        if k == slice.len() {
            f(slice);
            return;
        }
        for i in k..slice.len() {
            slice.swap(k, i);
            permutations(slice, k + 1, f);
            slice.swap(k, i);
        }
    }

    #[test]
    fn remove_cycles_linear_chain_no_back_edges() {
        // 0 → 1 → 2: no cycles
        let edges = vec![(0, 1), (1, 2)];
        let out = remove_cycles(3, &edges);
        assert_eq!(out.forward.len(), 2);
        assert!(out.back.is_empty());
        assert_eq!(out.topo, vec![0, 1, 2], "reverse postorder is the chain");
    }

    #[test]
    fn remove_cycles_simple_cycle() {
        // 0 → 1 → 0: one back-edge
        let edges = vec![(0, 1), (1, 0)];
        let out = remove_cycles(2, &edges);
        assert_eq!(out.forward.len() + out.back.len(), 2);
        assert_eq!(out.back.len(), 1);
        // The order has to respect what survived, not what was cut.
        assert!(topo_respects(&out.topo, &out.forward));
    }

    #[test]
    fn assign_layers_diamond() {
        // root(0) → A(1) → C(3)
        // root(0) → B(2) → C(3)
        let fwd = vec![(0, 1), (0, 2), (1, 3), (2, 3)];
        let layers = assign_layers(4, &fwd, &remove_cycles(4, &fwd).topo);
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
        let layers = assign_layers(4, &fwd, &remove_cycles(4, &fwd).topo);
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
        let sg = insert_dummy_nodes(2, &layers, &fwd, &[]);

        // 3 nodes total: 0, 1, dummy
        // Three nodes for two real ones: index 2 is past `dag_len`, which is
        // the whole of what marks it a dummy.
        assert_eq!(sg.positions.len(), 3);
        assert_eq!(sg.layers[1], vec![2], "the dummy sits on the skipped layer");

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
    // -----------------------------------------------------------------------
    // Group-aware ordering
    // -----------------------------------------------------------------------

    /// Nodes of `ids` in their final top-to-bottom order within a layer.
    fn order_by_y(result: &LayoutResult, ids: &[usize]) -> Vec<usize> {
        let mut ids = ids.to_vec();
        ids.sort_by(|&a, &b| {
            let ay = result.position(a).unwrap().y;
            let by_ = result.position(b).unwrap().y;
            ay.partial_cmp(&by_).unwrap()
        });
        ids
    }

    /// True when no group id appears in more than one run of `order`.
    fn is_contiguous(order: &[usize], groups: &[Option<u32>]) -> bool {
        let mut seen: Vec<u32> = Vec::new();
        let mut prev: Option<u32> = None;
        for &u in order {
            let g = groups[u];
            if g != prev {
                if let Some(g) = g {
                    if seen.contains(&g) {
                        return false;
                    }
                    seen.push(g);
                }
                prev = g;
            }
        }
        true
    }

    /// Pairs of edges whose endpoints sit in opposite vertical order at the two
    /// ends, i.e. edges that cross, taken from the final real-node positions.
    fn count_crossings(
        result: &LayoutResult,
        edges: &[(usize, usize)],
        config: &LayoutConfig,
    ) -> usize {
        let layer = |u: usize| (result.position(u).unwrap().x / config.level_separation).round();
        let y = |u: usize| result.position(u).unwrap().y;
        let mut crossings = 0;
        for (i, &(u1, v1)) in edges.iter().enumerate() {
            for &(u2, v2) in &edges[i + 1..] {
                if u1 == u2 || v1 == v2 {
                    continue;
                }
                if layer(u1) != layer(u2) || layer(v1) != layer(v2) {
                    continue;
                }
                if (y(u1) - y(u2)) * (y(v1) - y(v2)) < 0.0 {
                    crossings += 1;
                }
            }
        }
        crossings
    }

    /// A root fanning out to `n` parents, each with one child in the next layer:
    /// the children's order is driven entirely by their parents' order.
    fn paired_layers(n: usize) -> (Dag<u32>, Vec<usize>, Vec<usize>) {
        let mut dag: Dag<u32> = Dag::new();
        let parents: Vec<usize> = (0..n).map(|i| dag.add_node(i as u32)).collect();
        let children: Vec<usize> = (0..n).map(|i| dag.add_node((n + i) as u32)).collect();
        for i in 0..n {
            dag.add_edge(parents[i], children[i]).unwrap();
        }
        (dag, parents, children)
    }

    #[test]
    fn empty_groups_reproduce_the_ungrouped_layout() {
        // The regression guard: the grouped entry point with nothing grouped
        // must return the same bits as the ungrouped one, edge routing included.
        let mut dag: Dag<u32> = Dag::new();
        for i in 0..12 {
            dag.add_node(i);
        }
        // A shape with a fan-out, a re-join and a long-span edge (dummy nodes).
        for &(u, v) in &[
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 4),
            (2, 4),
            (3, 5),
            (4, 6),
            (5, 6),
            (0, 7),
            (7, 8),
            (8, 9),
            (6, 9),
            (2, 10),
            (10, 11),
            (0, 11),
        ] {
            dag.add_edge(u, v).unwrap();
        }

        let config = LayoutConfig::new();
        let plain = compute_sugiyama(&dag, &config).unwrap();
        let none_groups = vec![None; dag.len()];
        let grouped = compute_sugiyama_grouped(&dag, &config, &none_groups).unwrap();
        let empty = compute_sugiyama_grouped(&dag, &config, &[]).unwrap();

        for other in [&grouped, &empty] {
            assert_eq!(plain.positions().len(), other.positions().len());
            for (a, b) in plain.positions().iter().zip(other.positions()) {
                // Compare bit patterns: identical, not merely close.
                assert_eq!(a.x.to_bits(), b.x.to_bits());
                assert_eq!(a.y.to_bits(), b.y.to_bits());
            }
            assert_eq!(plain.bounds(), other.bounds());
            for (a, b) in [
                (plain.edges(), other.edges()),
                (plain.cross_edges(), other.cross_edges()),
                (plain.merged_edges(), other.merged_edges()),
                (plain.merge_trunks(), other.merge_trunks()),
            ] {
                assert_eq!(a.len(), b.len());
                for (pa, pb) in a.iter().zip(b) {
                    assert_eq!(pa.parent_id, pb.parent_id);
                    assert_eq!(pa.child_id, pb.child_id);
                    assert_eq!(pa.segments, pb.segments);
                }
            }
        }
    }

    #[test]
    fn interleaved_groups_come_out_contiguous() {
        // Structure interleaves the two groups: parents alternate A, B, A, B…
        // so without the group term the layer would read A B A B A B.
        let (dag, _parents, children) = paired_layers(6);
        let mut groups = vec![None; dag.len()];
        for (i, &c) in children.iter().enumerate() {
            groups[c] = Some(if i % 2 == 0 { 10 } else { 20 });
        }

        let config = LayoutConfig::new();
        let plain = compute_sugiyama(&dag, &config).unwrap();
        assert!(
            !is_contiguous(&order_by_y(&plain, &children), &groups),
            "test is pointless unless the ungrouped layout interleaves"
        );

        let grouped = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();
        let order = order_by_y(&grouped, &children);
        assert!(is_contiguous(&order, &groups), "layer order: {order:?}");
    }

    #[test]
    fn group_contiguity_survives_repeated_passes() {
        // Contiguity must be a property of every pass, not just the first —
        // each pass re-sorts from the previous pass's positions.
        let mut dag: Dag<u32> = Dag::new();
        let roots: Vec<usize> = (0..6).map(|i| dag.add_node(i)).collect();
        let mid: Vec<usize> = (0..6).map(|i| dag.add_node(6 + i)).collect();
        let leaves: Vec<usize> = (0..6).map(|i| dag.add_node(12 + i)).collect();
        for i in 0..6 {
            dag.add_edge(roots[i], mid[i]).unwrap();
            // Cross-wire the last layer (a permutation, so every leaf keeps
            // exactly one parent) so later passes keep churning.
            dag.add_edge(mid[i], leaves[(i * 5 + 1) % 6]).unwrap();
        }
        let mut groups = vec![None; dag.len()];
        for i in 0..6 {
            groups[mid[i]] = Some(if i % 2 == 0 { 3 } else { 8 });
            groups[leaves[i]] = Some(if i < 3 { 3 } else { 8 });
        }

        for passes in 1..=6 {
            let mut config = LayoutConfig::new();
            config.crossing_minimization_passes = Some(passes);
            let result = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();
            for layer in [&mid, &leaves] {
                let order = order_by_y(&result, layer);
                assert!(
                    is_contiguous(&order, &groups),
                    "passes={passes} order={order:?}"
                );
            }
        }
    }

    #[test]
    fn ungrouped_node_can_sort_between_two_groups() {
        // Group A pulls to the top, group B to the bottom, and the ungrouped
        // node's own barycenter puts it in between — it must stay there rather
        // than being flushed to one end of the layer.
        let (dag, _parents, children) = paired_layers(5);
        let mut groups = vec![None; dag.len()];
        groups[children[0]] = Some(1);
        groups[children[1]] = Some(1);
        groups[children[3]] = Some(2);
        groups[children[4]] = Some(2);

        let config = LayoutConfig::new();
        let result = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();
        let order = order_by_y(&result, &children);

        assert!(is_contiguous(&order, &groups), "layer order: {order:?}");
        let ungrouped_at = order.iter().position(|&u| u == children[2]).unwrap();
        assert_eq!(
            ungrouped_at, 2,
            "ungrouped node should sit between the groups, order: {order:?}"
        );
    }

    #[test]
    fn dummy_inherits_group_only_within_one_group() {
        // 0 → 1 stays inside group 7; 2 → 3 leaves group 7 for group 9.
        // Both span three layers, so each gets two dummies.
        let fwd = vec![(0, 1), (2, 3)];
        let layers = vec![0, 3, 0, 3];
        let groups = vec![Some(7), Some(7), Some(7), Some(9)];
        let sg = insert_dummy_nodes(4, &layers, &fwd, &groups);

        assert_eq!(sg.groups.len(), 8);
        assert_eq!(&sg.groups[..4], &groups[..]);
        // Dummies of the intra-group edge inherit it …
        assert_eq!(sg.groups[4], Some(7));
        assert_eq!(sg.groups[5], Some(7));
        // … those of the edge that crosses groups belong to neither, so they
        // cannot split either group's run.
        assert_eq!(sg.groups[6], None);
        assert_eq!(sg.groups[7], None);
    }

    #[test]
    fn dummy_of_edge_from_ungrouped_node_is_ungrouped() {
        let fwd = vec![(0, 1)];
        let layers = vec![0, 2];
        let groups = vec![None, Some(4)];
        let sg = insert_dummy_nodes(2, &layers, &fwd, &groups);
        assert_eq!(sg.groups, vec![None, Some(4), None]);
    }

    #[test]
    fn grouping_does_not_increase_crossings_when_it_matches_structure() {
        // Two disconnected components, numbered so that neither is a
        // contiguous block to begin with.  The grouping says exactly what the
        // edges already say, so holding each group together costs nothing —
        // here it removes two crossings the plain barycenter pass leaves.
        let mut dag: Dag<u32> = Dag::new();
        for i in 0..8 {
            dag.add_node(i);
        }
        let edges = [(0, 3), (0, 2), (5, 7), (1, 3), (4, 6), (6, 7), (4, 5)];
        for &(u, v) in &edges {
            dag.add_edge(u, v).unwrap();
        }
        // Group = connected component.
        let groups: Vec<Option<u32>> = (0..8).map(|i| Some(if i < 4 { 0 } else { 1 })).collect();

        let config = LayoutConfig::new();
        let plain = compute_sugiyama(&dag, &config).unwrap();
        let grouped = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();

        let before = count_crossings(&plain, &edges, &config);
        let after = count_crossings(&grouped, &edges, &config);
        assert!(before > 0, "test is pointless without crossings to remove");
        assert!(
            after <= before,
            "grouping added crossings: {before} -> {after}"
        );

        // Every layer of the grouped layout keeps its groups in one run.
        let by_layer = |layer: f32| -> Vec<usize> {
            (0..8)
                .filter(|&u| grouped.position(u).unwrap().x == layer * config.level_separation)
                .collect()
        };
        for layer in 0..3 {
            let order = order_by_y(&grouped, &by_layer(layer as f32));
            assert!(is_contiguous(&order, &groups), "layer {layer}: {order:?}");
        }
    }

    #[test]
    fn grouped_layout_is_deterministic() {
        let (dag, _parents, children) = paired_layers(7);
        let mut groups = vec![None; dag.len()];
        for (i, &c) in children.iter().enumerate() {
            groups[c] = Some([900u32, 3, 900, 17, 3, 17, 900][i]);
        }
        let config = LayoutConfig::new();
        let first = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();
        for _ in 0..5 {
            let again = compute_sugiyama_grouped(&dag, &config, &groups).unwrap();
            for (a, b) in first.positions().iter().zip(again.positions()) {
                assert_eq!(a.x.to_bits(), b.x.to_bits());
                assert_eq!(a.y.to_bits(), b.y.to_bits());
            }
        }
        assert!(is_contiguous(&order_by_y(&first, &children), &groups));
    }
}
