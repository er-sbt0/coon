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
use std::collections::{HashMap, HashSet, VecDeque};

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
        let (symbols, base) = build_base_graph(graph);
        if symbols.is_empty() {
            return Self::default();
        }

        let seed = previous.map(|prev| seed_partition(&symbols, prev));
        let dendrogram = build_dendrogram(base, seed);
        if dendrogram.is_empty() {
            return Self::default();
        }

        build_clustering(graph, &symbols, &dendrogram)
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
fn build_base_graph(graph: &CallGraph) -> (Vec<SymbolId>, WeightedGraph) {
    let mut all: Vec<SymbolId> = graph.function_ids().copied().collect();
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
    let mut mapping: HashMap<ClusterId, usize> = HashMap::new();
    let mut next = 0usize;
    let mut communities = Vec::with_capacity(symbols.len());

    for symbol in symbols {
        let community = match previous.cluster_of(symbol) {
            Some(old) => *mapping.entry(old).or_insert_with(|| {
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

fn modularity(graph: &WeightedGraph, communities: &[usize], count: usize) -> f64 {
    let m2 = graph.total_degree;
    if m2 <= 0.0 {
        return 0.0;
    }

    let mut internal = vec![0.0f64; count];
    let mut total = vec![0.0f64; count];
    for (node, &community) in communities.iter().enumerate() {
        total[community] += graph.degrees[node];
        internal[community] += 2.0 * graph.self_loops[node];
        for &(neighbour, weight) in &graph.adj[node] {
            if communities[neighbour] == community {
                // Each undirected edge is walked from both ends, which is exactly
                // the doubling the formula wants.
                internal[community] += weight;
            }
        }
    }

    internal
        .iter()
        .zip(&total)
        .map(|(inside, degree)| inside / m2 - (degree / m2) * (degree / m2))
        .sum()
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

/// Run Louvain to exhaustion, returning one partition per level.
///
/// `levels[0]` maps base nodes to level-0 communities; `levels[l]` maps level
/// `l - 1` communities to level `l` communities.
fn build_dendrogram(base: WeightedGraph, seed: Option<Vec<usize>>) -> Vec<Vec<usize>> {
    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut current = base;
    let mut initial = seed.unwrap_or_else(|| (0..current.node_count()).collect());

    let mut best_modularity = f64::NEG_INFINITY;
    loop {
        let (communities, count) = renumber(&local_moving(&current, initial));
        let quality = modularity(&current, &communities, count);

        // Level 0 is recorded whatever it looks like: the contract is that every
        // connected symbol has a finest cluster, even on a graph Louvain cannot
        // improve. Coarser levels have to earn their place.
        if !levels.is_empty() && quality <= best_modularity + MODULARITY_EPSILON {
            break;
        }
        levels.push(communities.clone());
        best_modularity = quality;

        if count == current.node_count() {
            // Nothing merged, so aggregating would reproduce the same graph.
            break;
        }
        current = aggregate(&current, &communities, count);
        initial = (0..count).collect();
    }

    levels
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
    fn layered_graph() -> CallGraph {
        let mut graph = CallGraph::new();
        let t1 = triangle(&mut graph, "t1", 10);
        let t2 = triangle(&mut graph, "t2", 40);
        let t3 = triangle(&mut graph, "t3", 70);
        let t4 = triangle(&mut graph, "t4", 100);

        for (i, (from, to)) in [(t1[1], t2[0]), (t1[2], t2[1]), (t2[2], t1[0])]
            .into_iter()
            .enumerate()
        {
            call(&mut graph, from, to, 600 + i as u32);
        }
        for (i, (from, to)) in [(t3[1], t4[0]), (t3[2], t4[1]), (t4[2], t3[0])]
            .into_iter()
            .enumerate()
        {
            call(&mut graph, from, to, 700 + i as u32);
        }
        call(&mut graph, t2[2], t3[0], 800);

        graph
    }
}
