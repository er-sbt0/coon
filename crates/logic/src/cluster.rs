//! Hierarchical community detection over the call graph.
//!
//! A whole call graph is unreadable at once — the point of clustering is to give
//! the frontends a coarser thing to draw first, and a way to drill in. We use
//! Louvain: greedy modularity maximisation with an aggregation step, which
//! yields a dendrogram rather than a single flat partition. Level 0 is the
//! finest grouping, the last level the coarsest.
//!
//! **Isolated functions are left out of the clustering entirely.** A function
//! with no calls in either direction gets no cluster at all, and
//! [`Clustering::cluster_of`] returns `None` for it. This is deliberate: a
//! real project loaded through clangd carries a long tail of symbols that
//! nothing in the indexed sources calls, and giving each one a singleton
//! top-level cluster would drown the cluster map in hundreds of one-node boxes.
//! A self-recursive function with no other edges counts as isolated too, since
//! self-loops are dropped when the base graph is built, and what is left
//! connects it to nothing.
//!
//! The implementation works in index space — every hot loop indexes `Vec`s
//! rather than hashing `SymbolId`s — so it stays usable on the ~10k-node graphs
//! a mid-sized C++ project produces.

use model::{CallGraph, SymbolId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// Modularity differences below this are summation noise, not real gains, and
/// treating them as improvements makes the level count depend on floating-point
/// accidents.
const MODULARITY_EPSILON: f64 = 1e-9;

/// Above this many members, picking a gateway by within-cluster reachability
/// costs a BFS per member. Fall back to out-degree, which is one pass.
const REACHABILITY_MEMBER_LIMIT: usize = 200;

/// Local moving provably terminates on strict gains, but floating point can in
/// principle let two nodes trade places forever. Cap the sweeps rather than
/// risk a hang on a pathological graph.
const MAX_LOCAL_MOVING_SWEEPS: usize = 100;

/// Index handle for a cluster within one [`Clustering`], valid until the next
/// recompute and no longer.
///
/// Ids are assigned sequentially when the dendrogram is flattened, in the order
/// the clusters at a level sort by their lowest member, so they are *not* names
/// for a community: the same group of functions routinely comes back under a
/// different id once the graph grows, because one new symbol sorting ahead of
/// the others renumbers everything behind it. Seeding
/// ([`Clustering::compute_seeded`]) keeps the *grouping* stable across a
/// recompute; it says nothing at all about the numbering.
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
/// naming the same community for as long as that community keeps its lowest
/// member — which seeded Louvain, regrouping only where new edges argue for it,
/// mostly does.
///
/// It is a durable handle rather than an identity, and the two ways it can slip
/// are worth being plain about:
///
/// - A cluster that splits keeps its key on whichever half still holds the
///   anchor; the other half comes back under a key of its own.
/// - A cluster whose anchor member moves elsewhere gets a new key, and the old
///   key now names whichever cluster took that member in.
///
/// Either way a key someone was holding stops resolving to what they opened,
/// and a box they had expanded comes back closed. That is a re-collapse, not
/// corruption: nothing is lost that the next click does not restore, which is
/// exactly what a [`ClusterId`] — stale after *every* recompute, and silently
/// pointing at some unrelated community — cannot promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterKey {
    pub level: usize,
    /// The cluster's lowest member, ordered by uuid bytes — the same ordering
    /// every other ordering in this module descends from.
    pub anchor: SymbolId,
}

/// One community at one level of the dendrogram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub id: ClusterId,
    /// The handle to use for anything that outlives this `Clustering`.
    pub key: ClusterKey,
    pub level: usize,
    pub parent: Option<ClusterId>,
    /// Sub-clusters one level finer; empty at level 0.
    pub children: Vec<ClusterId>,
    /// Every leaf function under this cluster, transitively, ordered by symbol
    /// id so two runs produce byte-identical output.
    pub members: Vec<SymbolId>,
    pub entry_point: Option<SymbolId>,
    pub label: String,
}

/// A hierarchical partition of a [`CallGraph`]'s connected functions.
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
    /// What Louvain decided before [`make_convex`] cut it up, kept only to seed
    /// the next recompute.
    ///
    /// Seeding with the *finished* clusters instead is what the obvious reading
    /// of "start from last time" gives, and it destroys the stability it exists
    /// to provide: the repaired partition is not a partition local moving would
    /// have settled on, so Louvain immediately moves functions out of it and
    /// the whole clustering lands somewhere new on a graph that never changed.
    /// Louvain's own output is a fixed point, so seeding with that reproduces
    /// it exactly, and the repair — deterministic — reproduces the rest.
    seed: HashMap<SymbolId, usize>,
}

impl Clustering {
    /// Cluster the graph from scratch. Equivalent to `compute_seeded(graph, None)`.
    pub fn compute(graph: &CallGraph) -> Self {
        Self::compute_seeded(graph, None)
    }

    /// Cluster the graph, optionally starting the first local-moving pass from
    /// a previous result instead of from every-node-its-own-community.
    ///
    /// The graph arrives incrementally from clangd, and Louvain is only a greedy
    /// heuristic: recomputing from singletons after each batch reshuffles
    /// clusters that did not change, which reads to the user as the map
    /// exploding for no reason. Seeding from the previous level-0 assignment
    /// keeps symbols that were grouped together grouped together unless the new
    /// edges actually argue otherwise.
    pub fn compute_seeded(graph: &CallGraph, previous: Option<&Clustering>) -> Self {
        Self::compute_scoped(graph, None, previous)
    }

    /// Cluster one corner of the graph: only the functions in `scope`, with
    /// every call that leaves it ignored.
    ///
    /// This is not a filter applied to the whole-program clustering, and could
    /// not be. Louvain finds the seams where a graph is weakly connected, and
    /// which seams those are depends on what else is in the graph: run over one
    /// function's neighbourhood, the communities that come back are the parts
    /// *of that neighbourhood* — the stages of a request path, say — where the
    /// whole-program run would have shown the request path itself as one
    /// undivided community. That is exactly the question a user asks by
    /// pointing at a function, so it gets its own run rather than a view onto
    /// the global one.
    ///
    /// Isolation is judged inside the scope too: a function whose only calls
    /// leave it has no edges here, and is dropped like any other isolated
    /// symbol. Entry points and labels still read the full graph, because being
    /// called from outside the scope is what makes a member a front door.
    pub fn compute_within(
        graph: &CallGraph,
        scope: &HashSet<SymbolId>,
        previous: Option<&Clustering>,
    ) -> Self {
        Self::compute_scoped(graph, Some(scope), previous)
    }

    fn compute_scoped(
        graph: &CallGraph,
        scope: Option<&HashSet<SymbolId>>,
        previous: Option<&Clustering>,
    ) -> Self {
        let (symbols, base) = build_base_graph(graph, scope);
        if symbols.is_empty() {
            return Self::default();
        }

        // Louvain's first pass, and only its first pass. The coarser levels it
        // would go on to build by aggregating this one are never used: they
        // group an undirected copy of the call graph, so they straddle its
        // direction, and `make_convex` builds the hierarchy above the finest
        // level for itself — under the constraint, rather than repairing
        // groupings that were made in ignorance of it.
        let start = previous
            .map(|prev| seed_partition(&symbols, prev))
            .unwrap_or_else(|| (0..base.node_count()).collect());
        let (finest, _) = renumber(&local_moving(&base, start));

        let seed: HashMap<SymbolId, usize> = symbols.iter().copied().zip(finest.clone()).collect();

        // Modularity is blind to direction. Restoring the direction afterwards
        // is what keeps the cluster map readable as flow rather than as a knot
        // of mutual arrows.
        let dendrogram = make_convex(graph, &symbols, finest);

        let mut clustering = build_clustering(graph, &symbols, &dendrogram);
        clustering.seed = seed;
        clustering
    }

    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// The coarsest level — the clusters a frontend should draw first.
    pub fn top_level(&self) -> &[ClusterId] {
        self.levels.last().map(Vec::as_slice).unwrap_or(&[])
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
    /// key from before a recompute that moved its anchor into another
    /// community.
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

/// An undirected weighted graph over `0..n`, the shape Louvain actually works on.
struct WeightedGraph {
    /// Neighbours and weights, never containing the node itself.
    adj: Vec<Vec<(usize, f64)>>,
    /// Weight of each node's self-loop. Aggregated levels *must* keep these:
    /// a community's internal weight becomes a self-loop on its super-node, and
    /// dropping it makes modularity at level 1+ measure the wrong graph.
    self_loops: Vec<f64>,
    /// `k_i`, with the self-loop counted twice as the undirected convention wants.
    degrees: Vec<f64>,
    /// `2m` — the sum of all degrees.
    total_degree: f64,
}

impl WeightedGraph {
    fn new(adj_maps: Vec<HashMap<usize, f64>>, self_loops: Vec<f64>) -> Self {
        let mut adj: Vec<Vec<(usize, f64)>> = adj_maps
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();
        // Neighbour order leaks into tie-breaking during local moving, so pin it.
        for neighbours in &mut adj {
            neighbours.sort_unstable_by_key(|(index, _)| *index);
        }

        let degrees: Vec<f64> = adj
            .iter()
            .zip(&self_loops)
            .map(|(neighbours, loop_weight)| {
                neighbours.iter().map(|(_, w)| *w).sum::<f64>() + 2.0 * loop_weight
            })
            .collect();
        let total_degree = degrees.iter().sum();

        Self {
            adj,
            self_loops,
            degrees,
            total_degree,
        }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }
}

/// Build the undirected weighted graph the clustering runs on, plus the symbol
/// list that gives every index its meaning.
///
/// `CallGraph::get_callees` returns one entry per distinct *call site*, not per
/// distinct callee — the graph deduplicates on (caller, callee, location). So
/// the duplicates in that `Vec` are exactly the number of times the caller calls
/// the callee, and counting them is how we get an edge weight out of an API that
/// exposes no weights and no `edges()` accessor.
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
    // Every downstream ordering — community numbering, member lists, cluster ids —
    // descends from this sort, which is what makes two runs byte-identical.
    all.sort_unstable_by_key(|s| s.0);

    let position: HashMap<SymbolId, usize> = all.iter().enumerate().map(|(i, s)| (*s, i)).collect();

    let mut weights: Vec<HashMap<usize, f64>> = vec![HashMap::new(); all.len()];
    for (i, symbol) in all.iter().enumerate() {
        for callee in graph.get_callees(symbol) {
            let Some(&j) = position.get(&callee.id) else {
                continue;
            };
            // Recursion says nothing about which community a function belongs to.
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

/// Translate a previous clustering into initial communities for the first pass.
///
/// Symbols the previous run grouped together share a fresh community index;
/// symbols it never saw start on their own.
fn seed_partition(symbols: &[SymbolId], previous: &Clustering) -> Vec<usize> {
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let mut communities = Vec::with_capacity(symbols.len());

    for symbol in symbols {
        let community = match previous.seed.get(symbol) {
            Some(&old) => *mapping.entry(old).or_insert_with(|| {
                let assigned = next;
                next += 1;
                assigned
            }),
            None => {
                let assigned = next;
                next += 1;
                assigned
            }
        };
        communities.push(community);
    }

    communities
}

// ---------------------------------------------------------------------------
// Louvain
// ---------------------------------------------------------------------------

/// Phase one: move nodes between communities while any move raises modularity.
fn local_moving(graph: &WeightedGraph, mut communities: Vec<usize>) -> Vec<usize> {
    let n = graph.node_count();
    let m2 = graph.total_degree;
    if n == 0 || m2 <= 0.0 {
        return communities;
    }

    // Community labels are node indices, so `n` slots always suffice.
    let mut community_degree = vec![0.0f64; n];
    for (node, &community) in communities.iter().enumerate() {
        community_degree[community] += graph.degrees[node];
    }

    let mut link_weight = vec![0.0f64; n];
    let mut touched: Vec<usize> = Vec::new();

    for _ in 0..MAX_LOCAL_MOVING_SWEEPS {
        let mut moved = false;

        for node in 0..n {
            let origin = communities[node];

            for &(neighbour, weight) in &graph.adj[node] {
                let community = communities[neighbour];
                // Weights are strictly positive, so a zero entry means untouched.
                if link_weight[community] == 0.0 {
                    touched.push(community);
                }
                link_weight[community] += weight;
            }
            // Evaluate every candidate with the node already pulled out, which is
            // what makes the gains comparable to each other.
            community_degree[origin] -= graph.degrees[node];

            let mut best = origin;
            let mut best_gain =
                link_weight[origin] - community_degree[origin] * graph.degrees[node] / m2;
            // Candidates arrive in neighbour order; sorting them makes the winner
            // of a tie depend on the graph alone.
            touched.sort_unstable();
            for &candidate in &touched {
                if candidate == origin {
                    continue;
                }
                let gain =
                    link_weight[candidate] - community_degree[candidate] * graph.degrees[node] / m2;
                if gain > best_gain + MODULARITY_EPSILON {
                    best_gain = gain;
                    best = candidate;
                }
            }

            community_degree[best] += graph.degrees[node];
            if best != origin {
                communities[node] = best;
                moved = true;
            }

            for &community in &touched {
                link_weight[community] = 0.0;
            }
            touched.clear();
        }

        if !moved {
            break;
        }
    }

    communities
}

/// Compact arbitrary community labels to `0..count`, in first-appearance order.
fn renumber(communities: &[usize]) -> (Vec<usize>, usize) {
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let compacted = communities
        .iter()
        .map(|&community| {
            *mapping.entry(community).or_insert_with(|| {
                let assigned = next;
                next += 1;
                assigned
            })
        })
        .collect();
    (compacted, next)
}

/// Phase two: collapse each community into a single node.
fn aggregate(graph: &WeightedGraph, communities: &[usize], count: usize) -> WeightedGraph {
    let mut adj_maps: Vec<HashMap<usize, f64>> = vec![HashMap::new(); count];
    let mut self_loops = vec![0.0f64; count];

    for (node, &community) in communities.iter().enumerate() {
        self_loops[community] += graph.self_loops[node];
        for &(neighbour, weight) in &graph.adj[node] {
            let other = communities[neighbour];
            if other == community {
                // Seen once from each end, so halve to recover the edge's weight.
                self_loops[community] += weight / 2.0;
            } else {
                *adj_maps[community].entry(other).or_insert(0.0) += weight;
            }
        }
    }

    WeightedGraph::new(adj_maps, self_loops)
}

// ---------------------------------------------------------------------------
// Convexity
// ---------------------------------------------------------------------------

/// Make every community *convex*: if two of its members are joined by a call
/// path, everything on that path is a member too.
///
/// Modularity has no notion of direction — `build_base_graph` hands Louvain an
/// undirected copy of the call graph — so a community is free to straddle a
/// one-way chain. Group `a` and `c` but not the `b` between them and the
/// aggregate the cluster map draws carries both `A -> B` and `B -> A`, which
/// reads as "these two call each other" over code where nothing of the sort
/// happens. Measured on jemalloc's `test_malloc_conf_2` neighbourhood, that was
/// three quarters of the edges on the map, against a call graph that is a DAG
/// apart from a single component: 91% of the calls behind those back edges ran
/// one way only.
///
/// Convex communities are exactly the communities whose quotient graph is
/// acyclic, which is why this is the thing to fix rather than the drawing. Two
/// consequences follow, and neither is a choice:
///
/// * Mutually recursive functions share a community. A strongly-connected
///   component split between two communities makes those two call each other by
///   definition, so no acyclic map can separate them.
/// * A community that straddles a path is split. The split follows a
///   topological order that prefers to stay inside one community, so a
///   community that straddles nothing comes through whole.
///
/// Every level is repaired, coarsest last, each over the quotient of the level
/// below — so the hierarchy stays a refinement and drilling in never uncovers a
/// cycle the level above had hidden.
fn make_convex(graph: &CallGraph, symbols: &[SymbolId], mut finest: Vec<usize>) -> Vec<Vec<usize>> {
    let succ = directed_edges(graph, symbols);
    let (component, component_count) = strongly_connected(&succ);

    // Mutual recursion first: whatever Louvain decided, the members of one
    // component have to end up together. They go where most of them already
    // were, which keeps the most of Louvain's judgement; ties go to the lowest
    // community, so the answer does not depend on hash order.
    let mut votes: Vec<HashMap<usize, usize>> = vec![HashMap::new(); component_count];
    for (node, &comp) in component.iter().enumerate() {
        *votes[comp].entry(finest[node]).or_insert(0) += 1;
    }
    let component_community: Vec<usize> = votes
        .into_iter()
        .map(|tally| {
            tally
                .into_iter()
                .max_by_key(|&(of, count)| (count, std::cmp::Reverse(of)))
                .expect("every component has at least one member")
                .0
        })
        .collect();

    // Level 0 is repaired over the condensation — the finest DAG there is.
    let condensation = quotient(&succ, &component, component_count);
    let (label_of_component, _) = repair_partition(&condensation, &component_community);
    for (node, &comp) in component.iter().enumerate() {
        finest[node] = label_of_component[comp];
    }

    // The coarser levels are rebuilt rather than repaired. Louvain found them
    // by aggregating the partition it had *before* the repair, over the same
    // undirected copy that lost the direction in the first place, so repairing
    // them means cutting groupings that were formed across the cuts just made:
    // on jemalloc that left 289 communities coarsening to 103 and then to 70, a
    // hierarchy that barely coarsens and a map of seventy boxes. Merging up from
    // the repaired level instead keeps the constraint in front of the decision
    // rather than behind it.
    let mut levels = vec![finest];
    let mut units = level_graph(graph, symbols, &levels[0]);
    let mut unit_dag = quotient(&succ, &levels[0], units.node_count());
    loop {
        let (merged, count) = agglomerate_acyclically(&units, &unit_dag);
        if count == units.node_count() {
            break;
        }
        units = aggregate(&units, &merged, count);
        unit_dag = quotient(&unit_dag, &merged, count);
        levels.push(merged);
    }
    let dendrogram = levels;

    debug_assert!(
        dendrogram
            .iter()
            .skip(1)
            .all(|level| level.iter().copied().max().map_or(0, |m| m + 1) < level.len()),
        "a level that merged nothing is never recorded"
    );

    dendrogram
}

/// The weighted graph one level up: communities as nodes, the calls between
/// them as weights.
///
/// Built the same way [`build_base_graph`] builds the bottom one — a call adds
/// its weight in both directions, an internal one lands on the community's
/// self-loop — so that `modularity` means the same thing at every level.
fn level_graph(graph: &CallGraph, symbols: &[SymbolId], community: &[usize]) -> WeightedGraph {
    let count = community.iter().copied().max().map_or(0, |m| m + 1);
    let position: HashMap<SymbolId, usize> =
        symbols.iter().enumerate().map(|(i, s)| (*s, i)).collect();

    let mut adj_maps: Vec<HashMap<usize, f64>> = vec![HashMap::new(); count];
    let mut self_loops = vec![0.0f64; count];
    for (i, symbol) in symbols.iter().enumerate() {
        for callee in graph.get_callees(symbol) {
            let Some(&j) = position.get(&callee.id) else {
                continue;
            };
            if i == j {
                continue;
            }
            let (a, b) = (community[i], community[j]);
            if a == b {
                self_loops[a] += 1.0;
            } else {
                *adj_maps[a].entry(b).or_insert(0.0) += 1.0;
                *adj_maps[b].entry(a).or_insert(0.0) += 1.0;
            }
        }
    }

    WeightedGraph::new(adj_maps, self_loops)
}

/// Build one coarser level by merging communities in pairs, best modularity
/// gain first, refusing any merge that would put a cycle back.
///
/// This stands in for Louvain's own aggregation at every level above the first.
/// Local moving cannot be constrained the same way — it moves one node at a
/// time between communities that are already fixed, so a move that breaks
/// acyclicity is only visible after the fact — whereas a merge either keeps the
/// quotient a DAG or does not, and the test is cheap: `a` and `b` can join
/// unless some third community sits on a path between them.
///
/// Greedy pairwise merging is weaker than local moving at finding communities,
/// and that is the trade. What it buys is a hierarchy whose every level can be
/// drawn as flow.
fn agglomerate_acyclically(graph: &WeightedGraph, dag: &[Vec<usize>]) -> (Vec<usize>, usize) {
    let n = graph.node_count();
    let mut community: Vec<usize> = (0..n).collect();
    if n < 2 || graph.total_degree <= 0.0 {
        return (community, n);
    }

    // Live communities, their degree, and the weight between them. Merging
    // folds one into the other and leaves the empty one behind.
    let mut degree = graph.degrees.clone();
    let mut between: Vec<HashMap<usize, f64>> = vec![HashMap::new(); n];
    for (node, neighbours) in graph.adj.iter().enumerate() {
        for &(other, weight) in neighbours {
            between[node].insert(other, weight);
        }
    }
    let mut alive: Vec<bool> = vec![true; n];
    let total = graph.total_degree;
    // Reused by every candidate pair of every sweep. See `BetweenScratch`.
    let mut scratch = BetweenScratch::new(n.div_ceil(64));
    let mut gain_scratch = GainScratch::new(n);

    loop {
        let live = dag_of_alive(dag, &community, n);
        let reaches = Reachability::of(&live);
        let backwards = Reachability::of(&reversed(&live));
        // One bitset for the whole sweep below. Nothing dies until a merge is
        // chosen, and building this per candidate pair would cost exactly the
        // per-node pass `between_closure` no longer makes.
        let mut alive_mask = vec![0u64; n.div_ceil(64)];
        for (index, &is_alive) in alive.iter().enumerate() {
            if is_alive {
                alive_mask[index / 64] |= 1 << (index % 64);
            }
        }

        // Candidates, best first: the modularity gain of joining two
        // communities, in the usual agglomerative form — the weight they
        // already share, less the weight they would share if the edges were
        // thrown at random.
        let mut candidates: Vec<(f64, usize, usize)> = Vec::new();
        for a in 0..n {
            if !alive[a] {
                continue;
            }
            for (&b, &weight) in &between[a] {
                if b <= a || !alive[b] {
                    continue;
                }
                // dQ = 2 * (e_ab/2m - a_a*a_b), with `total` standing for 2m.
                let gain = 2.0 * (weight / total - (degree[a] * degree[b]) / (total * total));
                if gain > MODULARITY_EPSILON {
                    candidates.push((gain, a, b));
                }
            }
        }
        // Lowest pair wins a tie, so the same graph merges the same way.
        candidates.sort_by(|x, y| {
            y.0.partial_cmp(&x.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| (x.1, x.2).cmp(&(y.1, y.2)))
        });

        let mut chosen: Option<Vec<usize>> = None;
        for (gain, a, b) in candidates {
            let group = between_closure(&mut scratch, &reaches, &backwards, &alive_mask, a, b);
            if group.len() == 2 {
                chosen = Some(group.to_vec());
                break;
            }
            // Blocked: something sits on a path between the two. Joining them
            // alone would put a cycle back, but joining them *with whatever is
            // between them* cannot — and is the merge Louvain would have made
            // by moving nodes one at a time, which pairwise merging cannot see.
            // Worth it only if the whole set still gains.
            let _ = gain;
            if set_gain(&mut gain_scratch, group, &between, &degree, total) > MODULARITY_EPSILON {
                chosen = Some(group.to_vec());
                break;
            }
        }

        let Some(group) = chosen else { break };
        let keep = *group.iter().min().expect("a merge joins at least two");
        for &gone in group.iter().filter(|&&g| g != keep) {
            alive[gone] = false;
            degree[keep] += degree[gone];
            for member in community.iter_mut() {
                if *member == gone {
                    *member = keep;
                }
            }
            let folded: Vec<(usize, f64)> = between[gone].iter().map(|(&o, &w)| (o, w)).collect();
            for (other, weight) in folded {
                between[other].remove(&gone);
                if other == keep {
                    continue;
                }
                *between[keep].entry(other).or_insert(0.0) += weight;
                *between[other].entry(keep).or_insert(0.0) += weight;
            }
            between[keep].remove(&gone);
            between[gone].clear();
        }
    }

    let (dense, count) = renumber(&community);
    (dense, count)
}

/// The smallest set containing `a` and `b` that is closed under "lies on a path
/// between two members".
///
/// Merging exactly this set is the smallest merge involving both that keeps the
/// quotient acyclic: anything left on a path between two of its members would
/// be a cycle through the merged community. Comes back as just `[a, b]` when
/// nothing is in the way — which is *not* the common case, and it is worth
/// being plain about that: over a 10,000-function project only 1,186 of the
/// 182,332 pairs asked about came back unblocked, and two thirds came back with
/// fifteen or more communities wedged between them. The caller's `set_gain`
/// then has to price all of them, which is why that, and not this, is where the
/// sweep now spends its time.
///
/// In a DAG, "something in the group reaches y and y reaches something in the
/// group" is enough to place `y` between two members: the two cannot be the
/// same node, or that member would reach itself.
///
/// Asked of every candidate at once rather than one at a time, because this
/// runs once per candidate pair per merge and testing each live community in
/// turn made it the cost of the entire clustering — 18s to cluster a
/// 10,000-function project. Both halves of the test are unions of rows the two
/// closures already hold: `reaches.row(m)` *is* `{y : m reaches y}`, and
/// `backwards.row(m)` *is* `{y : y reaches m}`, so the communities between the
/// group are
///
/// ```text
/// (⋃ reaches.row(m)) & (⋃ backwards.row(m)) & alive & !group
/// ```
///
/// which is a word-wise AND over `n/64` words instead of a pass over `n`
/// communities. Neither union ever shrinks — the group only grows — so a round
/// ORs in the rows of what the last round added and nothing else.
///
/// The unions describe the group as it stood when the round began, never as it
/// grows mid-scan. That is deliberate and matches what the per-candidate form
/// did: widening the target while testing against it would make the result
/// depend on the order candidates happened to be visited.
fn between_closure<'s>(
    scratch: &'s mut BetweenScratch,
    reaches: &Reachability,
    backwards: &Reachability,
    alive: &[u64],
    a: usize,
    b: usize,
) -> &'s [usize] {
    let words = alive.len();
    let BetweenScratch {
        forward,
        backward,
        inside,
        added,
        group,
    } = scratch;
    forward.fill(0);
    backward.fill(0);
    inside.fill(0);
    group.clear();
    group.extend([a, b]);
    // The members whose rows are not folded into the unions yet.
    added.clear();
    added.extend([a, b]);

    loop {
        for &member in added.iter() {
            inside[member / 64] |= 1 << (member % 64);
            for (word, row) in forward.iter_mut().zip(reaches.row(member)) {
                *word |= row;
            }
            for (word, row) in backward.iter_mut().zip(backwards.row(member)) {
                *word |= row;
            }
        }

        added.clear();
        for word in 0..words {
            let mut between = forward[word] & backward[word] & alive[word] & !inside[word];
            while between != 0 {
                added.push(word * 64 + between.trailing_zeros() as usize);
                between &= between - 1;
            }
        }
        if added.is_empty() {
            // Ascending, as the `BTreeSet` this used to be gave it. The order
            // reaches the caller's fold of `between[gone]` into `between[keep]`,
            // and that is a sum of floats — so it is part of the answer, not a
            // presentational detail.
            group.sort_unstable();
            return group;
        }
        group.extend(added.iter().copied());
    }
}

/// Reusable buffers for [`between_closure`].
///
/// One per merge sweep rather than one per call, because the sweep asks about
/// every candidate pair — 182,000 of them on a 10,000-function project. Three
/// bitsets and a `BTreeSet` allocated and dropped per question came to 2.1s of
/// a 5s clustering, an order of magnitude more than the word math they carried.
struct BetweenScratch {
    /// Everything the group reaches.
    forward: Vec<u64>,
    /// Everything that reaches the group.
    backward: Vec<u64>,
    /// The group itself.
    inside: Vec<u64>,
    /// Members found by the round in progress, not yet folded into the unions.
    added: Vec<usize>,
    group: Vec<usize>,
}

impl BetweenScratch {
    fn new(words: usize) -> Self {
        Self {
            forward: vec![0u64; words],
            backward: vec![0u64; words],
            inside: vec![0u64; words],
            added: Vec::new(),
            group: Vec::new(),
        }
    }
}

/// The same graph with every edge turned round.
fn reversed(dag: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); dag.len()];
    for (node, targets) in dag.iter().enumerate() {
        for &target in targets {
            out[target].push(node);
        }
    }
    out
}

/// The modularity gain of merging a whole set at once.
///
/// The pairwise formula generalised: everything the members already share,
/// less what a random graph with the same degrees would have given them.
/// Read by gathering rather than by probing: `between[u]` is walked once and
/// its weights dropped into a slot per group position, so the inner sum is a
/// scan of `slots[i + 1..]` instead of one hash lookup per pair. The pairs are
/// what there are a lot of — 196 million of them over a 10,000-function
/// project, against 112 million entries in the `between` rows behind them.
///
/// `slots` stays zero between calls, so an absent pair reads as the `0.0` the
/// probe form got from `unwrap_or`, and `shared` accumulates over exactly the
/// same positions in exactly the same order. That order is the answer and not
/// merely tidy: these are floats, and addition is neither associative nor
/// commutative, so summing the same weights in another order is a different
/// number.
fn set_gain(
    scratch: &mut GainScratch,
    group: &[usize],
    between: &[HashMap<usize, f64>],
    degree: &[f64],
    total: f64,
) -> f64 {
    let GainScratch {
        stamp,
        slot_of,
        slots,
        generation,
    } = scratch;
    // Bumping the stamp retires the last call's membership marks in O(1). At
    // 181,000 calls a sweep, clearing an array the size of the whole quotient
    // would cost more than the probes this is replacing.
    *generation += 1;
    slots.clear();
    slots.resize(group.len(), 0.0);
    for (i, &u) in group.iter().enumerate() {
        stamp[u] = *generation;
        slot_of[u] = i as u32;
    }

    let mut shared = 0.0;
    let mut degree_sum = 0.0;
    let mut degree_squares = 0.0;
    for (i, &u) in group.iter().enumerate() {
        degree_sum += degree[u];
        degree_squares += degree[u] * degree[u];
        for (&v, &weight) in &between[u] {
            if stamp[v] == *generation {
                slots[slot_of[v] as usize] = weight;
            }
        }
        for &slot in &slots[i + 1..] {
            shared += slot;
        }
        // Only the slots just written need putting back, which is what keeps
        // the invariant "zero between calls" cheap.
        for &v in between[u].keys() {
            if stamp[v] == *generation {
                slots[slot_of[v] as usize] = 0.0;
            }
        }
    }
    2.0 * shared / total - (degree_sum * degree_sum - degree_squares) / (total * total)
}

/// Reusable buffers for [`set_gain`], one per merge sweep.
struct GainScratch {
    /// `stamp[v] == generation` marks `v` as a member of the group being priced.
    stamp: Vec<u64>,
    /// Which group position `v` holds, meaningful only while `v` is stamped.
    slot_of: Vec<u32>,
    /// One weight per group position. Zero except inside the gather.
    slots: Vec<f64>,
    generation: u64,
}

impl GainScratch {
    fn new(units: usize) -> Self {
        Self {
            stamp: vec![0; units],
            slot_of: vec![0; units],
            slots: Vec::new(),
            generation: 0,
        }
    }
}

/// The directed graph over live communities, for the acyclicity test.
fn dag_of_alive(dag: &[Vec<usize>], community: &[usize], n: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (node, targets) in dag.iter().enumerate() {
        let from = community[node];
        for &target in targets {
            let to = community[target];
            if from != to {
                out[from].insert(to);
            }
        }
    }
    out.into_iter()
        .map(|set| set.into_iter().collect())
        .collect()
}

/// The call graph's edges in the index space the clustering works in.
///
/// Unlike `build_base_graph` this keeps the direction, which is the whole point
/// of it: the undirected copy is what loses the property being restored here.
fn directed_edges(graph: &CallGraph, symbols: &[SymbolId]) -> Vec<Vec<usize>> {
    let position: HashMap<SymbolId, usize> =
        symbols.iter().enumerate().map(|(i, s)| (*s, i)).collect();
    symbols
        .iter()
        .map(|symbol| {
            let mut targets: Vec<usize> = graph
                .get_callees(symbol)
                .iter()
                .filter_map(|callee| position.get(&callee.id).copied())
                .collect();
            targets.sort_unstable();
            targets.dedup();
            targets
        })
        .enumerate()
        .map(|(i, targets)| targets.into_iter().filter(|&j| j != i).collect())
        .collect()
}

/// Tarjan's strongly-connected components, iteratively: the recursion depth
/// here is the call depth of the project being read, which is not ours to bound.
///
/// Returns each node's component and how many there are. Components are what
/// convexity has to treat as indivisible.
fn strongly_connected(succ: &[Vec<usize>]) -> (Vec<usize>, usize) {
    let n = succ.len();
    let (mut index, mut low) = (vec![usize::MAX; n], vec![0usize; n]);
    let mut on_stack = vec![false; n];
    let mut component = vec![usize::MAX; n];
    let (mut stack, mut next_index, mut next_component) = (Vec::new(), 0usize, 0usize);

    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        // Each frame is a node and how far through its successors we are.
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some((node, resume)) = work.last().copied() {
            if resume == 0 {
                index[node] = next_index;
                low[node] = next_index;
                next_index += 1;
                stack.push(node);
                on_stack[node] = true;
            }

            let mut descended = false;
            for (offset, &next) in succ[node].iter().enumerate().skip(resume) {
                work.last_mut().expect("the frame we are working on").1 = offset + 1;
                if index[next] == usize::MAX {
                    work.push((next, 0));
                    descended = true;
                    break;
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
            }
            if descended {
                continue;
            }

            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = next_component;
                    if member == node {
                        break;
                    }
                }
                next_component += 1;
            }
        }
    }

    (component, next_component)
}

/// Collapse a graph onto one node per group: the edges between groups, with
/// duplicates and self-loops dropped.
fn quotient(succ: &[Vec<usize>], group: &[usize], group_count: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); group_count];
    for (node, targets) in succ.iter().enumerate() {
        let from = group[node];
        for &target in targets {
            let to = group[target];
            if from != to {
                out[from].insert(to);
            }
        }
    }
    out.into_iter()
        .map(|set| set.into_iter().collect())
        .collect()
}

/// Split only the communities that actually form cycles, and only against each
/// other.
///
/// Every cycle in the quotient graph lies inside one of its strongly-connected
/// components, so the communities outside those components are already convex
/// enough and are left exactly as they are — which matters, because splitting a
/// community that was not the problem costs a box on the map for nothing.
///
/// Within one tangled component the fix is to cut its communities into the
/// maximal runs of a topological order. A run occupies consecutive positions of
/// that order and every edge runs forward, so nothing can sit on a path between
/// two of its members: ordering the runs by position topologically sorts what
/// was a knot. A community the order never has to leave and come back to yields
/// one run and survives whole, which is why the order is greedy about staying
/// put.
///
/// No path between two members of a tangled component can leave it and come
/// back — that would put whatever it passed through on a cycle, and so inside
/// the component — which is what lets each component be repaired against the
/// subgraph it induces, in ignorance of everything else.
///
/// Returns the new community of each unit, and for each new community the one
/// it was cut from.
fn repair_partition(succ: &[Vec<usize>], community: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let community_count = community.iter().copied().max().map_or(0, |m| m + 1);
    let quotient_graph = quotient(succ, community, community_count);
    let (tangle, _) = strongly_connected(&quotient_graph);

    let mut tangle_size: HashMap<usize, usize> = HashMap::new();
    for &of in &tangle {
        *tangle_size.entry(of).or_insert(0) += 1;
    }
    let tangled = |of: usize| tangle_size.get(&tangle[of]).copied().unwrap_or(0) > 1;

    let mut label = vec![usize::MAX; succ.len()];
    let mut origin: Vec<usize> = Vec::new();

    // Untangled communities keep their membership; only their numbering moves.
    // Taken in community order so the result does not depend on hash order.
    let mut kept: HashMap<usize, usize> = HashMap::new();
    for of in 0..community_count {
        if !tangled(of) {
            kept.insert(of, origin.len());
            origin.push(of);
        }
    }
    for (unit, &of) in community.iter().enumerate() {
        if let Some(&new) = kept.get(&of) {
            label[unit] = new;
        }
    }

    // Each tangled component is cut against the subgraph it induces.
    let mut components: Vec<usize> = (0..community_count).filter(|&of| tangled(of)).collect();
    components.sort_unstable_by_key(|&of| tangle[of]);
    let mut done: HashSet<usize> = HashSet::new();
    for of in components {
        if !done.insert(tangle[of]) {
            continue;
        }
        let inside: Vec<usize> = (0..succ.len())
            .filter(|&unit| tangle[community[unit]] == tangle[of])
            .collect();
        let local: HashMap<usize, usize> =
            inside.iter().enumerate().map(|(i, &u)| (u, i)).collect();
        let induced: Vec<Vec<usize>> = inside
            .iter()
            .map(|unit| {
                succ[*unit]
                    .iter()
                    .filter_map(|target| local.get(target).copied())
                    .collect()
            })
            .collect();
        let inner_community: Vec<usize> = inside.iter().map(|&unit| community[unit]).collect();

        let order = clustered_topological_order(&induced, &inner_community);
        let mut run_of: Vec<usize> = vec![usize::MAX; inside.len()];
        let mut run_origin: Vec<usize> = Vec::new();
        let mut current: Option<usize> = None;
        for &local_unit in &order {
            let was = inner_community[local_unit];
            if current != Some(was) {
                run_origin.push(was);
                current = Some(was);
            }
            run_of[local_unit] = run_origin.len() - 1;
        }

        let representative = merge_severed_runs(&induced, &run_of, &run_origin);

        // Number the surviving runs in the order they were cut, so the output
        // does not depend on which pairs the merge happened to consider first.
        let mut numbered: HashMap<usize, usize> = HashMap::new();
        for &rep in representative.iter() {
            numbered.entry(rep).or_insert_with(|| {
                origin.push(run_origin[rep]);
                origin.len() - 1
            });
        }
        for (local_unit, &run) in run_of.iter().enumerate() {
            label[inside[local_unit]] = numbered[&representative[run]];
        }
    }

    // Unreachable while every caller hands in a DAG, which they do. Left total
    // rather than panicking: a cycle here costs one map its acyclicity, and
    // taking the whole clustering down with it helps nobody.
    debug_assert!(
        label.iter().all(|&l| l != usize::MAX),
        "a DAG orders all of its nodes"
    );
    for unit in 0..succ.len() {
        if label[unit] == usize::MAX {
            origin.push(community[unit]);
            label[unit] = origin.len() - 1;
        }
    }

    (label, origin)
}

/// Put back the cuts that were not holding anything up.
///
/// The run split is blunt on purpose — it cuts a community every time the
/// topological order has to step outside it — but the order is one order among
/// many, and most of those steps are an artefact of the one it picked rather
/// than something the call graph insists on. On the demo project the blunt cut
/// alone turned 14 top-level communities into 16, over half of them holding a
/// single group.
///
/// So two runs of the same community are rejoined whenever that cannot put a
/// cycle back: when no *third* run sits on a path between them. Runs that
/// neither reaches pass trivially, which is the common case and the one that
/// recovers most of the damage.
///
/// Returns each run's representative — itself, if nothing merged into it.
fn merge_severed_runs(
    induced: &[Vec<usize>],
    run_of: &[usize],
    run_origin: &[usize],
) -> Vec<usize> {
    let count = run_origin.len();
    let mut representative: Vec<usize> = (0..count).collect();

    loop {
        let edges = quotient(
            induced,
            &run_of_representative(run_of, &representative),
            count,
        );
        let reaches = Reachability::of(&edges);

        // Lowest pair first, so the merges happen in a fixed order.
        let mut merged = None;
        'pairs: for a in 0..count {
            if representative[a] != a {
                continue;
            }
            for b in (a + 1)..count {
                if representative[b] != b || run_origin[a] != run_origin[b] {
                    continue;
                }
                let between = (0..count).filter(|&y| representative[y] == y).any(|y| {
                    y != a
                        && y != b
                        && ((reaches.reaches(a, y) && reaches.reaches(y, b))
                            || (reaches.reaches(b, y) && reaches.reaches(y, a)))
                });
                if !between {
                    merged = Some((a, b));
                    break 'pairs;
                }
            }
        }

        let Some((keep, gone)) = merged else {
            return representative;
        };
        for rep in representative.iter_mut() {
            if *rep == gone {
                *rep = keep;
            }
        }
    }
}

/// Each unit's run, as its representative — the grouping `quotient` needs while
/// runs are being merged.
fn run_of_representative(run_of: &[usize], representative: &[usize]) -> Vec<usize> {
    run_of.iter().map(|&run| representative[run]).collect()
}

/// Who can reach whom, one bit per node.
///
/// A `Vec<Vec<bool>>` filled by a traversal per node is the obvious shape and
/// was far too slow to ship: this gets recomputed after every merge, and at a
/// few hundred communities that is hundreds of full sweeps — 3s to cluster a
/// 2000-function project, against 12ms before any of this. The input is always
/// a DAG, so reachability instead falls out of a single reverse topological
/// pass in which each node ORs together what its successors reach, over rows of
/// machine words rather than bytes.
///
/// Rows are handed out whole ([`Reachability::row`]) as well as probed one bit
/// at a time, because the caller that dominates this — [`between_closure`] —
/// wants the union of several nodes' reach rather than an answer about any one
/// of them, and a union of rows is an OR over `n/64` words.
struct Reachability {
    /// Row-major and flat: node `i`'s row is `rows[i * words..][..words]`. A
    /// `Vec<Vec<u64>>` is the same bits in `n` separate allocations, and this is
    /// rebuilt twice per merge — on a 10,000-function project that was twelve
    /// million allocations spent on the shape of the container.
    rows: Vec<u64>,
    words: usize,
}

impl Reachability {
    fn of(dag: &[Vec<usize>]) -> Self {
        let n = dag.len();
        let words = n.div_ceil(64);
        let mut rows = vec![0u64; n * words];
        // One scratch row for the whole pass. A node's row cannot be built in
        // place: it is read from as its successors' rows are ORed in, which is
        // the same borrow.
        let mut reached = vec![0u64; words];
        // A node's reach is known once every successor's is, which is what
        // walking the topological order backwards guarantees.
        for &node in plain_topological_order(dag).iter().rev() {
            reached.fill(0);
            for &next in &dag[node] {
                reached[next / 64] |= 1 << (next % 64);
                for (word, other) in reached.iter_mut().zip(&rows[next * words..][..words]) {
                    *word |= other;
                }
            }
            rows[node * words..][..words].copy_from_slice(&reached);
        }
        Self { rows, words }
    }

    fn reaches(&self, from: usize, to: usize) -> bool {
        self.rows[from * self.words + to / 64] >> (to % 64) & 1 == 1
    }

    /// Everything `node` reaches, as a bitset — one bit per node, `n/64` words.
    fn row(&self, node: usize) -> &[u64] {
        &self.rows[node * self.words..][..self.words]
    }
}

/// Kahn's algorithm, taking whatever is ready in index order.
fn plain_topological_order(dag: &[Vec<usize>]) -> Vec<usize> {
    let mut waiting_on = vec![0usize; dag.len()];
    for targets in dag {
        for &target in targets {
            waiting_on[target] += 1;
        }
    }
    let mut ready: Vec<usize> = (0..dag.len()).filter(|&n| waiting_on[n] == 0).collect();
    let mut order = Vec::with_capacity(dag.len());
    while let Some(node) = ready.pop() {
        order.push(node);
        for &target in &dag[node] {
            waiting_on[target] -= 1;
            if waiting_on[target] == 0 {
                ready.push(target);
            }
        }
    }
    order
}

/// Topologically order a DAG, staying inside one community for as long as the
/// edges allow.
///
/// Kahn's algorithm, with the ready set kept per community so that "carry on
/// with the community we are already in" is a lookup rather than a scan. When
/// the current community runs dry it starts the largest run available, because
/// the number of runs is what this is trying to keep down. Ties go to the lowest
/// community, then the lowest unit, so the same graph always orders the same way.
fn clustered_topological_order(succ: &[Vec<usize>], community: &[usize]) -> Vec<usize> {
    let mut waiting_on = vec![0usize; succ.len()];
    for targets in succ {
        for &target in targets {
            waiting_on[target] += 1;
        }
    }

    let mut ready: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    for (unit, &count) in waiting_on.iter().enumerate() {
        if count == 0 {
            ready.entry(community[unit]).or_default().insert(unit);
        }
    }

    let mut order = Vec::with_capacity(succ.len());
    let mut current: Option<usize> = None;
    while !ready.is_empty() {
        let pick = match current.filter(|of| ready.contains_key(of)) {
            Some(of) => of,
            None => {
                *ready
                    .iter()
                    .max_by_key(|(of, units)| (units.len(), std::cmp::Reverse(**of)))
                    .expect("the loop runs only while something is ready")
                    .0
            }
        };

        let units = ready.get_mut(&pick).expect("just picked");
        let unit = *units.iter().next().expect("empty sets are never kept");
        units.remove(&unit);
        if units.is_empty() {
            ready.remove(&pick);
        }
        current = Some(pick);
        order.push(unit);

        for &target in &succ[unit] {
            waiting_on[target] -= 1;
            if waiting_on[target] == 0 {
                ready.entry(community[target]).or_default().insert(target);
            }
        }
    }

    order
}

// ---------------------------------------------------------------------------
// Dendrogram -> clusters
// ---------------------------------------------------------------------------

fn build_clustering(
    graph: &CallGraph,
    symbols: &[SymbolId],
    dendrogram: &[Vec<usize>],
) -> Clustering {
    // Which level-`l` community each base node ends up in.
    let mut owners: Vec<Vec<usize>> = Vec::with_capacity(dendrogram.len());
    owners.push(dendrogram[0].clone());
    for level in &dendrogram[1..] {
        let previous = owners.last().expect("a level was just pushed");
        owners.push(previous.iter().map(|&community| level[community]).collect());
    }

    // Members per community per level. Base indices are already in symbol order,
    // so pushing in index order leaves each member list sorted.
    let mut members: Vec<Vec<Vec<usize>>> = Vec::with_capacity(owners.len());
    for (level, owner) in owners.iter().enumerate() {
        let count = dendrogram[level].iter().copied().max().unwrap_or(0) + 1;
        let mut buckets = vec![Vec::new(); count];
        for (node, &community) in owner.iter().enumerate() {
            buckets[community].push(node);
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
        for &community in &populated {
            let id = ClusterId(next_id);
            next_id += 1;
            slots[community] = Some(id);
            level_ids.push(id);
        }
        levels.push(level_ids);
        ids.push(slots);
        order.push(populated);
    }

    let mut clusters: Vec<Cluster> = Vec::with_capacity(next_id as usize);
    for (level, populated) in order.iter().enumerate() {
        for &community in populated {
            let id = ids[level][community].expect("populated communities all have ids");
            let member_symbols: Vec<SymbolId> = members[level][community]
                .iter()
                .map(|&i| symbols[i])
                .collect();

            let parent = dendrogram
                .get(level + 1)
                .and_then(|next| ids[level + 1][next[community]]);

            let children = if level == 0 {
                Vec::new()
            } else {
                order[level - 1]
                    .iter()
                    .filter(|&&child| dendrogram[level][child] == community)
                    .filter_map(|&child| ids[level - 1][child])
                    .collect()
            };

            // Populated communities are the only ones given ids, so there is
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
        .filter_map(|(community, bucket)| ids[0][community].map(|id| (bucket, id)))
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
        // Filled in by the caller, which is the only place that still has the
        // partition Louvain produced before it was made convex.
        seed: HashMap::new(),
    }
}

/// Pick the member that best serves as the cluster's front door.
///
/// A front door is entered from outside *and* leads inward, and it is the
/// second half that carries the label. Ranking purely by incoming calls from
/// outside sounds right and is not: the most-called member of almost any
/// cluster is a shared leaf. It labelled this project's networking community
/// `log_write` and its startup community `parse_integer` — both genuinely the
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
    // naming, which is how the community holding `main` came to be labelled
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
            Location::new("test.c".to_string(), line, 0),
        ))
    }

    /// Call sites are deduplicated by location, so each call needs its own line.
    fn call(graph: &mut CallGraph, from: SymbolId, to: SymbolId, line: u32) {
        graph.add_call(from, to, Location::new("test.c".to_string(), line, 0));
    }

    /// A directed triangle `a -> b -> c`, `a -> c`.
    fn triangle(graph: &mut CallGraph, prefix: &str, base_line: u32) -> [SymbolId; 3] {
        let a = add(graph, &format!("{prefix}_a"), base_line);
        let b = add(graph, &format!("{prefix}_b"), base_line + 1);
        let c = add(graph, &format!("{prefix}_c"), base_line + 2);
        call(graph, a, b, base_line + 100);
        call(graph, b, c, base_line + 101);
        call(graph, a, c, base_line + 102);
        [a, b, c]
    }

    /// Two triangles joined by a single call from the first into the second.
    fn two_triangles() -> (CallGraph, [SymbolId; 3], [SymbolId; 3]) {
        let mut graph = CallGraph::new();
        let left = triangle(&mut graph, "left", 10);
        let right = triangle(&mut graph, "right", 40);
        call(&mut graph, left[2], right[0], 500);
        (graph, left, right)
    }

    /// Everything the public API exposes, as comparable text.
    fn fingerprint(clustering: &Clustering) -> String {
        let mut out = String::new();
        for level in 0..clustering.level_count() {
            out.push_str(&format!("level {level}:"));
            for id in clustering.level(level) {
                let cluster = clustering.cluster(*id).unwrap();
                out.push_str(&format!(
                    " [{} parent={:?} children={:?} label={} entry={:?} members={:?}]",
                    cluster.id.0,
                    cluster.parent,
                    cluster.children,
                    cluster.label,
                    cluster.entry_point,
                    cluster.members
                ));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn two_triangles_split_into_two_clusters() {
        let (graph, left, right) = two_triangles();
        let clustering = Clustering::compute(&graph);

        assert_eq!(clustering.level(0).len(), 2);

        let left_cluster = clustering.cluster_of(&left[0]).unwrap();
        let right_cluster = clustering.cluster_of(&right[0]).unwrap();
        assert_ne!(left_cluster, right_cluster);
        for member in left {
            assert_eq!(clustering.cluster_of(&member), Some(left_cluster));
        }
        for member in right {
            assert_eq!(clustering.cluster_of(&member), Some(right_cluster));
        }
    }

    /// The partition at one level, as sorted names — readable enough to assert
    /// against literally, so a test failure says which functions moved.
    fn grouping_at(graph: &CallGraph, clustering: &Clustering, level: usize) -> Vec<Vec<String>> {
        let mut groups: Vec<Vec<String>> = clustering
            .level(level)
            .iter()
            .map(|id| {
                let mut names: Vec<String> = clustering
                    .cluster(*id)
                    .unwrap()
                    .members
                    .iter()
                    .map(|m| graph.get_function(m).unwrap().name.clone())
                    .collect();
                names.sort();
                names
            })
            .collect();
        groups.sort();
        groups
    }

    /// Pins the exact hierarchy of a fixture small enough to read, because the
    /// properties the other tests assert — every level partitions the symbols,
    /// ancestry is a chain — hold just as well for a grouping that has silently
    /// changed.
    ///
    /// Asserted as names rather than as a hash of something larger: the merge
    /// decisions behind this turn on `f64` comparisons, so a fixture big enough
    /// to need hashing is a fixture that could come out differently under a
    /// different instruction selection, and a test that only fails on somebody
    /// else's machine is worse than no test at all.
    ///
    /// What that size buys and what it costs, measured by mutation rather than
    /// assumed: dropping the convexity repair at level 0 fails this test, but
    /// disabling the blocked-set merge in `agglomerate_acyclically`, skipping
    /// `merge_severed_runs`, and coarsening `MODULARITY_EPSILON` all leave it
    /// green. Those branches need a graph with enough communities to wedge one
    /// between two others, and every fixture that small merges nothing at all.
    /// So this guards the repair, not the agglomeration — the gap is real and
    /// is left here in writing rather than implied away.
    #[test]
    fn the_hierarchy_of_a_known_graph_does_not_drift() {
        let graph = layered_graph();
        let clustering = Clustering::compute(&graph);
        assert_eq!(clustering.level_count(), 2);

        // Level 0 is Louvain's, repaired for convexity — which is why `t3` and
        // `t4` come out as interleaved pairs with a singleton at each end
        // rather than as two clean triangles: the order has to leave `t3` and
        // come back, and what it leaves behind cannot rejoin without putting a
        // cycle back on the map.
        assert_eq!(
            grouping_at(&graph, &clustering, 0),
            vec![
                vec!["t1_a", "t1_b", "t1_c"],
                vec!["t2_a", "t2_b", "t2_c"],
                vec!["t3_a"],
                vec!["t3_b", "t4_a"],
                vec!["t3_c", "t4_b"],
                vec!["t4_c"],
            ]
        );

        // Level 1 is `agglomerate_acyclically`'s own: the pairs, merged back up
        // under the constraint into the two halves the fixture is built from.
        assert_eq!(
            grouping_at(&graph, &clustering, 1),
            vec![
                vec!["t1_a", "t1_b", "t1_c", "t2_a", "t2_b", "t2_c"],
                vec!["t3_a", "t3_b", "t3_c", "t4_a", "t4_b", "t4_c"],
            ]
        );

        // The front doors come with the grouping and are just as easy to move
        // by accident.
        let labels: HashSet<&str> = clustering
            .top_level()
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().label.as_str())
            .collect();
        assert_eq!(labels, HashSet::from(["t1_a", "t3_a"]));
    }

    #[test]
    fn recomputing_gives_identical_output() {
        let (graph, _, _) = two_triangles();
        assert_eq!(
            fingerprint(&Clustering::compute(&graph)),
            fingerprint(&Clustering::compute(&graph))
        );
    }

    #[test]
    fn compute_matches_unseeded_compute_seeded() {
        let (graph, _, _) = two_triangles();
        assert_eq!(
            fingerprint(&Clustering::compute(&graph)),
            fingerprint(&Clustering::compute_seeded(&graph, None))
        );
    }

    #[test]
    fn isolated_symbols_are_left_out() {
        let (mut graph, left, right) = two_triangles();
        let lonely = add(&mut graph, "lonely", 900);
        let recursive = add(&mut graph, "recursive", 901);
        call(&mut graph, recursive, recursive, 950);

        let clustering = Clustering::compute(&graph);

        assert_eq!(clustering.cluster_of(&lonely), None);
        assert_eq!(
            clustering.cluster_of(&recursive),
            None,
            "self-recursion alone connects a function to nothing"
        );
        for member in left.iter().chain(right.iter()) {
            assert!(clustering.cluster_of(member).is_some());
        }
    }

    #[test]
    fn every_level_partitions_the_connected_symbols() {
        let graph = layered_graph();
        let clustering = Clustering::compute(&graph);

        let connected: HashSet<SymbolId> = graph
            .function_ids()
            .copied()
            .filter(|id| clustering.cluster_of(id).is_some())
            .collect();
        assert!(!connected.is_empty());

        for level in 0..clustering.level_count() {
            let mut seen: HashSet<SymbolId> = HashSet::new();
            for id in clustering.level(level) {
                for member in &clustering.cluster(*id).unwrap().members {
                    assert!(seen.insert(*member), "symbol in two clusters at one level");
                }
            }
            assert_eq!(seen, connected, "level {level} does not cover every symbol");
        }
    }

    #[test]
    fn ancestry_is_a_chain_to_the_top() {
        let graph = layered_graph();
        let clustering = Clustering::compute(&graph);
        assert!(
            clustering.level_count() >= 2,
            "fixture is meant to produce a real hierarchy"
        );

        let symbol = *graph
            .function_ids()
            .find(|id| clustering.cluster_of(id).is_some())
            .unwrap();
        let chain = clustering.ancestry(&symbol);

        assert_eq!(chain.len(), clustering.level_count());
        for (level, id) in chain.iter().enumerate() {
            let cluster = clustering.cluster(*id).unwrap();
            assert_eq!(cluster.level, level);
            assert!(cluster.members.contains(&symbol));
            if level > 0 {
                assert_eq!(
                    clustering.cluster(chain[level - 1]).unwrap().parent,
                    Some(*id)
                );
                assert!(cluster.children.contains(&chain[level - 1]));
            }
        }
        assert!(clustering.top_level().contains(chain.last().unwrap()));
    }

    #[test]
    fn the_gateway_member_becomes_the_entry_point() {
        let (graph, _, right) = two_triangles();
        let clustering = Clustering::compute(&graph);

        // The right triangle is only entered through `right_a`.
        let cluster = clustering
            .cluster(clustering.cluster_of(&right[0]).unwrap())
            .unwrap();
        assert_eq!(cluster.entry_point, Some(right[0]));
        assert_eq!(cluster.label, "right_a");
    }

    /// The rule these two exercise is easier to pin down directly than through
    /// `compute`, where it would ride on whichever communities Louvain happened
    /// to form.
    #[test]
    fn a_popular_leaf_does_not_outrank_the_member_that_fronts_the_cluster() {
        let mut graph = CallGraph::new();
        let outside = add(&mut graph, "outside", 1);
        let hub = add(&mut graph, "hub", 2);
        let first = add(&mut graph, "first", 3);
        let second = add(&mut graph, "second", 4);
        let helper = add(&mut graph, "helper", 5);

        // `hub` is the way in, called once. `helper` is a shared leaf called
        // five times from outside — more popular, and the wrong label.
        call(&mut graph, outside, hub, 100);
        for line in 200..205 {
            call(&mut graph, outside, helper, line);
        }
        call(&mut graph, hub, first, 300);
        call(&mut graph, first, second, 301);
        call(&mut graph, second, helper, 302);

        let members = [hub, first, second, helper];
        assert_eq!(choose_entry_point(&graph, &members), Some(hub));
    }

    #[test]
    fn a_member_nothing_calls_can_still_name_its_cluster() {
        let mut graph = CallGraph::new();
        let outside = add(&mut graph, "outside", 1);
        let root = add(&mut graph, "root", 2);
        let middle = add(&mut graph, "middle", 3);
        let popular = add(&mut graph, "popular", 4);

        // Nothing calls `root` at all, the way nothing calls `main`. Ranking by
        // incoming calls alone would hand the cluster to `popular`.
        call(&mut graph, root, middle, 100);
        call(&mut graph, middle, popular, 101);
        for line in 200..203 {
            call(&mut graph, outside, popular, line);
        }

        let members = [root, middle, popular];
        assert_eq!(choose_entry_point(&graph, &members), Some(root));
    }

    #[test]
    fn a_cluster_with_no_external_callers_falls_back_to_reach() {
        let (graph, left, _) = two_triangles();
        let clustering = Clustering::compute(&graph);

        // Nothing calls into the left triangle, so the member reaching the most
        // others wins: `left_a` reaches both of the others.
        let cluster = clustering
            .cluster(clustering.cluster_of(&left[0]).unwrap())
            .unwrap();
        assert_eq!(cluster.entry_point, Some(left[0]));
        assert_eq!(cluster.label, "left_a");
    }

    #[test]
    fn seeding_keeps_previous_groupings_together() {
        let (graph, left, right) = two_triangles();
        let previous = Clustering::compute(&graph);

        let mut grown = graph.clone();
        let extra = add(&mut grown, "extra", 800);
        call(&mut grown, right[2], extra, 801);

        let seeded = Clustering::compute_seeded(&grown, Some(&previous));

        let left_cluster = seeded.cluster_of(&left[0]).unwrap();
        for member in left {
            assert_eq!(seeded.cluster_of(&member), Some(left_cluster));
        }
        let right_cluster = seeded.cluster_of(&right[0]).unwrap();
        for member in right {
            assert_eq!(seeded.cluster_of(&member), Some(right_cluster));
        }
        assert_ne!(left_cluster, right_cluster);
        assert!(seeded.cluster_of(&extra).is_some());
    }

    #[test]
    fn a_key_anchors_on_the_lowest_member_and_is_unique_within_its_level() {
        let graph = layered_graph();
        let clustering = Clustering::compute(&graph);

        for level in 0..clustering.level_count() {
            let mut seen: HashSet<ClusterKey> = HashSet::new();
            for id in clustering.level(level) {
                let cluster = clustering.cluster(*id).unwrap();
                let lowest = cluster
                    .members
                    .iter()
                    .min_by_key(|member| member.0)
                    .copied()
                    .expect("a populated cluster");

                assert_eq!(cluster.key.level, level);
                assert_eq!(cluster.key.anchor, lowest);
                // Uniqueness is what makes the key a name at all, and it comes
                // from the clusters at a level partitioning the symbols.
                assert!(
                    seen.insert(cluster.key),
                    "two clusters at level {level} share a key"
                );
                assert_eq!(
                    clustering.by_key(cluster.key).map(|c| c.id),
                    Some(cluster.id)
                );
            }
        }
    }

    /// The reason [`ClusterKey`] exists. A frontend holds a reference to a
    /// cluster, the graph grows underneath it, and the reference has to still
    /// mean the community the user was looking at.
    #[test]
    fn a_key_outlives_the_recompute_that_renumbers_the_ids() {
        let (graph, _, _) = two_triangles();
        let before = Clustering::compute(&graph);
        assert_eq!(before.level(0).len(), 2);

        // Hold on to the first cluster at level 0 — the one nothing is about to
        // be added to, so its membership, and therefore its anchor, is the same
        // on both sides of the recompute.
        let kept = before.cluster(before.level(0)[0]).unwrap().clone();

        // Grow the *other* cluster by a symbol that sorts below everything in
        // the graph. That cluster now holds the lowest member at level 0, so it
        // takes the first id and the ids behind it all shift along.
        let other = before.cluster(*before.level(0).last().unwrap()).unwrap();
        let attach_to = other.members[0];

        let mut grown = graph.clone();
        let (name, line) = name_sorting_below(&graph);
        let extra = add(&mut grown, &name, line);
        call(&mut grown, extra, attach_to, 5_000);

        let after = Clustering::compute_seeded(&grown, Some(&before));

        let resolved = after
            .by_key(kept.key)
            .expect("the key still names a cluster after the recompute");
        assert_eq!(
            resolved.members, kept.members,
            "the key resolves to the community the user was holding"
        );

        // And the id does not, which is the whole point: had the workspace
        // stored `kept.id`, it would now be naming the community that grew.
        assert_ne!(
            resolved.id, kept.id,
            "the fixture is meant to renumber the level; it no longer does"
        );
        assert_ne!(
            after.cluster(kept.id).map(|c| c.members.clone()),
            Some(kept.members.clone()),
            "the raw id now names a different set of functions"
        );
    }

    #[test]
    fn a_key_naming_a_level_that_does_not_exist_resolves_to_nothing() {
        let (graph, left, _) = two_triangles();
        let clustering = Clustering::compute(&graph);
        let key = clustering
            .cluster(clustering.cluster_of(&left[0]).unwrap())
            .unwrap()
            .key;

        assert!(clustering.by_key(key).is_some());
        assert!(clustering
            .by_key(ClusterKey {
                level: clustering.level_count(),
                anchor: key.anchor,
            })
            .is_none());
        assert!(
            clustering
                .by_key(ClusterKey {
                    level: 0,
                    anchor: SymbolId::from_content("nobody", "nowhere.c", 1),
                })
                .is_none(),
            "an anchor no cluster is keyed on names nothing either"
        );
    }

    /// A name and line whose `SymbolId` sorts below every id already in `graph`.
    ///
    /// A `SymbolId` is a v5 uuid of the symbol's name and location, so a name
    /// that sorts ahead of the existing ones cannot be written down by hand; it
    /// has to be searched for. It is worth searching for, because cluster ids
    /// are handed out in symbol order and a new lowest symbol therefore
    /// renumbers its whole level — which is exactly the hazard a stored
    /// `ClusterId` walks into.
    fn name_sorting_below(graph: &CallGraph) -> (String, u32) {
        let lowest = graph
            .function_ids()
            .map(|symbol| symbol.0)
            .min()
            .expect("a non-empty graph");

        (0u32..10_000)
            .map(|n| (format!("early_{n}"), 900 + n))
            .find(|(name, line)| SymbolId::from_content(name, "test.c", *line).0 < lowest)
            .expect("one of ten thousand uuids sorts below a handful of others")
    }

    #[test]
    fn an_empty_graph_yields_an_empty_clustering() {
        let graph = CallGraph::new();
        let clustering = Clustering::compute(&graph);

        assert!(clustering.is_empty());
        assert_eq!(clustering.level_count(), 0);
        assert!(clustering.top_level().is_empty());
        assert!(clustering.level(0).is_empty());
        assert_eq!(clustering.clusters().count(), 0);
        assert!(clustering
            .ancestry(&SymbolId::from_content("nobody", "nowhere.c", 1))
            .is_empty());
    }

    /// Four triangles, paired tightly and the pairs joined loosely, so Louvain
    /// has something to merge on a second pass.
    /// Four triangles in a chain, with one of them mutually recursive.
    ///
    /// The chain is what gives the fixture a hierarchy to have levels of: four
    /// groups that a coarser level can pair up. It runs one way on purpose.
    /// Wiring the triangles to call each other *back* — which this fixture used
    /// to do — makes the whole chain one strongly-connected component, and a
    /// component cannot be split across clusters without those clusters calling
    /// each other, so the clustering has no choice but to return it as one
    /// lump. The recursion inside `t2` keeps that path covered at a size that
    /// leaves the rest of the structure intact.
    fn layered_graph() -> CallGraph {
        let mut graph = CallGraph::new();
        let t1 = triangle(&mut graph, "t1", 10);
        let t2 = triangle(&mut graph, "t2", 40);
        let t3 = triangle(&mut graph, "t3", 70);
        let t4 = triangle(&mut graph, "t4", 100);

        // Triangles paired tightly, pairs joined by a single call: that is what
        // gives Louvain a second level to find — triangles below, pairs above.
        for (i, (from, to)) in [
            (t1[1], t2[0]),
            (t1[2], t2[1]),
            (t1[0], t2[2]),
            (t3[1], t4[0]),
            (t3[2], t4[1]),
            (t3[0], t4[2]),
        ]
        .into_iter()
        .enumerate()
        {
            call(&mut graph, from, to, 600 + i as u32);
        }
        call(&mut graph, t2[2], t3[0], 800);
        // t2 calls itself in a circle: a -> b -> c -> a.
        call(&mut graph, t2[2], t2[0], 900);

        graph
    }

    // ---- Convexity ----

    /// A chain whose two ends are called together often enough that modularity
    /// wants them in one community — with four functions in between that it
    /// does not want. Grouping the ends and not the middle is exactly the
    /// straddle that puts a mutual arrow on the map.
    fn straddling_graph() -> CallGraph {
        let mut graph = CallGraph::new();
        let chain: Vec<SymbolId> = (0..6)
            .map(|i| add(&mut graph, &format!("step{i}"), 10 + i))
            .collect();
        for (i, pair) in chain.windows(2).enumerate() {
            call(&mut graph, pair[0], pair[1], 100 + i as u32);
        }
        // The ends, tied together by repeated calls.
        for line in 0..5 {
            call(&mut graph, chain[0], chain[5], 200 + line);
        }
        graph
    }

    /// Which cluster each clustered symbol sits in at `level`.
    fn owners_at(clustering: &Clustering, level: usize) -> HashMap<SymbolId, ClusterId> {
        clustering
            .level(level)
            .iter()
            .flat_map(|id| {
                let cluster = clustering.cluster(*id).expect("listed at its level");
                cluster.members.iter().map(move |m| (*m, cluster.id))
            })
            .collect()
    }

    /// Whether the clusters at `level` call each other in a circle.
    fn level_has_a_cycle(graph: &CallGraph, clustering: &Clustering, level: usize) -> bool {
        let owner = owners_at(clustering, level);
        let mut edges: HashMap<ClusterId, HashSet<ClusterId>> = HashMap::new();
        for caller in graph.function_ids() {
            let Some(&from) = owner.get(caller) else {
                continue;
            };
            for callee in graph.get_callees(caller) {
                if let Some(&to) = owner.get(&callee.id) {
                    if from != to {
                        edges.entry(from).or_default().insert(to);
                    }
                }
            }
        }

        // Depth-first, colouring grey on the way down: an edge back into the
        // grey set is a cycle.
        let mut black: HashSet<ClusterId> = HashSet::new();
        for start in clustering.level(level) {
            let mut grey: HashSet<ClusterId> = HashSet::new();
            let mut stack = vec![(*start, false)];
            while let Some((node, leaving)) = stack.pop() {
                if leaving {
                    grey.remove(&node);
                    black.insert(node);
                    continue;
                }
                if black.contains(&node) {
                    continue;
                }
                if !grey.insert(node) {
                    return true;
                }
                stack.push((node, true));
                for next in edges.get(&node).into_iter().flatten() {
                    if grey.contains(next) {
                        return true;
                    }
                    stack.push((*next, false));
                }
            }
        }
        false
    }

    /// The guarantee the whole convexity pass exists to provide: whatever level
    /// the map is drawn at, the arrows between boxes run one way. Two boxes
    /// pointing at each other says "these call each other" over code where, in a
    /// call graph that is a DAG, nothing of the sort happens.
    #[test]
    fn no_level_draws_a_cycle_between_clusters() {
        for (name, graph) in [
            ("a straddling chain", straddling_graph()),
            ("layered triangles", layered_graph()),
            ("two triangles", two_triangles().0),
        ] {
            let clustering = Clustering::compute(&graph);
            assert!(!clustering.is_empty(), "{name}: nothing clustered");
            for level in 0..clustering.level_count() {
                assert!(
                    !level_has_a_cycle(&graph, &clustering, level),
                    "{name}: the clusters at level {level} call each other in a circle"
                );
            }
        }
    }

    /// A scoped map is drawn from its own clustering and needs the same of it.
    #[test]
    fn a_scoped_clustering_draws_as_flow_too() {
        let graph = straddling_graph();
        let scope: HashSet<SymbolId> = graph.function_ids().copied().collect();
        let clustering = Clustering::compute_within(&graph, &scope, None);

        for level in 0..clustering.level_count() {
            assert!(!level_has_a_cycle(&graph, &clustering, level));
        }
    }

    /// Functions that call each other in a circle cannot be told apart by a map
    /// that runs one way, so they have to share a cluster — splitting them is
    /// precisely what would make two clusters call each other.
    #[test]
    fn mutually_recursive_functions_share_a_cluster() {
        let mut graph = CallGraph::new();
        let ring: Vec<SymbolId> = (0..4)
            .map(|i| add(&mut graph, &format!("ring{i}"), 10 + i))
            .collect();
        for (i, pair) in ring.windows(2).enumerate() {
            call(&mut graph, pair[0], pair[1], 100 + i as u32);
        }
        call(&mut graph, ring[3], ring[0], 150);
        // Something else for the clustering to prefer, so that keeping the ring
        // together is not simply the only option on the table.
        let other = triangle(&mut graph, "other", 200);
        call(&mut graph, ring[2], other[0], 300);

        let clustering = Clustering::compute(&graph);
        let home = clustering
            .cluster_of(&ring[0])
            .expect("a function in a cycle is not isolated");
        for member in &ring {
            assert_eq!(
                clustering.cluster_of(member),
                Some(home),
                "the ring was split across clusters"
            );
        }
    }

    /// The straddle itself: the repair has to move something, and what it moves
    /// must stay a partition of the same functions.
    #[test]
    fn repairing_a_straddle_still_covers_every_connected_symbol() {
        let graph = straddling_graph();
        let clustering = Clustering::compute(&graph);

        let connected: HashSet<SymbolId> = graph
            .function_ids()
            .filter(|id| !graph.get_callers(id).is_empty() || !graph.get_callees(id).is_empty())
            .copied()
            .collect();
        let covered: HashSet<SymbolId> = owners_at(&clustering, 0).into_keys().collect();
        assert_eq!(covered, connected);
    }

    // ---- Clustering one corner of the graph ----

    #[test]
    fn a_scoped_clustering_holds_nothing_from_outside_the_scope() {
        let (graph, left, right) = two_triangles();
        let scope: HashSet<SymbolId> = left.iter().copied().collect();

        let clustering = Clustering::compute_within(&graph, &scope, None);

        assert!(!clustering.is_empty(), "the left triangle is connected");
        for cluster in clustering.clusters() {
            for member in &cluster.members {
                assert!(
                    scope.contains(member),
                    "{member:?} is outside the scope and must not be clustered"
                );
            }
        }
        for symbol in right {
            assert!(
                clustering.cluster_of(&symbol).is_none(),
                "a symbol the scope leaves out has no cluster to be in"
            );
        }
    }

    #[test]
    fn a_scope_can_split_what_the_whole_graph_kept_together() {
        // Two tight triangles with a single call between them: over the whole
        // graph Louvain has bigger seams to cut and leaves the pair alone at the
        // top level, but scoped to just these six it is *the* seam.
        let (graph, left, right) = two_triangles();
        let scope: HashSet<SymbolId> = left.iter().chain(right.iter()).copied().collect();

        let scoped = Clustering::compute_within(&graph, &scope, None);

        let left_cluster = scoped.cluster_of(&left[0]).expect("clustered");
        let right_cluster = scoped.cluster_of(&right[0]).expect("clustered");
        assert_ne!(
            left_cluster, right_cluster,
            "the one call joining them is the weakest edge in the scope"
        );
    }

    #[test]
    fn a_function_whose_only_calls_leave_the_scope_is_isolated_within_it() {
        let (graph, left, right) = two_triangles();
        // Both of these have plenty of calls in the full graph; none of them
        // has its other end in this scope.
        let scope: HashSet<SymbolId> = [left[0], right[0]].into_iter().collect();

        let clustering = Clustering::compute_within(&graph, &scope, None);

        assert!(
            clustering.is_empty(),
            "a call whose other end is out of scope is no edge at all"
        );
    }

    #[test]
    fn scoping_to_everything_is_clustering_everything() {
        let graph = layered_graph();
        let scope: HashSet<SymbolId> = graph.function_ids().copied().collect();

        assert_eq!(
            fingerprint(&Clustering::compute_within(&graph, &scope, None)),
            fingerprint(&Clustering::compute(&graph)),
            "the scope is a restriction, and restricting to all of it is a no-op"
        );
    }

    #[test]
    fn an_empty_scope_clusters_nothing() {
        let (graph, _, _) = two_triangles();
        assert!(Clustering::compute_within(&graph, &HashSet::new(), None).is_empty());
    }
}
