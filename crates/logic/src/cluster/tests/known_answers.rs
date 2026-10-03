//! Graphs small enough to work the right answer out for, and the answer.
//!
//! Each fixture states the boxes and the cluster graph it expects, and why.
//! For the modularity methods the "why" is checked rather than trusted: the
//! reference in `oracle` tries every partition there is and must land on the
//! same answer as the hand derivation before the clusterer is held to it. For
//! Markov there is no optimum to search for, so it is held to the answer only
//! where the groups are cliques joined by a call or two — the case MCL is
//! textbook-certain about.
//!
//! Every answer is read off the opening rung, the one the method stands
//! behind (`Clustering::opening_level`), at the narrowest target the settings
//! allow, so that a fixture above four functions is always coarsened.

use super::oracle::{rung, Reference};
use super::*;

/// A graph from a list of calls, `(caller, callee, times)`. Functions are
/// defined in the order they first appear, each call site on its own line.
pub(super) fn graph_of(calls: &[(&str, &str, u32)]) -> CallGraph {
    let mut graph = CallGraph::new();
    let mut ids: HashMap<&str, SymbolId> = HashMap::new();
    let mut site = 10_000u32;
    for &(from, to, times) in calls {
        for name in [from, to] {
            let line = ids.len() as u32 + 1;
            ids.entry(name)
                .or_insert_with(|| add(&mut graph, name, line));
        }
        call_often(&mut graph, ids[from], ids[to], site, times);
        site += times;
    }
    graph
}

/// Every call of an ordered clique: each member calls every later one,
/// `times` times. Cohesive as a DAG allows.
pub(super) fn clique<'a>(names: &[&'a str], times: u32) -> Vec<(&'a str, &'a str, u32)> {
    let mut calls = Vec::new();
    for i in 0..names.len() {
        for j in i + 1..names.len() {
            calls.push((names[i], names[j], times));
        }
    }
    calls
}

/// One rung as a test states it: the boxes, each as sorted names, and the
/// cluster graph between them, each box named by its members joined by a
/// space.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    boxes: Vec<Vec<String>>,
    between: BTreeMap<(String, String), usize>,
    inside: BTreeMap<String, usize>,
}

fn box_name(members: &[String]) -> String {
    members.join(" ")
}

impl Answer {
    fn expected(
        boxes: &[&[&str]],
        between: &[(&str, &str, usize)],
        inside: &[(&str, usize)],
    ) -> Self {
        let boxes = groups(boxes);
        let named = |text: &str| {
            let mut members: Vec<String> = text.split(' ').map(String::from).collect();
            members.sort();
            let name = box_name(&members);
            assert!(
                boxes.iter().any(|b| box_name(b) == name),
                "`{text}` is not one of the boxes"
            );
            name
        };
        Self {
            between: between
                .iter()
                .map(|&(a, b, calls)| ((named(a), named(b)), calls))
                .collect(),
            inside: inside.iter().map(|&(a, calls)| (named(a), calls)).collect(),
            boxes,
        }
    }

    /// What `settings` draws on its opening rung.
    fn computed(graph: &CallGraph, settings: ClusterSettings) -> Self {
        let clustering = Clustering::compute(graph, settings);
        let level = clustering.opening_level();
        let reference = Reference::of(graph, None);
        let box_of = rung(&reference, &clustering, level);

        let mut members: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (index, slot) in box_of.iter().enumerate() {
            if let Some(position) = slot {
                members
                    .entry(*position)
                    .or_default()
                    .push(reference.names[index].clone());
            }
        }
        let name = |position: &usize| box_name(&members[position]);
        let quotient = reference.quotient(&box_of);
        let mut boxes: Vec<Vec<String>> = members.values().cloned().collect();
        boxes.sort();
        Self {
            between: quotient
                .between
                .iter()
                .map(|((a, b), calls)| ((name(a), name(b)), *calls))
                .collect(),
            inside: quotient
                .inside
                .iter()
                .map(|(a, calls)| (name(a), *calls))
                .collect(),
            boxes,
        }
    }
}

/// Every best partition the exhaustive search finds, as sorted names.
fn optima(graph: &CallGraph, resolution: f64, acyclic_only: bool) -> Vec<Vec<Vec<String>>> {
    let reference = Reference::of(graph, None);
    let (_, winners) = reference.best_partitions(resolution, acyclic_only);
    let mut out: Vec<Vec<Vec<String>>> = winners.iter().map(|p| reference.names_of(p)).collect();
    out.sort();
    out
}

fn modularity_at(method: ClusterMethod, resolution: f64) -> ClusterSettings {
    ClusterSettings::new(method)
        .with_grain(resolution)
        .with_target_top(4)
}

fn markov_at(inflation: f64) -> ClusterSettings {
    ClusterSettings::new(ClusterMethod::Markov)
        .with_grain(inflation)
        .with_target_top(4)
}

/// Both modularity methods at γ, then — where the fixture is one MCL is
/// certain about — Markov at its default inflation.
fn every_method(resolution: f64, with_markov: bool) -> Vec<ClusterSettings> {
    let mut out = vec![
        modularity_at(ClusterMethod::Modularity, resolution),
        modularity_at(ClusterMethod::Leiden, resolution),
    ];
    if with_markov {
        out.push(markov_at(MARKOV_INFLATION));
    }
    out
}

fn assert_answer(graph: &CallGraph, settings: &[ClusterSettings], expected: &Answer) {
    for &settings in settings {
        assert_eq!(&Answer::computed(graph, settings), expected, "{settings:?}");
    }
}

// -- cliques joined by a call --------------------------------------------------

/// Two triangles, `a b c` and `d e f`, joined by `c → d`. Seven calls, so
/// m = 7; each triangle holds three and has degree 7, so splitting at the
/// bridge scores 2 × (3/7 − ¼) = 5/14, where one box scores 0 and moving any
/// one function across costs it two of its three calls. Unique.
#[test]
fn two_triangles_split_at_the_bridge() {
    let mut calls = clique(&["a", "b", "c"], 1);
    calls.extend(clique(&["d", "e", "f"], 1));
    calls.push(("c", "d", 1));
    let graph = graph_of(&calls);

    let expected = Answer::expected(
        &[&["a", "b", "c"], &["d", "e", "f"]],
        &[("a b c", "d e f", 1)],
        &[("a b c", 3), ("d e f", 3)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(1.0, true), &expected);
}

/// Three triangles in a row, joined `c0 → a1` and `c1 → a2`. m = 11; the
/// three boxes have degree 7, 8 and 7, so Q = 9/11 − 162/484 ≈ 0.483, where
/// merging the last two (L = 7, d = 15) drops it to 10/11 − 274/484 ≈ 0.343.
/// The search confirms nothing else beats it.
#[test]
fn a_chain_of_triangles_is_one_box_a_triangle() {
    let mut calls = Vec::new();
    for t in ["0", "1", "2"] {
        let names = [format!("a{t}"), format!("b{t}"), format!("c{t}")];
        let names: Vec<&str> = names.iter().map(|n| &*n.clone().leak()).collect();
        calls.extend(clique(&names, 1));
    }
    calls.push(("c0", "a1", 1));
    calls.push(("c1", "a2", 1));
    let graph = graph_of(&calls);

    let expected = Answer::expected(
        &[
            &["a0", "b0", "c0"],
            &["a1", "b1", "c1"],
            &["a2", "b2", "c2"],
        ],
        &[("a0 b0 c0", "a1 b1 c1", 1), ("a1 b1 c1", "a2 b2 c2", 1)],
        &[("a0 b0 c0", 3), ("a1 b1 c1", 3), ("a2 b2 c2", 3)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(1.0, true), &expected);
}

/// Three groups of three, every call inside a group made three times and
/// every call between groups once: `p → q`, `q → r` and `p → r`, which is
/// one way round. The boxes are the planted groups and the cluster graph
/// carries exactly the planted weights — the test that a weight is read as a
/// count of call sites, not as a yes or no.
#[test]
fn planted_groups_come_back_with_their_weights() {
    let mut calls = clique(&["p1", "p2", "p3"], 3);
    calls.extend(clique(&["q1", "q2", "q3"], 3));
    calls.extend(clique(&["r1", "r2", "r3"], 3));
    calls.extend([("p3", "q1", 1), ("q3", "r1", 1), ("p1", "r3", 1)]);
    let graph = graph_of(&calls);

    let expected = Answer::expected(
        &[
            &["p1", "p2", "p3"],
            &["q1", "q2", "q3"],
            &["r1", "r2", "r3"],
        ],
        &[
            ("p1 p2 p3", "q1 q2 q3", 1),
            ("q1 q2 q3", "r1 r2 r3", 1),
            ("p1 p2 p3", "r1 r2 r3", 1),
        ],
        &[("p1 p2 p3", 9), ("q1 q2 q3", 9), ("r1 r2 r3", 9)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    // Markov is held to the same groups by `mcl_unites_two_attractors_of_one_triangle`.
    assert_answer(&graph, &every_method(1.0, false), &expected);
}

/// The planted groups again, at one call each. Every triangle's flow
/// settles on two attractors, its first and last member, sharing it half and
/// half — MCL's overlap case, which reads as one cluster. But the two are
/// only one box with the middle member in it: `p1 → p2 → p3` makes `{p1 p3}`
/// and `{p2}` call each other. Uniting the attractors before anything had
/// drained refused the union as a cycle, drained `p2` into `p1`, and left
/// `{p1 p2} {p3}` — six boxes, three of them alone.
#[test]
fn mcl_unites_two_attractors_of_one_triangle() {
    let mut calls = clique(&["p1", "p2", "p3"], 1);
    calls.extend(clique(&["q1", "q2", "q3"], 1));
    calls.extend(clique(&["r1", "r2", "r3"], 1));
    calls.extend([("p3", "q1", 1), ("q3", "r1", 1), ("p1", "r3", 1)]);
    let graph = graph_of(&calls);

    let expected = Answer::expected(
        &[
            &["p1", "p2", "p3"],
            &["q1", "q2", "q3"],
            &["r1", "r2", "r3"],
        ],
        &[
            ("p1 p2 p3", "q1 q2 q3", 1),
            ("q1 q2 q3", "r1 r2 r3", 1),
            ("p1 p2 p3", "r1 r2 r3", 1),
        ],
        &[("p1 p2 p3", 3), ("q1 q2 q3", 3), ("r1 r2 r3", 3)],
    );
    assert_answer(&graph, &[markov_at(MARKOV_INFLATION)], &expected);
}

/// The first triangle of `two_triangles_split_at_the_bridge` with `b` calling
/// back into `a`: `a` and `b` are one function as far as grouping goes, and
/// the answer is the same two boxes, the first now holding four calls.
#[test]
fn mutual_recursion_inside_a_group_changes_nothing_but_the_count() {
    let mut calls = clique(&["a", "b", "c"], 1);
    calls.push(("b", "a", 1));
    calls.extend(clique(&["d", "e", "f"], 1));
    calls.push(("c", "d", 1));
    let graph = graph_of(&calls);

    let expected = Answer::expected(
        &[&["a", "b", "c"], &["d", "e", "f"]],
        &[("a b c", "d e f", 1)],
        &[("a b c", 4), ("d e f", 3)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(1.0, true), &expected);
}

// -- the resolution ----------------------------------------------------------

/// Two ordered 4-cliques joined by three calls, `d → e`, `c → f`, `d → f`.
/// m = 15, and either clique has degree 15 = m, so splitting at the bridge
/// scores 12/15 − γ/2 against one box's 1 − γ: the split wins exactly when
/// γ > 2·3/15 = 0.4. γ is the user's knob, so the knob must move the answer
/// across that line.
fn two_cliques_three_bridges() -> CallGraph {
    let mut calls = clique(&["a", "b", "c", "d"], 1);
    calls.extend(clique(&["e", "f", "g", "h"], 1));
    calls.extend([("d", "e", 1), ("c", "f", 1), ("d", "f", 1)]);
    graph_of(&calls)
}

#[test]
fn below_the_crossover_resolution_two_cliques_are_one_box() {
    let graph = two_cliques_three_bridges();
    let expected = Answer::expected(
        &[&["a", "b", "c", "d", "e", "f", "g", "h"]],
        &[],
        &[("a b c d e f g h", 15)],
    );
    assert_eq!(optima(&graph, 0.25, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(0.25, false), &expected);
}

#[test]
fn above_the_crossover_resolution_two_cliques_are_two_boxes() {
    let graph = two_cliques_three_bridges();
    let expected = Answer::expected(
        &[&["a", "b", "c", "d"], &["e", "f", "g", "h"]],
        &[("a b c d", "e f g h", 3)],
        &[("a b c d", 6), ("e f g h", 6)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(1.0, false), &expected);
}

// -- the DAG rule --------------------------------------------------------------

/// `a → c` and `b → d` five times each, `a → b` and `b → c` once, and a
/// separate pair `x → y` five times. m = 17.
///
/// Unconstrained, the answer is `{a c} {b d}` (Q ≈ 0.339 for the four) — but
/// `a → b` and `b → c` then run between those two boxes in both directions.
/// Any box holding `a` and `c` without `b` has that cycle, and every other
/// split of the four was worked: `{a} {c} {b d}` ≈ 0.107, `{a b c} {d}` ≈
/// 0.078, `{a b d} {c}` ≈ 0.042, against ≈ 0.208 for all four together. So
/// the rule costs a third of the four's modularity and puts them in one box,
/// and the pair beside them is untouched.
#[test]
fn the_dag_rule_takes_the_best_acyclic_grouping_not_the_best_grouping() {
    let graph = graph_of(&[
        ("a", "c", 5),
        ("b", "d", 5),
        ("a", "b", 1),
        ("b", "c", 1),
        ("x", "y", 5),
    ]);

    assert_eq!(
        optima(&graph, 1.0, false),
        vec![groups(&[&["a", "c"], &["b", "d"], &["x", "y"]])],
        "the fixture has a cyclic optimum, or it tests nothing"
    );
    let expected = Answer::expected(
        &[&["a", "b", "c", "d"], &["x", "y"]],
        &[],
        &[("a b c d", 12), ("x y", 5)],
    );
    assert_eq!(optima(&graph, 1.0, true), vec![expected.boxes.clone()]);
    assert_answer(&graph, &every_method(1.0, false), &expected);
}

// -- a shared utility ----------------------------------------------------------

/// A hub that four callers call once each, every caller also calling a
/// helper of its own three times. m = 16. The callers with their helpers
/// score 4 × (3/16 − (7/32)²) ≈ 0.559, less the hub alone at (4/32)²: 0.543.
/// The hub joining one pair is better — 0.551 — and which pair is a four-way
/// tie; joining two pairs falls to 0.463. So the hub is a member of one
/// caller's box, and never the glue of two.
#[test]
fn a_hub_joins_one_caller_and_glues_none() {
    let mut calls = Vec::new();
    for i in 0..4 {
        let caller: &str = format!("c{i}").leak();
        let helper: &str = format!("h{i}").leak();
        calls.push((caller, "hub", 1));
        calls.push((caller, helper, 3));
    }
    let graph = graph_of(&calls);

    let winners = optima(&graph, 1.0, true);
    let pair = |i: usize| vec![format!("c{i}"), format!("h{i}")];
    let mut expected: Vec<Vec<Vec<String>>> = (0..4)
        .map(|joined| {
            let mut boxes: Vec<Vec<String>> = (0..4)
                .map(|i| {
                    let mut b = pair(i);
                    if i == joined {
                        b.push("hub".into());
                        b.sort();
                    }
                    b
                })
                .collect();
            boxes.sort();
            boxes
        })
        .collect();
    expected.sort();
    assert_eq!(winners, expected);

    for settings in every_method(1.0, false) {
        let computed = Answer::computed(&graph, settings);
        assert!(
            winners.contains(&computed.boxes),
            "{settings:?} drew {:?}",
            computed.boxes
        );
    }
    // MCL has no optimum to search, but it has the property: the hub's flow
    // is spread four ways and does not pull two callers into one system.
    let markov = Answer::computed(&graph, markov_at(MARKOV_INFLATION));
    for b in &markov.boxes {
        let callers = b.iter().filter(|n| n.starts_with('c')).count();
        assert!(
            callers <= 1,
            "MCL glued callers together: {:?}",
            markov.boxes
        );
    }
}
