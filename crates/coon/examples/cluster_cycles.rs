//! Headless measurement: how often does the cluster map draw a mutual edge?
//!
//! Loads a project through the same clangd path the app uses, resolves the
//! call hierarchy of one function's whole neighbourhood, clusters it exactly
//! as `Ctrl+Shift+M` does, and reports every pair of top-level communities
//! that call each other both ways — with the weight each way, and the
//! individual calls behind the minority direction.
//!
//! The app's own edge warmup is not enough to measure against: it seeds once,
//! from whatever symbols have landed at that moment, so a function the sweep
//! missed sits in the graph with no callees at all. This walks the
//! neighbourhood itself, asking for every function it reaches, until the
//! frontier stops growing.
//!
//! Throwaway diagnostic, not part of the product.
//!
//!     cargo run --release -p coon --example cluster_cycles -- <project> <fn> [cap] [secs]

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ::core::layout::{compute_cluster_layout, LayoutNodeId, LayoutOrientation};
use ::core::{spawn_lsp_bridge, Command, CoreApp};
use logic::{Clustering, GraphTraversal};
use lsp::LspRequest;
use model::{CallDirection, CallGraph, LspLoadPhase, SymbolId};

/// How many call-hierarchy requests to have outstanding at once.
const IN_FLIGHT: usize = 8;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let project = args
        .next()
        .expect("usage: cluster_cycles <project> <fn> [cap] [secs]");
    let target = args
        .next()
        .expect("usage: cluster_cycles <project> <fn> [cap] [secs]");
    let cap: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1500);
    let budget = Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or(1800));
    // Resolving the neighbourhood costs a clangd sweep a minute long; cache it
    // so the clustering experiments below can be re-run in a second.
    let cache = args.next();

    if let Some(path) = cache.as_ref().filter(|p| std::path::Path::new(p).exists()) {
        let json = std::fs::read_to_string(path).expect("cache unreadable");
        let graph: CallGraph = serde_json::from_str(&json).expect("cache unparseable");
        eprintln!(
            "loaded cached graph: {} functions, {} calls",
            graph.node_count(),
            graph.edge_count()
        );
        let root = graph
            .find_function_by_name(&target)
            .map(|f| f.id)
            .expect("cached graph has no such function");
        report(&graph, root, &target);
        return;
    }

    let lsp = spawn_lsp_bridge(&project);
    let mut core = CoreApp::new_with_lsp(CallGraph::new(), lsp);
    let started = Instant::now();

    // ---- Phase 1: let clangd hand over the symbols. ----
    let mut symbols_done = false;
    let mut last_edges = 0usize;
    let mut settled_at = Instant::now();
    loop {
        for event in core.tick() {
            if let ::core::AppEvent::LspProgress(phase) = event {
                match phase {
                    LspLoadPhase::Completed => symbols_done = true,
                    LspLoadPhase::Failed(e) => {
                        eprintln!("clangd failed: {e}");
                        std::process::exit(1);
                    }
                    _ => {}
                }
            }
        }
        let edges = core.call_graph.edge_count();
        if edges != last_edges {
            last_edges = edges;
            settled_at = Instant::now();
        }
        // Zero functions is not a finished load: clangd answers the loader's
        // discovery query late when the index is cold, and that reply is
        // ingested here rather than dropped.
        if symbols_done
            && core.call_graph.node_count() > 0
            && settled_at.elapsed() > Duration::from_secs(10)
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(240) {
            eprintln!("symbols never settled; carrying on with what loaded");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!(
        "[{:>4}s] symbols in: {} functions, {} calls",
        started.elapsed().as_secs(),
        core.call_graph.node_count(),
        core.call_graph.edge_count()
    );

    // The loader streams at most `MAX_INITIAL_SYMBOLS`, so the function asked
    // for is often simply not in the graph yet. Ctrl+P pulls it in with a named
    // workspace-symbol query; do the same.
    for attempt in 0..4 {
        if core.call_graph.find_function_by_name(&target).is_some() {
            break;
        }
        if let Some(tx) = core.lsp.request_tx.as_ref() {
            let _ = tx.send(LspRequest::GetWorkspaceSymbols {
                request_id: uuid::Uuid::new_v4().to_string(),
                query: target.clone(),
            });
            eprintln!(
                "[{:>4}s] asking clangd for '{target}' (attempt {})",
                started.elapsed().as_secs(),
                attempt + 1
            );
        }
        let waited = Instant::now();
        while waited.elapsed() < Duration::from_secs(15) {
            core.tick();
            if core.call_graph.find_function_by_name(&target).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let Some(root) = core.call_graph.find_function_by_name(&target).map(|f| f.id) else {
        eprintln!("no function named {target}");
        std::process::exit(1);
    };
    eprintln!(
        "[{:>4}s] found {target}; graph now {} functions",
        started.elapsed().as_secs(),
        core.call_graph.node_count()
    );

    // ---- Phase 2: resolve the neighbourhood's edges. ----
    let mut requested: HashSet<SymbolId> = HashSet::new();
    let mut retried = false;
    'saturate: loop {
        let reachable = GraphTraversal::new(&core.call_graph).find_reachable_from(&root);
        let todo: Vec<SymbolId> = reachable
            .into_iter()
            .filter(|id| !requested.contains(id))
            .collect();
        if todo.is_empty() {
            // A dropped request looks exactly like a leaf, and a starved sweep
            // measures a graph that is mostly missing. Ask again for everything
            // that came back childless before believing the frontier closed.
            if retried {
                break;
            }
            retried = true;
            let childless: Vec<SymbolId> = requested
                .iter()
                .copied()
                .filter(|id| core.call_graph.get_callees(id).is_empty())
                .collect();
            eprintln!(
                "[{:>4}s] frontier closed at {} function(s); retrying {} childless",
                started.elapsed().as_secs(),
                requested.len(),
                childless.len()
            );
            if childless.is_empty() {
                break;
            }
            for id in &childless {
                requested.remove(id);
                core.lsp.loading_states.remove(id);
            }
            continue;
        }
        for batch in todo.chunks(IN_FLIGHT) {
            for &id in batch {
                requested.insert(id);
                core.dispatch(Command::ExpandFunction(id));
            }
            let waited = Instant::now();
            while !core.lsp.pending_requests.is_empty()
                && waited.elapsed() < Duration::from_secs(30)
            {
                core.tick();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if requested.len() % 100 < IN_FLIGHT {
                eprintln!(
                    "[{:>4}s] resolved {} function(s); graph {} nodes / {} calls",
                    started.elapsed().as_secs(),
                    requested.len(),
                    core.call_graph.node_count(),
                    core.call_graph.edge_count()
                );
            }
            if requested.len() >= cap || started.elapsed() > budget {
                eprintln!("stopping the sweep at {} function(s)", requested.len());
                break 'saturate;
            }
        }
    }

    let graph = &core.call_graph;
    eprintln!(
        "[{:>4}s] neighbourhood resolved: {} requested, graph {} functions / {} calls",
        started.elapsed().as_secs(),
        requested.len(),
        graph.node_count(),
        graph.edge_count()
    );

    let neighbourhood = GraphTraversal::new(graph).find_reachable_from(&root).len();
    if neighbourhood < 100 {
        eprintln!(
            "sweep starved: only {neighbourhood} function(s) reachable from {target}. \
             clangd answered too little to measure; not caching."
        );
        std::process::exit(2);
    }

    if let Some(path) = cache.as_ref() {
        match serde_json::to_string(graph) {
            Ok(json) => {
                let _ = std::fs::write(path, json);
                eprintln!("cached the graph to {path}");
            }
            Err(e) => eprintln!("could not cache the graph: {e}"),
        }
    }

    report(graph, root, &target);
}

/// One run of the measurement over a resolved graph.
fn report(graph: &CallGraph, root: SymbolId, target: &str) {
    let reachable: HashSet<SymbolId> = GraphTraversal::new(graph)
        .find_reachable_from(&root)
        .into_iter()
        .collect();

    println!("== scope: {target} ==");
    println!("reachable functions: {}", reachable.len());

    let baseline = measure(graph, &reachable);
    println!("\n-- baseline: cluster everything reachable --");
    baseline.summarise();
    baseline.detail(graph, 12);

    scc_analysis(graph, &reachable, &baseline);
    merge_simulation(&baseline);
    straddler_count(&baseline);

    // The back edges are dominated by shared utility leaves — mutex helpers,
    // accessors, atomics. Hold them out of the clustering and see what is left.
    println!("\n-- holding out shared leaves --");
    println!("  a leaf is a function with no callees inside the scope;");
    println!("  `shared` is how many distinct callers inside the scope it has.\n");
    println!(
        "  {:>8}  {:>8}  {:>8}  {:>8}  {:>7}  {:>7}",
        "min callers", "held out", "clusters", "edges", "pairs", "mutual%"
    );
    for k in [2usize, 3, 5, 10, usize::MAX] {
        let held: HashSet<SymbolId> = if k == usize::MAX {
            HashSet::new()
        } else {
            reachable
                .iter()
                .copied()
                .filter(|id| {
                    let callees_inside = graph
                        .get_callees(id)
                        .iter()
                        .filter(|c| reachable.contains(&c.id))
                        .count();
                    if callees_inside > 0 {
                        return false;
                    }
                    let callers_inside = graph
                        .get_callers(id)
                        .iter()
                        .filter(|c| reachable.contains(&c.id))
                        .count();
                    callers_inside >= k
                })
                .collect()
        };
        let scope: HashSet<SymbolId> = reachable.difference(&held).copied().collect();
        let m = measure(graph, &scope);
        println!(
            "  {:>11}  {:>8}  {:>8}  {:>8}  {:>7}  {:>6.0}%",
            if k == usize::MAX {
                "none".to_string()
            } else {
                k.to_string()
            },
            held.len(),
            m.clusters,
            m.edges,
            m.pairs.len(),
            m.mutual_share() * 100.0
        );
    }
}

/// The cluster map's edges for one scope, and the mutual pairs among them.
struct Measured {
    clustering: Clustering,
    clusters: usize,
    edges: usize,
    pairs: Vec<(LayoutNodeId, LayoutNodeId, usize, usize)>,
    calls: HashMap<(LayoutNodeId, LayoutNodeId), Vec<(String, String)>>,
}

impl Measured {
    fn mutual_share(&self) -> f64 {
        if self.edges == 0 {
            return 0.0;
        }
        (self.pairs.len() * 2) as f64 / self.edges as f64
    }

    fn summarise(&self) {
        println!("top-level clusters:  {}", self.clusters);
        println!("cluster edges drawn: {}", self.edges);
        println!(
            "mutual pairs:        {} ({} of {} edges, {:.0}%)",
            self.pairs.len(),
            self.pairs.len() * 2,
            self.edges,
            self.mutual_share() * 100.0
        );
        let ratios: Vec<f64> = self
            .pairs
            .iter()
            .map(|&(_, _, f, b)| f as f64 / b as f64)
            .collect();
        println!(
            "lopsided (>=5:1): {}   balanced (<2:1): {}",
            ratios.iter().filter(|r| **r >= 5.0).count(),
            ratios.iter().filter(|r| **r < 2.0).count()
        );
    }

    fn detail(&self, graph: &CallGraph, limit: usize) {
        let label = |node: &LayoutNodeId| -> String {
            match node {
                LayoutNodeId::Cluster(key) => self
                    .clustering
                    .by_key(*key)
                    .map(|c| format!("{} [{} members]", c.label, c.members.len()))
                    .unwrap_or_else(|| format!("{key:?}")),
                LayoutNodeId::Symbol(id) => graph
                    .get_function(id)
                    .map(|f| f.name.clone())
                    .unwrap_or_else(|| format!("{id:?}")),
            }
        };

        println!("\n-- mutual pairs, heaviest first --");
        for (a, b, fwd, back) in self.pairs.iter().take(limit) {
            println!(
                "\n{}\n  ->{fwd}  <-{back}   ratio {:.1}:1\n{}",
                label(a),
                *fwd as f64 / *back as f64,
                label(b)
            );
            if let Some(back_calls) = self.calls.get(&(*b, *a)) {
                // What the back edge is actually made of is the whole question:
                // a handful of shared leaves, or genuine two-way traffic.
                let mut by_callee: HashMap<&str, usize> = HashMap::new();
                for (_, callee) in back_calls {
                    *by_callee.entry(callee.as_str()).or_insert(0) += 1;
                }
                let mut top: Vec<(&str, usize)> = by_callee.into_iter().collect();
                top.sort_by_key(|&(name, n)| (std::cmp::Reverse(n), name.to_string()));
                let shown: Vec<String> = top
                    .iter()
                    .take(5)
                    .map(|(name, n)| format!("{name} x{n}"))
                    .collect();
                println!("   {} call(s) into: {}", back_calls.len(), shown.join(", "));
            }
        }
    }
}

fn measure(graph: &CallGraph, scope: &HashSet<SymbolId>) -> Measured {
    let clustering = Clustering::compute_within(graph, scope, None);
    if clustering.is_empty() {
        return Measured {
            clustering,
            clusters: 0,
            edges: 0,
            pairs: Vec::new(),
            calls: HashMap::new(),
        };
    }

    // The map as first opened: top level, nothing expanded.
    let layout = compute_cluster_layout(
        graph,
        &clustering,
        &HashSet::new(),
        CallDirection::Outgoing,
        LayoutOrientation::LeftRight,
    );

    let weights: HashMap<(LayoutNodeId, LayoutNodeId), usize> = layout
        .edges
        .iter()
        .map(|e| ((e.source, e.target), e.weight))
        .collect();

    // Which top-level cluster owns each member — the attribution
    // `aggregate_calls` makes, rebuilt here so the calls can be named.
    let mut owner: HashMap<SymbolId, LayoutNodeId> = HashMap::new();
    for &id in clustering.top_level() {
        if let Some(cluster) = clustering.cluster(id) {
            for member in &cluster.members {
                owner.insert(*member, LayoutNodeId::Cluster(cluster.key));
            }
        }
    }
    let mut calls: HashMap<(LayoutNodeId, LayoutNodeId), Vec<(String, String)>> = HashMap::new();
    for caller in graph.function_ids() {
        let Some(&from) = owner.get(caller) else {
            continue;
        };
        for callee in graph.get_callees(caller) {
            let Some(&to) = owner.get(&callee.id) else {
                continue;
            };
            if from == to {
                continue;
            }
            let caller_name = graph
                .get_function(caller)
                .map(|f| f.name.clone())
                .unwrap_or_default();
            calls
                .entry((from, to))
                .or_default()
                .push((caller_name, callee.name.clone()));
        }
    }

    let mut pairs: Vec<(LayoutNodeId, LayoutNodeId, usize, usize)> = Vec::new();
    for (&(a, b), &fwd) in &weights {
        if let Some(&back) = weights.get(&(b, a)) {
            // Report each pair once, heavier direction first.
            if fwd > back || (fwd == back && a.to_string() < b.to_string()) {
                pairs.push((a, b, fwd, back));
            }
        }
    }
    pairs.sort_by_key(|&(_, _, fwd, back)| std::cmp::Reverse(fwd + back));

    let clusters = clustering.top_level().len();
    Measured {
        clustering,
        clusters,
        edges: layout.edges.len(),
        pairs,
        calls,
    }
}

/// Is the neighbourhood's call graph itself cyclic?
///
/// This is what separates the two explanations. If the subgraph is a DAG, every
/// mutual pair on the map was manufactured by the partition straddling a
/// one-way path, and no amount of edge-drawing honesty makes it go away. If it
/// has large strongly-connected components, some of the cycles are real and the
/// map is telling the truth.
fn scc_analysis(graph: &CallGraph, scope: &HashSet<SymbolId>, m: &Measured) {
    let nodes: Vec<SymbolId> = {
        let mut v: Vec<SymbolId> = scope.iter().copied().collect();
        v.sort_unstable_by_key(|s| s.0);
        v
    };
    let index: HashMap<SymbolId, usize> = nodes.iter().enumerate().map(|(i, s)| (*s, i)).collect();
    let adj: Vec<Vec<usize>> = nodes
        .iter()
        .map(|id| {
            graph
                .get_callees(id)
                .iter()
                .filter_map(|c| index.get(&c.id).copied())
                .collect()
        })
        .collect();

    // Iterative Tarjan — the recursion depth here is the call depth of jemalloc.
    let n = nodes.len();
    let (mut idx, mut low, mut on_stack) = (vec![usize::MAX; n], vec![0usize; n], vec![false; n]);
    let mut comp = vec![usize::MAX; n];
    let (mut stack, mut next_index, mut next_comp) = (Vec::new(), 0usize, 0usize);

    for start in 0..n {
        if idx[start] != usize::MAX {
            continue;
        }
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(frame) = work.last().copied() {
            let (v, pi) = frame;
            if pi == 0 {
                idx[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            let mut descended = false;
            for (k, &w) in adj[v].iter().enumerate().skip(pi) {
                work.last_mut().expect("v is on the work stack").1 = k + 1;
                if idx[w] == usize::MAX {
                    work.push((w, 0));
                    descended = true;
                    break;
                } else if on_stack[w] {
                    low[v] = low[v].min(idx[w]);
                }
            }
            if descended {
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == idx[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    comp[w] = next_comp;
                    if w == v {
                        break;
                    }
                }
                next_comp += 1;
            }
        }
    }

    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for &c in &comp {
        *sizes.entry(c).or_insert(0) += 1;
    }
    let mut big: Vec<usize> = sizes.values().copied().filter(|&s| s > 1).collect();
    big.sort_unstable_by(|a, b| b.cmp(a));
    let in_cycles: usize = big.iter().sum();

    println!("\n-- is the call graph itself cyclic? --");
    println!("functions in scope:            {}", n);
    println!("strongly-connected components: {}", next_comp);
    println!(
        "components larger than one:    {} (holding {} function(s), largest {})",
        big.len(),
        in_cycles,
        big.first().copied().unwrap_or(0)
    );

    // How many of the calls behind a back edge are inside one component —
    // that is, part of a real cycle rather than a straddled one-way path.
    let (mut total, mut cyclic) = (0usize, 0usize);
    let by_name: HashMap<&str, SymbolId> = nodes
        .iter()
        .filter_map(|id| graph.get_function(id).map(|f| (f.name.as_str(), *id)))
        .collect();
    for (a, b, _, _) in &m.pairs {
        if let Some(back_calls) = m.calls.get(&(*b, *a)) {
            for (caller, callee) in back_calls {
                total += 1;
                let both = by_name
                    .get(caller.as_str())
                    .zip(by_name.get(callee.as_str()));
                if let Some((u, v)) = both {
                    if let (Some(&cu), Some(&cv)) = (index.get(u), index.get(v)) {
                        if comp[cu] == comp[cv] {
                            cyclic += 1;
                        }
                    }
                }
            }
        }
    }
    println!(
        "calls behind back edges:       {total}, of which inside one component: {cyclic} ({:.0}%)",
        if total == 0 {
            0.0
        } else {
            cyclic as f64 * 100.0 / total as f64
        }
    );
}

/// What happens if mutually-calling communities are simply merged?
///
/// The hierarchy already has a level above, so merging is cheap to reach for —
/// but only if it leaves a map with more than one box on it.
fn merge_simulation(m: &Measured) {
    let mut parent: HashMap<LayoutNodeId, LayoutNodeId> = HashMap::new();
    fn find(parent: &mut HashMap<LayoutNodeId, LayoutNodeId>, x: LayoutNodeId) -> LayoutNodeId {
        let p = *parent.entry(x).or_insert(x);
        if p == x {
            return x;
        }
        let root = find(parent, p);
        parent.insert(x, root);
        root
    }

    for (a, b, _, _) in &m.pairs {
        let (ra, rb) = (find(&mut parent, *a), find(&mut parent, *b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    let mut groups: HashMap<LayoutNodeId, usize> = HashMap::new();
    let keys: Vec<LayoutNodeId> = parent.keys().copied().collect();
    for node in keys {
        let root = find(&mut parent, node);
        *groups.entry(root).or_insert(0) += 1;
    }
    let untouched = m.clusters.saturating_sub(parent.len());
    let largest = groups.values().copied().max().unwrap_or(0);

    println!("\n-- if mutually-calling communities were merged --");
    println!(
        "{} cluster(s) -> {} group(s) ({} merged group(s) + {} untouched); largest merge swallows {} cluster(s)",
        m.clusters,
        groups.len() + untouched,
        groups.len(),
        untouched,
        largest
    );
}

/// How much would have to move to make the cluster graph acyclic?
///
/// Every back edge is a set of calls; moving either end of all of them across
/// the cut removes it. The count of functions involved is the difference
/// between a repair pass and a re-partition.
fn straddler_count(m: &Measured) {
    let mut callers: HashSet<&str> = HashSet::new();
    let mut callees: HashSet<&str> = HashSet::new();
    let mut narrow = 0usize;
    for (a, b, _, _) in &m.pairs {
        let Some(back_calls) = m.calls.get(&(*b, *a)) else {
            continue;
        };
        let pair_callers: HashSet<&str> = back_calls.iter().map(|(c, _)| c.as_str()).collect();
        let pair_callees: HashSet<&str> = back_calls.iter().map(|(_, c)| c.as_str()).collect();
        if pair_callers.len().min(pair_callees.len()) <= 3 {
            narrow += 1;
        }
        callers.extend(pair_callers);
        callees.extend(pair_callees);
    }
    println!("\n-- how much would have to move --");
    println!(
        "distinct functions making back-edge calls: {}   receiving them: {}",
        callers.len(),
        callees.len()
    );
    println!(
        "pairs whose back edge has <=3 distinct functions on one end: {} of {}",
        narrow,
        m.pairs.len()
    );
}
