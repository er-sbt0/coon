//! A second, deliberately naive reading of what a clustering should be, for
//! the clusterer to be checked against.
//!
//! Nothing here shares code with the module under test. It works on names and
//! small indices rather than `WeightedGraph`, recomputes everything from the
//! `CallGraph` each time it is asked, and prefers the textbook formula to the
//! fast one: it is only worth anything as long as it is obviously right, and
//! it is only ever run on graphs a few dozen functions wide.

use super::*;
use std::collections::BTreeSet;

/// A call graph as the clusterer weighs it, by name: every function in scope,
/// and one entry per call site between two of them.
///
/// Recursion is dropped, as `build_base_graph` drops it — a function cannot
/// be grouped with itself, so a self-call is not evidence of anything — and so
/// is any call with an end outside the scope. Names must be unique, which every
/// fixture in this module arranges.
pub(super) struct Reference {
    /// Sorted, so an index is stable for a given graph.
    pub names: Vec<String>,
    pub calls: Vec<(usize, usize)>,
    ids: HashMap<SymbolId, usize>,
}

impl Reference {
    pub fn of(graph: &CallGraph, scope: Option<&HashSet<SymbolId>>) -> Self {
        let mut named: Vec<(String, SymbolId)> = graph
            .function_ids()
            .filter(|id| scope.is_none_or(|s| s.contains(id)))
            .map(|id| (super::named(graph, id), *id))
            .collect();
        named.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(
            named.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "the reference reads a graph by name, so names must be unique"
        );
        let ids: HashMap<SymbolId, usize> = named
            .iter()
            .enumerate()
            .map(|(index, (_, id))| (*id, index))
            .collect();
        let calls = graph
            .edges()
            .filter_map(|edge| Some((*ids.get(&edge.caller)?, *ids.get(&edge.callee)?)))
            .filter(|(from, to)| from != to)
            .collect();
        Self {
            names: named.into_iter().map(|(name, _)| name).collect(),
            calls,
            ids,
        }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn index_of(&self, symbol: &SymbolId) -> Option<usize> {
        self.ids.get(symbol).copied()
    }

    /// The functions any clustering of this graph must place: those with at
    /// least one call to or from another function in scope.
    pub fn clustered(&self) -> BTreeSet<usize> {
        self.calls.iter().flat_map(|&(a, b)| [a, b]).collect()
    }

    /// Who reaches whom along calls, by a walk from every function. Quadratic
    /// and proud of it.
    fn reaches(&self) -> Vec<Vec<bool>> {
        let mut successors = vec![Vec::new(); self.len()];
        for &(a, b) in &self.calls {
            successors[a].push(b);
        }
        (0..self.len())
            .map(|start| {
                let mut seen = vec![false; self.len()];
                seen[start] = true;
                let mut stack = vec![start];
                while let Some(at) = stack.pop() {
                    for &next in &successors[at] {
                        if !seen[next] {
                            seen[next] = true;
                            stack.push(next);
                        }
                    }
                }
                seen
            })
            .collect()
    }

    /// Groups of functions that each reach every other: two functions are in
    /// one group exactly when each reaches the other. Singletons included.
    pub fn strong_components(&self) -> Vec<BTreeSet<usize>> {
        let reaches = self.reaches();
        let mut placed = vec![false; self.len()];
        let mut components = Vec::new();
        for a in 0..self.len() {
            if placed[a] {
                continue;
            }
            let component: BTreeSet<usize> = (0..self.len())
                .filter(|&b| reaches[a][b] && reaches[b][a])
                .collect();
            for &member in &component {
                placed[member] = true;
            }
            components.push(component);
        }
        components
    }

    /// Groups joined by calls in either direction. Singletons included.
    pub fn weak_components(&self) -> Vec<BTreeSet<usize>> {
        let mut label: Vec<usize> = (0..self.len()).collect();
        // Relabel to the smaller end of every call until nothing changes:
        // slow, and impossible to get wrong.
        loop {
            let mut changed = false;
            for &(a, b) in &self.calls {
                let low = label[a].min(label[b]);
                for end in [a, b] {
                    if label[end] != low {
                        label[end] = low;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let mut groups: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for (node, &root) in label.iter().enumerate() {
            groups.entry(root).or_default().insert(node);
        }
        groups.into_values().collect()
    }

    /// Newman–Girvan modularity with a resolution, straight off the formula:
    ///
    /// Q = Σ_c [ L_c / m − γ (d_c / 2m)² ]
    ///
    /// over the graph with every call taken as an undirected edge, where `m` is
    /// the number of calls, `L_c` the calls with both ends in box `c`, and
    /// `d_c` the calls at its members, counted at each end. `box_of[i]` is
    /// the box of function `i`, `None` for one no box holds — which must be
    /// one no call touches, or the partition is not one of this graph.
    pub fn modularity(&self, box_of: &[Option<usize>], resolution: f64) -> f64 {
        let m = self.calls.len() as f64;
        let mut inside: HashMap<usize, f64> = HashMap::new();
        let mut degree: HashMap<usize, f64> = HashMap::new();
        for &(a, b) in &self.calls {
            let (ca, cb) = (
                box_of[a].expect("every called function is in a box"),
                box_of[b].expect("every called function is in a box"),
            );
            if ca == cb {
                *inside.entry(ca).or_default() += 1.0;
            }
            *degree.entry(ca).or_default() += 1.0;
            *degree.entry(cb).or_default() += 1.0;
        }
        degree
            .iter()
            .map(|(c, d)| {
                inside.get(c).copied().unwrap_or(0.0) / m - resolution * (d / (2.0 * m)).powi(2)
            })
            .sum()
    }

    /// The cluster graph of a partition: calls between each ordered pair of
    /// boxes, and calls kept inside each box.
    pub fn quotient(&self, box_of: &[Option<usize>]) -> Quotient {
        let mut quotient = Quotient::default();
        for &(a, b) in &self.calls {
            let (ca, cb) = (
                box_of[a].expect("every called function is in a box"),
                box_of[b].expect("every called function is in a box"),
            );
            if ca == cb {
                *quotient.inside.entry(ca).or_default() += 1;
            } else {
                *quotient.between.entry((ca, cb)).or_default() += 1;
            }
        }
        quotient
    }

    /// Every way of partitioning the clustered functions whose modularity is
    /// highest, with that modularity — found by trying every partition there
    /// is. With `acyclic_only`, partitions whose cluster graph has a cycle are
    /// not candidates, which is the optimum the clusterer is held to.
    ///
    /// The partitions of a set of `n` are the Bell number of `n`: 115,975 at
    /// ten, 678,570 at eleven. Ten is the cap.
    pub fn best_partitions(
        &self,
        resolution: f64,
        acyclic_only: bool,
    ) -> (f64, Vec<Vec<BTreeSet<usize>>>) {
        let nodes: Vec<usize> = self.clustered().into_iter().collect();
        assert!(nodes.len() <= 10, "brute force is capped at ten functions");

        let mut best = f64::NEG_INFINITY;
        let mut winners: Vec<Vec<usize>> = Vec::new();
        for_each_partition(nodes.len(), |assignment| {
            let mut box_of = vec![None; self.len()];
            for (slot, &node) in nodes.iter().enumerate() {
                box_of[node] = Some(assignment[slot]);
            }
            if acyclic_only && !self.quotient(&box_of).is_acyclic() {
                return;
            }
            let q = self.modularity(&box_of, resolution);
            if q > best + 1e-9 {
                best = q;
                winners.clear();
            }
            if q > best - 1e-9 {
                winners.push(assignment.to_vec());
            }
        });

        let partitions = winners
            .into_iter()
            .map(|assignment| {
                let mut boxes: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
                for (slot, &group) in assignment.iter().enumerate() {
                    boxes.entry(group).or_default().insert(nodes[slot]);
                }
                boxes.into_values().collect()
            })
            .collect();
        (best, partitions)
    }

    /// A partition as sorted names, for stating one in a test.
    pub fn names_of(&self, partition: &[BTreeSet<usize>]) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = partition
            .iter()
            .map(|group| group.iter().map(|&i| self.names[i].clone()).collect())
            .collect();
        out.sort();
        out
    }
}

/// Calls between boxes and inside them, boxes named by index.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Quotient {
    pub between: BTreeMap<(usize, usize), usize>,
    pub inside: BTreeMap<usize, usize>,
}

impl Quotient {
    /// Whether the boxes, with an arrow wherever one calls another, have no
    /// cycle — two boxes calling each other included. Kahn's algorithm.
    pub fn is_acyclic(&self) -> bool {
        let boxes: BTreeSet<usize> = self.between.keys().flat_map(|&(a, b)| [a, b]).collect();
        let mut indegree: BTreeMap<usize, usize> = boxes.iter().map(|&b| (b, 0)).collect();
        for &(_, to) in self.between.keys() {
            *indegree.get_mut(&to).unwrap() += 1;
        }
        let mut ready: Vec<usize> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&b, _)| b)
            .collect();
        let mut removed = 0;
        while let Some(at) = ready.pop() {
            removed += 1;
            for &(from, to) in self.between.keys() {
                if from == at {
                    let d = indegree.get_mut(&to).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        ready.push(to);
                    }
                }
            }
        }
        removed == boxes.len()
    }
}

/// Call `visit` with every partition of `n` items, as a restricted growth
/// string: item 0 is in box 0, and every later item is in a box already used
/// or the next new one. Each partition comes up exactly once.
pub(super) fn for_each_partition(n: usize, mut visit: impl FnMut(&[usize])) {
    if n == 0 {
        visit(&[]);
        return;
    }
    let mut assignment = vec![0usize; n];
    // `ceiling[i]` is the highest box used by items before `i`.
    let mut ceiling = vec![0usize; n];
    loop {
        visit(&assignment);
        // Advance the last item that can still move to a higher box, and
        // reset everything after it to box 0.
        let mut i = n - 1;
        loop {
            if i == 0 {
                return;
            }
            if assignment[i] <= ceiling[i] {
                assignment[i] += 1;
                break;
            }
            i -= 1;
        }
        for j in i + 1..n {
            assignment[j] = 0;
            ceiling[j] = ceiling[j - 1].max(assignment[j - 1]);
        }
    }
}

/// The boxes of one rung of a computed clustering, in the reference's index
/// space: `box_of[i]` is the position in `clustering.level(level)` of the box
/// holding function `i`, or `None`.
pub(super) fn rung(
    reference: &Reference,
    clustering: &Clustering,
    level: usize,
) -> Vec<Option<usize>> {
    let mut box_of = vec![None; reference.len()];
    for (position, id) in clustering.level(level).iter().enumerate() {
        for member in &clustering
            .cluster(*id)
            .expect("a listed id resolves")
            .members
        {
            let index = reference
                .index_of(member)
                .expect("a clustered function is one of the graph's");
            box_of[index] = Some(position);
        }
    }
    box_of
}

// -- the reference, checked against arithmetic done by hand ------------------

/// Two triangles joined by one call: `a→b→c`, `a→c`, the same on `d e f`,
/// and `c→d` between them. Seven calls.
fn two_triangles() -> CallGraph {
    let mut graph = CallGraph::new();
    let ids: Vec<SymbolId> = ["a", "b", "c", "d", "e", "f"]
        .iter()
        .enumerate()
        .map(|(i, name)| add(&mut graph, name, i as u32 + 1))
        .collect();
    let [a, b, c, d, e, f] = ids[..] else {
        unreachable!()
    };
    for (line, (from, to)) in [(a, b), (b, c), (a, c), (d, e), (e, f), (d, f), (c, d)]
        .into_iter()
        .enumerate()
    {
        call(&mut graph, from, to, 100 + line as u32);
    }
    graph
}

fn partition_of(reference: &Reference, groups: &[&[&str]]) -> Vec<Option<usize>> {
    let mut box_of = vec![None; reference.len()];
    for (group, names) in groups.iter().enumerate() {
        for name in *names {
            let index = reference
                .names
                .iter()
                .position(|n| n == name)
                .expect("a named function");
            box_of[index] = Some(group);
        }
    }
    box_of
}

#[test]
fn the_reference_counts_partitions_by_the_bell_numbers() {
    for (n, bell) in [(0, 1), (1, 1), (2, 2), (3, 5), (4, 15), (5, 52), (8, 4140)] {
        let mut seen = HashSet::new();
        let mut count = 0;
        for_each_partition(n, |assignment| {
            count += 1;
            seen.insert(assignment.to_vec());
        });
        assert_eq!(count, bell, "n = {n}");
        assert_eq!(seen.len(), bell, "n = {n}: no partition comes up twice");
    }
}

/// Worked by hand. m = 7. Each triangle holds three calls and its members'
/// degrees are 2, 2 and 3, so each box has d = 7:
/// Q = 2 × (3/7 − (7/14)²) = 6/7 − 1/2 = 5/14.
#[test]
fn the_reference_modularity_matches_a_hand_computation() {
    let graph = two_triangles();
    let reference = Reference::of(&graph, None);
    let split = partition_of(&reference, &[&["a", "b", "c"], &["d", "e", "f"]]);
    assert!((reference.modularity(&split, 1.0) - 5.0 / 14.0).abs() < 1e-12);

    // Everything in one box: L = m and d = 2m, so Q = 1 − γ.
    let whole = partition_of(&reference, &[&["a", "b", "c", "d", "e", "f"]]);
    assert!(reference.modularity(&whole, 1.0).abs() < 1e-12);
    assert!((reference.modularity(&whole, 0.5) - 0.5).abs() < 1e-12);

    // Singletons: no L at all, and degrees 2, 2, 3, 3, 2, 2 over 2m = 14.
    let alone = partition_of(
        &reference,
        &[&["a"], &["b"], &["c"], &["d"], &["e"], &["f"]],
    );
    let expected = -(4.0 * 4.0 + 2.0 * 9.0) / 196.0;
    assert!((reference.modularity(&alone, 1.0) - expected).abs() < 1e-12);
}

#[test]
fn the_reference_quotient_counts_calls_between_and_inside() {
    let graph = two_triangles();
    let reference = Reference::of(&graph, None);
    let split = partition_of(&reference, &[&["a", "b", "c"], &["d", "e", "f"]]);
    let quotient = reference.quotient(&split);
    assert_eq!(quotient.between, BTreeMap::from([((0, 1), 1)]));
    assert_eq!(quotient.inside, BTreeMap::from([(0, 3), (1, 3)]));
    assert!(quotient.is_acyclic());

    // `a→b` and `c→d` cross between these two in opposite directions.
    let crossed = partition_of(&reference, &[&["a", "d", "e", "f"], &["b", "c"]]);
    assert!(!reference.quotient(&crossed).is_acyclic());
}

#[test]
fn the_reference_finds_the_two_triangles() {
    let graph = two_triangles();
    let reference = Reference::of(&graph, None);
    let (q, best) = reference.best_partitions(1.0, true);
    assert!((q - 5.0 / 14.0).abs() < 1e-12);
    assert_eq!(
        best.iter()
            .map(|p| reference.names_of(p))
            .collect::<Vec<_>>(),
        vec![groups(&[&["a", "b", "c"], &["d", "e", "f"]])]
    );
}

#[test]
fn the_reference_reads_components_off_the_calls() {
    let mut graph = CallGraph::new();
    let a = add(&mut graph, "a", 1);
    let b = add(&mut graph, "b", 2);
    let c = add(&mut graph, "c", 3);
    let d = add(&mut graph, "d", 4);
    let lonely = add(&mut graph, "lonely", 5);
    call(&mut graph, a, b, 10);
    call(&mut graph, b, a, 11);
    call(&mut graph, b, c, 12);
    call(&mut graph, d, d, 13);
    let _ = lonely;

    let reference = Reference::of(&graph, None);
    let names = |sets: Vec<BTreeSet<usize>>| reference.names_of(&sets);
    assert_eq!(
        names(reference.strong_components()),
        groups(&[&["a", "b"], &["c"], &["d"], &["lonely"]])
    );
    assert_eq!(
        names(reference.weak_components()),
        groups(&[&["a", "b", "c"], &["d"], &["lonely"]])
    );
    // Recursion is not a call between two functions, so `d` is not clustered.
    assert_eq!(
        reference.names_of(&[reference.clustered()]),
        groups(&[&["a", "b", "c"]])
    );
}
