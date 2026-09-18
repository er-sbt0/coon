//! Hierarchical coarsening of the call graph by heavy-edge matching.
//!
//! A whole call graph is unreadable at once — the point of clustering is to give
//! the frontends a coarser thing to draw first, and a way to zoom in. The
//! method is the one multilevel graph partitioners coarsen with, most notably
//! METIS (Karypis and Kumar, 1998): each round visits the nodes lowest degree
//! first and pairs each still-unmatched node with the unmatched neighbour it
//! shares the heaviest edge with, then merges every pair and sums the weights
//! between the merged nodes. A round costs `O(E)` and removes up to half the
//! nodes, so the rounds stack into a zoom ladder with a predictable shrink rate:
//! level 0 is one round of merges, the top level is what is left when
//! [`TARGET_TOP_CLUSTERS`] boxes would fit on a canvas.
//!
//! What that buys, and what it costs, are the same property. A cluster here is
//! a *merge tree*, not a community: it is whatever heavy calls pulled together,
//! so it is size-controlled by construction — every level is a known fraction
//! of the one below, which is what makes progressive rendering of a very large
//! graph possible at all. But because only pairs ever merge, the boundaries
//! depend on the visit order, and a deep enough round will join two groups that
//! share a single edge. The method is not looking for the seams in the program
//! and should not be read as having found any.
//!
//! It is fully deterministic once the visit order and the tie-breaking are
//! fixed, which they are: nodes are indexed in symbol-id order, visited by
//! ascending degree with the index breaking ties, and a node picks the heaviest
//! incident edge with the *lowest* partner index breaking ties. Two runs over
//! the same graph produce byte-identical output, and no seeding or history is
//! needed to get it — which is why there is none.
//!
//! **Isolated functions are left out of the clustering entirely.** A function
//! with no calls in either direction gets no cluster at all, and
//! [`Clustering::cluster_of`] returns `None` for it. This is deliberate: a
//! real project loaded through clangd carries a long tail of symbols that
//! nothing in the indexed sources calls, and giving each one a box of its own
//! would drown the map in hundreds of one-node boxes. A self-recursive function
//! with no other edges counts as isolated too, since self-loops are dropped
//! when the base graph is built, and what is left connects it to nothing.
//!
//! The quotient of a level is **not** acyclic, and is not made so. Merging two
//! adjacent nodes can put a cluster on both ends of a one-way path, so a level
//! can draw `A -> B` alongside `B -> A` over code where nothing calls back.
//! That is a fact about pair merging, and the price of the predictable shrink
//! rate; the map answers it where it shows, by drawing such a pair as one
//! double-headed edge rather than as two arrows.
//!
//! The implementation works in index space — every hot loop indexes `Vec`s
//! rather than hashing `SymbolId`s — so it stays usable on the ~10k-node graphs
//! a mid-sized C++ project produces.

use model::{CallGraph, SymbolId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Stop coarsening once the coarsest level is this wide or narrower.
///
/// This is the whole parameterisation of the method: rounds are not counted,
/// they are run until the top of the ladder is something a person can take in
/// at a glance.
///
/// A round removes up to half the nodes, so the top rung lands anywhere in
/// `(TARGET_TOP_CLUSTERS / 2, TARGET_TOP_CLUSTERS]` — the ladder steps over the
/// target rather than onto it, and the constant sets a *zone*, not a count.
/// Twelve puts that zone at roughly six to twelve boxes, which is a map a
/// person reads rather than counts. It was twenty first, and twenty is worse
/// than it sounds for exactly this reason: the demo project's 57 functions
/// crossed it at 17 boxes of two to four functions each, a map barely coarser
/// than the list of functions it was standing in for. Halving once more is the
/// difference between summarising a program and reprinting it.
pub const TARGET_TOP_CLUSTERS: usize = 12;

/// A round that merges nothing ends the coarsening on its own, so this only
/// bounds the pathological case. Each productive round removes at least one
/// node, and a graph large enough to need 64 rounds of halving does not exist.
const MAX_ROUNDS: usize = 64;

/// Above this many members, picking a gateway by within-cluster reachability
/// costs a BFS per member. Fall back to out-degree, which is one pass.
const REACHABILITY_MEMBER_LIMIT: usize = 200;

/// Which algorithm builds the coarsening ladder.
///
/// One entry today, and named anyway. A clustering is a claim about a program's
/// shape, and every method makes a different one — a coarsener answers "what
/// merges cleanly", where a community detector answers "where are the seams".
/// Which claim is on screen is the user's to pick, so it is a value that
/// travels with the workspace and the snapshot rather than a constant compiled
/// into one call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ClusterMethod {
    /// Heavy-edge matching, the multilevel coarsener described at the top of
    /// this module.
    #[default]
    HeavyEdge,
}

impl ClusterMethod {
    /// Every method, in the order a picker should list them.
    pub const ALL: &'static [ClusterMethod] = &[ClusterMethod::HeavyEdge];

    /// What to write in the picker.
    pub fn label(self) -> &'static str {
        match self {
            ClusterMethod::HeavyEdge => "Heavy-edge matching",
        }
    }

    /// One line on what the method looks for, for the picker's second row.
    pub fn description(self) -> &'static str {
        match self {
            ClusterMethod::HeavyEdge => "Merges the busiest call pairs, round by round",
        }
    }
}

/// Index handle for a cluster within one [`Clustering`], valid until the next
/// recompute and no longer.
///
/// Ids are assigned sequentially when the ladder is flattened, in the order the
/// clusters at a level sort by their lowest member, so they are *not* names for
/// a group: the same set of functions routinely comes back under a different id
/// once the graph grows, because one new symbol sorting ahead of the others
/// renumbers everything behind it.
///
/// So an id is for indexing the clustering you are holding right now. Anything
/// that outlives it — workspace state, a snapshot the frontend keeps, a command
/// arriving back over the wire — must name clusters with a [`ClusterKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterId(pub u32);

/// A name for a cluster that survives a recompute.
///
/// The clusters at one level partition the clustered symbols — every clustered
/// symbol is in exactly one of them — so any member chosen by a rule that looks
/// only at the member set picks out its cluster uniquely within that level. The
/// lowest member by uuid is the cheapest such rule, and it is already the one
/// `build_clustering` uses to order clusters within a level, so the anchor is
/// simply the first entry of [`Cluster::members`]. A level plus that anchor is
/// therefore something a frontend can hold across a recompute, and it goes on
/// naming the same group for as long as that group keeps its lowest member.
///
/// It is a durable handle rather than an identity, and the way it slips is
/// worth being plain about: a cluster whose anchor is merged into a different
/// pair comes back under a key of its own, and the old key now names whichever
/// cluster took that member in. A key someone was holding then stops resolving
/// to what they opened. That is a re-collapse, not corruption: nothing is lost
/// that the next click does not restore, which is exactly what a [`ClusterId`]
/// — stale after *every* recompute, and silently pointing at some unrelated
/// group — cannot promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterKey {
    pub level: usize,
    /// The cluster's lowest member, ordered by uuid bytes — the same ordering
    /// every other ordering in this module descends from.
    pub anchor: SymbolId,
}

/// One cluster at one level of the ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub id: ClusterId,
    /// The handle to use for anything that outlives this `Clustering`.
    pub key: ClusterKey,
    pub level: usize,
    pub parent: Option<ClusterId>,
    /// The one or two clusters one level finer that merged into this one; empty
    /// at level 0.
    pub children: Vec<ClusterId>,
    /// Every leaf function under this cluster, transitively, ordered by symbol
    /// id so two runs produce byte-identical output.
    pub members: Vec<SymbolId>,
    pub entry_point: Option<SymbolId>,
    pub label: String,
}

/// A coarsening ladder over a [`CallGraph`]'s connected functions.
///
/// Level 0 is the finest — one round of pair merges — and the last level the
/// coarsest. Every level partitions the same set of symbols; a level is not a
/// subset of the map above it but the same map drawn at a finer grain, which is
/// what lets a frontend present the ladder as a zoom rather than as a tree to
/// open.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Clustering {
    /// Indexed by `ClusterId.0`.
    clusters: Vec<Cluster>,
    /// `levels[l]` holds the ids at level `l`, finest first.
    levels: Vec<Vec<ClusterId>>,
    /// Level-0 lookup; isolated symbols are absent by design.
    index: HashMap<SymbolId, ClusterId>,
    /// `anchors[l]` resolves a level-`l` [`ClusterKey`]'s anchor to its cluster,
    /// so [`Clustering::by_key`] is a lookup rather than a scan over every
    /// cluster at the level.
    anchors: Vec<HashMap<SymbolId, ClusterId>>,
    /// Which method built it, so a holder can tell whether the clustering it
    /// has is the one that was asked for.
    method: ClusterMethod,
}

impl Clustering {
    /// Cluster the whole graph.
    pub fn compute(graph: &CallGraph, method: ClusterMethod) -> Self {
        Self::compute_scoped(graph, None, method)
    }

    /// Coarsen one corner of the graph: only the functions in `scope`, with
    /// every call that leaves it ignored.
    ///
    /// This is not a filter applied to the whole-program ladder, and could not
    /// be. The matching is driven by which edges are heaviest *among those
    /// present*, and by how many rounds it takes to reach the target width —
    /// both of which change completely when the graph is one function's
    /// neighbourhood rather than the program. Run over a neighbourhood, the
    /// boxes that come back divide that neighbourhood, where the whole-program
    /// run would have merged the lot into a single box several rounds down.
    /// That is exactly the question a user asks by pointing at a function, so
    /// it gets its own run rather than a view onto the global one.
    ///
    /// Isolation is judged inside the scope too: a function whose only calls
    /// leave it has no edges here, and is dropped like any other isolated
    /// symbol. Entry points and labels still read the full graph, because being
    /// called from outside the scope is what makes a member a front door.
    pub fn compute_within(
        graph: &CallGraph,
        scope: &HashSet<SymbolId>,
        method: ClusterMethod,
    ) -> Self {
        Self::compute_scoped(graph, Some(scope), method)
    }

    /// The method this ladder was built with.
    pub fn method(&self) -> ClusterMethod {
        self.method
    }

    fn compute_scoped(
        graph: &CallGraph,
        scope: Option<&HashSet<SymbolId>>,
        method: ClusterMethod,
    ) -> Self {
        let (symbols, base) = build_base_graph(graph, scope);
        if symbols.is_empty() {
            return Self {
                method,
                ..Self::default()
            };
        }

        let ladder = match method {
            ClusterMethod::HeavyEdge => coarsen(base),
        };
        let mut clustering = build_clustering(graph, &symbols, &ladder);
        clustering.method = method;
        clustering
    }

    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    /// How many rungs the ladder has. The zoom control's range.
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// The coarsest level — the boxes a frontend should draw first.
    pub fn top_level(&self) -> &[ClusterId] {
        self.levels.last().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The index of the coarsest level, or `None` when nothing is clustered.
    pub fn top_level_index(&self) -> Option<usize> {
        self.levels.len().checked_sub(1)
    }

    /// The clusters at `level`, or an empty slice if the level does not exist.
    pub fn level(&self, level: usize) -> &[ClusterId] {
        self.levels.get(level).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn cluster(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.get(id.0 as usize)
    }

    /// The cluster named by `key`, or `None` when nothing at that level is
    /// anchored on that symbol — a level this clustering does not have, or a
    /// key from before a recompute that merged its anchor elsewhere.
    pub fn by_key(&self, key: ClusterKey) -> Option<&Cluster> {
        self.anchors
            .get(key.level)
            .and_then(|level| level.get(&key.anchor))
            .and_then(|id| self.cluster(*id))
    }

    /// The finest (level 0) cluster holding `symbol`, or `None` if the symbol is
    /// isolated or absent.
    pub fn cluster_of(&self, symbol: &SymbolId) -> Option<ClusterId> {
        self.index.get(symbol).copied()
    }

    /// The cluster at `level` holding `symbol`, or `None` if the symbol is
    /// isolated, absent, or the level does not exist.
    ///
    /// Every level partitions the same symbols, so this always answers for a
    /// clustered symbol — which is what a flat map at one rung needs in order
    /// to say which box a function is in without walking the ladder itself.
    pub fn cluster_of_at(&self, symbol: &SymbolId, level: usize) -> Option<ClusterId> {
        let mut current = self.cluster_of(symbol)?;
        while let Some(cluster) = self.cluster(current) {
            if cluster.level >= level {
                return (cluster.level == level).then_some(current);
            }
            current = cluster.parent?;
        }
        None
    }

    /// The chain of clusters containing `symbol`, finest first, coarsest last.
    pub fn ancestry(&self, symbol: &SymbolId) -> Vec<ClusterId> {
        let mut chain = Vec::new();
        let mut current = self.cluster_of(symbol);
        while let Some(id) = current {
            chain.push(id);
            current = self.cluster(id).and_then(|c| c.parent);
        }
        chain
    }

    pub fn clusters(&self) -> impl Iterator<Item = &Cluster> + '_ {
        self.clusters.iter()
    }
}

// ---------------------------------------------------------------------------
// Weighted graph in index space
// ---------------------------------------------------------------------------

struct WeightedGraph {
    /// Neighbours and weights, sorted by index and never containing the node
    /// itself.
    adj: Vec<Vec<(usize, f64)>>,
    /// Weight of each node's self-loop — the calls that merging a pair turned
    /// inward. Carried so a coarser round can tell a heavy internal edge from a
    /// heavy external one, and so nothing about the original graph is lost as
    /// the ladder climbs.
    self_loops: Vec<f64>,
}

impl WeightedGraph {
    fn new(adj_maps: Vec<HashMap<usize, f64>>, self_loops: Vec<f64>) -> Self {
        let mut adj: Vec<Vec<(usize, f64)>> = adj_maps
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();
        // The matching breaks weight ties on the lower partner index, which it
        // gets for free by scanning in index order — so this sort is not
        // cosmetic, it *is* the tie-break.
        for neighbours in &mut adj {
            neighbours.sort_unstable_by_key(|(index, _)| *index);
        }

        Self { adj, self_loops }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }
}

/// Build the undirected weighted graph the coarsening runs on, plus the symbol
/// list that gives every index its meaning.
///
/// `CallGraph::get_callees` returns one entry per distinct *call site*, not per
/// distinct callee — the graph deduplicates on (caller, callee, location). So
/// the duplicates in that `Vec` are exactly the number of times the caller calls
/// the callee, and counting them is how we get an edge weight out of an API that
/// exposes no weights and no `edges()` accessor. Those counts are the whole
/// input to "heaviest": two functions that call each other twenty times merge
/// before either merges with something it touches once.
///
/// Direction is dropped here. Matching has no use for it — a call is a reason
/// to put two functions in one box whichever way it runs — and keeping it would
/// mean a node had two heaviest edges to choose between.
///
/// `scope`, when given, is the set of functions to consider; everything else is
/// left out, and so are the calls that would have reached it. Restricting the
/// symbol list is enough to restrict the edges too, because a call whose other
/// end has no index is skipped exactly as a call to an unknown symbol is.
fn build_base_graph(
    graph: &CallGraph,
    scope: Option<&HashSet<SymbolId>>,
) -> (Vec<SymbolId>, WeightedGraph) {
    let mut all: Vec<SymbolId> = graph
        .function_ids()
        .filter(|id| scope.is_none_or(|s| s.contains(id)))
        .copied()
        .collect();
    // Every downstream ordering — the visit order, member lists, cluster ids —
    // descends from this sort, which is what makes two runs byte-identical.
    all.sort_unstable_by_key(|s| s.0);

    let position: HashMap<SymbolId, usize> = all.iter().enumerate().map(|(i, s)| (*s, i)).collect();

    let mut weights: Vec<HashMap<usize, f64>> = vec![HashMap::new(); all.len()];
    for (i, symbol) in all.iter().enumerate() {
        for callee in graph.get_callees(symbol) {
            let Some(&j) = position.get(&callee.id) else {
                continue;
            };
            // A function cannot be merged with itself, so recursion is weight
            // with nowhere to go.
            if i == j {
                continue;
            }
            *weights[i].entry(j).or_insert(0.0) += 1.0;
            *weights[j].entry(i).or_insert(0.0) += 1.0;
        }
    }

    // Drop the isolated symbols and compact the index space around what is left.
    let kept: Vec<usize> = (0..all.len()).filter(|i| !weights[*i].is_empty()).collect();
    let mut compacted = vec![usize::MAX; all.len()];
    for (new, &old) in kept.iter().enumerate() {
        compacted[old] = new;
    }

    let symbols: Vec<SymbolId> = kept.iter().map(|&i| all[i]).collect();
    let adj_maps: Vec<HashMap<usize, f64>> = kept
        .iter()
        .map(|&old| {
            weights[old]
                .iter()
                .map(|(&j, &w)| (compacted[j], w))
                .collect()
        })
        .collect();
    let self_loops = vec![0.0; symbols.len()];

    (symbols, WeightedGraph::new(adj_maps, self_loops))
}

// ---------------------------------------------------------------------------
// Heavy-edge matching
// ---------------------------------------------------------------------------

/// Run rounds of matching until the top of the ladder is about
/// [`TARGET_TOP_CLUSTERS`] wide.
///
/// The return is the ladder in the shape [`build_clustering`] reads: entry 0
/// maps a base node to its level-0 cluster, and entry `l` maps a level-`l - 1`
/// cluster to its level-`l` one. So the number of entries is the number of
/// rounds, and every entry is one rung of zoom.
///
/// A graph already at or below the target still gets a rung, made of
/// singletons. A map has to have something to draw, and "every function is its
/// own box" is the honest answer for a graph that small — coarsening it further
/// would merge a program small enough to read whole.
fn coarsen(base: WeightedGraph) -> Vec<Vec<usize>> {
    let mut ladder: Vec<Vec<usize>> = Vec::new();
    let mut current = base;

    while current.node_count() > TARGET_TOP_CLUSTERS && ladder.len() < MAX_ROUNDS {
        let (matching, count) = heavy_edge_match(&current);
        // Every node hub-like enough to have had its neighbours claimed before
        // its turn comes stays single, and a round where that is *all* of them
        // is a round that would add a rung identical to the one below it. Two
        // levels the user cannot tell apart is worse than stopping wide.
        if count == current.node_count() {
            break;
        }
        current = aggregate(&current, &matching, count);
        ladder.push(matching);
    }

    if ladder.is_empty() {
        ladder.push((0..current.node_count()).collect());
    }
    ladder
}

/// One round: pair each node with its heaviest available neighbour.
///
/// Returns the assignment of nodes to merged groups, renumbered so the groups
/// are `0..count`. A node whose neighbours were all claimed before its turn
/// comes stays in a group of its own — which is how hubs behave here, and why
/// they survive the ladder as recognisable single functions long after their
/// callees have been merged away: everything adjacent to a hub has the hub as a
/// neighbour, and something else claims each of them first.
///
/// Both halves of the visit rule matter. Ascending degree first, because a node
/// with one edge has one chance to be matched at all and a hub has dozens, so
/// serving the poorest first is what keeps the round's merge count high. The
/// index second, because the degree ties are the common case and something has
/// to break them the same way on every run.
fn heavy_edge_match(graph: &WeightedGraph) -> (Vec<usize>, usize) {
    let n = graph.node_count();

    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&node| (graph.adj[node].len(), node));

    const UNMATCHED: usize = usize::MAX;
    let mut mate = vec![UNMATCHED; n];

    for &node in &order {
        if mate[node] != UNMATCHED {
            continue;
        }

        // `adj` is sorted by index and the comparison is strict, so among
        // equally heavy candidates the lowest index wins.
        let mut best: Option<(usize, f64)> = None;
        for &(neighbour, weight) in &graph.adj[node] {
            if mate[neighbour] != UNMATCHED {
                continue;
            }
            if best.is_none_or(|(_, heaviest)| weight > heaviest) {
                best = Some((neighbour, weight));
            }
        }

        match best {
            Some((partner, _)) => {
                mate[node] = partner;
                mate[partner] = node;
            }
            None => mate[node] = node,
        }
    }

    // A pair takes the lower of its two indices as its provisional group, which
    // keeps the assignment in index order for `renumber` to compact.
    let groups: Vec<usize> = (0..n).map(|node| node.min(mate[node])).collect();
    renumber(&groups)
}

fn renumber(groups: &[usize]) -> (Vec<usize>, usize) {
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let compacted = groups
        .iter()
        .map(|&group| {
            *mapping.entry(group).or_insert_with(|| {
                let assigned = next;
                next += 1;
                assigned
            })
        })
        .collect();
    (compacted, next)
}

/// Collapse each merged group into a single node, summing the weights between
/// them.
///
/// Summing is the point: it is what makes the *next* round see two groups that
/// are joined by many separate light calls as one heavy edge, which is how a
/// subsystem grows out of pair merges at all rather than the method simply
/// picking out the twenty busiest call sites in the program.
fn aggregate(graph: &WeightedGraph, groups: &[usize], count: usize) -> WeightedGraph {
    let mut adj_maps: Vec<HashMap<usize, f64>> = vec![HashMap::new(); count];
    let mut self_loops = vec![0.0f64; count];

    for (node, &group) in groups.iter().enumerate() {
        self_loops[group] += graph.self_loops[node];
        for &(neighbour, weight) in &graph.adj[node] {
            let other = groups[neighbour];
            if other == group {
                // Seen once from each end, so halve to recover the edge's weight.
                self_loops[group] += weight / 2.0;
            } else {
                *adj_maps[group].entry(other).or_insert(0.0) += weight;
            }
        }
    }

    WeightedGraph::new(adj_maps, self_loops)
}

// ---------------------------------------------------------------------------
// Flattening the ladder
// ---------------------------------------------------------------------------

/// Turn the round-by-round assignments into the addressable [`Clustering`] the
/// rest of the app reads.
fn build_clustering(graph: &CallGraph, symbols: &[SymbolId], ladder: &[Vec<usize>]) -> Clustering {
    // Which level-`l` cluster each base node ends up in.
    let mut owners: Vec<Vec<usize>> = Vec::with_capacity(ladder.len());
    owners.push(ladder[0].clone());
    for level in &ladder[1..] {
        let previous = owners.last().expect("a level was just pushed");
        owners.push(previous.iter().map(|&group| level[group]).collect());
    }

    // Members per cluster per level. Base indices are already in symbol order,
    // so pushing in index order leaves each member list sorted.
    let mut members: Vec<Vec<Vec<usize>>> = Vec::with_capacity(owners.len());
    for (level, owner) in owners.iter().enumerate() {
        let count = ladder[level].iter().copied().max().unwrap_or(0) + 1;
        let mut buckets = vec![Vec::new(); count];
        for (node, &group) in owner.iter().enumerate() {
            buckets[group].push(node);
        }
        members.push(buckets);
    }

    // Assign ids finest-to-coarsest, clusters within a level ordered by their
    // lowest member — which, given the index order, is the lowest symbol id.
    let mut ids: Vec<Vec<Option<ClusterId>>> = Vec::with_capacity(members.len());
    let mut order: Vec<Vec<usize>> = Vec::with_capacity(members.len());
    let mut next_id = 0u32;
    let mut levels: Vec<Vec<ClusterId>> = Vec::with_capacity(members.len());

    for buckets in &members {
        let mut populated: Vec<usize> = (0..buckets.len())
            .filter(|&c| !buckets[c].is_empty())
            .collect();
        populated.sort_unstable_by_key(|&c| buckets[c][0]);

        let mut level_ids = Vec::with_capacity(populated.len());
        let mut slots = vec![None; buckets.len()];
        for &group in &populated {
            let id = ClusterId(next_id);
            next_id += 1;
            slots[group] = Some(id);
            level_ids.push(id);
        }
        levels.push(level_ids);
        ids.push(slots);
        order.push(populated);
    }

    let mut clusters: Vec<Cluster> = Vec::with_capacity(next_id as usize);
    for (level, populated) in order.iter().enumerate() {
        for &group in populated {
            let id = ids[level][group].expect("populated clusters all have ids");
            let member_symbols: Vec<SymbolId> =
                members[level][group].iter().map(|&i| symbols[i]).collect();

            let parent = ladder
                .get(level + 1)
                .and_then(|next| ids[level + 1][next[group]]);

            let children = if level == 0 {
                Vec::new()
            } else {
                order[level - 1]
                    .iter()
                    .filter(|&&child| ladder[level][child] == group)
                    .filter_map(|&child| ids[level - 1][child])
                    .collect()
            };

            // Populated clusters are the only ones given ids, so there is
            // always a lowest member for the key to anchor on.
            let key = ClusterKey {
                level,
                anchor: member_symbols[0],
            };

            let entry_point = choose_entry_point(graph, &member_symbols);
            let label = entry_point
                .and_then(|id| graph.get_function(&id))
                .map(|f| f.name.clone())
                .unwrap_or_default();

            clusters.push(Cluster {
                id,
                key,
                level,
                parent,
                children,
                members: member_symbols,
                entry_point,
                label,
            });
        }
    }

    let index: HashMap<SymbolId, ClusterId> = members[0]
        .iter()
        .enumerate()
        .filter_map(|(group, bucket)| ids[0][group].map(|id| (bucket, id)))
        .flat_map(|(bucket, id)| bucket.iter().map(move |&node| (symbols[node], id)))
        .collect();

    let mut anchors: Vec<HashMap<SymbolId, ClusterId>> = vec![HashMap::new(); levels.len()];
    for cluster in &clusters {
        anchors[cluster.level].insert(cluster.key.anchor, cluster.id);
    }

    Clustering {
        clusters,
        levels,
        index,
        anchors,
        // Set by the caller, which is the half that knows which method asked
        // for this ladder.
        method: ClusterMethod::default(),
    }
}

/// Pick the member that best serves as the cluster's front door.
///
/// A merge tree has no name of its own, so the box is named after the way in.
/// A front door is entered from outside *and* leads inward, and it is the
/// second half that carries the label. Ranking purely by incoming calls from
/// outside sounds right and is not: the most-called member of almost any
/// cluster is a shared leaf. It labelled this project's networking cluster
/// `log_write` and its startup cluster `parse_integer` — both genuinely the
/// most-called members, both read as the wrong subsystem entirely.
///
/// So the ranking is how much of the cluster a member reaches from inside,
/// among the members that outside callers can actually enter through. A
/// cluster nothing calls into — the one holding `main`, typically — drops the
/// qualifier and ranks every member. Incoming calls break ties, which is where
/// that count belongs: it separates two equally central members rather than
/// choosing between a hub and a leaf.
fn choose_entry_point(graph: &CallGraph, members: &[SymbolId]) -> Option<SymbolId> {
    if members.is_empty() {
        return None;
    }

    let inside: HashSet<SymbolId> = members.iter().copied().collect();

    // Multiplicity matters: five calls into a function from outside make it more
    // of an entrance than one does.
    let external: Vec<usize> = members
        .iter()
        .map(|member| {
            graph
                .get_callers(member)
                .iter()
                .filter(|caller| !inside.contains(&caller.id))
                .count()
        })
        .collect();

    // Above the limit a BFS per member gets expensive, and how many members a
    // function calls directly stands in well enough for how much it fronts.
    let dominance: Vec<usize> = if members.len() > REACHABILITY_MEMBER_LIMIT {
        members
            .iter()
            .map(|member| {
                graph
                    .get_callees(member)
                    .iter()
                    .filter(|callee| inside.contains(&callee.id))
                    .count()
            })
            .collect()
    } else {
        members
            .iter()
            .map(|member| internal_reach(graph, member, &inside))
            .collect()
    };

    // A function nothing calls anywhere is a front door by definition — `main`,
    // a thread body, a callback the runtime invokes. It has no incoming calls to
    // rank it by, so it would otherwise be filtered out of its own cluster's
    // naming, which is how the cluster holding `main` came to be labelled
    // `parse_integer`.
    let unreached: Vec<bool> = members
        .iter()
        .map(|member| graph.get_callers(member).is_empty())
        .collect();

    let entered_from_outside = external.iter().any(|&count| count > 0);

    members
        .iter()
        .enumerate()
        .filter(|(i, _)| !entered_from_outside || external[*i] > 0 || unreached[*i])
        .max_by(|(i, left), (j, right)| {
            dominance[*i]
                .cmp(&dominance[*j])
                .then(external[*i].cmp(&external[*j]))
                // Definition order is the tie-break a reader would use
                // themselves, so the lower one has to rank higher — hence the
                // reversed comparison against a `max_by`. The uuid behind it
                // only exists so the answer is never ambiguous.
                .then_with(|| tie_break_key(graph, right).cmp(&tie_break_key(graph, left)))
        })
        .map(|(_, member)| *member)
}

/// How many other members this one reaches, following calls that stay inside
/// the cluster.
fn internal_reach(graph: &CallGraph, start: &SymbolId, inside: &HashSet<SymbolId>) -> usize {
    let mut seen: HashSet<SymbolId> = HashSet::from([*start]);
    let mut queue = VecDeque::from([*start]);

    while let Some(current) = queue.pop_front() {
        for callee in graph.get_callees(&current) {
            if inside.contains(&callee.id) && seen.insert(callee.id) {
                queue.push_back(callee.id);
            }
        }
    }

    seen.len() - 1
}

fn tie_break_key(graph: &CallGraph, symbol: &SymbolId) -> (String, u32, uuid_bytes::Bytes) {
    let location = graph.get_function(symbol).map(|f| &f.definition_location);
    (
        location.map(|l| l.file_path.clone()).unwrap_or_default(),
        location.map(|l| l.line).unwrap_or(u32::MAX),
        uuid_bytes::Bytes(*symbol.0.as_bytes()),
    )
}

/// A thin `Ord` wrapper so the tie-break key can end in the raw uuid without
/// `logic` taking a direct dependency on the `uuid` crate.
mod uuid_bytes {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    pub struct Bytes(pub [u8; 16]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    /// Distinct lines keep the generated `SymbolId`s distinct.
    fn add(graph: &mut CallGraph, name: &str, line: u32) -> SymbolId {
        graph.add_function(FunctionNode::new(
            name.to_string(),
            name.to_string(),
            Location::new("src/x.c".to_string(), line, 0),
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
                let mut names: Vec<String> =
                    cluster.members.iter().map(|m| named(graph, m)).collect();
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

    // -- the matching itself ------------------------------------------------

    /// How one round groups a graph, as names, so a test can state the pairing
    /// it expects. Driven directly rather than through `Clustering::compute`,
    /// which stops before the first round on a graph this small.
    fn one_round(graph: &CallGraph) -> Vec<Vec<String>> {
        let (symbols, base) = build_base_graph(graph, None);
        let (matching, count) = heavy_edge_match(&base);

        let mut buckets = vec![Vec::new(); count];
        for (node, &group) in matching.iter().enumerate() {
            buckets[group].push(named(graph, &symbols[node]));
        }
        for bucket in &mut buckets {
            bucket.sort();
        }
        buckets.sort();
        buckets
    }

    /// The one property the whole method rests on: a node merges with the
    /// neighbour it shares the most calls with, not the first one it meets.
    #[test]
    fn a_node_pairs_with_its_heaviest_neighbour() {
        let mut graph = CallGraph::new();
        let a = add(&mut graph, "a", 1);
        let light = add(&mut graph, "light", 2);
        let heavy = add(&mut graph, "heavy", 3);
        // `a` touches both, but calls `heavy` five times and `light` once.
        call(&mut graph, a, light, 10);
        call_often(&mut graph, a, heavy, 20, 5);
        // Keep both partners from being singletons the round would skip.
        let other = add(&mut graph, "other", 4);
        call(&mut graph, light, other, 30);
        call(&mut graph, heavy, other, 40);

        assert_eq!(
            one_round(&graph),
            vec![vec!["a", "heavy"], vec!["light", "other"]],
            "a should merge with the neighbour it calls five times"
        );
    }

    /// Lowest degree first, so the node with one chance to be matched gets
    /// served before the hub that has many.
    #[test]
    fn a_hub_is_left_over_once_its_neighbours_have_paired_off() {
        let mut graph = CallGraph::new();
        let hub = add(&mut graph, "hub", 1);
        let leaves: Vec<SymbolId> = (0..4)
            .map(|i| add(&mut graph, &format!("leaf{i}"), 10 + i))
            .collect();
        for (i, leaf) in leaves.iter().enumerate() {
            call(&mut graph, hub, *leaf, 100 + i as u32);
        }
        // Pair the leaves up among themselves, more heavily than the hub calls
        // any of them — the shape a hub actually has, called once from each of
        // many places while the pairs work together.
        call_often(&mut graph, leaves[0], leaves[1], 200, 3);
        call_often(&mut graph, leaves[2], leaves[3], 210, 3);

        assert_eq!(
            one_round(&graph),
            vec![vec!["hub"], vec!["leaf0", "leaf1"], vec!["leaf2", "leaf3"],],
            "the hub should be left over once its neighbours are claimed"
        );
    }

    /// Merging sums the weights between merged nodes, which is what lets many
    /// light calls add up to one heavy edge a later round will follow.
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
        let graph = WeightedGraph::new(adj, vec![0.0; 4]);

        let (matching, count) = heavy_edge_match(&graph);
        assert_eq!(count, 2, "two pairs should merge into two nodes");

        let coarse = aggregate(&graph, &matching, count);
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

        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);
        assert!(
            clustering.level_count() > 1,
            "200 functions should take more than one round"
        );

        let widths: Vec<usize> = (0..clustering.level_count())
            .map(|level| clustering.level(level).len())
            .collect();
        for pair in widths.windows(2) {
            assert!(
                pair[1] < pair[0],
                "every rung must be coarser than the one below: {widths:?}"
            );
        }
        assert!(
            *widths.last().unwrap() <= TARGET_TOP_CLUSTERS,
            "the top should be at most {TARGET_TOP_CLUSTERS} boxes wide: {widths:?}"
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

        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

        let expected: HashSet<SymbolId> = graph.function_ids().copied().collect();
        for level in 0..clustering.level_count() {
            let mut seen: HashSet<SymbolId> = HashSet::new();
            for id in clustering.level(level) {
                for member in &clustering.cluster(*id).unwrap().members {
                    assert!(
                        seen.insert(*member),
                        "{member:?} is in two clusters at {level}"
                    );
                }
            }
            assert_eq!(seen, expected, "level {level} does not cover the graph");
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
    fn the_ladder_is_a_refinement_of_itself() {
        let (graph, _) = chain_of(60);
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
                    (1..=2).contains(&cluster.children.len()),
                    "a round merges pairs, so a cluster has one or two children"
                );
            }
        }
    }

    #[test]
    fn recomputing_gives_identical_output() {
        let (graph, _) = chain_of(60);
        assert_eq!(
            fingerprint(
                &graph,
                &Clustering::compute(&graph, ClusterMethod::HeavyEdge)
            ),
            fingerprint(
                &graph,
                &Clustering::compute(&graph, ClusterMethod::HeavyEdge)
            ),
        );
    }

    #[test]
    fn an_empty_graph_yields_an_empty_clustering() {
        let clustering = Clustering::compute(&CallGraph::new(), ClusterMethod::HeavyEdge);
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

        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

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

    // -- scoping ------------------------------------------------------------

    #[test]
    fn a_scoped_clustering_holds_nothing_from_outside_the_scope() {
        let (graph, ids) = chain_of(60);
        let scope: HashSet<SymbolId> = ids[..10].iter().copied().collect();

        let clustering = Clustering::compute_within(&graph, &scope, ClusterMethod::HeavyEdge);

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

        let clustering = Clustering::compute_within(&graph, &scope, ClusterMethod::HeavyEdge);

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
                &Clustering::compute_within(&graph, &everything, ClusterMethod::HeavyEdge)
            ),
            fingerprint(
                &graph,
                &Clustering::compute(&graph, ClusterMethod::HeavyEdge)
            ),
        );
    }

    #[test]
    fn an_empty_scope_clusters_nothing() {
        let (graph, _) = chain_of(60);
        let clustering =
            Clustering::compute_within(&graph, &HashSet::new(), ClusterMethod::HeavyEdge);
        assert!(clustering.is_empty());
    }
}
