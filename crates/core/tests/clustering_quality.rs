//! Does `logic::cluster` recover the module structure of a C project from call
//! structure alone?
//!
//! That is the bet the clustering feature makes. The coarsener never sees a
//! file path: it only sees who calls whom, and how often. If the clusters it
//! merges line up with `src/net`, `src/store`, `src/proto` and friends, then
//! the edge weighting is carrying real information and the boxes will mean
//! something on a canvas. If they do not, no amount of rendering will rescue
//! them, and we would much rather learn that here than after the feature ships.
//!
//! Heavy-edge matching is not looking for modules — it merges the busiest pair
//! it can find, round after round — so this is a real test of whether call
//! frequency tracks module structure, and not a tautology about an algorithm
//! that was told to find seams.
//!
//! The demo graph is the specimen. It fakes a small key-value daemon with a
//! real directory layout, deep call chains, a second root, recursion, a cycle
//! and widely shared leaves — the shapes that make clustering hard. The
//! directory a function is defined in is the ground truth we score against,
//! and it is never fed to the clusterer.
//!
//! Four of the tests here assert; two only print. Run
//!
//! ```text
//! cargo test -p core --test clustering_quality -- --nocapture
//! ```
//!
//! and read the composition report — a person looking at that output is the
//! real quality gate. The assertions only catch the cases where the result has
//! degenerated badly enough that nobody needs to look.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use logic::cluster::{Cluster, ClusterId, ClusterMethod, Clustering};
use model::{CallGraph, SymbolId};

/// The directory part of a source path: `src/net/server.c` is `src/net`, and a
/// top-level file like `src/main.c` is just `src`. This is our stand-in for
/// "which module does this function belong to" — coarse, but it is exactly the
/// grouping a reader of the source tree would expect the graph to mirror.
fn directory_of(file_path: &str) -> &str {
    match file_path.rfind('/') {
        Some(i) => &file_path[..i],
        None => "",
    }
}

/// The functions clustering is expected to cover: everything with at least one
/// call in either direction. Functions that neither call nor are called are
/// deliberately excluded from the clustering, so every membership check here
/// is made against this set rather than against the whole graph.
fn connected_symbols(graph: &CallGraph) -> HashSet<SymbolId> {
    graph
        .functions()
        .filter(|f| !graph.get_callers(&f.id).is_empty() || !graph.get_callees(&f.id).is_empty())
        .map(|f| f.id)
        .collect()
}

/// The demo graph plus its clustering, and the two lookups every test needs:
/// a symbol's name (for readable output) and its source directory (for
/// scoring). Built once per test — the demo graph is small enough that sharing
/// it across tests would only buy flakiness.
struct Fixture {
    graph: CallGraph,
    clustering: Clustering,
    directories: HashMap<SymbolId, String>,
    names: HashMap<SymbolId, String>,
}

impl Fixture {
    fn new() -> Self {
        // `::core::` rather than `core::`: the workspace crate of that name
        // shadows Rust's built-in one, and the leading `::` picks ours.
        let graph = ::core::create_demo_call_graph();
        let clustering = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

        let mut directories = HashMap::new();
        let mut names = HashMap::new();
        for func in graph.functions() {
            directories.insert(
                func.id,
                directory_of(&func.definition_location.file_path).to_string(),
            );
            names.insert(func.id, func.name.clone());
        }

        Self {
            graph,
            clustering,
            directories,
            names,
        }
    }

    fn name_of(&self, symbol: &SymbolId) -> &str {
        self.names.get(symbol).map(String::as_str).unwrap_or("?")
    }

    fn directory_of_symbol(&self, symbol: &SymbolId) -> &str {
        self.directories
            .get(symbol)
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// How a cluster's members are spread across source directories, largest
    /// share first. Ties break on the directory name so the printed report is
    /// stable between runs and diffable between commits.
    fn directory_histogram(&self, members: &[SymbolId]) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for member in members {
            *counts
                .entry(self.directory_of_symbol(member).to_string())
                .or_insert(0) += 1;
        }
        let mut histogram: Vec<(String, usize)> = counts.into_iter().collect();
        histogram.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        histogram
    }

    /// One line per cluster, in the shape a human can scan:
    /// `server_run · 11 fns — src/net 9, src/util 2`.
    fn describe(&self, cluster: &Cluster) -> String {
        let histogram = self.directory_histogram(&cluster.members);
        let spread = histogram
            .iter()
            .map(|(dir, n)| format!("{dir} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let label = if cluster.label.is_empty() {
            format!("<unlabelled #{}>", cluster.id.0)
        } else {
            cluster.label.clone()
        };
        format!("{label} · {} fns — {spread}", cluster.members.len())
    }

    /// The share of the possible cluster pairs that anything calls between —
    /// how much of a hairball the map at this rung would be.
    fn crossing_density(&self, level: usize) -> f64 {
        let ids = self.clustering.level(level);
        let mut owner: HashMap<SymbolId, usize> = HashMap::new();
        for (slot, id) in ids.iter().enumerate() {
            for member in &self.clustering.cluster(*id).unwrap().members {
                owner.insert(*member, slot);
            }
        }

        let mut pairs: HashSet<(usize, usize)> = HashSet::new();
        for caller in self.graph.function_ids() {
            let Some(&a) = owner.get(caller) else {
                continue;
            };
            for callee in self.graph.get_callees(caller) {
                let Some(&b) = owner.get(&callee.id) else {
                    continue;
                };
                if a != b {
                    pairs.insert(if a < b { (a, b) } else { (b, a) });
                }
            }
        }

        let n = ids.len();
        if n < 2 {
            return 0.0;
        }
        pairs.len() as f64 / (n * (n - 1) / 2) as f64
    }

    fn top_level_clusters(&self) -> Vec<&Cluster> {
        self.clustering
            .top_level()
            .iter()
            .map(|id| {
                self.clustering.cluster(*id).unwrap_or_else(|| {
                    panic!(
                        "top_level() named cluster #{} but cluster() does not know it",
                        id.0
                    )
                })
            })
            .collect()
    }

    /// The coarsest cluster a symbol belongs to, via `ancestry`.
    fn top_level_of(&self, symbol: &SymbolId) -> Option<ClusterId> {
        self.clustering.ancestry(symbol).last().copied()
    }
}

/// The main deliverable: a readable picture of what the clustering actually
/// did. Nothing here asserts — this exists so a person can decide whether the
/// communities look like modules, and so the diff in that judgement is visible
/// when the algorithm changes.
#[test]
fn report_cluster_composition() {
    let fixture = Fixture::new();

    println!();
    println!("=== clustering of the demo call graph ===");
    println!(
        "{} functions, {} calls, {} levels, {} top-level clusters",
        fixture.graph.node_count(),
        fixture.graph.edge_count(),
        fixture.clustering.level_count(),
        fixture.clustering.top_level().len()
    );
    println!();

    println!("--- top level ---");
    for cluster in fixture.top_level_clusters() {
        println!("  {}", fixture.describe(cluster));
    }

    println!();
    println!("--- one level finer ---");
    for cluster in fixture.top_level_clusters() {
        println!("  {}", fixture.describe(cluster));
        if cluster.children.is_empty() {
            // A top-level cluster with no children is already a leaf cluster:
            // the hierarchy bottomed out here, so there is nothing finer to show.
            println!("      (no finer level — this cluster is already level 0)");
            continue;
        }
        for child_id in &cluster.children {
            match fixture.clustering.cluster(*child_id) {
                Some(child) => println!("      {}", fixture.describe(child)),
                None => println!("      <dangling child #{}>", child_id.0),
            }
        }
    }
    println!();
}

/// Communities should roughly follow the source tree. We score that as
/// *directory purity*: for each top-level cluster, the fraction of its members
/// that come from its single most common directory, averaged over clusters and
/// weighted by cluster size so a handful of two-function clusters cannot carry
/// the score.
///
/// The 0.6 threshold is a judgement call, not a derived constant. Perfect
/// recovery is neither possible nor desirable: the shared leaves (`xmalloc`,
/// `log_write`, `buf_append`) are called from every directory in the project
/// and genuinely belong to whichever community pulls hardest, so some mixing
/// is the algorithm working, not failing. 0.6 says "most of a cluster is one
/// module"; below that, clusters are mixtures and a reader gets no useful
/// mental model from them.
///
/// If this starts failing, do not reach for the threshold first. It almost
/// certainly means the edge weighting changed — most likely that the shared
/// utility leaves are now dominating the modularity score and smearing every
/// module together, or that call multiplicity stopped being weighted and the
/// tight intra-module chains lost their advantage over the cross-module ones.
/// Read the composition report before touching this number.
#[test]
fn top_level_clusters_mostly_follow_the_source_tree() {
    let fixture = Fixture::new();
    let clusters = fixture.top_level_clusters();
    assert!(
        !clusters.is_empty(),
        "the demo graph produced no clusters at all"
    );

    let mut dominant = 0usize;
    let mut total = 0usize;
    let mut breakdown = String::new();
    for cluster in &clusters {
        let histogram = fixture.directory_histogram(&cluster.members);
        let largest = histogram.first().map(|(_, n)| *n).unwrap_or(0);
        dominant += largest;
        total += cluster.members.len();

        let purity = if cluster.members.is_empty() {
            0.0
        } else {
            largest as f64 / cluster.members.len() as f64
        };
        breakdown.push_str(&format!(
            "\n    {:.2}  {}",
            purity,
            fixture.describe(cluster)
        ));
    }

    assert!(total > 0, "top-level clusters between them hold no members");

    // Weighting each cluster's purity by its size makes this the same thing as
    // the share of all clustered functions that sit in their cluster's
    // majority directory, which is the number that is actually interpretable.
    let weighted_purity = dominant as f64 / total as f64;
    println!("weighted directory purity: {weighted_purity:.3}{breakdown}");

    assert!(
        weighted_purity > 0.6,
        "clusters no longer track the source tree: weighted directory purity \
         {weighted_purity:.3} is at or below 0.6.{breakdown}"
    );
}

/// A count the user can navigate. One cluster tells them nothing they did not
/// already know, and forty clusters over fifty-odd functions is no better than
/// showing the raw nodes — the whole point is to collapse the graph into a few
/// things a person can hold in their head at once.
///
/// The upper bound is the method's own contract rather than a judgement call:
/// coarsening runs until the top rung is at most `TARGET_TOP_CLUSTERS` wide, so
/// a top level wider than that means the ladder stopped early — a round that
/// merged nothing, which on a real graph means the matching has stopped
/// finding pairs and the map has quietly become a node list. The lower bound is
/// the judgement: collapsing a whole program into one or two boxes hides the
/// structure instead of revealing it. The second assertion is the one that says
/// the boxes are worth drawing, and it is about size rather than count, because
/// a map of twelve two-function boxes satisfies every count you could name and
/// still summarises nothing.
#[test]
fn top_level_holds_a_navigable_number_of_clusters() {
    let fixture = Fixture::new();
    let count = fixture.clustering.top_level().len();
    let names: Vec<String> = fixture
        .top_level_clusters()
        .iter()
        .map(|c| fixture.describe(c))
        .collect();

    assert!(
        count > 2,
        "{count} top-level cluster(s) for {} functions — the whole graph \
         collapsed into one blob, which hides the structure instead of \
         revealing it: {names:#?}",
        fixture.graph.node_count()
    );
    assert!(
        count <= logic::cluster::TARGET_TOP_CLUSTERS,
        "{count} top-level clusters, above the {} the coarsening is supposed to \
         stop at — a round must have merged nothing: {names:#?}",
        logic::cluster::TARGET_TOP_CLUSTERS
    );

    let clustered: usize = fixture
        .top_level_clusters()
        .iter()
        .map(|c| c.members.len())
        .sum();
    let per_cluster = clustered as f64 / count as f64;
    assert!(
        per_cluster >= 3.0,
        "{count} top-level clusters hold {clustered} functions between them — \
         {per_cluster:.1} apiece, which is barely coarser than the raw node \
         list: {names:#?}"
    );
}

/// A cluster is only navigable if it has a name, and the name is only useful
/// if it is unique: two clusters called `log_write` are two clusters the user
/// cannot tell apart in a breadcrumb or a picker. A duplicate is the signature
/// of the gateway heuristic falling back on the same shared leaf twice, which
/// means it failed to find the real entry point for at least one of them.
#[test]
fn every_top_level_cluster_is_usefully_named() {
    let fixture = Fixture::new();
    let mut labels: BTreeMap<String, usize> = BTreeMap::new();

    for cluster in fixture.top_level_clusters() {
        assert!(
            !cluster.label.trim().is_empty(),
            "top-level cluster #{} has no label; its members are {:?}",
            cluster.id.0,
            cluster
                .members
                .iter()
                .map(|m| fixture.name_of(m))
                .collect::<Vec<_>>()
        );

        let entry = cluster.entry_point.unwrap_or_else(|| {
            panic!(
                "top-level cluster '{}' has no entry point, so nothing justifies its name",
                cluster.label
            )
        });
        assert!(
            cluster.members.contains(&entry),
            "cluster '{}' names an entry point ({}) that is not one of its members",
            cluster.label,
            fixture.name_of(&entry)
        );
        assert_eq!(
            cluster.label,
            fixture.name_of(&entry),
            "cluster label and entry-point name disagree; the label is supposed \
             to be the entry point's bare name"
        );

        *labels.entry(cluster.label.clone()).or_insert(0) += 1;
    }

    let duplicates: Vec<(&String, &usize)> = labels.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        duplicates.is_empty(),
        "two top-level clusters share a label, so the gateway heuristic picked \
         a poor name for at least one of them: {duplicates:?}"
    );
}

/// The invariants the rest of the system will lean on. These are cheap to
/// state and expensive to discover later: a renderer that walks `children`, or
/// a breadcrumb that walks `ancestry`, breaks in confusing ways if the
/// hierarchy is not actually a partition refinement.
#[test]
fn the_hierarchy_is_a_well_formed_partition_refinement() {
    let fixture = Fixture::new();
    let clustering = &fixture.clustering;

    assert!(
        !clustering.is_empty(),
        "the demo graph is richly connected; an empty clustering is a bug, not a result"
    );
    assert!(clustering.level_count() > 0, "clustering reports no levels");

    // Clustering covers exactly the functions that participate in at least one
    // call. Nothing connected may be dropped, and nothing isolated may be
    // invented.
    let expected = connected_symbols(&fixture.graph);
    let mut covered: HashSet<SymbolId> = HashSet::new();
    for func in fixture.graph.functions() {
        match clustering.cluster_of(&func.id) {
            Some(_) => {
                covered.insert(func.id);
            }
            None => assert!(
                !expected.contains(&func.id),
                "'{}' has calls but was left out of the clustering",
                func.name
            ),
        }
    }
    assert_eq!(
        covered.len(),
        expected.len(),
        "clustering covers {} symbols but {} take part in calls",
        covered.len(),
        expected.len()
    );

    // Per-cluster shape: non-empty, duplicate-free, and consistent with the
    // level it claims to be at.
    for cluster in clustering.clusters() {
        assert!(
            !cluster.members.is_empty(),
            "cluster #{} (level {}) has no members; an empty community should \
             never have been emitted",
            cluster.id.0,
            cluster.level
        );

        let unique: HashSet<SymbolId> = cluster.members.iter().copied().collect();
        assert_eq!(
            unique.len(),
            cluster.members.len(),
            "cluster '{}' lists a member twice",
            cluster.label
        );

        if cluster.level == 0 {
            assert!(
                cluster.children.is_empty(),
                "level-0 cluster '{}' has children; level 0 is the finest level",
                cluster.label
            );
        } else {
            assert!(
                !cluster.children.is_empty(),
                "cluster '{}' is at level {} but has no children, so nothing at \
                 the finer level accounts for its members",
                cluster.label,
                cluster.level
            );

            // A cluster is exactly its children pooled together — no member
            // appearing from nowhere, none lost on the way up.
            let mut from_children: HashSet<SymbolId> = HashSet::new();
            for child_id in &cluster.children {
                let child = clustering.cluster(*child_id).unwrap_or_else(|| {
                    panic!(
                        "cluster '{}' names child #{} which does not exist",
                        cluster.label, child_id.0
                    )
                });
                assert_eq!(
                    child.level,
                    cluster.level - 1,
                    "cluster '{}' (level {}) has a child at level {}; children \
                     must be exactly one level finer",
                    cluster.label,
                    cluster.level,
                    child.level
                );
                assert!(
                    child.parent == Some(cluster.id),
                    "child '{}' does not point back at its parent '{}'",
                    child.label,
                    cluster.label
                );
                from_children.extend(child.members.iter().copied());
            }
            assert_eq!(
                from_children, unique,
                "cluster '{}' is not the union of its children",
                cluster.label
            );
        }
    }

    // Parentlessness and top-level-ness are the same property, stated twice.
    let top: BTreeSet<ClusterId> = clustering.top_level().iter().copied().collect();
    for cluster in clustering.clusters() {
        let is_top = top.contains(&cluster.id);
        assert_eq!(
            is_top,
            cluster.parent.is_none(),
            "cluster '{}' is {}in top_level() but {}has a parent",
            cluster.label,
            if is_top { "" } else { "not " },
            if cluster.parent.is_some() {
                ""
            } else {
                "does not "
            }
        );
    }

    // Every level is a partition of the same symbol set. Louvain aggregates
    // communities of communities, so a level that covers fewer symbols than
    // its neighbours means a community was orphaned on the way up rather than
    // carried forward as a singleton.
    for level in 0..clustering.level_count() {
        let mut seen: HashSet<SymbolId> = HashSet::new();
        for id in clustering.level(level) {
            let cluster = clustering
                .cluster(*id)
                .unwrap_or_else(|| panic!("level({level}) named unknown cluster #{}", id.0));
            assert_eq!(
                cluster.level, level,
                "cluster '{}' appears in level({level}) but reports level {}",
                cluster.label, cluster.level
            );
            for member in &cluster.members {
                assert!(
                    seen.insert(*member),
                    "'{}' belongs to two clusters at level {level}; a level must \
                     be a partition",
                    fixture.name_of(member)
                );
            }
        }
        assert_eq!(
            seen.len(),
            expected.len(),
            "level {level} covers {} symbols but every level should partition \
             the same {} clustered symbols",
            seen.len(),
            expected.len()
        );
    }

    // `ancestry` is the breadcrumb: it starts at the symbol's own cluster and
    // walks parent links up to a top-level one.
    for symbol in &expected {
        let ancestry = clustering.ancestry(symbol);
        assert!(
            !ancestry.is_empty(),
            "'{}' is clustered but has no ancestry",
            fixture.name_of(symbol)
        );
        assert!(
            Some(ancestry[0]) == clustering.cluster_of(symbol),
            "ancestry of '{}' does not start at its own cluster",
            fixture.name_of(symbol)
        );
        for pair in ancestry.windows(2) {
            let child = clustering.cluster(pair[0]).unwrap();
            assert!(
                child.parent == Some(pair[1]),
                "ancestry of '{}' takes a step that is not a parent link",
                fixture.name_of(symbol)
            );
        }
        let last = *ancestry.last().unwrap();
        assert!(
            top.contains(&last),
            "ancestry of '{}' ends at cluster #{}, which is not top-level",
            fixture.name_of(symbol),
            last.0
        );
    }
}

/// The shared utilities are where clustering is most likely to go wrong.
/// `xmalloc` is called from all five directories, `log_write` from four:
/// modularity has to put them *somewhere*, and whichever community wins is
/// partly an artefact of the weighting. There is no right answer to assert
/// against, so this only reports — but a human reading the output should check
/// that these landed somewhere defensible (their own utility cluster, or the
/// module that leans on them hardest) rather than being scattered at random
/// across the modules they serve.
#[test]
fn report_where_the_cross_cutting_helpers_landed() {
    let fixture = Fixture::new();

    // Cross-cutting is a property of the graph, not a hardcoded list: a helper
    // is cross-cutting when its callers come from three or more directories.
    // Self-recursion does not count as a caller from anywhere.
    let mut shared: Vec<(usize, String, SymbolId, BTreeSet<String>)> = Vec::new();
    for func in fixture.graph.functions() {
        let caller_dirs: BTreeSet<String> = fixture
            .graph
            .get_callers(&func.id)
            .iter()
            .filter(|c| c.id != func.id)
            .map(|c| fixture.directory_of_symbol(&c.id).to_string())
            .collect();
        if caller_dirs.len() >= 3 {
            shared.push((caller_dirs.len(), func.name.clone(), func.id, caller_dirs));
        }
    }
    shared.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    println!();
    println!("=== cross-cutting helpers (callers span 3+ directories) ===");
    for (_, name, id, caller_dirs) in &shared {
        let landed = match fixture.top_level_of(id) {
            Some(cluster_id) => match fixture.clustering.cluster(cluster_id) {
                Some(cluster) => fixture.describe(cluster),
                None => format!("<unknown cluster #{}>", cluster_id.0),
            },
            None => "<not clustered>".to_string(),
        };
        println!(
            "  {name} (defined in {}, called from {})",
            fixture.directory_of_symbol(id),
            caller_dirs.iter().cloned().collect::<Vec<_>>().join(" ")
        );
        println!("      landed in: {landed}");
    }
    println!();
}

/// The map opens on a rung that reads.
///
/// Coarsening runs until the top rung is a handful of boxes, which is where a
/// user zooms out *to* and not where they should land: merging two clusters
/// unions their neighbourhoods, so density roughly doubles per round and the
/// top rung comes out a near-complete quotient. Measured on a real
/// 188-function project the ladder ran 5%, 11%, 24%, 50%, 94%, and the map used
/// to open on the last of those — 34 of the 45 possible box pairs drawn, half
/// of them in both directions.
///
/// So the opening rung is bounded from both sides, and this checks both on the
/// demo, where the same shape shows at a size a person can read.
#[test]
fn the_map_opens_on_a_rung_that_reads() {
    let fixture = Fixture::new();
    let clustering = &fixture.clustering;
    let top = clustering.level_count() - 1;
    let opening = clustering.opening_level();

    let describe = |level: usize| {
        let boxes = clustering.level(level).len();
        let held: usize = clustering
            .level(level)
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().members.len())
            .sum();
        format!(
            "rung {} — {boxes} boxes, {:.1} fns/box, density {:.0}%",
            level + 1,
            held as f64 / boxes as f64,
            fixture.crossing_density(level) * 100.0
        )
    };
    for level in 0..clustering.level_count() {
        println!(
            "  {}{}",
            describe(level),
            if level == opening {
                "   <- opens here"
            } else {
                ""
            }
        );
    }

    let box_size = |level: usize| {
        let boxes = clustering.level(level).len();
        let held: usize = clustering
            .level(level)
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().members.len())
            .sum();
        held as f64 / boxes as f64
    };

    assert!(
        fixture.crossing_density(top) > logic::cluster::MAX_READABLE_DENSITY,
        "the demo's top rung is supposed to be the hairball this test is about: {}",
        describe(top)
    );
    assert!(
        opening < top,
        "so the map must not open on it: {}",
        describe(opening)
    );
    assert!(
        box_size(opening) >= logic::cluster::MIN_BOX_SIZE,
        "a map of one- and two-function boxes is the sidebar with arrows on it: {}",
        describe(opening)
    );

    // And the rung is exactly where the two bounds meet. Worth stating even
    // though it restates the rule, because the density this test measures is
    // counted off the call graph the way the map draws it, where the one
    // `logic::cluster` picks the rung with is counted off the undirected
    // quotient it coarsens. The two are supposed to be the same number.
    let readable = (0..clustering.level_count())
        .rposition(|level| fixture.crossing_density(level) <= logic::cluster::MAX_READABLE_DENSITY)
        .unwrap_or(top);
    let useful = (0..clustering.level_count())
        .position(|level| box_size(level) >= logic::cluster::MIN_BOX_SIZE)
        .unwrap_or(0);
    assert_eq!(
        opening,
        readable.max(useful).min(top),
        "readable up to rung {}, useful from rung {}",
        readable + 1,
        useful + 1
    );
}

/// Every other assertion in this file is worthless if the clustering is a coin
/// flip. Heavy-edge matching is deterministic by construction — the visit order
/// is by degree then index, the partner tie-break is the lower index — but
/// "by construction" is exactly the kind of claim that quietly stops being true
/// when a `HashMap` iteration creeps into the pipeline. The comparison is on
/// member sets keyed by function name rather than on cluster ids, because ids
/// are free to be renumbered: it is the grouping that has to hold still.
#[test]
fn clustering_is_deterministic() {
    /// The grouping a clustering induces at one rung, stripped of cluster
    /// identity: a set of sets of function names.
    fn partition(
        clustering: &Clustering,
        level: usize,
        names: &HashMap<SymbolId, String>,
    ) -> BTreeSet<BTreeSet<String>> {
        clustering
            .level(level)
            .iter()
            .filter_map(|id| clustering.cluster(*id))
            .map(|cluster| {
                cluster
                    .members
                    .iter()
                    .map(|m| names.get(m).cloned().unwrap_or_else(|| "?".to_string()))
                    .collect::<BTreeSet<String>>()
            })
            .collect()
    }

    let graph = ::core::create_demo_call_graph();
    let names: HashMap<SymbolId, String> =
        graph.functions().map(|f| (f.id, f.name.clone())).collect();

    let first = Clustering::compute(&graph, ClusterMethod::HeavyEdge);
    let second = Clustering::compute(&graph, ClusterMethod::HeavyEdge);

    assert_eq!(
        first.level_count(),
        second.level_count(),
        "two runs over the same graph built ladders of different heights"
    );
    // Every rung, not just the top: a round that merged a different pair
    // low down can still come out the same width at the top.
    for level in 0..first.level_count() {
        assert_eq!(
            partition(&first, level, &names),
            partition(&second, level, &names),
            "two runs over the same graph disagree at rung {level}; the \
             clustering is not deterministic, which makes every quality check \
             here a coin flip"
        );
    }
}
