use super::*;
use model::{FunctionNode, Location};

mod invariants;
mod known_answers;
mod oracle;

/// Distinct lines keep the generated `SymbolId`s distinct.
fn add(graph: &mut CallGraph, name: &str, line: u32) -> SymbolId {
    add_in(graph, name, "src/x.c", line)
}

/// The same, for the tie-break tests, which need two files to compare.
fn add_in(graph: &mut CallGraph, name: &str, file: &str, line: u32) -> SymbolId {
    graph.add_function(FunctionNode::new(
        name.to_string(),
        name.to_string(),
        Location::new(file.to_string(), line, 0),
    ))
}

fn call(graph: &mut CallGraph, from: SymbolId, to: SymbolId, line: u32) {
    graph.add_call(from, to, Location::new("src/x.c".to_string(), line, 0));
}

/// `times` separate call sites, so the edge weighs `times`.
fn call_often(graph: &mut CallGraph, from: SymbolId, to: SymbolId, base: u32, times: u32) {
    for offset in 0..times {
        call(graph, from, to, base + offset);
    }
}

fn named(graph: &CallGraph, symbol: &SymbolId) -> String {
    graph
        .get_function(symbol)
        .map(|f| f.name.clone())
        .unwrap_or_default()
}

/// The members of every cluster at one level, as names, so a test can state
/// the grouping it expects without knowing any ids.
fn grouping_at(graph: &CallGraph, clustering: &Clustering, level: usize) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = clustering
        .level(level)
        .iter()
        .filter_map(|id| clustering.cluster(*id))
        .map(|cluster| {
            let mut names: Vec<String> = cluster.members.iter().map(|m| named(graph, m)).collect();
            names.sort();
            names
        })
        .collect();
    groups.sort();
    groups
}

fn fingerprint(graph: &CallGraph, clustering: &Clustering) -> String {
    (0..clustering.level_count())
        .map(|level| format!("{:?}", grouping_at(graph, clustering, level)))
        .collect::<Vec<_>>()
        .join("|")
}

// -- the objective --------------------------------------------------------

/// How one pass at `resolution` groups a graph, as names, so a test can
/// state the grouping it expects. Driven directly rather than through
/// `Clustering::compute`, which stops before the first pass on a graph this
/// small.
fn one_pass(graph: &CallGraph, resolution: f64) -> Vec<Vec<String>> {
    partition_names(graph, |base| louvain_pass(base, resolution))
}

/// How one MCL pass at `inflation` groups a graph, as names.
fn one_flow_pass(graph: &CallGraph, inflation: f64) -> Vec<Vec<String>> {
    partition_names(graph, |base| markov_pass(base, inflation))
}

/// The boxes one `partition` of the base graph makes, as sorted names.
fn partition_names(
    graph: &CallGraph,
    partition: impl FnOnce(&WeightedGraph) -> (Vec<usize>, usize),
) -> Vec<Vec<String>> {
    let base = build_base_graph(graph, None);
    let (groups, count) = partition(&base.graph);

    let mut buckets = vec![Vec::new(); count];
    for (node, &unit) in base.units.iter().enumerate() {
        buckets[groups[unit]].push(named(graph, &base.symbols[node]));
    }
    for bucket in &mut buckets {
        bucket.sort();
    }
    buckets.sort();
    buckets
}

/// The property the method rests on: a node joins the neighbour it shares
/// the most calls with, not the first one it meets.
#[test]
fn a_node_joins_the_neighbour_it_calls_most() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let light = add(&mut graph, "light", 2);
    let heavy = add(&mut graph, "heavy", 3);
    // `a` touches both, but calls `heavy` five times and `light` once.
    call(&mut graph, a, light, 10);
    call_often(&mut graph, a, heavy, 20, 5);
    let other = add(&mut graph, "other", 4);
    call(&mut graph, light, other, 30);
    call(&mut graph, heavy, other, 40);

    assert_eq!(
        one_pass(&graph, 1.0),
        vec![vec!["a", "heavy"], vec!["light", "other"]],
        "a should join the neighbour it calls five times"
    );
}

/// The fault the previous method had, stated as a test: a utility that
/// three unrelated pairs each call once must not pull the three pairs into
/// one box. Under the null model a call to something everything calls is
/// expected, and expected calls do not pay for a merge.
#[test]
fn a_shared_utility_does_not_glue_its_callers_together() {
    let mut graph = CallGraph::new();
    let utility = add(&mut graph, "utility", 1);
    let mut pairs = Vec::new();
    for i in 0..3u32 {
        let caller = add(&mut graph, &format!("caller{i}"), 10 + i * 2);
        let helper = add(&mut graph, &format!("helper{i}"), 11 + i * 2);
        call_often(&mut graph, caller, helper, 100 + i * 10, 3);
        call(&mut graph, caller, utility, 200 + i);
        pairs.push((format!("caller{i}"), format!("helper{i}")));
    }

    let groups = one_pass(&graph, 1.0);
    for (caller, helper) in &pairs {
        let holding: Vec<&Vec<String>> = groups
            .iter()
            .filter(|g| g.contains(caller) || g.contains(helper))
            .collect();
        assert_eq!(holding.len(), 1, "{caller} and {helper} belong together");
        let others = holding[0]
            .iter()
            .filter(|name| name.starts_with("caller") || name.starts_with("helper"))
            .filter(|name| *name != caller && *name != helper)
            .count();
        assert_eq!(
            others, 0,
            "the utility must not glue another pair onto {caller}: {groups:?}"
        );
    }
}

/// A sweep can undo a grouping a previous sweep made: a node that no longer
/// fits the community it is in leaves it. That is what the method has and
/// pair matching had not.
#[test]
fn a_node_leaves_a_community_it_no_longer_fits() {
    // Index order sweeps `a` first, when only `b` is there to join; then
    // `b`, `c`, `d` form the group `a` actually belongs to. The second sweep
    // has to move `a` across.
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    let d = add(&mut graph, "d", 4);
    let e = add(&mut graph, "e", 5);
    call(&mut graph, a, b, 10);
    call_often(&mut graph, a, c, 20, 2);
    call_often(&mut graph, a, d, 30, 2);
    call_often(&mut graph, c, d, 40, 3);
    call_often(&mut graph, b, e, 50, 4);

    assert_eq!(
        one_pass(&graph, 1.0),
        vec![vec!["a", "c", "d"], vec!["b", "e"]],
    );
}

/// Resolution is the grain: what does not pay at 8 pays at 1.
#[test]
fn a_finer_resolution_merges_less() {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = (0..8)
        .map(|i| add(&mut graph, &format!("f{i}"), i + 1))
        .collect();
    // Two tight quads, joined by one call.
    for quad in [&ids[..4], &ids[4..]] {
        for (i, from) in quad.iter().enumerate() {
            for to in &quad[i + 1..] {
                call_often(&mut graph, *from, *to, 100 + i as u32 * 10, 2);
            }
        }
    }
    call(&mut graph, ids[3], ids[4], 500);

    let fine = one_pass(&graph, 8.0);
    let plain = one_pass(&graph, 1.0);
    assert!(
        fine.len() > plain.len(),
        "resolution 8 should cut finer than 1: {fine:?} vs {plain:?}"
    );
    assert_eq!(plain.len(), 2, "plain modularity finds the two quads");
}

/// Merging sums the weights between merged nodes, which is what lets many
/// light calls add up to one heavy edge a later pass will follow.
#[test]
fn weights_between_merged_nodes_are_summed() {
    let mut adj = vec![HashMap::new(); 4];
    // Two pairs, joined by two separate weight-1 edges.
    adj[0].insert(1, 3.0);
    adj[1].insert(0, 3.0);
    adj[2].insert(3, 3.0);
    adj[3].insert(2, 3.0);
    for (left, right) in [(0usize, 2usize), (1, 3)] {
        adj[left].insert(right, 1.0);
        adj[right].insert(left, 1.0);
    }
    // Directed 0 -> 1, 2 -> 3, 0 -> 2, 1 -> 3: a DAG whose pairs can merge.
    let graph = WeightedGraph::new(
        adj,
        vec![0.0; 4],
        vec![vec![1, 2], vec![3], vec![3], vec![]],
    );

    let (groups, count) = louvain_pass(&graph, 1.0);
    assert_eq!(count, 2, "two pairs should merge into two nodes");

    let coarse = aggregate(&graph, &groups, count);
    let weight = coarse.adj[0]
        .iter()
        .find(|(other, _)| *other == 1)
        .map(|(_, w)| *w);
    assert_eq!(
        weight,
        Some(2.0),
        "the two weight-1 edges should arrive as one weight-2 edge"
    );
    assert_eq!(coarse.self_loops[0], 3.0, "the merged edge turns inward");
}
// -- the ladder ---------------------------------------------------------

/// A chain long enough to need several rounds: each rung must be strictly
/// coarser than the one below, and the top no wider than the target.
#[test]
fn the_ladder_narrows_to_the_target_width() {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = (0..200)
        .map(|i| add(&mut graph, &format!("f{i}"), i + 1))
        .collect();
    for (i, pair) in ids.windows(2).enumerate() {
        call(&mut graph, pair[0], pair[1], 1000 + i as u32);
    }

    for &method in ClusterMethod::ALL {
        let clustering = Clustering::compute(&graph, method);
        assert!(
            clustering.level_count() > 1,
            "{method:?}: 200 functions should take more than one round"
        );

        let widths: Vec<usize> = (0..clustering.level_count())
            .map(|level| clustering.level(level).len())
            .collect();
        for pair in widths.windows(2) {
            assert!(
                pair[1] < pair[0],
                "{method:?}: every rung must be coarser than the one below: {widths:?}"
            );
        }
        assert!(
            *widths.last().unwrap() <= TARGET_TOP_CLUSTERS,
            "{method:?}: the top should be at most {TARGET_TOP_CLUSTERS} boxes wide: {widths:?}"
        );
    }
}

// -- where the map opens -------------------------------------------------

/// When plain modularity already fits the target there is nothing coarser
/// to build, so the opening rung is the top.
#[test]
fn a_ladder_that_fits_the_target_opens_at_the_top() {
    let (graph, _) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    assert!(clustering.top_level().len() <= TARGET_TOP_CLUSTERS);
    assert_eq!(
        clustering.opening_level(),
        clustering.level_count() - 1,
        "no rung was built past the modularity optimum"
    );
}

/// When it does not fit, the rungs past it exist for zooming out and are
/// not where the map lands.
#[test]
fn a_ladder_that_overshoots_the_target_opens_below_the_top() {
    // Forty tight triangles, each joined to the next by one call: plain
    // modularity keeps them apart, which is far wider than the target.
    let mut graph = CallGraph::new();
    let mut previous: Option<SymbolId> = None;
    for t in 0..40u32 {
        let a = add(&mut graph, &format!("t{t}a"), 100 + t * 3);
        let b = add(&mut graph, &format!("t{t}b"), 101 + t * 3);
        let c = add(&mut graph, &format!("t{t}c"), 102 + t * 3);
        call_often(&mut graph, a, b, 1000 + t * 30, 3);
        call_often(&mut graph, b, c, 1010 + t * 30, 3);
        call_often(&mut graph, a, c, 1020 + t * 30, 3);
        if let Some(p) = previous {
            call(&mut graph, p, a, 5000 + t);
        }
        previous = Some(c);
    }

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
    let top = clustering.level_count() - 1;
    let opening = clustering.opening_level();

    assert!(clustering.top_level().len() <= TARGET_TOP_CLUSTERS);
    assert!(
        opening < top,
        "the rungs past the optimum are for zooming out"
    );
    assert!(
        clustering.level(opening).len() > TARGET_TOP_CLUSTERS,
        "and the opening rung is the optimum, which did not fit"
    );
}

#[test]
fn the_opening_rung_is_one_the_ladder_has() {
    for graph in [chain_of(60).0, chain_of(3).0, CallGraph::new()] {
        let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
        assert!(
            clustering.opening_level() < clustering.level_count().max(1),
            "the opening rung has to be drawable"
        );
    }
}
// -- the singleton fold ---------------------------------------------------

/// A function nothing pays to take in comes through as a box holding one
/// function beside boxes holding thirty. On a real project that was
/// `fputc`, alone, next to clusters of 22 and 29.
#[test]
fn no_box_holds_one_function_at_the_top_of_the_ladder() {
    // A hub called once from each of twenty pairs: at the finer resolutions
    // no pair gains by taking it, and the top must not leave it alone.
    let mut graph = CallGraph::new();
    let hub = add(&mut graph, "hub", 1);
    for i in 0..20u32 {
        let left = add(&mut graph, &format!("leaf{i}a"), 100 + i * 2);
        let right = add(&mut graph, &format!("leaf{i}b"), 101 + i * 2);
        call_often(&mut graph, left, right, 1000 + i * 20, 4);
        call(&mut graph, hub, left, 5000 + i);
    }

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
    let top = clustering.level_count() - 1;
    assert!(top > 0, "this fixture is big enough to coarsen");

    let alone: Vec<&str> = clustering
        .level(top)
        .iter()
        .filter(|id| clustering.cluster(**id).unwrap().members.len() == 1)
        .map(|id| clustering.cluster(*id).unwrap().label.as_str())
        .collect();
    assert!(alone.is_empty(), "left alone at the top: {alone:?}");
}

/// Folded into the heaviest edge, like everything else the method decides.
#[test]
fn a_leftover_singleton_folds_into_its_heaviest_neighbour() {
    let mut adj = vec![HashMap::new(); 3];
    // Node 0 is alone; it touches 1 lightly and 2 heavily.
    for (other, weight) in [(1usize, 1.0f64), (2, 9.0)] {
        adj[0].insert(other, weight);
        adj[other].insert(0, weight);
    }
    let graph = WeightedGraph::new(adj, vec![0.0; 3], vec![vec![1, 2], vec![], vec![]]);

    let (fold, count) = fold_singletons(&graph, &[1, 5, 5]).expect("a singleton to fold");

    assert_eq!(count, 2, "three boxes became two");
    assert_eq!(
        fold[0], fold[2],
        "the singleton joins the neighbour it shares the heaviest edge with"
    );
    assert_ne!(fold[0], fold[1]);
}

#[test]
fn nothing_is_folded_when_no_box_is_alone() {
    let mut adj = vec![HashMap::new(); 2];
    adj[0].insert(1usize, 3.0f64);
    adj[1].insert(0usize, 3.0f64);
    let graph = WeightedGraph::new(adj, vec![0.0; 2], vec![vec![1], vec![]]);

    assert!(fold_singletons(&graph, &[4, 6]).is_none());
}

/// The fold is overruled by the heaviest edge, and the cycle rule overrules
/// the fold: a neighbour it cannot legally join is passed over.
#[test]
fn a_singleton_folds_past_a_neighbour_that_would_close_a_cycle() {
    // 2 -> 0 heavily, 1 -> 0 lightly, 2 -> 1. Joining 2 makes a box that
    // calls 1 and is called by it; joining 1 does not.
    let mut adj = vec![HashMap::new(); 3];
    for (a, b, weight) in [(2usize, 0usize, 9.0f64), (1, 0, 1.0), (2, 1, 1.0)] {
        adj[a].insert(b, weight);
        adj[b].insert(a, weight);
    }
    let graph = WeightedGraph::new(adj, vec![0.0; 3], vec![vec![], vec![0], vec![0, 1]]);

    let (fold, count) = fold_singletons(&graph, &[1, 5, 5]).expect("a singleton to fold");

    assert_eq!(count, 2);
    assert_eq!(
        fold[0], fold[1],
        "the singleton takes the next neighbour down"
    );
    assert_ne!(fold[0], fold[2]);
}

// -- the quotient is a DAG ------------------------------------------------

/// Whether the boxes at `level`, with the calls between them, form a DAG.
fn rung_is_a_dag(graph: &CallGraph, clustering: &Clustering, level: usize) -> bool {
    let ids = clustering.level(level);
    let mut successors: HashMap<ClusterId, HashSet<ClusterId>> = HashMap::new();
    for id in ids {
        for member in &clustering.cluster(*id).unwrap().members {
            for callee in graph.get_callees(member) {
                if let Some(other) = clustering.cluster_of_at(&callee.id, level) {
                    if other != *id {
                        successors.entry(*id).or_default().insert(other);
                    }
                }
            }
        }
    }

    let mut indegree: HashMap<ClusterId, usize> = ids.iter().map(|id| (*id, 0)).collect();
    for targets in successors.values() {
        for target in targets {
            *indegree.get_mut(target).unwrap() += 1;
        }
    }
    let mut ready: Vec<ClusterId> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut removed = 0usize;
    while let Some(id) = ready.pop() {
        removed += 1;
        for target in successors.get(&id).into_iter().flatten() {
            let degree = indegree.get_mut(target).unwrap();
            *degree -= 1;
            if *degree == 0 {
                ready.push(*target);
            }
        }
    }
    removed == ids.len()
}

/// A layered DAG with some structure to find: `n` functions where each
/// calls one to three of the next eight, chosen by a fixed generator so
/// the fixture is the same every run.
fn layered_dag(n: u32) -> (CallGraph, Vec<SymbolId>) {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = (0..n)
        .map(|i| add(&mut graph, &format!("f{i}"), i + 1))
        .collect();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut line = 1000u32;
    for i in 0..n as usize {
        let fan = 1 + (next() % 3) as usize;
        for _ in 0..fan {
            let j = i + 1 + (next() % 8) as usize;
            if j < n as usize {
                let times = 1 + (next() % 3) as u32;
                call_often(&mut graph, ids[i], ids[j], line, times);
                line += times;
            }
        }
    }
    (graph, ids)
}

/// Two pairs that pay to merge, and a one-way chain that crosses between
/// them in both directions: exactly the shape modularity alone draws as a
/// pair of boxes calling each other.
fn crossing_graph() -> CallGraph {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    let d = add(&mut graph, "d", 4);
    call_often(&mut graph, a, c, 100, 5);
    call_often(&mut graph, b, d, 150, 5);
    call(&mut graph, a, b, 200);
    call(&mut graph, b, c, 201);
    for i in 0..5 {
        let left = add(&mut graph, &format!("spare{i}l"), 300 + i * 2);
        let right = add(&mut graph, &format!("spare{i}r"), 301 + i * 2);
        call_often(&mut graph, left, right, 400 + i * 10, 5);
    }
    graph
}

/// The property this module now promises, of every method: over a call
/// graph with no cycle, no rung of the ladder has one either.
#[test]
fn every_rung_of_a_dag_is_a_dag() {
    let crossing = crossing_graph();
    let (layered, _) = layered_dag(60);
    for (name, graph) in [("crossing", &crossing), ("layered", &layered)] {
        for &method in ClusterMethod::ALL {
            let clustering = Clustering::compute(graph, method);
            let connected = clustering
                .clusters()
                .filter(|c| c.level == 0)
                .map(|c| c.members.len())
                .sum::<usize>();
            assert!(
                clustering.level(0).len() < connected,
                "{name} by {method:?}: the fixture should coarsen"
            );
            for level in 0..clustering.level_count() {
                assert!(
                    rung_is_a_dag(graph, &clustering, level),
                    "{name} by {method:?}: rung {level} has a cycle: {:?}",
                    grouping_at(graph, &clustering, level)
                );
            }
        }
    }
}

/// The pairs still merge; the crossing is what is refused.
#[test]
fn a_move_that_would_close_a_quotient_cycle_is_refused() {
    let graph = crossing_graph();
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
    let groups = grouping_at(&graph, &clustering, 0);
    let boxed = |x: &str, y: &str| {
        groups
            .iter()
            .any(|g| g.iter().any(|m| m == x) && g.iter().any(|m| m == y))
    };

    assert!(
        boxed("a", "c") || boxed("b", "d"),
        "at least one heavy pair merges: {groups:?}"
    );
    assert!(rung_is_a_dag(&graph, &clustering, 0), "{groups:?}");
}

/// Functions that call each other are one unit from the finest rung up,
/// so the quotient of a graph with a real cycle in it is still a DAG.
#[test]
fn mutually_recursive_functions_share_a_box_at_every_rung() {
    let (mut graph, ids) = layered_dag(60);
    // The farthest function f10 reaches, then a call back from it: every
    // function on a path between the two is now in one strongly connected
    // component.
    let mut reach: Vec<SymbolId> = vec![ids[10]];
    let mut seen: HashSet<SymbolId> = reach.iter().copied().collect();
    let mut at = 0;
    while at < reach.len() {
        for callee in graph.get_callees(&reach[at]) {
            if seen.insert(callee.id) {
                reach.push(callee.id);
            }
        }
        at += 1;
    }
    let far = *reach.last().unwrap();
    assert_ne!(far, ids[10], "f10 should reach something");
    call(&mut graph, far, ids[10], 9000);

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
    assert!(clustering.level_count() > 1);
    for level in 0..clustering.level_count() {
        assert_eq!(
            clustering.cluster_of_at(&ids[10], level),
            clustering.cluster_of_at(&far, level),
            "a cycle is never split across boxes, at rung {level}"
        );
        assert!(rung_is_a_dag(&graph, &clustering, level), "rung {level}");
    }
}

#[test]
fn a_small_graph_with_a_cycle_condenses_it_into_one_box() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    call(&mut graph, a, b, 10);
    call(&mut graph, b, a, 11);
    call(&mut graph, b, c, 12);

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    assert_eq!(clustering.level_count(), 1);
    assert_eq!(
        grouping_at(&graph, &clustering, 0),
        vec![vec!["a", "b"], vec!["c"]]
    );
}

/// The finest rung of a graph small enough to read whole is the graph
/// itself, not a first round of merges applied anyway.
#[test]
fn a_graph_below_the_target_gets_one_rung_of_singletons() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    call(&mut graph, a, b, 10);
    call(&mut graph, b, c, 11);

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    assert_eq!(clustering.level_count(), 1);
    assert_eq!(
        grouping_at(&graph, &clustering, 0),
        vec![vec!["a"], vec!["b"], vec!["c"]]
    );
}

/// Every level covers the same symbols: a zoom, not a subtree to open.
#[test]
fn every_level_partitions_the_connected_symbols() {
    let (graph, _) = chain_of(60);
    let expected: HashSet<SymbolId> = graph.function_ids().copied().collect();

    for &method in ClusterMethod::ALL {
        let clustering = Clustering::compute(&graph, method);
        for level in 0..clustering.level_count() {
            let mut seen: HashSet<SymbolId> = HashSet::new();
            for id in clustering.level(level) {
                for member in &clustering.cluster(*id).unwrap().members {
                    assert!(
                        seen.insert(*member),
                        "{method:?}: {member:?} is in two clusters at {level}"
                    );
                }
            }
            assert_eq!(
                seen, expected,
                "{method:?}: level {level} does not cover the graph"
            );
        }
    }
}

fn chain_of(n: u32) -> (CallGraph, Vec<SymbolId>) {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = (0..n)
        .map(|i| add(&mut graph, &format!("f{i}"), i + 1))
        .collect();
    for (i, pair) in ids.windows(2).enumerate() {
        call(&mut graph, pair[0], pair[1], 1000 + i as u32);
    }
    (graph, ids)
}

#[test]
fn a_coarser_resolution_opens_on_fewer_boxes() {
    // γ scales what a merge has to beat, so turning it down lets groupings
    // through that plain modularity refuses — on the same graph, at the
    // rung each run opens on.
    let (graph, _) = layered_dag(120);
    let settings = ClusterSettings::new(ClusterMethod::Modularity);

    let fine = Clustering::compute(&graph, settings.with_grain(4.0));
    let plain = Clustering::compute(&graph, settings);
    let coarse = Clustering::compute(&graph, settings.with_grain(0.25));

    let opening = |c: &Clustering| c.level(c.opening_level()).len();
    assert!(
        opening(&fine) > opening(&plain),
        "γ = 4 should open finer than γ = 1: {} vs {}",
        opening(&fine),
        opening(&plain)
    );
    assert!(
        opening(&coarse) < opening(&plain),
        "γ = 0.25 should open coarser than γ = 1: {} vs {}",
        opening(&coarse),
        opening(&plain)
    );
}

#[test]
fn the_target_width_is_the_one_the_settings_asked_for() {
    // The target only decides how far past its own grain the ladder keeps
    // merging, so it binds the top rung and nothing else.
    let (graph, _) = layered_dag(150);
    for &method in ClusterMethod::ALL {
        let settings = ClusterSettings::new(method);
        for target in [5usize, 30] {
            let clustering = Clustering::compute(&graph, settings.with_target_top(target));
            let top = clustering.top_level().len();
            assert!(
                top <= target,
                "{method:?}: a target of {target} left a top rung of {top}"
            );
        }
    }
}

#[test]
fn a_tighter_target_only_adds_rungs_above_the_opening_one() {
    // Lowering it must not change what the method itself decided: the
    // grouping the map opens on is the same, with further zoom steps
    // stacked on top of it.
    let (graph, _) = layered_dag(150);
    let settings = ClusterSettings::new(ClusterMethod::Modularity);
    let wide = Clustering::compute(&graph, settings.with_target_top(30));
    let tight = Clustering::compute(&graph, settings.with_target_top(5));

    assert_eq!(
        wide.level(wide.opening_level()).len(),
        tight.level(tight.opening_level()).len(),
        "the opening rung is the method's, not the target's"
    );
    assert!(
        tight.level_count() > wide.level_count(),
        "a tighter target should add zoom steps: {} vs {}",
        tight.level_count(),
        wide.level_count()
    );
}

#[test]
fn a_higher_inflation_settles_into_more_boxes() {
    let (graph, _) = layered_dag(120);
    let settings = ClusterSettings::new(ClusterMethod::Markov);
    let slack = Clustering::compute(&graph, settings.with_grain(1.4));
    let sharp = Clustering::compute(&graph, settings.with_grain(3.0));
    assert!(
        sharp.level(sharp.opening_level()).len() > slack.level(slack.opening_level()).len(),
        "inflation 3 should open finer than 1.4"
    );
}

#[test]
fn each_method_keeps_its_own_grain() {
    // Turning the flow up and going back to modularity must not have moved
    // γ — the two knobs mean different things and are not one slider.
    let markov = ClusterSettings::new(ClusterMethod::Markov).with_grain(3.5);
    assert_eq!(markov.inflation(), 3.5);
    assert_eq!(markov.resolution(), DEFAULT_RESOLUTION);

    let back = markov.with_method(ClusterMethod::Modularity);
    assert_eq!(back.grain(), DEFAULT_RESOLUTION);
    assert_eq!(
        back.inflation(),
        3.5,
        "the flow's grain survived the detour"
    );
}

#[test]
fn settings_off_the_range_or_the_step_are_snapped_on_the_way_in() {
    // Sanitising is what makes the settings a cache key: a slider dragged
    // to the same place twice, or a frontend sending something absurd, has
    // to land on one value and not on two that only look alike.
    let from = |json: &str| serde_json::from_str::<ClusterSettings>(json).expect("valid json");

    let snapped =
        from(r#"{"method":"Modularity","resolution":0.9,"inflation":2.04,"target_top":12}"#);
    assert_eq!(snapped.resolution(), 1.0);
    assert_eq!(snapped.inflation(), 2.0);

    let clamped =
        from(r#"{"method":"Markov","resolution":-4.0,"inflation":99.0,"target_top":9000}"#);
    assert_eq!(clamped.resolution(), ClusterSettings::RESOLUTION.min);
    assert_eq!(clamped.inflation(), ClusterSettings::INFLATION.max);
    assert_eq!(
        clamped.target_top(),
        ClusterSettings::TARGET_TOP.max as usize
    );

    // And a field nobody sent is the default, so a frontend that knows only
    // about the method still sends something that works.
    let bare = from(r#"{"method":"Leiden"}"#);
    assert_eq!(bare, ClusterSettings::new(ClusterMethod::Leiden));

    // Two requests that agree are one cache key.
    let mut seen = HashSet::new();
    assert!(seen.insert(snapped));
    assert!(!seen.insert(from(
        r#"{"method":"Modularity","resolution":1.02,"inflation":1.99,"target_top":12}"#
    )));
}

#[test]
fn settings_survive_the_wire() {
    let settings = ClusterSettings::new(ClusterMethod::Markov)
        .with_grain(2.7)
        .with_target_top(7);
    let json = serde_json::to_string(&settings).expect("serialises");
    assert!(
        json.contains("2.7"),
        "the wire speaks plain numbers: {json}"
    );
    assert_eq!(
        serde_json::from_str::<ClusterSettings>(&json).expect("round trip"),
        settings
    );
}

#[test]
fn a_clustering_remembers_what_it_was_asked_for() {
    let (graph, _) = layered_dag(40);
    let settings = ClusterSettings::new(ClusterMethod::Leiden).with_grain(2.0);
    let clustering = Clustering::compute(&graph, settings);
    assert_eq!(clustering.settings(), settings);
    assert_eq!(clustering.method(), ClusterMethod::Leiden);
}

#[test]
fn the_ladder_is_a_refinement_of_itself() {
    let (graph, _) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    for level in 1..clustering.level_count() {
        for id in clustering.level(level) {
            let cluster = clustering.cluster(*id).unwrap();
            let from_children: HashSet<SymbolId> = cluster
                .children
                .iter()
                .flat_map(|child| clustering.cluster(*child).unwrap().members.clone())
                .collect();
            assert_eq!(
                from_children,
                cluster.members.iter().copied().collect::<HashSet<_>>(),
                "a cluster must hold exactly what its children hold"
            );
            assert!(
                !cluster.children.is_empty(),
                "a cluster above level 0 came from somewhere"
            );
        }
    }
}
#[test]
fn recomputing_gives_identical_output() {
    let (graph, _) = chain_of(60);
    for &method in ClusterMethod::ALL {
        assert_eq!(
            fingerprint(&graph, &Clustering::compute(&graph, method)),
            fingerprint(&graph, &Clustering::compute(&graph, method)),
            "{method:?}"
        );
    }
}

#[test]
fn an_empty_graph_yields_an_empty_clustering() {
    let clustering = Clustering::compute(&CallGraph::new(), ClusterMethod::Modularity);
    assert!(clustering.is_empty());
    assert_eq!(clustering.level_count(), 0);
    assert!(clustering.top_level().is_empty());
    assert_eq!(clustering.top_level_index(), None);
}

#[test]
fn isolated_symbols_are_left_out() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    call(&mut graph, a, b, 10);
    let lonely = add(&mut graph, "lonely", 3);
    let recursive = add(&mut graph, "recursive", 4);
    call(&mut graph, recursive, recursive, 20);

    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    assert!(clustering.cluster_of(&a).is_some());
    assert_eq!(clustering.cluster_of(&lonely), None);
    assert_eq!(
        clustering.cluster_of(&recursive),
        None,
        "a self-loop connects a function to nothing"
    );
}

// -- addressing ---------------------------------------------------------

#[test]
fn ancestry_is_a_chain_to_the_top() {
    let (graph, ids) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    let chain = clustering.ancestry(&ids[0]);
    assert_eq!(chain.len(), clustering.level_count());
    for (level, id) in chain.iter().enumerate() {
        let cluster = clustering.cluster(*id).unwrap();
        assert_eq!(cluster.level, level);
        assert!(cluster.members.contains(&ids[0]));
    }
    assert_eq!(
        clustering.cluster(*chain.last().unwrap()).unwrap().parent,
        None
    );
}

#[test]
fn a_symbol_can_be_found_at_every_rung() {
    let (graph, ids) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    for level in 0..clustering.level_count() {
        let id = clustering
            .cluster_of_at(&ids[7], level)
            .unwrap_or_else(|| panic!("f7 should be in a cluster at level {level}"));
        let cluster = clustering.cluster(id).unwrap();
        assert_eq!(cluster.level, level);
        assert!(cluster.members.contains(&ids[7]));
    }
    assert_eq!(
        clustering.cluster_of_at(&ids[7], clustering.level_count()),
        None,
        "a level the ladder does not have holds nothing"
    );
}

#[test]
fn a_key_anchors_on_the_lowest_member_and_resolves_back() {
    let (graph, _) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    for cluster in clustering.clusters() {
        let mut sorted = cluster.members.clone();
        sorted.sort_unstable_by_key(|s| s.0);
        assert_eq!(
            cluster.key.anchor, sorted[0],
            "the anchor is the lowest member"
        );
        assert_eq!(cluster.key.level, cluster.level);
        assert_eq!(
            clustering.by_key(cluster.key).map(|c| c.id),
            Some(cluster.id)
        );
    }
}

#[test]
fn a_key_naming_a_level_that_does_not_exist_resolves_to_nothing() {
    let (graph, ids) = chain_of(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    assert!(clustering
        .by_key(ClusterKey {
            level: clustering.level_count() + 3,
            anchor: ids[0],
        })
        .is_none());
}

// -- naming -------------------------------------------------------------

/// The member outside callers arrive at, and that leads inward — not the
/// most-called member, which is generally a shared leaf.
#[test]
fn a_cluster_is_named_after_its_front_door() {
    let mut graph = CallGraph::new();
    let outside = add(&mut graph, "outside", 1);
    let door = add(&mut graph, "door", 2);
    let middle = add(&mut graph, "middle", 3);
    let leaf = add(&mut graph, "leaf", 4);

    call(&mut graph, outside, door, 10);
    call(&mut graph, door, middle, 11);
    call(&mut graph, middle, leaf, 12);
    // The leaf is called far more often, from outside as well as in.
    call_often(&mut graph, outside, leaf, 20, 8);

    let members = vec![door, middle, leaf];
    assert_eq!(
        choose_entry_point(&graph, &members).map(|s| named(&graph, &s)),
        Some("door".to_string())
    );
}

/// `main` has no callers at all, so a rule that ranks by incoming calls
/// would filter it out of naming its own cluster.
#[test]
fn a_member_nothing_calls_can_still_name_its_cluster() {
    let mut graph = CallGraph::new();
    let main = add(&mut graph, "main", 1);
    let helper = add(&mut graph, "helper", 2);
    call(&mut graph, main, helper, 10);

    assert_eq!(
        choose_entry_point(&graph, &[main, helper]).map(|s| named(&graph, &s)),
        Some("main".to_string())
    );
}

/// Two members an outside caller enters equally often, each fronting the
/// same amount of cluster: nothing about the calls separates them, so the
/// one defined first takes the name — and takes it whichever order the
/// members happen to be listed in, since that order is the partition's
/// business and not a reader's.
#[test]
fn a_tie_is_broken_by_definition_order() {
    let mut graph = CallGraph::new();
    let outside = add(&mut graph, "outside", 1);
    let early = add(&mut graph, "early", 2);
    let late = add(&mut graph, "late", 3);
    let early_leaf = add(&mut graph, "early_leaf", 4);
    let late_leaf = add(&mut graph, "late_leaf", 5);

    call(&mut graph, outside, early, 10);
    call(&mut graph, outside, late, 11);
    call(&mut graph, early, early_leaf, 12);
    call(&mut graph, late, late_leaf, 13);

    for members in [
        vec![early, late, early_leaf, late_leaf],
        vec![late_leaf, late, early_leaf, early],
    ] {
        let names: Vec<String> = members.iter().map(|m| named(&graph, m)).collect();
        assert_eq!(
            choose_entry_point(&graph, &members).map(|s| named(&graph, &s)),
            Some("early".to_string()),
            "listed as {names:?}"
        );
    }
}

/// The tie-break is a definition site read the way a person reads one:
/// file first, then line. `in_a` is defined far further down its file and
/// still wins, because the file sorts earlier.
#[test]
fn a_tie_across_files_is_broken_by_the_file_before_the_line() {
    let mut graph = CallGraph::new();
    let outside = add_in(&mut graph, "outside", "src/main.c", 1);
    let in_a = add_in(&mut graph, "in_a", "src/a.c", 90);
    let in_b = add_in(&mut graph, "in_b", "src/b.c", 10);
    let a_leaf = add_in(&mut graph, "a_leaf", "src/a.c", 91);
    let b_leaf = add_in(&mut graph, "b_leaf", "src/b.c", 11);

    call(&mut graph, outside, in_a, 10);
    call(&mut graph, outside, in_b, 11);
    call(&mut graph, in_a, a_leaf, 12);
    call(&mut graph, in_b, b_leaf, 13);

    assert_eq!(
        choose_entry_point(&graph, &[in_a, in_b, a_leaf, b_leaf]).map(|s| named(&graph, &s)),
        Some("in_a".to_string())
    );
}

/// Past [`REACHABILITY_MEMBER_LIMIT`] members the ranking swaps the BFS
/// for a count of direct callees, and the two answers genuinely differ:
/// `head` fronts a long chain through one callee, `hub` calls three and
/// leads nowhere after. Which one names the cluster therefore pins where
/// the limit falls — 200 members still get the reachability, 201 get the
/// stand-in.
#[test]
fn a_cluster_past_the_reachability_limit_ranks_by_direct_callees() {
    fn fixture(chain: u32) -> (CallGraph, Vec<SymbolId>) {
        let mut graph = CallGraph::new();
        let outside = add(&mut graph, "outside", 1);
        let hub = add(&mut graph, "hub", 2);
        let head = add(&mut graph, "head", 3);
        let mut members = vec![hub, head];

        for i in 0..3u32 {
            let callee = add(&mut graph, &format!("hub_callee{i}"), 10 + i);
            call(&mut graph, hub, callee, 100 + i);
            members.push(callee);
        }

        let mut previous = head;
        for i in 0..chain {
            let link = add(&mut graph, &format!("link{i}"), 1000 + i);
            call(&mut graph, previous, link, 2000 + i);
            members.push(link);
            previous = link;
        }

        call(&mut graph, outside, hub, 20);
        call(&mut graph, outside, head, 21);
        (graph, members)
    }

    let (graph, members) = fixture(REACHABILITY_MEMBER_LIMIT as u32 - 5);
    assert_eq!(members.len(), REACHABILITY_MEMBER_LIMIT);
    assert_eq!(
        choose_entry_point(&graph, &members).map(|s| named(&graph, &s)),
        Some("head".to_string())
    );

    let (graph, members) = fixture(REACHABILITY_MEMBER_LIMIT as u32 - 4);
    assert_eq!(members.len(), REACHABILITY_MEMBER_LIMIT + 1);
    assert_eq!(
        choose_entry_point(&graph, &members).map(|s| named(&graph, &s)),
        Some("hub".to_string())
    );
}

/// The labels a known graph comes out with, rung by rung, written down.
/// Determinism does not pin this: a run matching itself says nothing
/// about matching the run before the ranking was rewritten, and the entry
/// point is what `DrillIntoCluster` roots a tree at, so every one of these
/// is a user-visible answer. A pseudo-random layered DAG is used because
/// its clusters are full of members nothing separates but their location.
#[test]
fn the_labels_of_a_known_graph_do_not_move() {
    let (graph, _) = layered_dag(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

    let labels: Vec<Vec<&str>> = (0..clustering.level_count())
        .map(|level| {
            clustering
                .level(level)
                .iter()
                .filter_map(|id| clustering.cluster(*id))
                .map(|cluster| cluster.label.as_str())
                .collect()
        })
        .collect();

    assert_eq!(
        labels,
        vec![
            vec![
                "f3", "f30", "f45", "f35", "f17", "f5", "f19", "f47", "f20", "f44", "f53", "f54",
                "f0", "f24", "f51", "f4", "f16", "f32", "f39", "f23", "f29", "f15", "f40", "f8",
                "f26", "f22", "f43", "f34",
            ],
            vec![
                "f3", "f30", "f43", "f35", "f15", "f5", "f19", "f47", "f20", "f53", "f54", "f0",
                "f22", "f4", "f32", "f23", "f29", "f40", "f8", "f34",
            ],
            vec!["f3", "f30", "f43", "f5", "f47", "f20", "f54", "f0", "f32", "f23", "f40", "f34",],
            vec!["f0", "f30", "f43", "f5", "f20", "f40"],
        ]
    );
}

// -- scoping ------------------------------------------------------------

#[test]
fn a_scoped_clustering_holds_nothing_from_outside_the_scope() {
    let (graph, ids) = chain_of(60);
    let scope: HashSet<SymbolId> = ids[..10].iter().copied().collect();

    let clustering = Clustering::compute_within(&graph, &scope, ClusterMethod::Modularity);

    for cluster in clustering.clusters() {
        for member in &cluster.members {
            assert!(scope.contains(member), "{member:?} is outside the scope");
        }
    }
    assert_eq!(clustering.cluster_of(&ids[30]), None);
}

#[test]
fn a_function_whose_only_calls_leave_the_scope_is_isolated_within_it() {
    let (graph, ids) = chain_of(60);
    // `ids[0]` calls `ids[1]` and nothing else; scope it away from it.
    let scope: HashSet<SymbolId> = [ids[0], ids[30], ids[31]].into_iter().collect();

    let clustering = Clustering::compute_within(&graph, &scope, ClusterMethod::Modularity);

    assert_eq!(clustering.cluster_of(&ids[0]), None);
    assert!(clustering.cluster_of(&ids[30]).is_some());
}

#[test]
fn scoping_to_everything_is_clustering_everything() {
    let (graph, _) = chain_of(60);
    let everything: HashSet<SymbolId> = graph.function_ids().copied().collect();

    assert_eq!(
        fingerprint(
            &graph,
            &Clustering::compute_within(&graph, &everything, ClusterMethod::Modularity)
        ),
        fingerprint(
            &graph,
            &Clustering::compute(&graph, ClusterMethod::Modularity)
        ),
    );
}

#[test]
fn an_empty_scope_clusters_nothing() {
    let (graph, _) = chain_of(60);
    let clustering = Clustering::compute_within(&graph, &HashSet::new(), ClusterMethod::Modularity);
    assert!(clustering.is_empty());
}

// -- Markov flow ----------------------------------------------------------

/// Two tightly knit groups joined by one call. Each group is an ordered
/// clique — every function calls every later one — so the base graph is
/// a DAG and the groups are as cohesive as a DAG allows.
fn two_groups_one_stray_call() -> CallGraph {
    let mut graph = CallGraph::new();
    let left: Vec<SymbolId> = ["a", "b", "c", "d"]
        .iter()
        .enumerate()
        .map(|(i, name)| add(&mut graph, name, i as u32 + 1))
        .collect();
    let right: Vec<SymbolId> = ["e", "f", "g", "h"]
        .iter()
        .enumerate()
        .map(|(i, name)| add(&mut graph, name, i as u32 + 10))
        .collect();
    let mut line = 100;
    for group in [&left, &right] {
        for i in 0..group.len() {
            for j in i + 1..group.len() {
                call(&mut graph, group[i], group[j], line);
                line += 1;
            }
        }
    }
    call(&mut graph, left[3], right[0], line);
    graph
}

/// The property the method is for: proximity is many short paths, not one
/// call, so a single stray call between two groups does not glue them.
#[test]
fn a_stray_call_does_not_glue_two_groups_together() {
    let graph = two_groups_one_stray_call();
    assert_eq!(
        one_flow_pass(&graph, MARKOV_INFLATION),
        vec![vec!["a", "b", "c", "d"], vec!["e", "f", "g", "h"]]
            .into_iter()
            .map(|g| g.into_iter().map(String::from).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
}

/// The knob: a higher inflation starves weak flow sooner and the walk
/// settles into more boxes.
#[test]
fn a_higher_inflation_splits_finer() {
    let (graph, _) = layered_dag(60);
    let coarse = one_flow_pass(&graph, 1.4).len();
    let fine = one_flow_pass(&graph, 4.0).len();
    assert!(
        fine > coarse,
        "inflation 4 made {fine} boxes, inflation 1.4 made {coarse}"
    );
}

/// Every column of the settled flow is a distribution over rows in
/// ascending order — what [`markov_pass`] reads off relies on both.
#[test]
fn settled_flow_columns_are_sorted_distributions() {
    let (graph, _) = layered_dag(60);
    let base = build_base_graph(&graph, None);
    for (node, column) in markov_flow(&base.graph, MARKOV_INFLATION)
        .iter()
        .enumerate()
    {
        let total: f64 = column.iter().map(|(_, w)| w).sum();
        assert!((total - 1.0).abs() < 1e-9, "column {node} sums to {total}");
        assert!(
            column.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "column {node} is not sorted: {column:?}"
        );
    }
}

/// The map opens on the first pass — one run of MCL at the default
/// inflation is the method's answer — and that rung is what the pass
/// said, up to the singleton fold, which only ever merges.
#[test]
fn the_markov_map_opens_on_its_first_pass() {
    let (graph, _) = layered_dag(60);
    let clustering = Clustering::compute(&graph, ClusterMethod::Markov);
    assert_eq!(clustering.opening_level(), 0);

    let opening = grouping_at(&graph, &clustering, 0);
    for pure in one_flow_pass(&graph, MARKOV_INFLATION) {
        assert!(
            opening
                .iter()
                .any(|rung| pure.iter().all(|name| rung.contains(name))),
            "{pure:?} was split across boxes of the opening rung {opening:?}"
        );
    }
}

/// A node whose flow drains into a system it may not join for the DAG
/// rule takes the legal system it shares the most calls with instead,
/// rather than staying alone: `x` calls into the pair and is called back
/// by it, and both of its callees are wanted by the same attractor.
#[test]
fn a_node_blocked_from_its_attractor_drains_by_its_calls() {
    let graph = crossing_graph();
    let clustering = Clustering::compute(&graph, ClusterMethod::Markov);
    assert!(rung_is_a_dag(&graph, &clustering, 0));
    let groups = grouping_at(&graph, &clustering, 0);
    let boxed = |x: &str, y: &str| {
        groups
            .iter()
            .any(|g| g.iter().any(|m| m == x) && g.iter().any(|m| m == y))
    };
    assert!(
        boxed("a", "c") || boxed("b", "d"),
        "at least one heavy pair shares a box: {groups:?}"
    );
}

#[test]
fn markov_is_listed_after_modularity() {
    assert_eq!(
        ClusterMethod::ALL,
        &[
            ClusterMethod::Modularity,
            ClusterMethod::Leiden,
            ClusterMethod::Markov
        ]
    );
    assert_ne!(
        ClusterMethod::Markov.label(),
        ClusterMethod::Modularity.label()
    );
    assert!(!ClusterMethod::Markov.description().is_empty());
    assert_eq!(
        Clustering::compute(&layered_dag(60).0, ClusterMethod::Markov).method(),
        ClusterMethod::Markov
    );
}

// -- Leiden ----------------------------------------------------------------

/// Whether every box at one level is connected by the calls among its
/// own members — the property Leiden promises and Louvain does not.
fn rung_is_connected(graph: &CallGraph, clustering: &Clustering, level: usize) -> bool {
    clustering.level(level).iter().all(|id| {
        let members: HashSet<SymbolId> = clustering
            .cluster(*id)
            .unwrap()
            .members
            .iter()
            .copied()
            .collect();
        let start = *members.iter().next().unwrap();
        let mut seen: HashSet<SymbolId> = HashSet::from([start]);
        let mut stack = vec![start];
        while let Some(at) = stack.pop() {
            let mut around: Vec<SymbolId> =
                graph.get_callees(&at).into_iter().map(|f| f.id).collect();
            around.extend(graph.get_callers(&at).into_iter().map(|f| f.id));
            for other in around {
                if members.contains(&other) && seen.insert(other) {
                    stack.push(other);
                }
            }
        }
        seen.len() == members.len()
    })
}

/// The refinement of a partition handed in by hand, as names. Lets a test
/// start from a grouping the moves would never make and watch the
/// refinement take it apart.
fn refinement_of(graph: &CallGraph, grouping: &[&[&str]]) -> Vec<Vec<String>> {
    partition_names(graph, |base| {
        let n = base.node_count();
        let names: Vec<String> = base_names(graph, base, n);
        let community: Vec<usize> = names
            .iter()
            .map(|name| {
                grouping
                    .iter()
                    .position(|g| g.contains(&name.as_str()))
                    .expect("every function is in a group")
            })
            .collect();
        let mover = Mover::new(base, 1.0, community, grouping.len().max(n));
        refine(&mover)
    })
}

/// The name of every base node, by index. Only valid on a graph with no
/// cycle, where no node condenses.
fn base_names(graph: &CallGraph, base: &WeightedGraph, n: usize) -> Vec<String> {
    let full = build_base_graph(graph, None);
    assert_eq!(full.graph.node_count(), base.node_count());
    (0..n)
        .map(|node| {
            let symbol = full
                .units
                .iter()
                .position(|&unit| unit == node)
                .expect("every node has a symbol");
            named(graph, &full.symbols[symbol])
        })
        .collect()
}

fn one_leiden_pass(graph: &CallGraph, resolution: f64) -> Vec<Vec<String>> {
    partition_names(graph, |base| leiden_pass(base, resolution))
}

fn groups(names: &[&[&str]]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = names
        .iter()
        .map(|g| {
            let mut g: Vec<String> = g.iter().map(|s| s.to_string()).collect();
            g.sort();
            g
        })
        .collect();
    out.sort();
    out
}

/// The mechanism of the guarantee: a box whose two halves share no call
/// is rebuilt as two boxes, because a merge needs a call to go through.
#[test]
fn a_box_in_two_pieces_is_refined_into_two() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    let d = add(&mut graph, "d", 4);
    call_often(&mut graph, a, b, 100, 3);
    call_often(&mut graph, c, d, 200, 3);

    assert_eq!(
        refinement_of(&graph, &[&["a", "b", "c", "d"]]),
        groups(&[&["a", "b"], &["c", "d"]])
    );
}

/// A node whose one call into a box is less than the null model expects
/// of a node as busy as it is stays out of the box on refinement, however
/// the moves had placed it.
#[test]
fn a_poorly_connected_node_is_not_merged_on_refinement() {
    let mut graph = CallGraph::new();
    let clique: Vec<SymbolId> = ["a", "b", "c", "d"]
        .iter()
        .enumerate()
        .map(|(i, name)| add(&mut graph, name, i as u32 + 1))
        .collect();
    let mut line = 100;
    for i in 0..clique.len() {
        for j in i + 1..clique.len() {
            call_often(&mut graph, clique[i], clique[j], line, 5);
            line += 5;
        }
    }
    let v = add(&mut graph, "v", 50);
    let w = add(&mut graph, "w", 51);
    call(&mut graph, v, clique[0], 500);
    call_often(&mut graph, v, w, 600, 20);

    let refined = refinement_of(&graph, &[&["a", "b", "c", "d", "v"], &["w"]]);
    assert!(
        refined.contains(&vec!["v".to_string()]),
        "v has one call into the box and twenty elsewhere: {refined:?}"
    );
}

/// A refinement merge is held to the DAG rule. `a` calls `c` heavily and
/// `b` calls `d` heavily, and a one-way chain `a -> b -> c` crosses
/// between them. Both pairs pay to merge, but `{a, c}` would sit on both
/// ends of `b`: a cycle of the refined quotient, though the box holding
/// all four was on none. So that merge is refused, from either end, and
/// only `b` and `d` come out together.
#[test]
fn a_refinement_merge_that_would_close_a_cycle_is_refused() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    let d = add(&mut graph, "d", 4);
    call_often(&mut graph, a, c, 100, 5);
    call_often(&mut graph, b, d, 150, 5);
    call(&mut graph, a, b, 200);
    call(&mut graph, b, c, 201);

    assert_eq!(
        refinement_of(&graph, &[&["a", "b", "c", "d"]]),
        groups(&[&["a"], &["b", "d"], &["c"]])
    );
}

/// The promise, over every rung of ladders that exercise every regime.
#[test]
fn every_box_of_every_leiden_rung_is_connected() {
    let (layered, _) = layered_dag(200);
    let (chain, _) = chain_of(60);
    let crossing = crossing_graph();
    let stray = two_groups_one_stray_call();
    for (name, graph) in [
        ("layered", &layered),
        ("chain", &chain),
        ("crossing", &crossing),
        ("stray", &stray),
    ] {
        let clustering = Clustering::compute(graph, ClusterMethod::Leiden);
        for level in 0..clustering.level_count() {
            assert!(
                rung_is_connected(graph, &clustering, level),
                "{name}: a box on rung {level} is not connected: {:?}",
                grouping_at(graph, &clustering, level)
            );
            assert!(
                rung_is_a_dag(graph, &clustering, level),
                "{name}: rung {level} has a cycle"
            );
        }
    }
}

/// One pass to convergence climbs the same objective Louvain does, so on
/// a graph with a clean answer the two agree.
#[test]
fn leiden_finds_the_two_groups_louvain_finds() {
    let graph = two_groups_one_stray_call();
    assert_eq!(one_leiden_pass(&graph, 1.0), one_pass(&graph, 1.0));
    assert_eq!(
        one_leiden_pass(&graph, 1.0),
        groups(&[&["a", "b", "c", "d"], &["e", "f", "g", "h"]])
    );
}

/// A pass is a run to convergence, not one round: on a graph Louvain
/// needs several passes for, one Leiden pass is already as coarse as
/// Louvain's ladder gets at that resolution.
#[test]
fn a_leiden_pass_runs_to_convergence() {
    let (graph, _) = layered_dag(200);
    let leiden = one_leiden_pass(&graph, 1.0).len();
    let base = build_base_graph(&graph, None);
    let mut current = base.graph;
    let mut count = current.node_count();
    loop {
        let (groups, merged) = louvain_pass(&current, 1.0);
        if merged == count {
            break;
        }
        current = aggregate(&current, &groups, merged);
        count = merged;
    }
    assert!(
        leiden <= count + count / 4,
        "one Leiden pass left {leiden} boxes where Louvain converged to {count}"
    );
}

/// The zero-temperature choice is what makes this a function of the graph.
#[test]
fn leiden_recomputes_identically() {
    let (graph, _) = layered_dag(120);
    let first = Clustering::compute(&graph, ClusterMethod::Leiden);
    let second = Clustering::compute(&graph, ClusterMethod::Leiden);
    assert_eq!(fingerprint(&graph, &first), fingerprint(&graph, &second));
    assert_eq!(first.method(), ClusterMethod::Leiden);
}
