use grid::{Dag, LayoutConfig, LayoutEngine};

// ---------------------------------------------------------------------------
// Helper: count edge crossings between two adjacent layers.
//
// Two edges (u1→v1) and (u2→v2) cross when u1 appears before u2 in their
// shared source layer but v1 appears after v2 in their shared target layer.
// We approximate this by using x-positions as layer proxies and y-positions
// as within-layer order.
// ---------------------------------------------------------------------------
fn count_crossings(layout: &grid::LayoutResult, edges: &[(usize, usize)]) -> usize {
    let mut crossings = 0;
    for i in 0..edges.len() {
        for j in (i + 1)..edges.len() {
            let (u1, v1) = edges[i];
            let (u2, v2) = edges[j];
            let pu1 = match layout.position(u1) {
                Some(p) => p,
                None => continue,
            };
            let pv1 = match layout.position(v1) {
                Some(p) => p,
                None => continue,
            };
            let pu2 = match layout.position(u2) {
                Some(p) => p,
                None => continue,
            };
            let pv2 = match layout.position(v2) {
                Some(p) => p,
                None => continue,
            };

            // Only count crossings for edges sharing the same source layer and target layer
            if (pu1.x - pu2.x).abs() < 0.1 && (pv1.x - pv2.x).abs() < 0.1 {
                // Edges cross when the relative y-order reverses between layers
                if (pu1.y < pu2.y) != (pv1.y < pv2.y) {
                    crossings += 1;
                }
            }
        }
    }
    crossings
}

// ---------------------------------------------------------------------------
// Test 1: Diamond DAG — no duplicate node positions
// ---------------------------------------------------------------------------
#[test]
fn diamond_dag_no_duplicate_nodes() {
    // root → A → C
    // root → B → C
    let mut dag: Dag<u32> = Dag::new();
    let root = dag.add_node(0);
    let a = dag.add_node(1);
    let b = dag.add_node(2);
    let c = dag.add_node(3);
    dag.add_edge(root, a).unwrap();
    dag.add_edge(root, b).unwrap();
    dag.add_edge(a, c).unwrap();
    dag.add_edge(b, c).unwrap();

    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();

    // Exactly 4 node positions — C is NOT duplicated
    assert_eq!(result.positions().len(), 4);

    let c_pos = result.position(c).unwrap();
    let duplicates = result.positions().iter().filter(|&&p| p == c_pos).count();
    assert_eq!(duplicates, 1, "C must appear at exactly one position");
}

// ---------------------------------------------------------------------------
// Test 2: Cycle handled without panic, back-edge goes to cross_edges
// ---------------------------------------------------------------------------
#[test]
fn cycle_is_handled_without_panic() {
    let mut dag: Dag<u32> = Dag::new();
    let a = dag.add_node(0);
    let b = dag.add_node(1);
    dag.add_edge(a, b).unwrap();
    dag.add_edge(b, a).unwrap(); // back-edge

    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();

    assert_eq!(result.positions().len(), 2);
    // One of the two edges is classified as a back-edge
    assert_eq!(result.cross_edges().len(), 1);
    // One forward edge is routed normally
    assert_eq!(result.edges().len(), 1);
}

// ---------------------------------------------------------------------------
// Test 3: Layer assignment is correct for diamond
// ---------------------------------------------------------------------------
#[test]
fn diamond_layer_assignment_is_correct() {
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
    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();
    let level_sep = config.level_separation;

    // layer 0 → x = 0.0
    assert_eq!(result.position(root).unwrap().x, 0.0);
    // layer 1 → x = level_sep
    assert_eq!(result.position(a).unwrap().x, level_sep);
    assert_eq!(result.position(b).unwrap().x, level_sep);
    // layer 2 → x = 2 * level_sep
    assert_eq!(result.position(c).unwrap().x, 2.0 * level_sep);
}

// ---------------------------------------------------------------------------
// Test 4: Crossing minimization does not increase crossings
// ---------------------------------------------------------------------------
#[test]
fn crossing_minimization_does_not_increase_crossings() {
    // Graph with a known crossing opportunity:
    // Layer 0: root
    // Layer 1: A(1), B(2), C(3)  — added in reverse order to create initial crossings
    // Layer 2: D(4), E(5)
    // Edges: root→A, root→B, root→C, A→E, B→D, C→D
    // Initial order (by add_node): A=1, B=2, C=3, D=4, E=5
    // A→E and B→D cross if A is above B in layer 1 but E is below D in layer 2.
    let mut dag: Dag<u32> = Dag::new();
    let root = dag.add_node(0); // layer 0
    let a = dag.add_node(1); // layer 1
    let b = dag.add_node(2); // layer 1
    let c = dag.add_node(3); // layer 1
    let d = dag.add_node(4); // layer 2
    let e = dag.add_node(5); // layer 2
    dag.add_edge(root, a).unwrap();
    dag.add_edge(root, b).unwrap();
    dag.add_edge(root, c).unwrap();
    dag.add_edge(a, e).unwrap();
    dag.add_edge(b, d).unwrap();
    dag.add_edge(c, d).unwrap();

    // Compute with 0 passes (no minimization)
    let mut config_before = LayoutConfig::new();
    config_before.crossing_minimization_passes = Some(0);
    let mut engine_before = LayoutEngine::with_config(config_before).unwrap();
    let result_before = engine_before.compute_dag(&dag).unwrap();

    // Compute with default passes (minimization enabled)
    let config_after = LayoutConfig::new();
    let mut engine_after = LayoutEngine::with_config(config_after).unwrap();
    let result_after = engine_after.compute_dag(&dag).unwrap();

    let real_edges = vec![(root, a), (root, b), (root, c), (a, e), (b, d), (c, d)];
    let crossings_before = count_crossings(&result_before, &real_edges);
    let crossings_after = count_crossings(&result_after, &real_edges);

    assert!(
        crossings_after <= crossings_before,
        "Crossings after ({crossings_after}) should not exceed crossings before ({crossings_before})"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Single-node graph — no panic, exactly one position, no edges
// ---------------------------------------------------------------------------
#[test]
fn single_node_no_panic() {
    let mut dag: Dag<&str> = Dag::new();
    dag.add_node("only");

    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();

    assert_eq!(result.positions().len(), 1);
    assert!(result.edges().is_empty());
    assert!(result.cross_edges().is_empty());

    let pos = result.position(0).unwrap();
    assert_eq!(pos.x, 0.0);
    assert_eq!(pos.y, 0.0);
}

// ---------------------------------------------------------------------------
// Extra: long-span edge produces a single EdgePath (not one per dummy hop)
// ---------------------------------------------------------------------------
#[test]
fn long_span_edge_produces_single_edge_path() {
    // 0 → 1 → 2 → 3 but also direct 0 → 3 (spans 3 layers)
    let mut dag: Dag<u32> = Dag::new();
    for i in 0..4 {
        dag.add_node(i);
    }
    dag.add_edge(0, 1).unwrap();
    dag.add_edge(1, 2).unwrap();
    dag.add_edge(2, 3).unwrap();
    dag.add_edge(0, 3).unwrap(); // long-span: 2 dummy nodes needed

    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();

    // 4 real nodes
    assert_eq!(result.positions().len(), 4);

    // The long-span edge (0→3) should be exactly one EdgePath
    let long_paths: Vec<_> = result
        .edges()
        .iter()
        .filter(|e| e.parent_id == 0 && e.child_id == 3)
        .collect();
    assert_eq!(long_paths.len(), 1);
}

// ---------------------------------------------------------------------------
// Extra: disconnected graph (two components) — both roots at layer 0
// ---------------------------------------------------------------------------
#[test]
fn disconnected_graph_both_roots_at_layer_zero() {
    // Component 1: 0 → 1
    // Component 2: 2 → 3
    let mut dag: Dag<u32> = Dag::new();
    for i in 0..4 {
        dag.add_node(i);
    }
    dag.add_edge(0, 1).unwrap();
    dag.add_edge(2, 3).unwrap();

    let mut engine = LayoutEngine::new();
    let result = engine.compute_dag(&dag).unwrap();

    assert_eq!(result.positions().len(), 4);

    let level_sep = LayoutConfig::new().level_separation;
    // Roots (0 and 2) at layer 0
    assert_eq!(result.position(0).unwrap().x, 0.0);
    assert_eq!(result.position(2).unwrap().x, 0.0);
    // Children (1 and 3) at layer 1
    assert_eq!(result.position(1).unwrap().x, level_sep);
    assert_eq!(result.position(3).unwrap().x, level_sep);
}
