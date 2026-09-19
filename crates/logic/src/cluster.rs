//! Hierarchical clustering of the call graph by modularity.
//!
//! A whole call graph is unreadable at once — the point of clustering is to give
//! the frontends a coarser thing to draw first, and a way to zoom in. The
//! objective is *cohesion*: a box should hold functions that call each other
//! more than chance would predict given how busy each of them is. That is
//! modularity (Newman and Girvan, 2004), and the optimiser is the Louvain
//! method (Blondel et al., 2008): sweep the nodes, move each into whichever
//! neighbouring community gains the most, repeat until nothing moves, then
//! collapse every community into one node and go again. Each collapse is one
//! rung of the zoom ladder.
//!
//! Why cohesion and not the heaviest edge. This module was heavy-edge matching
//! before — the coarsener METIS shrinks a graph with — and on a real project it
//! grouped by what merges cheaply, which is not what belongs together: 328 of
//! the 370 calls weighed one, so "heaviest" was a tie decided by symbol id;
//! leaves were served first and took the one utility they called, and
//! `log_write`, `mem_free` and `strcmp` then grew a box of every caller they
//! had, cutting straight across the source tree. Of the 33 boxes the map
//! opened on, 26 spanned more than one file, and 13 pairs of boxes called each
//! other both ways where the by-file grouping of the same code has one such
//! pair. Modularity answers both faults at once. A call to a utility that
//! everything calls is *expected* under the null model, so it does not pay for
//! a merge, and a sweep can move a function out of a box it was put in wrongly,
//! where a pair once matched was matched for good.
//!
//! The rungs are a *resolution* ladder. Modularity takes a resolution γ
//! (Reichardt and Bornholdt, 2006) that scales the null model: above one only
//! tightly knit groups pay for themselves, at one it is plain modularity, and
//! below one merging goes on past it. [`coarsen`] runs one pass per entry of
//! [`RESOLUTION_LADDER`], finest first, keeping a rung for every pass that
//! changed something; then passes at one until the optimiser is done; and only
//! then, only while the top is still wider than [`TARGET_TOP_CLUSTERS`], halves
//! γ below one. So every rung is a cohesive grouping at some grain, the last
//! rung at γ = 1 is where the map opens, and the top is a summary a person can
//! take in at a glance, at most a few zoom steps further out.
//!
//! What it does not claim: the resolution limit is real — plain modularity
//! cannot see a module smaller than about √(2m) calls' worth inside a much
//! larger program. Those show at the finer rungs, which is what they are for.
//!
//! It is fully deterministic: nodes are indexed in symbol-id order and swept in
//! that order, candidate communities are tried in ascending id, and a move needs
//! a strictly better gain, so a tie keeps a node where it is. Two runs over the
//! same graph produce byte-identical output, and no seeding or history is
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
//! **The quotient of every rung is acyclic.** Modularity alone puts no
//! constraint on direction: a community can sit on both ends of a one-way
//! path, and left to itself the optimiser drew `A -> B` beside `B -> A` over
//! code where nothing calls back — on a real project 19 of the 55 lines on the
//! top rung ran both ways, over a call graph with no cycle in it. So a move is
//! legal only if the community it joins sits on no directed cycle of the
//! quotient afterwards ([`joins_a_cycle`]); a candidate that would close one
//! is skipped and the next best is scored. Every pass starts from a DAG and
//! hands one on, and the singleton fold is held to the same rule. The base
//! graph is made a DAG first by condensing its strongly connected components:
//! mutually recursive functions are one unit from the finest rung up, which
//! is where they belong anyway. On the project above the constraint cost 0.04
//! of modularity and four boxes at the opening rung.
//!
//! The implementation works in index space — every hot loop indexes `Vec`s
//! rather than hashing `SymbolId`s — so it stays usable on the ~10k-node graphs
//! a mid-sized C++ project produces.

use model::{CallGraph, SymbolId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Stop coarsening once the coarsest level is this wide or narrower.
///
/// Plain modularity decides how many boxes a program *has*; this decides how
/// few a person can take in at a glance. When the modularity optimum is wider
/// than this, the ladder keeps merging at ever lower resolution until the top
/// fits, and each of those rungs is one more zoom step out from where the map
/// opens. When it is narrower, the constant does nothing: a program with seven
/// modules gets seven boxes, not twelve.
///
/// A pass merges whatever pays at its resolution, so the top lands at or under
/// the target rather than on it. Twelve puts the top at roughly six to twelve
/// boxes, which is a map a person reads rather than counts.
pub const TARGET_TOP_CLUSTERS: usize = 12;

/// The resolutions the ladder runs down to plain modularity, finest first.
///
/// γ scales the null model: a function joins a box when the calls between them
/// beat γ times what its busyness alone would predict. At 8 only a function and
/// the helpers it all but owns pay for the merge, and the rung is boxes of two
/// and three; each halving lets the next larger grouping through; 1 is plain
/// modularity, the rung the map opens on. Halving rather than a finer step
/// because the gain compares a ratio, and a rung a reader can tell from the one
/// below it needs that ratio to move by a factor, not a fraction. A pass that
/// changes nothing adds no rung, so a small program simply has fewer.
pub const RESOLUTION_LADDER: &[f64] = &[8.0, 4.0, 2.0, 1.0];

/// How far below plain modularity the ladder will go to reach
/// [`TARGET_TOP_CLUSTERS`].
///
/// A floor rather than a count of halvings, because a graph of disconnected
/// components has nothing any resolution will merge, and the ladder has to stop
/// wide rather than spin.
const MIN_RESOLUTION: f64 = 1.0 / 64.0;

/// A pass that merges nothing ends its phase on its own, so this only bounds
/// the pathological case. Each productive pass removes at least one node, and
/// a graph large enough to need 64 rounds of merging does not exist.
const MAX_ROUNDS: usize = 64;

/// Sweeps within one pass. Every sweep that moves a node raises modularity,
/// which is bounded, so the loop terminates on its own; this guards the
/// floating-point corner where a gain hovers at the tolerance.
const MAX_SWEEPS: usize = 64;

/// A move has to beat staying put by this much. Gains are differences of call
/// counts scaled by degrees over the total, so a real improvement is orders of
/// magnitude above it and a rounding artefact orders below.
const GAIN_EPSILON: f64 = 1e-9;

/// Above this many members, picking a gateway by within-cluster reachability
/// costs a BFS per member. Fall back to out-degree, which is one pass.
const REACHABILITY_MEMBER_LIMIT: usize = 200;

/// Which algorithm builds the coarsening ladder.
///
/// One entry today, and named anyway. A clustering is a claim about a program's
/// shape, and every method makes a different one — a community detector answers
/// "what belongs together", where a coarsener answers "what merges cheaply".
/// Which claim is on screen is the user's to pick, so it is a value that
/// travels with the workspace and the snapshot rather than a constant compiled
/// into one call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ClusterMethod {
    /// Modularity optimised by the Louvain method, described at the top of
    /// this module.
    #[default]
    Modularity,
}

impl ClusterMethod {
    /// Every method, in the order a picker should list them.
    pub const ALL: &'static [ClusterMethod] = &[ClusterMethod::Modularity];

    /// What to write in the picker.
    pub fn label(self) -> &'static str {
        match self {
            ClusterMethod::Modularity => "Modularity",
        }
    }

    /// One line on what the method looks for, for the picker's second row.
    pub fn description(self) -> &'static str {
        match self {
            ClusterMethod::Modularity => {
                "Groups functions that call each other more than chance would"
            }
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
    /// The rung a map should open on — the last one built at plain
    /// modularity. Not necessarily the top, and never past it.
    opening: usize,
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
    /// be. What a merge is worth is judged against the null model of the graph
    /// being clustered — how busy each function is out of *all* the calls
    /// present — and by how many passes it takes to reach the target width,
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
        let base = build_base_graph(graph, scope);
        if base.symbols.is_empty() {
            return Self {
                method,
                ..Self::default()
            };
        }

        let mut ladder = match method {
            ClusterMethod::Modularity => coarsen(base.graph, base.sizes),
        };
        // The ladder's first rung maps condensed nodes; `build_clustering`
        // reads a map from symbols, so the condensation is composed in.
        let first: Vec<usize> = base
            .units
            .iter()
            .map(|&unit| ladder.levels[0][unit])
            .collect();
        ladder.levels[0] = first;
        let opening = ladder.opening();
        let mut clustering = build_clustering(graph, &base.symbols, &ladder.levels);
        clustering.opening = opening;
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

    /// The rung a map should open on.
    ///
    /// The last rung built at plain modularity, γ = 1: the grouping the
    /// objective itself stands behind. Coarser rungs exist only when that one
    /// was wider than [`TARGET_TOP_CLUSTERS`], and they are where a user zooms
    /// out *to*, not where they should land. See [`Ladder::opening`].
    pub fn opening_level(&self) -> usize {
        self.opening.min(self.levels.len().saturating_sub(1))
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
    /// Successors of each node, sorted and deduplicated: the direction `adj`
    /// dropped, kept so a move can be tested for closing a cycle in the
    /// quotient. Weights are not carried — the test is reachability, and the
    /// objective reads `adj`.
    out: Vec<Vec<usize>>,
}

impl WeightedGraph {
    fn new(
        adj_maps: Vec<HashMap<usize, f64>>,
        self_loops: Vec<f64>,
        mut out: Vec<Vec<usize>>,
    ) -> Self {
        let mut adj: Vec<Vec<(usize, f64)>> = adj_maps
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();
        // The singleton fold breaks weight ties on the lower partner index,
        // which it gets for free by scanning in index order — so this sort is
        // not cosmetic, it *is* the tie-break.
        for neighbours in &mut adj {
            neighbours.sort_unstable_by_key(|(index, _)| *index);
        }
        for successors in &mut out {
            successors.sort_unstable();
            successors.dedup();
        }

        Self {
            adj,
            self_loops,
            out,
        }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }
}

/// The graph the coarsening runs on: the connected symbols, condensed so that
/// it is a DAG.
struct Base {
    /// Every connected symbol, in id order — the meaning of every index.
    symbols: Vec<SymbolId>,
    /// The condensed node each symbol is, indexed like `symbols`.
    units: Vec<usize>,
    /// How many symbols each node of `graph` holds.
    sizes: Vec<usize>,
    graph: WeightedGraph,
}

/// Build the weighted graph the coarsening runs on, plus the symbol list that
/// gives every index its meaning.
///
/// `CallGraph::callee_ids` returns one entry per distinct *call site*, not per
/// distinct callee — the graph deduplicates on (caller, callee, location). So
/// the duplicates in that slice are exactly the number of times the caller calls
/// the callee, and counting them is how we get an edge weight out of an API
/// that stores none: `CallGraph::edges` exists but reports the same call sites
/// one at a time and aggregates nothing either, and walking per symbol is what
/// makes `scope` below cost the scope's calls rather than the project's. Those
/// counts are the whole input to the objective: two functions that call each
/// other twenty times are twenty times the reason to put them in one box.
///
/// The objective reads the calls undirected — a call is a reason to put two
/// functions in one box whichever way it runs — and the direction is kept
/// beside them in `out`, for the cycle test alone. The graph is then condensed:
/// every strongly connected component becomes one node, so what the coarsening
/// starts from is a DAG and [`joins_a_cycle`] has an invariant to keep rather
/// than one to hope for. Self-loops are dropped before that, so plain
/// recursion is not a component of its own.
///
/// `scope`, when given, is the set of functions to consider; everything else is
/// left out, and so are the calls that would have reached it. Restricting the
/// symbol list is enough to restrict the edges too, because a call whose other
/// end has no index is skipped exactly as a call to an unknown symbol is.
fn build_base_graph(graph: &CallGraph, scope: Option<&HashSet<SymbolId>>) -> Base {
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
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); all.len()];
    for (i, symbol) in all.iter().enumerate() {
        for callee in graph.callee_ids(symbol) {
            let Some(&j) = position.get(callee) else {
                continue;
            };
            // A function cannot be merged with itself, so recursion is weight
            // with nowhere to go.
            if i == j {
                continue;
            }
            *weights[i].entry(j).or_insert(0.0) += 1.0;
            *weights[j].entry(i).or_insert(0.0) += 1.0;
            out[i].push(j);
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
    let out: Vec<Vec<usize>> = kept
        .iter()
        .map(|&old| out[old].iter().map(|&j| compacted[j]).collect())
        .collect();
    let self_loops = vec![0.0; symbols.len()];
    let graph = WeightedGraph::new(adj_maps, self_loops, out);

    let (units, count) = condense(&graph.out);
    let sizes = merged_sizes(&vec![1; symbols.len()], &units, count);
    let graph = if count == symbols.len() {
        graph
    } else {
        aggregate(&graph, &units, count)
    };

    Base {
        symbols,
        units,
        sizes,
        graph,
    }
}

/// The strongly connected components of a graph given as successor lists,
/// numbered in order of lowest member so that the units come out in the same
/// order as the nodes. Returns the component of each node and the count.
///
/// Tarjan's algorithm with an explicit stack: a call chain a few thousand
/// functions deep is an ordinary thing in a real project, and a recursive walk
/// over it would overflow the thread's stack.
fn condense(out: &[Vec<usize>]) -> (Vec<usize>, usize) {
    let n = out.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut count = 0usize;
    let mut next = 0usize;
    // The call stack: a node and how many of its successors it has visited.
    let mut work: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        index[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;
        work.push((start, 0));

        while let Some(&mut (node, ref mut position)) = work.last_mut() {
            if *position < out[node].len() {
                let successor = out[node][*position];
                *position += 1;
                if index[successor] == usize::MAX {
                    index[successor] = next;
                    low[successor] = next;
                    next += 1;
                    stack.push(successor);
                    on_stack[successor] = true;
                    work.push((successor, 0));
                } else if on_stack[successor] {
                    low[node] = low[node].min(index[successor]);
                }
                continue;
            }

            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                loop {
                    let member = stack
                        .pop()
                        .expect("the root of a component is on the stack");
                    on_stack[member] = false;
                    component[member] = count;
                    if member == node {
                        break;
                    }
                }
                count += 1;
            }
        }
    }

    renumber(&component)
}

// ---------------------------------------------------------------------------
// Modularity
// ---------------------------------------------------------------------------

/// Run passes of the Louvain method down the resolution ladder until the top
/// is at most [`TARGET_TOP_CLUSTERS`] wide.
///
/// The return is the ladder in the shape [`build_clustering`] reads: entry 0
/// maps a base node to its level-0 cluster, and entry `l` maps a level-`l - 1`
/// cluster to its level-`l` one. So the number of entries is the number of
/// productive passes, and every entry is one rung of zoom.
///
/// Three phases, each a resolution regime:
///
/// 1. One pass per entry of [`RESOLUTION_LADDER`], finest first. A pass that
///    merges nothing adds no rung.
/// 2. Passes at γ = 1 until one merges nothing — the standard Louvain run to
///    its optimum. The last rung this leaves is the opening rung.
/// 3. Only while the top is still wider than the target: halve γ and pass
///    again, down to [`MIN_RESOLUTION`].
///
/// A graph already at or below the target still gets a rung, made of
/// singletons. A map has to have something to draw, and "every function is its
/// own box" is the honest answer for a graph that small — grouping it further
/// would merge a program small enough to read whole.
fn coarsen(base: WeightedGraph, sizes: Vec<usize>) -> Ladder {
    let mut run = Coarsening {
        levels: Vec::new(),
        sizes,
        current: base,
        opening: 0,
    };

    if run.current.node_count() > TARGET_TOP_CLUSTERS {
        for &resolution in RESOLUTION_LADDER {
            run.pass(resolution);
        }
        while run.pass(1.0) {}
        let mut resolution = 0.5;
        while run.current.node_count() > TARGET_TOP_CLUSTERS && resolution >= MIN_RESOLUTION {
            run.pass(resolution);
            resolution /= 2.0;
        }
    }

    if run.levels.is_empty() {
        // A graph small enough to read whole, or one nothing merges. Every
        // function is its own box, and there is nothing to fold: they are
        // *all* singletons, so the pass below would merge the program into one.
        return Ladder {
            levels: vec![(0..run.current.node_count()).collect()],
            opening: 0,
        };
    }

    if let Some((fold, count)) = fold_singletons(&run.current, &run.sizes) {
        // Composed into the last round rather than added as a rung of its own:
        // it is a repair to the top of the ladder, not a step of the zoom, and
        // a rung that differs from the one below it by one box is a rung the
        // user would step through for nothing.
        let last = run.levels.last_mut().expect("the ladder is not empty here");
        for group in last.iter_mut() {
            *group = fold[*group];
        }
        run.current = aggregate(&run.current, &fold, count);
    }

    Ladder {
        levels: run.levels,
        opening: run.opening,
    }
}

/// The ladder as it is being built: the quotient so far and what each of its
/// nodes stands for.
struct Coarsening {
    levels: Vec<Vec<usize>>,
    /// How many base functions each node of `current` holds.
    sizes: Vec<usize>,
    current: WeightedGraph,
    /// The last rung built at γ ≥ 1.
    opening: usize,
}

impl Coarsening {
    /// One pass at `resolution`, kept as a rung if it merged anything.
    /// Returns whether it did.
    fn pass(&mut self, resolution: f64) -> bool {
        if self.levels.len() >= MAX_ROUNDS {
            return false;
        }
        let (groups, count) = louvain_pass(&self.current, resolution);
        if count == self.current.node_count() {
            return false;
        }
        self.sizes = merged_sizes(&self.sizes, &groups, count);
        self.current = aggregate(&self.current, &groups, count);
        self.levels.push(groups);
        if resolution >= 1.0 {
            self.opening = self.levels.len() - 1;
        }
        true
    }
}

/// The coarsening, plus where it should open.
struct Ladder {
    /// Entry 0 maps a base node to its level-0 cluster, and entry `l` maps a
    /// level-`l - 1` cluster to its level-`l` one.
    levels: Vec<Vec<usize>>,
    /// The last rung built at plain modularity.
    opening: usize,
}

impl Ladder {
    /// The rung a map should open on.
    ///
    /// How far the ladder goes and where it opens are separate questions. The
    /// top is bounded by [`TARGET_TOP_CLUSTERS`] so a user has somewhere to zoom
    /// out to; the opening rung is the last one plain modularity built, because
    /// that is the grouping the objective stands behind — the rungs above it
    /// merge modules that did not want to merge, to fit a count, and the rungs
    /// below it are the same modules cut finer, for looking inside.
    fn opening(&self) -> usize {
        self.opening.min(self.levels.len().saturating_sub(1))
    }
}

/// One pass of the Louvain method at `resolution`: start every node in a
/// community of its own, sweep the nodes in index order moving each to the
/// neighbouring community that gains the most modularity, and repeat until a
/// sweep moves nothing.
///
/// Returns the assignment of nodes to communities, renumbered so they are
/// `0..count` in order of lowest member.
///
/// The gain of moving `i` into community `C`, with `i` taken out of wherever it
/// was, is `k_iC - γ · k_i · Σ_C / 2m` up to a constant factor: the calls
/// between `i` and `C`, less what the null model expects given `i`'s degree
/// and `C`'s total degree. Staying is a candidate like any other, scored with
/// `i` removed, so a node in a community it no longer fits leaves it. A move
/// needs a strictly better gain than staying, and candidates are tried in
/// ascending community id, so ties are deterministic and lean towards leaving
/// things be.
///
/// A candidate that would put its community on a directed cycle of the
/// quotient is skipped, however much it gains — see [`joins_a_cycle`]. The
/// graph handed in is a DAG, by construction at the base and by this rule at
/// every rung above it, and every community starts as one node, so the
/// quotient is acyclic before the first move and stays so after each.
fn louvain_pass(graph: &WeightedGraph, resolution: f64) -> (Vec<usize>, usize) {
    let n = graph.node_count();

    // A self-loop counts twice towards degree, as in the modularity definition,
    // which is what keeps the degrees of a quotient summing to the base's.
    let degree: Vec<f64> = (0..n)
        .map(|node| {
            graph.adj[node].iter().map(|(_, w)| w).sum::<f64>() + 2.0 * graph.self_loops[node]
        })
        .collect();
    let two_m: f64 = degree.iter().sum();
    if two_m <= 0.0 {
        return renumber(&(0..n).collect::<Vec<_>>());
    }

    let mut community: Vec<usize> = (0..n).collect();
    let mut community_degree: Vec<f64> = degree.clone();
    let mut members: Vec<Vec<usize>> = (0..n).map(|node| vec![node]).collect();
    let mut scratch = CycleScratch::new(n);
    // Calls from the node being placed into each neighbouring community.
    // A `BTreeMap` so the candidates come out in ascending id — the tie-break.
    let mut links: BTreeMap<usize, f64> = BTreeMap::new();

    for _ in 0..MAX_SWEEPS {
        let mut moved = false;
        for node in 0..n {
            links.clear();
            for &(neighbour, weight) in &graph.adj[node] {
                *links.entry(community[neighbour]).or_insert(0.0) += weight;
            }

            let own = community[node];
            community_degree[own] -= degree[node];

            let gain = |candidate: usize, into: f64, community_degree: &[f64]| {
                into - resolution * degree[node] * community_degree[candidate] / two_m
            };
            let mut best = own;
            let mut best_gain = gain(
                own,
                links.get(&own).copied().unwrap_or(0.0),
                &community_degree,
            );
            for (&candidate, &into) in &links {
                if candidate == own {
                    continue;
                }
                let candidate_gain = gain(candidate, into, &community_degree);
                if candidate_gain <= best_gain + GAIN_EPSILON {
                    continue;
                }
                // Better — but only legal if the quotient stays a DAG.
                let after = |other: usize| {
                    if other == node {
                        candidate
                    } else {
                        community[other]
                    }
                };
                if joins_a_cycle(graph, after, &members, &[node], candidate, &mut scratch) {
                    continue;
                }
                best = candidate;
                best_gain = candidate_gain;
            }

            community_degree[best] += degree[node];
            if best != own {
                community[node] = best;
                let at = members[own]
                    .iter()
                    .position(|&member| member == node)
                    .expect("a node is listed in its own community");
                members[own].swap_remove(at);
                members[best].push(node);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    renumber(&community)
}
/// Scratch for [`joins_a_cycle`], reused across the checks of one pass so a
/// check allocates nothing.
struct CycleScratch {
    /// Stamped rather than cleared: a group is visited when its entry carries
    /// the current stamp.
    seen: Vec<u32>,
    stamp: u32,
    queue: Vec<usize>,
}

impl CycleScratch {
    fn new(groups: usize) -> Self {
        Self {
            seen: vec![0; groups],
            stamp: 0,
            queue: Vec::new(),
        }
    }
}

/// Whether the quotient would have a directed cycle through `target` once the
/// nodes in `joining` count as its members.
///
/// `group_of` is the assignment *after* the move — it has to answer `target`
/// for every node in `joining` — and `members` is the assignment before it,
/// which is why a listed member is walked only when `group_of` still agrees it
/// belongs. The quotient before the move is acyclic, and a move adds only
/// edges that touch `target`, so any new cycle runs through `target`: the
/// search starts there and asks whether it gets back.
///
/// A breadth-first walk over the quotient, so it costs the part of the
/// program downstream of `target`, per candidate move. On a graph the size of
/// a mid-sized C++ project that is the cost to watch; the standard remedy —
/// keeping a topological order of the quotient, so a move that adds only
/// forward edges needs no search at all — is worth reaching for if it shows.
fn joins_a_cycle(
    graph: &WeightedGraph,
    group_of: impl Fn(usize) -> usize,
    members: &[Vec<usize>],
    joining: &[usize],
    target: usize,
    scratch: &mut CycleScratch,
) -> bool {
    let CycleScratch { seen, stamp, queue } = scratch;
    *stamp = stamp.wrapping_add(1);
    if *stamp == 0 {
        seen.fill(0);
        *stamp = 1;
    }
    let stamp = *stamp;

    queue.clear();
    queue.push(target);
    seen[target] = stamp;

    while let Some(group) = queue.pop() {
        let extra: &[usize] = if group == target { joining } else { &[] };
        for &node in members[group].iter().chain(extra) {
            if group_of(node) != group {
                continue;
            }
            for &successor in &graph.out[node] {
                let other = group_of(successor);
                if other == group {
                    continue;
                }
                if other == target {
                    return true;
                }
                if seen[other] != stamp {
                    seen[other] = stamp;
                    queue.push(other);
                }
            }
        }
    }
    false
}

/// How many base functions each merged group holds.
fn merged_sizes(sizes: &[usize], groups: &[usize], count: usize) -> Vec<usize> {
    let mut merged = vec![0usize; count];
    for (node, &group) in groups.iter().enumerate() {
        merged[group] += sizes[node];
    }
    merged
}

/// Fold every one-function cluster at the top of the ladder into the neighbour
/// it shares the heaviest edge with.
///
/// Returns `None` when there is nothing to fold.
///
/// A function no box gains by taking in comes through as a box of its own, and
/// at the top of the ladder that is a box holding one function beside boxes
/// holding thirty. On a real project that was `fputc`, alone, next to clusters
/// of 22 and 29 — a tenth of the map spent on a libc call, and a slot that says
/// nothing a label on an edge could not.
///
/// It is rare under modularity — a leaf's one call is usually worth more than
/// the null model expects of it — but a function whose few calls all go to a
/// very busy box can come out that way, and it is only at the last rung that
/// it has nowhere further to go. So this is the one place the objective is
/// overruled, and it is overruled by the heaviest edge. The cycle rule is not
/// overruled with it: a neighbour the fold would close a quotient cycle by
/// joining is passed over for the next heaviest, and a singleton with no legal
/// neighbour stays alone.
fn fold_singletons(graph: &WeightedGraph, sizes: &[usize]) -> Option<(Vec<usize>, usize)> {
    let n = graph.node_count();
    let mut group: Vec<usize> = (0..n).collect();
    let mut members: Vec<Vec<usize>> = (0..n).map(|node| vec![node]).collect();
    let mut scratch = CycleScratch::new(n);

    let mut folded = false;
    for (node, &size) in sizes.iter().enumerate() {
        if size != 1 {
            continue;
        }
        // Heaviest first. `adj` is sorted by index and the sort is stable, so
        // among equally heavy candidates the lowest index comes first.
        let mut candidates: Vec<(usize, f64)> = graph.adj[node].clone();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
        // A cluster with no neighbours at all is a whole connected component
        // that merged into one box. There is nothing to fold it into.
        for (partner, _) in candidates {
            let (mine, theirs) = (group[node], group[partner]);
            if mine == theirs {
                // An earlier fold already put the two together.
                break;
            }
            let after = |other: usize| {
                if group[other] == mine {
                    theirs
                } else {
                    group[other]
                }
            };
            if joins_a_cycle(graph, after, &members, &members[mine], theirs, &mut scratch) {
                continue;
            }
            let moving = std::mem::take(&mut members[mine]);
            for &member in &moving {
                group[member] = theirs;
            }
            members[theirs].extend(moving);
            folded = true;
            break;
        }
    }

    if !folded {
        return None;
    }
    Some(renumber(&group))
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
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); count];

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
        for &successor in &graph.out[node] {
            let other = groups[successor];
            if other != group {
                out[group].push(other);
            }
        }
    }

    WeightedGraph::new(adj_maps, self_loops, out)
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
        // Both set by the caller, which is the half still holding the ladder
        // these were measured from and the method that asked for it.
        opening: 0,
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

    // Above the limit a BFS per member gets expensive, and how many members a
    // function calls directly stands in well enough for how much it fronts.
    let by_reach = members.len() <= REACHABILITY_MEMBER_LIMIT;

    // One scratch pair for the whole cluster. This is called once per cluster
    // per rung, so on a real project it runs over roughly every function at
    // every level of the ladder, and a `HashSet` and a `VecDeque` allocated
    // and dropped per member is the bulk of that.
    let mut seen: HashSet<SymbolId> = HashSet::new();
    let mut queue: VecDeque<SymbolId> = VecDeque::new();

    let ranks: Vec<Rank<'_>> = members
        .iter()
        .map(|member| {
            let callers = graph.caller_ids(member);
            Rank {
                // Multiplicity matters: five calls into a function from
                // outside make it more of an entrance than one does.
                external: callers
                    .iter()
                    .filter(|caller| !inside.contains(*caller))
                    .count(),
                dominance: if by_reach {
                    internal_reach(graph, member, &inside, &mut seen, &mut queue)
                } else {
                    graph
                        .callee_ids(member)
                        .iter()
                        .filter(|callee| inside.contains(*callee))
                        .count()
                },
                // A function nothing calls anywhere is a front door by
                // definition — `main`, a thread body, a callback the runtime
                // invokes. It has no incoming calls to rank it by, so it would
                // otherwise be filtered out of its own cluster's naming, which
                // is how the cluster holding `main` came to be labelled
                // `parse_integer`.
                unreached: callers.is_empty(),
                tie_break: tie_break_key(graph, member),
            }
        })
        .collect();

    let entered_from_outside = ranks.iter().any(|rank| rank.external > 0);

    (0..members.len())
        .filter(|&i| !entered_from_outside || ranks[i].external > 0 || ranks[i].unreached)
        .max_by(|&i, &j| {
            ranks[i]
                .dominance
                .cmp(&ranks[j].dominance)
                .then(ranks[i].external.cmp(&ranks[j].external))
                // Definition order is the tie-break a reader would use
                // themselves, so the lower one has to rank higher — hence the
                // reversed comparison against a `max_by`. The uuid behind it
                // only exists so the answer is never ambiguous.
                .then_with(|| ranks[j].tie_break.cmp(&ranks[i].tie_break))
        })
        .map(|i| members[i])
}

/// What the ranking knows about one member, read off the graph once.
///
/// The three counts used to be three passes over `members`, two of which
/// walked the same caller list; the key used to be rebuilt inside the
/// comparator, which clones a path and so paid two allocations per comparison
/// over an O(n log n) fold. Gathering all four in one pass is the same
/// ordering, read once.
struct Rank<'g> {
    /// Calls arriving from outside the cluster, one per call site.
    external: usize,
    /// How much of the cluster this member fronts — reachability, or the
    /// direct-callee stand-in above [`REACHABILITY_MEMBER_LIMIT`].
    dominance: usize,
    /// Nothing calls this member from anywhere, inside or out.
    unreached: bool,
    /// Definition site then uuid, borrowed from the graph: the path is only
    /// ever compared, so there is nothing for the key to own.
    tie_break: (&'g str, u32, uuid_bytes::Bytes),
}

/// How many other members this one reaches, following calls that stay inside
/// the cluster.
///
/// `seen` and `queue` are the caller's scratch, cleared here rather than
/// allocated: this runs once per member and the sets it builds are the size of
/// the cluster.
fn internal_reach(
    graph: &CallGraph,
    start: &SymbolId,
    inside: &HashSet<SymbolId>,
    seen: &mut HashSet<SymbolId>,
    queue: &mut VecDeque<SymbolId>,
) -> usize {
    seen.clear();
    queue.clear();
    seen.insert(*start);
    queue.push_back(*start);

    while let Some(current) = queue.pop_front() {
        for callee in graph.callee_ids(&current) {
            if inside.contains(callee) && seen.insert(*callee) {
                queue.push_back(*callee);
            }
        }
    }

    seen.len() - 1
}

fn tie_break_key<'g>(graph: &'g CallGraph, symbol: &SymbolId) -> (&'g str, u32, uuid_bytes::Bytes) {
    let location = graph.get_function(symbol).map(|f| &f.definition_location);
    (
        location.map(|l| l.file_path.as_str()).unwrap_or_default(),
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

    // -- the objective --------------------------------------------------------

    /// How one pass at `resolution` groups a graph, as names, so a test can
    /// state the grouping it expects. Driven directly rather than through
    /// `Clustering::compute`, which stops before the first pass on a graph this
    /// small.
    fn one_pass(graph: &CallGraph, resolution: f64) -> Vec<Vec<String>> {
        let base = build_base_graph(graph, None);
        let (groups, count) = louvain_pass(&base.graph, resolution);

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

        let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);
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

    /// The property this module now promises: over a call graph with no cycle,
    /// no rung of the ladder has one either.
    #[test]
    fn every_rung_of_a_dag_is_a_dag() {
        let crossing = crossing_graph();
        let (layered, _) = layered_dag(60);
        for (name, graph) in [("crossing", &crossing), ("layered", &layered)] {
            let clustering = Clustering::compute(graph, ClusterMethod::Modularity);
            assert!(
                clustering.level_count() > 1,
                "{name}: the fixture should coarsen"
            );
            for level in 0..clustering.level_count() {
                assert!(
                    rung_is_a_dag(graph, &clustering, level),
                    "{name}: rung {level} has a cycle: {:?}",
                    grouping_at(graph, &clustering, level)
                );
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
        let clustering = Clustering::compute(&graph, ClusterMethod::Modularity);

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
        assert_eq!(
            fingerprint(
                &graph,
                &Clustering::compute(&graph, ClusterMethod::Modularity)
            ),
            fingerprint(
                &graph,
                &Clustering::compute(&graph, ClusterMethod::Modularity)
            ),
        );
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
                    "f3", "f30", "f45", "f35", "f17", "f5", "f19", "f47", "f20", "f44", "f53",
                    "f54", "f0", "f24", "f51", "f4", "f16", "f32", "f39", "f23", "f29", "f15",
                    "f40", "f8", "f26", "f22", "f43", "f34",
                ],
                vec![
                    "f3", "f30", "f43", "f35", "f15", "f5", "f19", "f47", "f20", "f53", "f54",
                    "f0", "f22", "f4", "f32", "f23", "f29", "f40", "f8", "f34",
                ],
                vec![
                    "f3", "f30", "f43", "f5", "f47", "f20", "f54", "f0", "f32", "f23", "f40",
                    "f34",
                ],
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
        let clustering =
            Clustering::compute_within(&graph, &HashSet::new(), ClusterMethod::Modularity);
        assert!(clustering.is_empty());
    }
}
