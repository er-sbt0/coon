//! Everything a clustering must be, whatever the method, the knobs or the
//! graph — checked against the reference in `oracle`, over every rung of
//! every method at several settings, on a corpus of graphs built to have the
//! awkward parts real projects have: cycles, recursion, repeated calls,
//! isolated functions, and pieces that share no call at all.
//!
//! None of these states what the right grouping *is*. They state what no
//! grouping may get wrong, which is what lets them run over graphs too big to
//! work an answer out for.

use super::oracle::{rung, Reference};
use super::*;
use std::collections::BTreeSet;

/// One thing a clustering got wrong, with enough said to find it.
fn fail(out: &mut Vec<String>, level: usize, what: String) {
    out.push(format!("rung {level}: {what}"));
}

/// Every way `clustering` breaks a rule, as sentences. Empty is a pass.
fn violations(
    graph: &CallGraph,
    scope: Option<&HashSet<SymbolId>>,
    clustering: &Clustering,
    asked: ClusterSettings,
) -> Vec<String> {
    let mut out = Vec::new();
    let reference = Reference::of(graph, scope);
    let clustered = reference.clustered();
    let name = |i: usize| reference.names[i].clone();

    if clustering.settings() != asked {
        out.push(format!(
            "remembers {:?} where {asked:?} was asked for",
            clustering.settings()
        ));
    }
    if clustered.is_empty() {
        if !clustering.is_empty() || clustering.level_count() != 0 {
            out.push("a graph with no calls in scope still has boxes".into());
        }
        return out;
    }
    let levels = clustering.level_count();
    if levels == 0 {
        out.push("a graph with calls has no rungs".into());
        return out;
    }
    if clustering.opening_level() >= levels {
        out.push(format!(
            "opens on rung {} of {levels}",
            clustering.opening_level()
        ));
    }
    if clustering.top_level_index() != Some(levels - 1) {
        out.push("the top rung is not the last one".into());
    }

    let strong: Vec<BTreeSet<usize>> = reference
        .strong_components()
        .into_iter()
        .filter(|c| c.len() > 1)
        .collect();
    let mut weak_of = vec![usize::MAX; reference.len()];
    for (component, members) in reference.weak_components().iter().enumerate() {
        for &member in members {
            weak_of[member] = component;
        }
    }
    let unit_count = reference
        .strong_components()
        .iter()
        .filter(|c| c.iter().any(|m| clustered.contains(m)))
        .count();

    let mut seen_ids = HashSet::new();
    for level in 0..levels {
        let ids = clustering.level(level);
        let box_of = rung(&reference, clustering, level);

        // -- a partition of the clustered functions --------------------------
        let mut placed = BTreeSet::new();
        for id in ids {
            let Some(cluster) = clustering.cluster(*id) else {
                fail(
                    &mut out,
                    level,
                    format!("{id:?} is listed but does not resolve"),
                );
                continue;
            };
            if !seen_ids.insert(*id) {
                fail(&mut out, level, format!("{id:?} is listed on two rungs"));
            }
            if cluster.members.is_empty() {
                fail(&mut out, level, format!("{id:?} is empty"));
                continue;
            }
            for member in &cluster.members {
                let index = reference.index_of(member).expect("a function of the graph");
                if !placed.insert(index) {
                    fail(&mut out, level, format!("{} is in two boxes", name(index)));
                }
            }
        }
        for missing in clustered.difference(&placed) {
            fail(&mut out, level, format!("{} is in no box", name(*missing)));
        }
        for extra in placed.difference(&clustered) {
            fail(
                &mut out,
                level,
                format!("{} touches no call but is boxed", name(*extra)),
            );
        }

        // -- each box's own bookkeeping --------------------------------------
        for (position, id) in ids.iter().enumerate() {
            let Some(cluster) = clustering.cluster(*id) else {
                continue;
            };
            if cluster.id != *id || cluster.level != level || cluster.key.level != level {
                fail(
                    &mut out,
                    level,
                    format!("{id:?} disagrees about who or where it is"),
                );
            }
            if !cluster.members.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                fail(
                    &mut out,
                    level,
                    format!("{id:?} lists its members out of order"),
                );
            }
            if cluster.members.first() != Some(&cluster.key.anchor) {
                fail(
                    &mut out,
                    level,
                    format!("{id:?} is not anchored on its lowest member"),
                );
            }
            if clustering.by_key(cluster.key).map(|c| c.id) != Some(*id) {
                fail(
                    &mut out,
                    level,
                    format!("{id:?}'s key does not lead back to it"),
                );
            }
            match cluster.entry_point {
                Some(entry) if cluster.members.contains(&entry) => {
                    if cluster.label != named(graph, &entry) {
                        fail(
                            &mut out,
                            level,
                            format!("{id:?} is not named after its entry point"),
                        );
                    }
                }
                _ => fail(
                    &mut out,
                    level,
                    format!("{id:?} has no entry point among its members"),
                ),
            }
            if position > 0 {
                let before = clustering.cluster(ids[position - 1]).unwrap();
                if before.members[0].0 >= cluster.members[0].0 {
                    fail(
                        &mut out,
                        level,
                        "boxes are not ordered by lowest member".into(),
                    );
                }
            }
            for member in &cluster.members {
                if clustering.cluster_of_at(member, level) != Some(*id) {
                    fail(
                        &mut out,
                        level,
                        format!("cluster_of_at misplaces {}", named(graph, member)),
                    );
                }
                let ancestry = clustering.ancestry(member);
                if ancestry.len() != levels || ancestry.get(level) != Some(id) {
                    fail(
                        &mut out,
                        level,
                        format!("the ancestry of {} skips this box", named(graph, member)),
                    );
                }
                if level == 0 && clustering.cluster_of(member) != Some(*id) {
                    fail(
                        &mut out,
                        level,
                        format!("cluster_of misplaces {}", named(graph, member)),
                    );
                }
            }
        }

        // -- nesting: every rung is the one below it, merged ----------------
        for id in ids {
            let Some(cluster) = clustering.cluster(*id) else {
                continue;
            };
            let expect_parent = level + 1 < levels;
            match cluster.parent.and_then(|p| clustering.cluster(p)) {
                Some(parent) if expect_parent => {
                    if parent.level != level + 1 || !parent.children.contains(id) {
                        fail(&mut out, level, format!("{id:?} and its parent disagree"));
                    }
                }
                None if !expect_parent => {}
                _ => fail(
                    &mut out,
                    level,
                    format!("{id:?} has the wrong parent for its rung"),
                ),
            }
            if level == 0 {
                if !cluster.children.is_empty() {
                    fail(
                        &mut out,
                        level,
                        format!("{id:?} has children on the finest rung"),
                    );
                }
                continue;
            }
            let mut union: Vec<SymbolId> = Vec::new();
            for child in &cluster.children {
                match clustering.cluster(*child) {
                    Some(c) if c.level == level - 1 && c.parent == Some(*id) => {
                        union.extend(&c.members)
                    }
                    _ => fail(
                        &mut out,
                        level,
                        format!("{id:?} has a child that is not its own"),
                    ),
                }
            }
            union.sort_by_key(|s| s.0);
            if union != cluster.members {
                fail(
                    &mut out,
                    level,
                    format!("{id:?} is not the union of its children"),
                );
            }
        }
        if level > 0 && ids.len() >= clustering.level(level - 1).len() {
            fail(&mut out, level, "a rung that merges nothing".into());
        }

        // -- the cluster graph -----------------------------------------------
        if !placed.is_subset(&clustered) || placed.len() != clustered.len() {
            // The checks below read every called function's box.
            continue;
        }
        let quotient = reference.quotient(&box_of);
        if !quotient.is_acyclic() {
            fail(&mut out, level, "the cluster graph has a cycle".into());
        }
        let between: usize = quotient.between.values().sum();
        let inside: usize = quotient.inside.values().sum();
        if between + inside != reference.calls.len() {
            fail(
                &mut out,
                level,
                "calls were lost between the graph and its boxes".into(),
            );
        }
        out.extend(
            aggregate_disagreements(graph, scope, &reference, &box_of, ids.len())
                .into_iter()
                .map(|what| format!("rung {level}: {what}")),
        );

        for component in &strong {
            let boxes: BTreeSet<_> = component.iter().map(|&m| box_of[m]).collect();
            if boxes.len() > 1 {
                let names: Vec<String> = component.iter().map(|&m| name(m)).collect();
                fail(&mut out, level, format!("the cycle {names:?} is split"));
            }
        }
        for position in 0..ids.len() {
            let pieces: BTreeSet<usize> = clustered
                .iter()
                .filter(|&&m| box_of[m] == Some(position))
                .map(|&m| weak_of[m])
                .collect();
            if pieces.len() > 1 {
                fail(
                    &mut out,
                    level,
                    format!(
                        "box {position} spans {} unconnected parts of the program",
                        pieces.len()
                    ),
                );
            }
            if asked.method() == ClusterMethod::Leiden
                && !box_is_connected(&reference, &box_of, position)
            {
                fail(
                    &mut out,
                    level,
                    format!("Leiden box {position} is not connected by its own calls"),
                );
            }
        }

        // -- the top rung folds its leftovers -------------------------------
        // Only once something merged: a ladder that never ran is all
        // singletons by design. And only where a fold was legal — a box every
        // one of whose neighbours would close a cycle has to stay alone.
        if level == levels - 1 && ids.len() < unit_count {
            for position in 0..ids.len() {
                let size = clustered
                    .iter()
                    .filter(|&&m| box_of[m] == Some(position))
                    .count();
                if size != 1 {
                    continue;
                }
                let neighbours: BTreeSet<usize> = quotient
                    .between
                    .keys()
                    .filter_map(|&(a, b)| match (a == position, b == position) {
                        (true, _) => Some(b),
                        (_, true) => Some(a),
                        _ => None,
                    })
                    .collect();
                let foldable = neighbours.iter().find(|&&into| {
                    let merged: Vec<Option<usize>> = box_of
                        .iter()
                        .map(|b| if *b == Some(position) { Some(into) } else { *b })
                        .collect();
                    reference.quotient(&merged).is_acyclic()
                });
                if let Some(into) = foldable {
                    let member = clustered
                        .iter()
                        .find(|&&m| box_of[m] == Some(position))
                        .unwrap();
                    fail(
                        &mut out,
                        level,
                        format!(
                            "{} is alone at the top but could join box {into}",
                            name(*member)
                        ),
                    );
                }
            }
        }
    }
    out
}

/// Whether the functions of one box reach each other through calls among
/// themselves, in either direction.
fn box_is_connected(reference: &Reference, box_of: &[Option<usize>], position: usize) -> bool {
    let members: Vec<usize> = (0..reference.len())
        .filter(|&m| box_of[m] == Some(position))
        .collect();
    let Some(&start) = members.first() else {
        return true;
    };
    let mut seen = BTreeSet::from([start]);
    let mut stack = vec![start];
    while let Some(at) = stack.pop() {
        for &(a, b) in &reference.calls {
            let other = if a == at {
                b
            } else if b == at {
                a
            } else {
                continue;
            };
            if box_of[other] == Some(position) && seen.insert(other) {
                stack.push(other);
            }
        }
    }
    seen.len() == members.len()
}

/// The clusterer's own cluster graph — `build_base_graph` then `aggregate`,
/// over the partition a rung drew — against the reference's, call for call.
/// This is the weighted graph every coarser pass reads, so a weight lost or
/// doubled here would be a ladder built on the wrong graph with nothing above
/// it able to tell.
fn aggregate_disagreements(
    graph: &CallGraph,
    scope: Option<&HashSet<SymbolId>>,
    reference: &Reference,
    box_of: &[Option<usize>],
    count: usize,
) -> Vec<String> {
    let base = build_base_graph(graph, scope);
    let mut groups = vec![usize::MAX; base.graph.node_count()];
    for (node, symbol) in base.symbols.iter().enumerate() {
        let index = reference.index_of(symbol).expect("a function of the graph");
        let Some(group) = box_of[index] else {
            return vec![format!(
                "{} is in the base graph but in no box",
                reference.names[index]
            )];
        };
        let unit = base.units[node];
        if groups[unit] != usize::MAX && groups[unit] != group {
            return vec!["one condensed unit is in two boxes".into()];
        }
        groups[unit] = group;
    }
    let coarse = aggregate(&base.graph, &groups, count);
    let quotient = reference.quotient(box_of);

    let mut out = Vec::new();
    for a in 0..count {
        let inside = quotient.inside.get(&a).copied().unwrap_or(0) as f64;
        if (coarse.self_loops[a] - inside).abs() > 1e-9 {
            out.push(format!(
                "aggregate keeps {} calls inside box {a} where the graph has {inside}",
                coarse.self_loops[a]
            ));
        }
        let calls =
            |from: usize, to: usize| quotient.between.get(&(from, to)).copied().unwrap_or(0);
        for b in 0..count {
            if a == b {
                continue;
            }
            let expected = (calls(a, b) + calls(b, a)) as f64;
            let weighed = coarse.adj[a]
                .iter()
                .find(|(n, _)| *n == b)
                .map_or(0.0, |(_, w)| *w);
            if (weighed - expected).abs() > 1e-9 {
                out.push(format!(
                    "aggregate weighs {a}–{b} at {weighed} where the graph has {expected}"
                ));
            }
            if coarse.out[a].contains(&b) != (calls(a, b) > 0) {
                out.push(format!("aggregate has the direction of {a}→{b} wrong"));
            }
        }
    }
    out
}

// -- the corpus --------------------------------------------------------------

/// A graph with the parts a tidy fixture leaves out, from a fixed generator so
/// it is the same every run: mostly forward calls, some repeated; some back
/// calls, which close cycles of every length; recursion; functions nothing
/// touches; and two halves that share no call.
fn messy_graph(seed: u64, n: u32) -> CallGraph {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = (0..n)
        .map(|i| add(&mut graph, &format!("m{i}"), i + 1))
        .collect();
    let mut state = seed;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let half = n as usize / 2;
    let mut line = 10_000u32;
    for i in 0..n as usize {
        // Every seventh function is left alone entirely.
        if i % 7 == 6 {
            continue;
        }
        let (start, end) = if i < half {
            (0, half)
        } else {
            (half, n as usize)
        };
        for _ in 0..1 + next() % 3 {
            let roll = next() % 10;
            let j = if roll < 2 {
                // A call back up: a cycle, of whatever length the gap is.
                start + (next() as usize % (i - start + 1))
            } else {
                (i + 1 + next() as usize % 6).min(end - 1)
            };
            if j % 7 == 6 {
                continue;
            }
            let times = 1 + (next() % 3) as u32;
            call_often(&mut graph, ids[i], ids[j], line, times);
            line += times;
        }
    }
    graph
}

/// Several programs that share nothing: ordered cliques of five, chains, and
/// a pair of mutually recursive functions with a tail.
fn forest() -> CallGraph {
    let mut graph = CallGraph::new();
    let mut line = 1u32;
    let mut fresh = |graph: &mut CallGraph, name: String| {
        line += 1;
        add(graph, &name, line)
    };
    let mut site = 50_000u32;
    let mut link = |graph: &mut CallGraph, a, b| {
        site += 1;
        call(graph, a, b, site);
    };
    for k in 0..4 {
        let ids: Vec<SymbolId> = (0..5)
            .map(|i| fresh(&mut graph, format!("k{k}_{i}")))
            .collect();
        for i in 0..5 {
            for j in i + 1..5 {
                link(&mut graph, ids[i], ids[j]);
            }
        }
    }
    for c in 0..3 {
        let ids: Vec<SymbolId> = (0..6)
            .map(|i| fresh(&mut graph, format!("c{c}_{i}")))
            .collect();
        for pair in ids.windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
    }
    let ping = fresh(&mut graph, "ping".into());
    let pong = fresh(&mut graph, "pong".into());
    let tail = fresh(&mut graph, "tail".into());
    link(&mut graph, ping, pong);
    link(&mut graph, pong, ping);
    link(&mut graph, pong, tail);
    graph
}

/// A hub everything calls, and callers that each also have a helper of their
/// own: the shape where a shared utility could glue a whole program together.
fn star() -> CallGraph {
    let mut graph = CallGraph::new();
    let hub = add(&mut graph, "hub", 1);
    for i in 0..16u32 {
        let caller = add(&mut graph, &format!("caller{i}"), 10 + i * 2);
        let helper = add(&mut graph, &format!("helper{i}"), 11 + i * 2);
        call(&mut graph, caller, hub, 1000 + i);
        call_often(&mut graph, caller, helper, 2000 + i * 10, 3);
    }
    graph
}

fn corpus() -> Vec<(&'static str, CallGraph)> {
    vec![
        ("layered_dag(60)", layered_dag(60).0),
        ("layered_dag(25)", layered_dag(25).0),
        ("chain_of(40)", chain_of(40).0),
        ("crossing_graph", crossing_graph()),
        ("two_groups_one_stray_call", two_groups_one_stray_call()),
        ("messy_graph(1, 60)", messy_graph(0x9E37_79B9_7F4A_7C15, 60)),
        ("messy_graph(2, 40)", messy_graph(0xD1B5_4A32_D192_ED03, 40)),
        ("messy_graph(3, 90)", messy_graph(0x2545_F491_4F6C_DD1D, 90)),
        ("forest", forest()),
        ("star", star()),
    ]
}

/// The settings each method is run at: its grain at the low end, the default
/// and the high end, each against the narrowest target and the default one.
fn settings_matrix() -> Vec<ClusterSettings> {
    let mut out = Vec::new();
    for &method in ClusterMethod::ALL {
        let grains: &[f64] = match method {
            ClusterMethod::Modularity | ClusterMethod::Leiden => &[0.5, 1.0, 2.0],
            ClusterMethod::Markov => &[1.5, 2.0, 3.0],
        };
        for &grain in grains {
            for target in [4, TARGET_TOP_CLUSTERS] {
                out.push(
                    ClusterSettings::new(method)
                        .with_grain(grain)
                        .with_target_top(target),
                );
            }
        }
    }
    out
}

fn report(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} violations:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn every_rung_of_every_method_keeps_every_invariant() {
    let mut failures = Vec::new();
    for (name, graph) in corpus() {
        for settings in settings_matrix() {
            let clustering = Clustering::compute(&graph, settings);
            for problem in violations(&graph, None, &clustering, settings) {
                failures.push(format!("{name} at {settings:?}: {problem}"));
            }
        }
    }
    report(failures);
}

/// A scope is a smaller graph with calls leaving it, which is a different
/// thing to get wrong: a call with one end outside must count for nothing.
#[test]
fn every_scoped_clustering_keeps_every_invariant() {
    let mut failures = Vec::new();
    for (name, graph) in corpus() {
        let mut ids: Vec<SymbolId> = graph.function_ids().copied().collect();
        ids.sort_by_key(|id| named(&graph, id));
        let scopes: [(&str, HashSet<SymbolId>); 2] = [
            ("first half", ids[..ids.len() / 2].iter().copied().collect()),
            ("every other", ids.iter().step_by(2).copied().collect()),
        ];
        for (which, scope) in &scopes {
            for &method in ClusterMethod::ALL {
                let settings = ClusterSettings::new(method).with_target_top(4);
                let clustering = Clustering::compute_within(&graph, scope, settings);
                for problem in violations(&graph, Some(scope), &clustering, settings) {
                    failures.push(format!("{name}, {which} at {settings:?}: {problem}"));
                }
            }
        }
    }
    report(failures);
}

/// The corpus is only worth sweeping if it reaches the code: graphs that
/// actually coarsen, cycles that actually condense, and parts that actually
/// share nothing.
#[test]
fn the_corpus_exercises_what_the_invariants_are_about() {
    let corpus = corpus();
    let mut climbed = false;
    let mut condensed = false;
    let mut disconnected = false;
    for (_, graph) in &corpus {
        let reference = Reference::of(graph, None);
        condensed |= reference.strong_components().iter().any(|c| c.len() > 1);
        disconnected |= reference
            .weak_components()
            .iter()
            .filter(|c| c.iter().any(|m| reference.clustered().contains(m)))
            .count()
            > 1;
        for &method in ClusterMethod::ALL {
            climbed |= Clustering::compute(graph, method).level_count() > 2;
        }
    }
    assert!(climbed && condensed && disconnected);
}

/// The checker is only worth running if it can fail. Hand it clusterings that
/// are wrong in the ways it looks for and see that it says so.
#[test]
fn the_invariants_catch_a_broken_clustering() {
    let graph = two_groups_one_stray_call();
    let settings = ClusterSettings::new(ClusterMethod::Modularity).with_target_top(4);
    let good = Clustering::compute(&graph, settings);
    assert_eq!(
        violations(&graph, None, &good, settings),
        Vec::<String>::new()
    );

    let complain = |clustering: &Clustering| violations(&graph, None, clustering, settings);

    // A function dropped from a box.
    let mut lost = good.clone();
    lost.clusters[0].members.pop();
    assert!(complain(&lost).iter().any(|p| p.contains("in no box")));

    // The second group's last function moved into the first: `d → e` still
    // leaves the first box, and `e f g → h` now come back into it.
    let a = graph.find_function_by_name("a").unwrap().id;
    let h = graph.find_function_by_name("h").unwrap().id;
    let mut crossed = good.clone();
    let (from, to) = (
        crossed.cluster_of_at(&h, 0).unwrap(),
        crossed.cluster_of_at(&a, 0).unwrap(),
    );
    assert_ne!(
        from, to,
        "the fixture splits the two groups at the finest rung"
    );
    crossed.clusters[from.0 as usize]
        .members
        .retain(|m| *m != h);
    crossed.clusters[to.0 as usize].members.push(h);
    crossed.clusters[to.0 as usize].members.sort_by_key(|s| s.0);
    let problems = complain(&crossed);
    assert!(
        problems.iter().any(|p| p.contains("cycle")),
        "{problems:#?}"
    );

    // The settings it remembers.
    assert!(violations(&graph, None, &good, settings.with_grain(2.0))
        .iter()
        .any(|p| p.contains("remembers")));
}
