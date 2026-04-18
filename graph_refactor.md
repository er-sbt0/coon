# Graph Layout Refactor: Reingold-Tilford → Sugiyama

## Background

The current graph view uses a **Reingold-Tilford (1981)** tree layout algorithm implemented in the `grid` crate. Because RT requires a pure tree, `CallGraphAdapter` converts the call graph (a DAG) into a tree via BFS — **duplicating nodes** when a function is reachable via multiple paths.

### Current Limitations

| Problem | Root Cause |
|---|---|
| Node duplication | RT requires a tree; same `SymbolId` can appear at N tree nodes |
| Lost shared-callee information | `symbol_to_node` tracks first occurrence only |
| No edge crossing minimization | Orthogonal H-V-H routing, no crossing heuristics |
| Cycles are dropped silently | Ancestor-path check skips back-edges entirely |
| `subtree_separation` unused | Defined in `LayoutConfig` but never applied in algorithm |

### Key Files

| File | Role |
|---|---|
| `crates/grid/src/engine.rs` | Reingold-Tilford implementation — **replace** |
| `crates/grid/src/tree.rs` | Arena tree structure — **replace with `Dag<T>`** |
| `crates/grid/src/result.rs` | `LayoutResult`, `Viewport`, `EdgePath` — **keep** |
| `crates/grid/src/edge_router.rs` | Orthogonal H-V-H routing — **keep** |
| `crates/grid/src/rendering.rs` | Ratatui rendering — **keep, extend** |
| `crates/tui/src/graph_adapter.rs` | DAG→tree BFS with node duplication — **replace** |
| `crates/tui/src/graph_view.rs` | State + rendering widget — **adapt** |
| `crates/model/src/graph.rs` | `CallGraph` source of truth — **untouched** |

---

## Target: Sugiyama Layered DAG Layout

The **Sugiyama framework** is the industry standard for directed graph layout (used by Graphviz `dot`, ELK, dagre). It handles true DAGs — no node duplication, explicit crossing minimization, proper layer hierarchy.

### Four-Phase Pipeline

1. **Cycle removal** — reverse a minimal set of back-edges to make the input acyclic
2. **Layer assignment** — assign each node an integer rank (depth from root)
3. **Crossing minimization** — order nodes within each layer to minimize edge crossings
4. **Coordinate assignment** — compute final x/y positions with compaction

---

## Implementation Phases

### Dependency Order

```
Phase 1: Dag<T> data structure
  └── Phase 2: Sugiyama engine  (uses Dag<T> + petgraph)
        └── Phase 3: Adapter + GraphView switch  (calls compute_dag)
              └── Phase 4: Cleanup + back-edge rendering + tests
```

Each phase boundary leaves the app compilable and runnable.

---

## Phase 1: `Dag<T>` Data Structure

**Complexity:** Simple (~2h)
**App state after:** Fully functional, RT path unchanged. `Dag<T>` exported but unused by `tui`.

### Goal

Add a DAG-aware arena node structure to the `grid` crate alongside the existing `Tree<T>`. Nothing is removed; the RT path continues working.

### Files Changed

| File | Change |
|---|---|
| `crates/grid/src/dag.rs` | New file |
| `crates/grid/src/lib.rs` | `mod dag; pub use dag::{Dag, DagNode};` |
| `Cargo.toml` (workspace) | Add `petgraph = "0.6"` to `[workspace.dependencies]` |
| `crates/grid/Cargo.toml` | Add `petgraph.workspace = true` |

### Implementation

Define `DagNode<T>` as an arena-allocated node:

```rust
pub struct DagNode<T> {
    pub data: T,
    pub successors: Vec<usize>,
    pub predecessors: Vec<usize>,
}
```

Define `Dag<T>` with the same arena-Vec pattern as `Tree<T>`:

```rust
pub struct Dag<T> {
    nodes: Vec<DagNode<T>>,
    version: u64,
}
```

Public API — mirrors `Tree<T>` where possible:

```rust
impl<T> Dag<T> {
    pub fn new() -> Self
    pub fn add_node(&mut self, data: T) -> usize
    pub fn add_edge(&mut self, from: usize, to: usize) -> Result<(), LayoutError>
    pub fn node(&self, id: usize) -> Result<&DagNode<T>, LayoutError>
    pub fn node_mut(&mut self, id: usize) -> Result<&mut DagNode<T>, LayoutError>
    pub fn nodes(&self) -> &[DagNode<T>]
    pub fn len(&self) -> usize
    pub fn version(&self) -> u64
    pub fn edges(&self) -> impl Iterator<Item = (usize, usize)> + '_
    pub fn roots(&self) -> Vec<usize>   // nodes with no predecessors
}
```

- `add_edge` records in both `successors` and `predecessors`. No cycle detection here — cycles are handled by the Sugiyama engine in Phase 2.
- Do **not** use `petgraph` as the internal representation for `Dag<T>`. Use the arena-Vec approach consistent with `Tree<T>`. `petgraph` is used only in Phase 2 for algorithm primitives (SCC, toposort).

### What to Preserve

`Tree<T>`, `LayoutEngine`, `engine.rs` — entirely untouched.

---

## Phase 2: Sugiyama Layout Engine

**Complexity:** Complex (~2–3 days)
**App state after:** `compute_dag` exists alongside `compute` for `Tree<T>`. TUI still calls the old path.

### Goal

Implement the complete four-phase Sugiyama algorithm as `LayoutEngine::compute_dag<T>()`. The existing `LayoutResult`, `Viewport`, `EdgePath`, `rendering.rs`, and `edge_router.rs` require zero changes — they are output-format compatible.

### Files Changed

| File | Change |
|---|---|
| `crates/grid/src/sugiyama.rs` | New file — algorithm implementation |
| `crates/grid/src/engine.rs` | Add `compute_dag<T>(&mut self, dag: &Dag<T>) -> Result<LayoutResult, LayoutError>` |
| `crates/grid/src/lib.rs` | `mod sugiyama;` (private); re-export `compute_dag` via `LayoutEngine` |

### Internal Structure: `SugiyamaGraph`

An internal expanded graph used during phases 2c–2d. Not exposed publicly.

```rust
struct SugiyamaGraph {
    layers: Vec<Vec<usize>>,       // layer index → ordered node indices
    positions: Vec<Option<usize>>, // node index → position within its layer
    is_dummy: Vec<bool>,           // node index → is dummy node
    edges: Vec<(usize, usize)>,    // all edges (real + dummy-chain)
    original_edge: Vec<(usize, usize)>, // dummy-chain edge → original (u, v)
}
```

**Index space convention:** real nodes occupy `0..dag.len()`; dummy nodes occupy `dag.len()..`. This makes slicing `positions[..dag.len()]` safe and unambiguous.

### Sub-step 2a: Cycle Removal

Call graphs can contain mutual recursion (e.g. `a → b → a`). Sugiyama requires a DAG.

```rust
fn remove_cycles(dag: &Dag<impl Sized>) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    // Returns: (forward_edges, back_edges)
    // 1. Build a petgraph::graph::DiGraph from dag.edges()
    // 2. Run petgraph::algo::tarjan_scc to find SCCs
    // 3. For each SCC with >1 node:
    //    - find a spanning tree via DFS within the SCC
    //    - edges not in the spanning tree pointing "backwards" → back_edges
    // 4. Return remaining edges as forward_edges
}
```

Back-edges are stored and later inserted into `LayoutResult::cross_edges()` after coordinate assignment. They are rendered with a distinct style (Phase 4).

### Sub-step 2b: Layer Assignment

```rust
fn assign_layers(n: usize, forward_edges: &[(usize, usize)]) -> Vec<usize> {
    // 1. Build temporary petgraph::DiGraph from forward_edges
    // 2. petgraph::algo::toposort to get topological order
    // 3. Longest-path rank: layer[u] = max(layer[pred] for pred in predecessors) + 1
    //    Roots (no predecessors) get layer 0
    // 4. Returns Vec<usize> of length n
}
```

A node's layer equals the length of the longest path from any root to it. This ensures direct callers always appear in a layer strictly before callees.

### Sub-step 2c: Dummy Node Insertion

For each edge `(u, v)` where `layer[v] > layer[u] + 1` (long-span edge), insert `layer[v] - layer[u] - 1` dummy nodes in the intermediate layers.

```rust
fn insert_dummy_nodes(
    dag_len: usize,
    layers: &[usize],
    forward_edges: &[(usize, usize)],
) -> SugiyamaGraph {
    // For each edge (u, v):
    //   if layer[v] == layer[u] + 1: add edge directly
    //   else: create chain u → d1 → d2 → ... → v
    //         where d_i are new dummy nodes appended past dag_len
    //         record original_edge[each segment] = (u, v)
}
```

The original `Dag<T>` is not mutated. `SugiyamaGraph` owns the expanded node/edge list.

### Sub-step 2d: Crossing Minimization

Barycenter heuristic — 2 alternating passes (top-down, then bottom-up):

```rust
fn minimize_crossings(g: &mut SugiyamaGraph, passes: usize) {
    for _ in 0..passes {
        // Top-down sweep: for each layer L from 1..max
        //   For each node u in layer L:
        //     barycenter = mean(position_in_layer[L-1] of each predecessor of u)
        //   Sort nodes in layer L by barycenter
        //   Update positions[u] for all u in L

        // Bottom-up sweep: same but use successors and sweep from max..1
    }
}
```

Two passes handle most practical call graph cases. A `crossing_minimization_passes` field can be added to `LayoutConfig` for tuning.

### Sub-step 2e: Coordinate Assignment

```rust
fn assign_coordinates(g: &SugiyamaGraph, config: &LayoutConfig) -> Vec<Position> {
    // x = layer_index * config.level_separation
    // y for each node in a layer:
    //   evenly spaced by config.node_separation
    //   layer block centered around y = 0 for balanced look
    // Positions are centers — consistent with existing Position semantics in types.rs
    // Returns Vec<Position> of length = total nodes including dummies
}
```

### Sub-step 2f: Edge Routing

After coordinate assignment, route edges using the existing `edge_router::compute_orthogonal_routing`.

For edges with dummy-node chains: gather all intermediate waypoints by traversing the dummy chain, then produce a **single** `EdgePath` per original `(u, v)` edge.

```rust
fn route_edges(
    g: &SugiyamaGraph,
    positions: &[Position],
    dag_len: usize,
) -> Vec<EdgePath> {
    // For each unique original_edge (u, v):
    //   collect waypoints: positions[u], positions[d1], ..., positions[v]
    //   produce one EdgePath with those waypoints
    //   (reuse edge_router for segment routing within each hop)
}
```

### Putting It Together: `LayoutEngine::compute_dag`

```rust
impl LayoutEngine {
    pub fn compute_dag<T>(&mut self, dag: &Dag<T>) -> Result<LayoutResult, LayoutError> {
        let (forward_edges, back_edges) = remove_cycles(dag);
        let layers = assign_layers(dag.len(), &forward_edges);
        let mut sg = insert_dummy_nodes(dag.len(), &layers, &forward_edges);
        minimize_crossings(&mut sg, self.config.crossing_minimization_passes.unwrap_or(2));
        let all_positions = assign_coordinates(&sg, &self.config);

        // Slice to real nodes only
        let node_positions = all_positions[..dag.len()].to_vec();
        let bounds = LayoutBounds::from_positions(&node_positions);

        let mut result = LayoutResult::new(node_positions, bounds, dag.version());

        // Route forward edges
        let routed = route_edges(&sg, &all_positions, dag.len());
        for edge in routed {
            result.push_edge(edge);
        }

        // Route back-edges as cross_edges
        for (u, v) in &back_edges {
            let path = compute_orthogonal_routing(*u, *v, all_positions[*u], all_positions[*v]);
            result.add_cross_edge(path);
        }

        Ok(result)
    }
}
```

### What to Preserve

- `LayoutEngine::compute(&Tree<T>)` — RT path not removed; both methods coexist
- `LayoutResult`, `Viewport`, `ViewportLayout`, `EdgePath`, `EdgeSegment` — zero changes
- `rendering.rs`, `edge_router.rs` — zero changes
- `LayoutConfig` — reused as-is; `level_separation` → x-spacing between layers; `node_separation` → y-spacing within a layer

### Risks

| Risk | Mitigation |
|---|---|
| Dummy-node index off-by-one in position slicing | Enforce convention: real nodes `0..dag.len()`, dummies `dag.len()..`. Slice before building `LayoutResult`. |
| Dummy-chain edge routing produces N EdgePaths instead of 1 | Track `original_edge` for every segment in `SugiyamaGraph`. Group by `(u, v)` before routing. |
| Barycenter gives poor layout for dense subgraphs | Cap at 3 passes max; expose `crossing_minimization_passes` in `LayoutConfig`. |

---

## Phase 3: Adapter + GraphView Switch

**Complexity:** Medium (~4h)
**App state after:** TUI calls `compute_dag`. RT code still compiles, just no longer called.

### Goal

Eliminate node duplication. `CallGraphAdapter::build_tree` is replaced by `build_dag`. `GraphViewState` fields switch from `Tree` to `Dag`.

### Files Changed

| File | Change |
|---|---|
| `crates/tui/src/graph_adapter.rs` | Replace `build_tree` with `build_dag` |
| `crates/tui/src/graph_view.rs` | Update state fields + layout + navigation + render call |

### `graph_adapter.rs`: Replace `build_tree` with `build_dag`

The core fix: when a `SymbolId` is already in `symbol_to_node`, add only an edge — no duplicate node.

```rust
pub fn build_dag(
    &mut self,
    graph: &CallGraph,
    root: &SymbolId,
    direction: CallDirection,
    max_depth: Option<usize>,
) -> Result<Dag<SymbolId>, LayoutError> {
    self.symbol_to_node.clear();
    self.node_to_symbol.clear();

    let mut dag = Dag::new();
    let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::new();

    let root_idx = dag.add_node(*root);
    self.symbol_to_node.insert(*root, root_idx);
    self.node_to_symbol.insert(root_idx, *root);
    queue.push_back((*root, 0));

    while let Some((symbol, depth)) = queue.pop_front() {
        if max_depth.map_or(false, |m| depth >= m) { continue; }
        let parent_idx = self.symbol_to_node[&symbol];

        for child_func in self.get_children(graph, &symbol, direction) {
            let child_idx = if let Some(&idx) = self.symbol_to_node.get(&child_func.id) {
                // Already in DAG: add edge only, no new node
                idx
            } else {
                let idx = dag.add_node(child_func.id);
                self.symbol_to_node.insert(child_func.id, idx);
                self.node_to_symbol.insert(idx, child_func.id);
                queue.push_back((child_func.id, depth + 1));
                idx
            };
            dag.add_edge(parent_idx, child_idx)?;
        }
    }
    Ok(dag)
}
```

**Remove:** `is_on_ancestor_path`, `parent_map`, ancestor-path BFS — these existed solely to prevent infinite loops during tree construction.

**Keep:** `symbol_to_node`, `node_to_symbol`, `get_children`, `get_symbol`, `get_node_index`, `CallDirection`.

After this change, `symbol_to_node` is a true 1:1 map — no "first-occurrence only" semantics needed.

### `graph_view.rs`: Update State and Layout

State field change:

```rust
// Before:
pub tree: Option<Tree<SymbolId>>,

// After:
pub dag: Option<Dag<SymbolId>>,
```

`update_layout` changes:

```rust
// Before:
let tree = self.adapter.build_tree(graph, root, self.direction, self.max_depth)?;
let layout = self.engine.compute_cached(&tree)?;

// After:
let dag = self.adapter.build_dag(graph, root, self.direction, self.max_depth)?;
let layout = self.engine.compute_dag(&dag)?;
self.dag = Some(dag);
```

`tree_version` field continues to work — `Dag::version()` uses the same pattern as `Tree::version()`.

### `graph_view.rs`: Update Navigation

Navigation methods currently scan all nodes O(N) to find parent. With `Dag<T>`, `predecessors` gives O(1) access:

| Method | Before | After |
|---|---|---|
| `navigate_to_parent` | Scan all nodes for one containing selected as child | `dag.node(idx)?.predecessors.first()` |
| `navigate_to_child` | `tree.node(idx)?.children[mid]` | `dag.node(idx)?.successors[mid]` |
| `navigate_next_sibling` | Find parent by scan, then next in children | `predecessors.first()` → `successors.iter().position(idx)` + wrap |
| `navigate_prev_sibling` | Same scan pattern | Same as above, wrapping other direction |

### `graph_view.rs`: Update Render Call

The `_tree` parameter in `render_tree_edges` was unused (note the `_` prefix). The render call simplifies:

```rust
// Before:
grid::render_tree_edges(buf, tree, layout, &state.viewport, inner_area, style);

// After:
grid::render_dag_edges(buf, layout, &state.viewport, inner_area, style);
// Or inline: iterate layout.edges() + layout.cross_edges() → render_edge_path(...)
```

Add `render_dag_edges` to `rendering.rs` as a thin wrapper over `layout.edges()` iteration.

### What to Preserve

- `CallDirection` enum — unchanged
- `get_children` helper — unchanged
- `GraphView` node rendering (`render_node`) — unchanged
- `Viewport`, pan/recenter logic — unchanged

### Risks

| Risk | Mitigation |
|---|---|
| Call sites depending on old "first-occurrence" `symbol_to_node` semantics | Grep all usages of `symbol_to_node` before removing the `or_insert` guard |
| Navigation methods break if `predecessors` is empty for root | Guard with `if predecessors.is_empty() { return; }` — same guard currently exists for tree root |

---

## Phase 4: Cleanup, Back-Edge Rendering, Tests

**Complexity:** Medium (~4h)
**App state after:** Full Sugiyama layout, visual back-edge distinction, RT code removable.

### Goal

Visually distinguish reversed back-edges (cycles), remove dead RT code, add regression tests.

### Files Changed

| File | Change |
|---|---|
| `crates/grid/src/rendering.rs` | Add `render_back_edges()` |
| `crates/tui/src/graph_view.rs` | Call `render_back_edges` after `render_dag_edges` |
| `crates/grid/tests/dag_layout_tests.rs` | New integration tests |
| `crates/grid/src/lib.rs` | Export `render_back_edges`; deprecate `render_tree_edges` |
| `crates/grid/src/tree.rs` | Mark deprecated or remove |
| `crates/grid/src/engine.rs` | Remove or deprecate `compute<T>` for `Tree<T>` |

### Back-Edge Rendering

In `rendering.rs`:

```rust
pub fn render_back_edges(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    for edge in layout.cross_edges() {
        render_edge_path(buf, edge, viewport, area, style);
    }
}
```

In `graph_view.rs`, after the existing edge render call:

```rust
grid::render_back_edges(
    buf,
    layout,
    &state.viewport,
    inner_area,
    Style::default().fg(Color::Red),
);
```

Red coloring signals back-edges (cyclic calls) visually without requiring arrowhead glyphs.

### Tests (`crates/grid/tests/dag_layout_tests.rs`)

```
diamond DAG produces no duplicate nodes
  Input:  root → A → C, root → B → C
  Assert: LayoutResult has exactly 4 node positions (root, A, B, C)
          C appears at exactly one position

cycle is handled without panic
  Input:  a → b → a
  Assert: no panic; exactly 2 node positions
          layout.cross_edges() has exactly 1 entry (the back-edge)

layer assignment is correct for diamond
  Input:  root → A → C, root → B → C
  Assert: layer[root] = 0, layer[A] = layer[B] = 1, layer[C] = 2

crossing minimization does not increase crossings
  Input:  10-node, 15-edge random DAG (fixed seed)
  Assert: crossing_count_after <= crossing_count_before

single-node graph does not panic
  Input:  single node, no edges
  Assert: exactly 1 position, no edges, no cross_edges
```

### Risks

| Risk | Mitigation |
|---|---|
| Back-edge direction: reversed edge has `parent` = original callee | Store `is_back_edge: bool` in `EdgePath` if clearer visual direction needed |
| Removing `Tree<T>` breaks existing example code | Update examples before removal; or leave `tree.rs` as deprecated but compilable |
| Crossing count test is flaky on different graph shapes | Use a fixed-seed, known-topology graph rather than random |

---

## Integration Points Summary

| Crate Boundary | Before | After |
|---|---|---|
| `tui::graph_adapter` → `grid` | `use grid::Tree` | `use grid::Dag` |
| `tui::graph_view` → `grid::engine` | `engine.compute_cached(&tree)` | `engine.compute_dag(&dag)` |
| `tui::graph_view` → `grid::rendering` | `render_tree_edges(buf, tree, ...)` | `render_dag_edges(buf, layout, ...)` |
| `grid::engine` → `petgraph` | none | Tarjan SCC + toposort for phases 2a–2b |
| `model::CallGraph` | source of truth | unchanged |

The `model` crate is entirely unaffected throughout all phases.

---

## Effort Summary

| Phase | Description | Effort |
|---|---|---|
| 1 | `Dag<T>` arena data structure | ~2h |
| 2 | Sugiyama engine (4 sub-phases + dummy chains + edge routing) | ~2–3 days |
| 3 | Adapter BFS rewrite + GraphView wiring | ~4h |
| 4 | Cleanup + back-edge rendering + tests | ~4h |

**Total estimated:** ~4–5 days of focused implementation.

The dominant cost is Phase 2. The two highest-risk implementation details are:
1. **Dummy-node index space** — real nodes `0..N`, dummies `N..`. Slicing `positions[..N]` before building `LayoutResult` must be done consistently.
2. **Dummy-chain collapse** — each original edge `(u, v)` must produce exactly one `EdgePath`, collecting all intermediate dummy waypoints, not one `EdgePath` per dummy-to-dummy hop.
