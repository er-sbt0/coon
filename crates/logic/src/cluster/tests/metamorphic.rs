//! Changes to a graph that must not change its clustering, or must change it
//! in a way known in advance.
//!
//! These need no expected answer: the clustering of one graph is the expected
//! answer for the other. That is what lets them run over the whole corpus at
//! every setting, where `known_answers` can only cover graphs small enough to
//! work out by hand.

use super::invariants::{corpus, settings_matrix};
use super::known_answers::{clique, graph_of};
use super::*;
use model::CallEdge;

/// The same graph rebuilt: the functions `keep` keeps, each defined as
/// `define` rewrites it, and each call turned into whatever `calls` makes of
/// it. A test compares functions by display name, so `define` can move a
/// function, or change its qualified name, and so its `SymbolId`, without the
/// comparison noticing.
fn rebuild(
    graph: &CallGraph,
    define: impl Fn(&FunctionNode) -> FunctionNode,
    keep: impl Fn(&SymbolId) -> bool,
    calls: impl Fn(&CallEdge) -> Vec<(SymbolId, SymbolId, Location)>,
) -> CallGraph {
    let mut rebuilt = CallGraph::new();
    let mut ids: HashMap<SymbolId, SymbolId> = HashMap::new();
    let mut functions: Vec<_> = graph.functions().filter(|f| keep(&f.id)).collect();
    functions.sort_by(|a, b| a.name.cmp(&b.name));
    for function in functions {
        let id = rebuilt.add_function(define(function));
        ids.insert(function.id, id);
    }
    for edge in graph.edges() {
        for (from, to, at) in calls(edge) {
            if let (Some(&from), Some(&to)) = (ids.get(&from), ids.get(&to)) {
                rebuilt.add_call(from, to, at);
            }
        }
    }
    rebuilt
}

fn as_is(function: &FunctionNode) -> FunctionNode {
    FunctionNode::new(
        function.name.clone(),
        function.qualified_name.clone(),
        function.definition_location.clone(),
    )
}

/// Each function under a qualified name prefixed by a hash of `salt` and its
/// name, so that every salt sorts the program into an unrelated order — the
/// order the sweeps visit in — while every display name stays put.
fn shuffled(salt: u64) -> impl Fn(&FunctionNode) -> FunctionNode {
    move |function| {
        let mut hash = salt ^ 0xcbf2_9ce4_8422_2325;
        for byte in function.name.bytes() {
            hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
        }
        FunctionNode::new(
            function.name.clone(),
            format!("{hash:016x}::{}", function.qualified_name),
            function.definition_location.clone(),
        )
    }
}

fn same_calls(edge: &CallEdge) -> Vec<(SymbolId, SymbolId, Location)> {
    vec![(edge.caller, edge.callee, edge.call_location.clone())]
}

/// Every rung's grouping by name, and the rung it opens on.
fn shape(graph: &CallGraph, clustering: &Clustering) -> (String, usize) {
    (fingerprint(graph, clustering), clustering.opening_level())
}

/// Run every graph of the corpus and its transform at every setting, and list
/// every pair whose shapes differ.
fn differences(transform: impl Fn(&CallGraph) -> CallGraph) -> Vec<String> {
    let mut out = Vec::new();
    for (name, graph) in corpus() {
        let changed = transform(&graph);
        for settings in settings_matrix() {
            let before = shape(&graph, &Clustering::compute(&graph, settings));
            let after = shape(&changed, &Clustering::compute(&changed, settings));
            if before != after {
                out.push(format!(
                    "{name} at {settings:?}:\n  before {before:?}\n  after  {after:?}"
                ));
            }
        }
    }
    out
}

fn report(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// -- transforms the objective cannot see ---------------------------------------

/// Every method reads calls as undirected weight and direction only through
/// the DAG rule, and a cycle reversed is still a cycle. So reading the program
/// the other way round — which is what an incoming map is — must group it
/// identically.
#[test]
fn reversing_every_call_changes_no_grouping() {
    report(differences(|graph| {
        rebuild(
            graph,
            as_is,
            |_| true,
            |edge| vec![(edge.callee, edge.caller, edge.call_location.clone())],
        )
    }));
}

/// Modularity is a ratio of weights, so multiplying every one leaves it where
/// it was; MCL normalises every column, and its self-loop is the heaviest
/// edge, which scales with the rest. A project that calls everything twice as
/// often is the same program.
#[test]
fn calling_everything_k_times_as_often_changes_no_grouping() {
    for k in [2u32, 3] {
        report(differences(|graph| {
            rebuild(
                graph,
                as_is,
                |_| true,
                |edge| {
                    (0..k)
                        .map(|copy| {
                            let mut at = edge.call_location.clone();
                            at.column = copy;
                            (edge.caller, edge.callee, at)
                        })
                        .collect()
                },
            )
        }));
    }
}

/// A function nothing calls and that calls nothing is not clustered, and a
/// call from a function to itself is not a call between two — so adding
/// either must leave every box as it was.
#[test]
fn isolated_functions_and_recursion_change_nothing() {
    report(differences(|graph| {
        let mut changed = rebuild(graph, as_is, |_| true, same_calls);
        let ids: Vec<SymbolId> = changed.function_ids().copied().collect();
        for (i, id) in ids.iter().enumerate().step_by(3) {
            changed.add_call(
                *id,
                *id,
                Location::new("src/x.c".into(), 90_000 + i as u32, 0),
            );
        }
        for i in 0..5 {
            add(&mut changed, &format!("unused{i}"), 80_000 + i);
        }
        changed
    }));
}

// -- scoping -----------------------------------------------------------------

/// `compute_within` claims to cluster the scope as if it were the whole
/// program — calls leaving it ignored, isolation judged inside it. Building
/// that smaller program and clustering it must therefore draw the same
/// boxes. Labels are not compared: those deliberately still read the full
/// graph, because being called from outside the scope is what makes a member
/// a front door.
#[test]
fn a_scoped_clustering_is_the_clustering_of_the_scope_alone() {
    let mut failures = Vec::new();
    for (name, graph) in corpus() {
        let mut ids: Vec<SymbolId> = graph.function_ids().copied().collect();
        ids.sort_by_key(|id| named(&graph, id));
        let scopes: [(&str, HashSet<SymbolId>); 3] = [
            ("first half", ids[..ids.len() / 2].iter().copied().collect()),
            ("every other", ids.iter().step_by(2).copied().collect()),
            (
                "all but every fifth",
                ids.iter()
                    .enumerate()
                    .filter(|(i, _)| i % 5 != 0)
                    .map(|(_, id)| *id)
                    .collect(),
            ),
        ];
        for (which, scope) in &scopes {
            let alone = rebuild(&graph, as_is, |id| scope.contains(id), same_calls);
            for settings in settings_matrix() {
                let scoped = shape(&graph, &Clustering::compute_within(&graph, scope, settings));
                let whole = shape(&alone, &Clustering::compute(&alone, settings));
                if scoped != whole {
                    failures.push(format!(
                        "{name}, {which} at {settings:?}:\n  scoped {scoped:?}\n  alone  {whole:?}"
                    ));
                }
            }
        }
    }
    report(failures);
}

// -- the order symbols are visited in ----------------------------------------

/// A `SymbolId` hashes the definition's file and line, so an edit above a
/// function, or the same project checked out somewhere else, gives every
/// function a new id. The map must not notice: the order the sweeps visit in
/// is by name, and a definition site only breaks a tie between equal names.
/// Each function moves by a different amount, so the order by line changes
/// too, and the path moves under all of them.
#[test]
fn moving_every_definition_changes_no_grouping() {
    report(differences(|graph| {
        rebuild(
            graph,
            |function| {
                let at = &function.definition_location;
                let shift = function.name.bytes().map(u32::from).sum::<u32>() * 37 % 1000;
                FunctionNode::new(
                    function.name.clone(),
                    function.qualified_name.clone(),
                    Location::new(
                        format!("/elsewhere/{}", at.file_path),
                        at.line * 7 + shift,
                        0,
                    ),
                )
            },
            |_| true,
            same_calls,
        )
    }));
}

/// The same `static` helper in two files, and two overloads in one: equal
/// qualified names, which the order breaks by file and then by line. Two
/// copies of a program whose functions all share their names with the
/// other's, clustered once in each file order: the boxes must be the same up
/// to which file each copy lives in.
#[test]
fn functions_sharing_a_name_are_ordered_by_where_they_are_defined() {
    let mut graph = CallGraph::new();
    let mut site = 1u32;
    for file in ["src/a.c", "src/b.c"] {
        let ids: Vec<SymbolId> = ["init", "parse", "emit", "free"]
            .iter()
            .enumerate()
            .map(|(i, name)| add_in(&mut graph, name, file, i as u32 + 1))
            .collect();
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                site += 1;
                call(&mut graph, ids[i], ids[j], site);
            }
        }
    }
    let ordered = Clustering::compute(&graph, ClusterMethod::Modularity);
    let mut keys: Vec<_> = graph
        .function_ids()
        .map(|id| visit_key(&graph, id))
        .collect();
    keys.sort();
    let files: Vec<&str> = keys.iter().map(|key| key.1).collect();
    assert_eq!(
        files,
        ["src/a.c", "src/b.c", "src/a.c", "src/b.c", "src/a.c", "src/b.c", "src/a.c", "src/b.c"],
        "equal names fall back to the file"
    );
    assert!(!ordered.is_empty());
}

/// Every sweep visits functions in symbol-id order, and a symbol id is a hash
/// — so the order is arbitrary, and an answer that depended on it would be
/// an accident of naming. Where the best grouping is unique, the methods must
/// find it from any order. Each fixture is one of `known_answers`, where that
/// uniqueness was established by exhaustive search; sixteen salted names
/// give sixteen unrelated orders.
#[test]
fn a_unique_answer_does_not_depend_on_the_order_symbols_are_visited_in() {
    let two_triangles = {
        let mut calls = clique(&["a", "b", "c"], 1);
        calls.extend(clique(&["d", "e", "f"], 1));
        calls.push(("c", "d", 1));
        graph_of(&calls)
    };
    let planted = |times| {
        let mut calls = clique(&["p1", "p2", "p3"], times);
        calls.extend(clique(&["q1", "q2", "q3"], times));
        calls.extend(clique(&["r1", "r2", "r3"], times));
        calls.extend([("p3", "q1", 1), ("q3", "r1", 1), ("p1", "r3", 1)]);
        graph_of(&calls)
    };
    let bridged = {
        let mut calls = clique(&["a", "b", "c", "d"], 1);
        calls.extend(clique(&["e", "f", "g", "h"], 1));
        calls.extend([("d", "e", 1), ("c", "f", 1), ("d", "f", 1)]);
        graph_of(&calls)
    };
    let dag_rule = graph_of(&[
        ("a", "c", 5),
        ("b", "d", 5),
        ("a", "b", 1),
        ("b", "c", 1),
        ("x", "y", 5),
    ]);

    let at = |method, grain: f64| {
        ClusterSettings::new(method)
            .with_grain(grain)
            .with_target_top(4)
    };
    let modularity = |grain| {
        vec![
            at(ClusterMethod::Modularity, grain),
            at(ClusterMethod::Leiden, grain),
        ]
    };
    let mut all = modularity(1.0);
    all.push(at(ClusterMethod::Markov, MARKOV_INFLATION));

    let cases: Vec<(&str, CallGraph, Vec<ClusterSettings>)> = vec![
        ("two triangles", two_triangles, all.clone()),
        ("planted ×1", planted(1), all.clone()),
        ("planted ×3", planted(3), all),
        ("bridged at 0.25", bridged.clone(), modularity(0.25)),
        ("bridged at 1", bridged, modularity(1.0)),
        ("dag rule", dag_rule, modularity(1.0)),
    ];

    let opening = |graph: &CallGraph, settings| {
        let clustering = Clustering::compute(graph, settings);
        grouping_at(graph, &clustering, clustering.opening_level())
    };
    let mut failures = Vec::new();
    for (name, graph, settings) in &cases {
        for &settings in settings {
            let expected = opening(graph, settings);
            for salt in 0..16 {
                let salted = rebuild(graph, shuffled(salt), |_| true, same_calls);
                let got = opening(&salted, settings);
                if got != expected {
                    failures.push(format!(
                        "{name}, salt {salt} at {settings:?}: {got:?} where unsalted is {expected:?}"
                    ));
                }
            }
        }
    }
    report(failures);
}
