//! Print the coarsening ladder of a graph, one per method, rung by rung, with
//! the two numbers `Ladder::opening` reads: how dense the quotient is and how
//! big a box is.
//!
//! With no argument it reports the demo graph; given a cluster-map DOT export
//! it rebuilds that project's call graph and reports it, which is how a real
//! project gets measured without a clangd run.

use logic::{ClusterMethod, Clustering};
use model::{CallGraph, FunctionNode, Location, SymbolId};
use std::collections::{HashMap, HashSet};

fn main() {
    match std::env::args().nth(1) {
        None => report("demo", &core::create_demo_call_graph()),
        Some(path) => {
            let text = std::fs::read_to_string(&path).expect("a readable DOT file");
            report(&path, &from_dot(&text));
        }
    }
}

/// Rebuild a call graph from a COON DOT export — a cluster map or a call
/// tree. A node's name and definition site are the two lines of its tooltip,
/// which is the same in both. A cluster map's edge weights come back through
/// `penwidth`, which is `1 + log2(calls)`, so they are exact up to the one
/// decimal place the export prints; a tree's edges carry no width and weigh
/// one call each.
fn from_dot(text: &str) -> CallGraph {
    let mut graph = CallGraph::new();
    let mut ids: HashMap<&str, SymbolId> = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        let Some((name, rest)) = line.split_once(" [label=\"") else {
            continue;
        };
        if !name.starts_with('n') {
            continue;
        }
        let Some((_, tail)) = rest.split_once("tooltip=\"") else {
            continue;
        };
        let Some((qualified, location)) = tail.split_once("\\n") else {
            continue;
        };
        let location = location.split('"').next().unwrap_or(location);
        let (file, at) = location.rsplit_once(':').unwrap_or((location, "0"));
        let id = graph.add_function(FunctionNode::new(
            qualified.to_string(),
            qualified.to_string(),
            Location::new(file.to_string(), at.parse().unwrap_or(0), 0),
        ));
        ids.insert(name, id);
    }

    for line in text.lines() {
        let line = line.trim();
        let Some((from, rest)) = line.split_once(" -> ") else {
            continue;
        };
        let to = rest.trim_end_matches(';');
        let (to, pen) = match to.split_once(" [penwidth=") {
            Some((to, tail)) => (
                to,
                tail.split(',')
                    .next()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(1.0),
            ),
            None => (to, 1.0f32),
        };
        let calls = 2f32.powf(pen - 1.0).round().max(1.0) as u32;
        let (Some(&a), Some(&b)) = (ids.get(from), ids.get(to)) else {
            continue;
        };
        for site in 0..calls {
            graph.add_call(a, b, Location::new("x".to_string(), site, 0));
        }
    }
    graph
}

fn report(name: &str, graph: &CallGraph) {
    for &method in ClusterMethod::ALL {
        report_method(name, graph, method);
    }
}

fn report_method(name: &str, graph: &CallGraph, method: ClusterMethod) {
    let clustering = Clustering::compute(graph, method);
    println!(
        "\n{name} by {}: {} functions, {} calls — opens on rung {} of {}",
        method.label(),
        graph.node_count(),
        graph.edge_count(),
        clustering.opening_level() + 1,
        clustering.level_count()
    );

    for level in 0..clustering.level_count() {
        let ids = clustering.level(level);
        let mut owner: HashMap<SymbolId, usize> = HashMap::new();
        for (slot, id) in ids.iter().enumerate() {
            for member in &clustering.cluster(*id).unwrap().members {
                owner.insert(*member, slot);
            }
        }

        let mut pairs: HashSet<(usize, usize)> = HashSet::new();
        let mut directed: HashSet<(usize, usize)> = HashSet::new();
        for caller in graph.function_ids() {
            let Some(&a) = owner.get(caller) else {
                continue;
            };
            for callee in graph.get_callees(caller) {
                let Some(&b) = owner.get(&callee.id) else {
                    continue;
                };
                if a == b {
                    continue;
                }
                directed.insert((a, b));
                pairs.insert(if a < b { (a, b) } else { (b, a) });
            }
        }

        let n = ids.len();
        let possible = n * n.saturating_sub(1) / 2;
        let mutual = directed
            .iter()
            .filter(|(a, b)| directed.contains(&(*b, *a)))
            .count()
            / 2;
        let sizes: Vec<usize> = ids
            .iter()
            .map(|id| clustering.cluster(*id).unwrap().members.len())
            .collect();
        let held: usize = sizes.iter().sum();
        println!(
            "  rung {:>2}: {n:>4} boxes  {:>4} lines  density {:>4.0}%  mutual {mutual:>3}  \
             {:>4.1} fns/box  singletons {:>2}{}",
            level + 1,
            pairs.len(),
            if possible > 0 {
                pairs.len() as f64 / possible as f64 * 100.0
            } else {
                0.0
            },
            held as f64 / n as f64,
            sizes.iter().filter(|s| **s == 1).count(),
            if level == clustering.opening_level() {
                "   <- opens here"
            } else {
                ""
            }
        );
    }

    // The opening rung's boxes, largest first, with how much of each is one
    // directory — the number the quality test scores a method by.
    let opening = clustering.opening_level();
    let mut boxes: Vec<(usize, String, String, usize)> = clustering
        .level(opening)
        .iter()
        .filter_map(|id| clustering.cluster(*id))
        .map(|cluster| {
            let mut dirs: HashMap<&str, usize> = HashMap::new();
            for member in &cluster.members {
                let file = graph
                    .get_function(member)
                    .map(|f| f.definition_location.file_path.as_str())
                    .unwrap_or("");
                *dirs
                    .entry(file.rsplit_once('/').map_or("", |(d, _)| d))
                    .or_insert(0) += 1;
            }
            let (dir, share) = dirs
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
                .unwrap_or(("", 0));
            (
                cluster.members.len(),
                cluster.label.clone(),
                dir.to_string(),
                share,
            )
        })
        .collect();
    boxes.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let held: usize = boxes.iter().map(|b| b.0).sum();
    let pure: usize = boxes.iter().map(|b| b.3).sum();
    println!(
        "  opening rung: {} boxes, directory purity {:.2}",
        boxes.len(),
        pure as f64 / held.max(1) as f64
    );
    for (size, label, dir, share) in boxes {
        println!("    {label:<28} {size:>3} fns  {share:>3} in {dir}");
    }
}
