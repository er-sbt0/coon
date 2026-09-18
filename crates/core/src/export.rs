//! Taking the graph out of COON as a file somebody else can read.
//!
//! The format is **Graphviz DOT**, and it is the only one, for four reasons
//! that are specific to this program rather than to graphs in general:
//!
//! - It is already the lingua franca of C/C++ call graphs. Doxygen, egypt,
//!   gprof2dot and callgrind_annotate all emit DOT, so an exported COON graph
//!   drops into a pipeline somebody already has, and `dot -Tsvg`/`-Tpng`/`-Tpdf`
//!   turns it into every raster and vector format anyone was going to ask for
//!   next. Exporting SVG directly would answer one of those questions and close
//!   the door on the rest.
//! - It is text, so every frontend can produce it. The TUI cannot render a PNG
//!   and the web client would have to grow a rendering library to; a string
//!   built in `core` travels down the pipes that already exist and lands in all
//!   three the same way.
//! - It carries the structure, not a picture of it. `networkx`, `gvpr` and
//!   `graph-tool` read DOT back, so an export is still a graph afterwards —
//!   which is what somebody exporting a call graph usually wants it for.
//! - `subgraph cluster_…` is a native DOT concept that maps exactly onto what
//!   [`crate::layout::compute_cluster_layout`] already produces. The cluster
//!   map is the view that is hardest to reproduce by hand and the one DOT
//!   renders with no translation at all.
//!
//! What gets exported is **what is on screen**: the active workspace's view,
//! with its depth limit, its collapsed and expanded nodes, its call direction
//! and its orientation. Exporting the whole loaded graph instead would hand
//! back something nobody has ever looked at — on a project the size of jemalloc
//! that is ~3,300 nodes — and the mechanisms in `layout` exist precisely
//! because that is unreadable.
//!
//! Deliberately absent: colour. The canvas reads its palette from the CSS
//! custom properties so the graph and the chrome stay on one theme, and there
//! is no way to write those into a `.dot` file without forking the palette and
//! letting the copy drift. So the export carries shape, dash and pen weight —
//! the parts that mean something — and leaves the colours to whoever renders
//! it.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::layout::{compute_cluster_layout, compute_layout, LayoutOrientation};
use crate::protocol::layout_options;
use crate::workspace::{CoreWorkspace, ViewMode};

/// Heaviest line an aggregated cluster edge is drawn with, and the lightest.
/// The same log scale the client uses for the same edges (`edgeWidth` in
/// `app.js`), so a rendered export and the canvas weight a relation alike.
const EDGE_PEN_MIN: f32 = 1.0;
const EDGE_PEN_MAX: f32 = 6.0;

/// The formats a graph can be exported as.
///
/// One variant, and the module doc says why. It is an enum rather than nothing
/// at all because the alternative is a `Command` with no payload, and adding a
/// second format would then change the shape of a message every frontend
/// sends — a variant here is an addition, not a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    /// Graphviz DOT. Render with `dot -Tsvg graph.dot -o graph.svg`.
    Dot,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Dot => "dot",
        }
    }
}

/// A finished export: what to call the file and what to put in it.
///
/// `core` does not write it. Where a file belongs is the one part of this that
/// differs per frontend — the TUI's working directory, a browser download, a
/// desktop save — and a server writing to its own disk on behalf of a browser
/// tab somewhere else would be the wrong answer in the one case that has a
/// network in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    pub filename: String,
    pub contents: String,
}

/// Export the active workspace's current view, or `None` when it is empty —
/// a tab with no root, or a map of a graph whose edges have not arrived yet.
pub fn export(core: &CoreApp, format: ExportFormat) -> Option<Export> {
    let ws = core.workspaces.current()?;
    let contents = match ws.view {
        ViewMode::ClusterMap => cluster_dot(core),
        ViewMode::Rooted => rooted_dot(core),
    }?;
    Some(Export {
        filename: filename(core, format)?,
        contents,
    })
}

// ---------------------------------------------------------------------------
// The rooted call tree
// ---------------------------------------------------------------------------

fn rooted_dot(core: &CoreApp) -> Option<String> {
    let ws = core.workspaces.current()?;
    let root = ws.root_symbol?;
    let graph = &core.call_graph;

    // The same call that builds the canvas, so the file holds the subgraph the
    // user is looking at rather than a second interpretation of the same
    // settings. Its keys are exactly the visible nodes.
    let positions = compute_layout(graph, Some(root), ws.direction, &layout_options(core));
    if positions.is_empty() {
        return None;
    }

    // `positions` is a HashMap, so an order has to be imposed or the same graph
    // exports to a different file every time and no two are diffable.
    let mut visible: Vec<SymbolId> = positions.keys().copied().collect();
    visible.sort_by_key(|id| sort_key(graph, id));
    let index_of: HashMap<SymbolId, usize> =
        visible.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let mut out = String::new();
    let root_label = graph
        .get_function(&root)
        .map(|f| f.qualified_name.as_str())
        .unwrap_or("unknown");
    let direction = match ws.direction {
        CallDirection::Outgoing => "callees",
        CallDirection::Incoming => "callers",
    };

    header(
        &mut out,
        ws.orientation,
        &format!(
            "{root_label} — {direction}, {} levels, {} functions",
            ws.depth,
            visible.len()
        ),
    );

    for id in &visible {
        let Some(func) = graph.get_function(id) else {
            continue;
        };
        // What the canvas draws on a node that is hiding children: the count,
        // and a dashed border saying the picture stops short here. On a static
        // export it matters more than on the canvas, where the `+` button is
        // one click away.
        let hidden = hidden_children(graph, id, &positions, ws);
        let label = if hidden > 0 {
            format!("{}  +{hidden}", func.name)
        } else {
            func.name.clone()
        };
        let tooltip = format!(
            "{}\n{}:{}",
            func.qualified_name, func.definition_location.file_path, func.definition_location.line
        );

        write!(
            out,
            "  n{} [label={}, tooltip={}",
            index_of[id],
            quote(&label),
            quote(&tooltip)
        )
        .ok();
        if hidden > 0 {
            out.push_str(", style=\"rounded,dashed\"");
        }
        out.push_str("];\n");
    }

    out.push('\n');

    // Always caller → callee, whichever way the layout runs: the direction
    // decides which way the tree is read, not which way the calls go, and the
    // client draws them this way for the same reason.
    // Collected as index pairs and sorted numerically rather than as finished
    // lines: sorting the text puts `n10` before `n2`, which costs nothing but
    // makes the file read as though the order meant something.
    let mut seen: HashSet<(SymbolId, SymbolId)> = HashSet::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for edge in graph.edges() {
        let (Some(&caller), Some(&callee)) =
            (index_of.get(&edge.caller), index_of.get(&edge.callee))
        else {
            continue;
        };
        // Collapsing a node in a DAG is often invisible otherwise: its children
        // stay on screen because another caller reaches them. Dropping the
        // edges *leaving* it is what makes the gesture show, and the client
        // does the same.
        let parent = match ws.direction {
            CallDirection::Outgoing => edge.caller,
            CallDirection::Incoming => edge.callee,
        };
        if ws.collapsed.contains(&parent) {
            continue;
        }
        if !seen.insert((edge.caller, edge.callee)) {
            continue;
        }
        pairs.push((caller, callee));
    }
    pairs.sort_unstable();
    for (caller, callee) in pairs {
        writeln!(out, "  n{caller} -> n{callee};").ok();
    }

    out.push_str("}\n");
    Some(out)
}

/// How many of a node's children the view is not showing — the `+N` the canvas
/// puts on it.
fn hidden_children(
    graph: &CallGraph,
    id: &SymbolId,
    visible: &HashMap<SymbolId, crate::layout::NodePos>,
    ws: &CoreWorkspace,
) -> usize {
    let children = match ws.direction {
        CallDirection::Outgoing => graph.get_callees(id),
        CallDirection::Incoming => graph.get_callers(id),
    };
    if children.is_empty() {
        return 0;
    }
    if ws.collapsed.contains(id) {
        return children.len();
    }
    children
        .iter()
        .filter(|child| !visible.contains_key(&child.id))
        .count()
}

// ---------------------------------------------------------------------------
// The cluster map
// ---------------------------------------------------------------------------

/// The cluster map, as one `subgraph cluster_…` per box.
///
/// The canvas draws a cluster as a single node, because a box holding two
/// hundred names is unreadable on screen. A file is not a screen: `subgraph
/// cluster_…` is the native DOT concept the map maps onto, and it is most of
/// why DOT was the format chosen at all, so the export opens each box and puts
/// the functions inside it — the grouping is the thing being exported, and a
/// picture of twenty opaque boxes carries none of it.
///
/// Which makes the rung the size control. The whole map is always every
/// clustered function in the project, so exporting the coarsest rung writes the
/// whole graph into twenty boxes; zoom in first and the same file comes back
/// finer-grained and no larger. That is the one piece of sizing the user does
/// by hand here, and the zoom control is where they were already doing it.
fn cluster_dot(core: &CoreApp) -> Option<String> {
    let ws = core.workspaces.current()?;
    let clustering = core.clustering_for(ws);
    let layout = compute_cluster_layout(
        &core.call_graph,
        clustering,
        core.map_level_for(ws),
        ws.direction,
        ws.orientation,
    );
    if layout.nodes.is_empty() {
        return None;
    }

    // Boxes in the order the layout listed them, which is the clustering's own
    // order and stable across runs where a HashMap iteration is not.
    let boxes: Vec<&crate::layout::MetaNode> = layout.metas.iter().collect();

    // Every member of every box, named. A function can only be in one box at a
    // rung, so one pass over the boxes names the lot.
    let mut name_of: HashMap<SymbolId, String> = HashMap::new();
    let mut members_of: Vec<Vec<SymbolId>> = Vec::with_capacity(boxes.len());
    for meta in &boxes {
        let members = match clustering.by_key(meta.key) {
            Some(cluster) => cluster.members.clone(),
            None => Vec::new(),
        };
        for member in &members {
            let next = name_of.len();
            name_of.entry(*member).or_insert_with(|| format!("n{next}"));
        }
        members_of.push(members);
    }

    let mut out = String::new();
    let scope = match ws.map_scope.and_then(|s| core.call_graph.get_function(&s)) {
        Some(func) => format!("{} — neighbourhood", func.qualified_name),
        None => "Whole project".to_string(),
    };
    header(
        &mut out,
        ws.orientation,
        &format!(
            "Cluster map: {scope}, {} clusters ({}, rung {} of {})",
            boxes.len(),
            ws.method.label(),
            core.map_level_for(ws) + 1,
            clustering.level_count(),
        ),
    );

    for (id, (meta, members)) in boxes.iter().zip(&members_of).enumerate() {
        // The `cluster_` prefix is not decoration: Graphviz draws a subgraph as
        // a labelled box only when its name starts with it.
        writeln!(&mut out, "  subgraph cluster_{id} {{").ok();
        writeln!(
            &mut out,
            "    label={};",
            quote(&format!("{}  ({})", meta.label, meta.count))
        )
        .ok();
        out.push_str("    style=\"rounded\";\n");
        for member in members {
            emit_function(&mut out, *member, &name_of, &core.call_graph, 2);
        }
        out.push_str("  }\n");
    }

    out.push('\n');
    for edge in call_edges(&core.call_graph, &name_of, ws.direction) {
        let (Some(source), Some(target)) = (name_of.get(&edge.0), name_of.get(&edge.1)) else {
            continue;
        };
        writeln!(
            &mut out,
            "  {source} -> {target} [penwidth={:.1}, tooltip={}];",
            pen_width(edge.2),
            quote(&count_of("call", edge.2))
        )
        .ok();
    }

    out.push_str("}\n");
    Some(out)
}

/// Every call between two exported functions, as (from, to, how many), sorted.
///
/// `CallGraph` exposes no `edges()` accessor that counts multiplicity, so the
/// calls are walked the way `logic::cluster` and `layout` walk them:
/// `get_callees` per function, which returns one entry per distinct call site.
/// Calls inside a box are kept — that is what the box is for here — and the
/// ones leaving the exported set are dropped, exactly as an isolated function
/// is absent from the map it was never clustered into.
fn call_edges(
    graph: &CallGraph,
    exported: &HashMap<SymbolId, String>,
    direction: CallDirection,
) -> Vec<(SymbolId, SymbolId, usize)> {
    let mut weights: HashMap<(SymbolId, SymbolId), usize> = HashMap::new();
    for caller in graph.function_ids() {
        if !exported.contains_key(caller) {
            continue;
        }
        for callee in graph.get_callees(caller) {
            if !exported.contains_key(&callee.id) || callee.id == *caller {
                continue;
            }
            let pair = match direction {
                CallDirection::Outgoing => (*caller, callee.id),
                CallDirection::Incoming => (callee.id, *caller),
            };
            *weights.entry(pair).or_insert(0) += 1;
        }
    }

    let mut edges: Vec<(SymbolId, SymbolId, usize)> = weights
        .into_iter()
        .map(|((from, to), weight)| (from, to, weight))
        .collect();
    // A HashMap iteration decided the order; the file has to not depend on it.
    edges.sort_by(|a, b| {
        exported[&a.0]
            .cmp(&exported[&b.0])
            .then_with(|| exported[&a.1].cmp(&exported[&b.1]))
    });
    edges
}

fn emit_function(
    out: &mut String,
    symbol: SymbolId,
    names: &HashMap<SymbolId, String>,
    graph: &CallGraph,
    depth: usize,
) {
    let (Some(name), Some(func)) = (names.get(&symbol), graph.get_function(&symbol)) else {
        return;
    };
    let tooltip = format!(
        "{}\n{}:{}",
        func.qualified_name, func.definition_location.file_path, func.definition_location.line
    );
    writeln!(
        out,
        "{}{name} [label={}, tooltip={}];",
        "  ".repeat(depth),
        quote(&func.name),
        quote(&tooltip)
    )
    .ok();
}

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

fn header(out: &mut String, orientation: LayoutOrientation, label: &str) {
    let rankdir = match orientation {
        LayoutOrientation::LeftRight => "LR",
        LayoutOrientation::TopDown => "TB",
    };
    out.push_str("// Exported by COON. Render with: dot -Tsvg <this file> -o graph.svg\n");
    out.push_str("digraph coon {\n");
    writeln!(
        out,
        "  graph [rankdir={rankdir}, labelloc=\"t\", label={}, fontname=\"Helvetica\", fontsize=12];",
        quote(label)
    )
    .ok();
    out.push_str("  node [shape=box, style=rounded, fontname=\"Helvetica\", fontsize=11];\n");
    out.push_str("  edge [arrowsize=0.7];\n\n");
}

/// Sort functions into an order two runs agree on. The name first so the file
/// reads sensibly, the location and then the id to break the ties C++ hands out
/// freely — jemalloc has nine distinct `thd_start`s.
fn sort_key(graph: &CallGraph, id: &SymbolId) -> (String, String, u32, uuid::Uuid) {
    match graph.get_function(id) {
        Some(func) => (
            func.qualified_name.clone(),
            func.definition_location.file_path.clone(),
            func.definition_location.line,
            id.0,
        ),
        None => (String::new(), String::new(), 0, id.0),
    }
}

/// `3 calls`, `1 call`. Every plural in this file is of the same shape.
fn count_of(noun: &str, n: usize) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The same log scale the client draws these edges on, so an exported map and
/// the canvas weight the same relation alike.
fn pen_width(weight: usize) -> f32 {
    (EDGE_PEN_MIN + (weight.max(1) as f32).log2()).min(EDGE_PEN_MAX)
}

/// A DOT quoted string. Backslash and quote are the only two characters the
/// language escapes, and `\n` inside a label is a line break rather than a
/// character to escape — which is why callers wanting one write it themselves.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Name the file after what is in it, so a directory of exports is readable.
fn filename(core: &CoreApp, format: ExportFormat) -> Option<String> {
    let ws = core.workspaces.current()?;
    let subject = match ws.view {
        ViewMode::ClusterMap => match ws.map_scope.and_then(|s| core.call_graph.get_function(&s)) {
            Some(func) => format!("{}-clusters", func.name),
            None => "clusters".to_string(),
        },
        ViewMode::Rooted => ws
            .root_symbol
            .and_then(|s| core.call_graph.get_function(&s))
            .map(|f| f.name.clone())
            .unwrap_or_else(|| "graph".to_string()),
    };
    Some(format!("{}.{}", slug(&subject), format.extension()))
}

/// Everything a filename should not carry, collapsed to `-`. C++ symbols reach
/// for `:`, `<`, `>`, `*` and spaces, all of which are either illegal or
/// annoying somewhere.
fn slug(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "graph".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::create_demo_call_graph;
    use model::{FunctionNode, Location};

    /// An app rooted at the named demo function, showing the call tree.
    fn rooted_at(name: &str) -> CoreApp {
        let graph = create_demo_call_graph();
        let id = graph
            .find_function_by_name(name)
            .unwrap_or_else(|| panic!("the demo graph has no `{name}`"))
            .id;
        let mut core = CoreApp::new(graph);
        core.dispatch(crate::Command::StartGraphFromFunction(id));
        let ws = core.workspaces.current_mut().unwrap();
        ws.view = ViewMode::Rooted;
        ws.root_symbol = Some(id);
        core
    }

    fn dot(core: &CoreApp) -> String {
        export(core, ExportFormat::Dot)
            .expect("the demo graph has something on screen")
            .contents
    }

    #[test]
    fn a_rooted_export_is_a_digraph_naming_what_it_shows() {
        let core = rooted_at("main");
        let out = dot(&core);

        assert!(out.starts_with("// Exported by COON"));
        assert!(out.contains("digraph coon {"));
        assert!(out.trim_end().ends_with('}'));
        assert!(
            out.contains("label=\"main — callees, 3 levels"),
            "the header says what the breadcrumb says; got:\n{out}"
        );
        assert!(out.contains(" -> "), "a call tree has calls in it");
    }

    #[test]
    fn the_export_is_the_view_not_the_graph() {
        let mut core = rooted_at("main");
        let whole = core.call_graph.node_count();

        let shallow = dot(&core).matches("[label=").count();
        core.dispatch(crate::Command::SetGraphDepth(crate::layout::MAX_DEPTH));
        let deep = dot(&core).matches("[label=").count();

        assert!(
            shallow < deep,
            "the depth limit has to reach the file: {shallow} vs {deep}"
        );
        assert!(
            deep <= whole,
            "and nothing outside the root's reach gets in"
        );
    }

    #[test]
    fn collapsing_a_node_drops_the_edges_leaving_it() {
        let mut core = rooted_at("main");
        let out = dot(&core);

        // `main`'s own first callee: collapsing it must cost the file at least
        // the edges out of it, the same way it costs the canvas them.
        let main = core.workspaces.current().unwrap().root_symbol.unwrap();
        let callee = core.call_graph.get_callees(&main)[0].id;
        core.dispatch(crate::Command::ToggleCollapsed(callee));
        let collapsed = dot(&core);

        assert!(
            collapsed.matches(" -> ").count() < out.matches(" -> ").count(),
            "a collapse that changes nothing in the file is a collapse the \
             reader cannot see"
        );
        assert!(
            collapsed.contains("rounded,dashed"),
            "and the node it left behind says it is hiding something"
        );
    }

    #[test]
    fn a_node_hiding_children_is_counted() {
        // Depth 1 puts every one of main's callees on screen with their own
        // callees left off, so the `+N` has to appear somewhere.
        let mut core = rooted_at("main");
        core.dispatch(crate::Command::SetGraphDepth(1));

        let out = dot(&core);
        assert!(
            out.contains("  +"),
            "a picture that stops short has to say so; got:\n{out}"
        );
    }

    #[test]
    fn two_exports_of_one_graph_are_byte_identical() {
        let core = rooted_at("main");
        assert_eq!(
            dot(&core),
            dot(&core),
            "layout and graph are both HashMaps; without an imposed order no \
             two exports of the same project would diff cleanly"
        );
    }

    #[test]
    fn a_cluster_map_exports_one_box_per_cluster_holding_its_functions() {
        let core = CoreApp::new(create_demo_call_graph());
        // A fresh workspace opens on the whole-program map.
        assert_eq!(
            core.workspaces.current().unwrap().view,
            ViewMode::ClusterMap
        );

        let out = dot(&core);
        let ws = core.workspaces.current().unwrap();
        let boxes = core.clustering_for(ws).level(core.map_level_for(ws)).len();

        assert_eq!(
            out.matches("subgraph cluster_").count(),
            boxes,
            "every cluster on the rung is a DOT cluster subgraph; got:\n{out}"
        );
        // The canvas draws a box as one node; the file opens it, which is the
        // whole reason the format is DOT.
        assert!(
            out.contains("tooltip=\"crc32_update\\n"),
            "a box holds its member functions, not a placeholder; got:\n{out}"
        );
        assert!(
            !out.contains("\\n+"),
            "and so nothing stands in for functions it is not showing"
        );
    }

    #[test]
    fn zooming_in_exports_the_rung_that_is_showing() {
        // The rung is the size control: the same functions, divided more finely.
        let mut core = CoreApp::new(create_demo_call_graph());
        let coarse = dot(&core);

        core.dispatch(crate::Command::SetMapLevel(0));
        let fine = dot(&core);

        let count = |out: &str| out.matches("subgraph cluster_").count();
        assert!(
            count(&fine) > count(&coarse),
            "the finest rung has more boxes than the coarsest: {} vs {}",
            count(&fine),
            count(&coarse)
        );
        let functions = |out: &str| out.matches("tooltip=").count();
        assert_eq!(
            functions(&fine),
            functions(&coarse),
            "and every rung covers the same functions"
        );
    }

    #[test]
    fn the_map_export_names_the_rung_and_the_method() {
        // A map of the same project at two zoom levels exports two different
        // pictures, and a reader opening the file months later has nothing else
        // to tell them which one they are looking at.
        let core = CoreApp::new(create_demo_call_graph());
        let out = dot(&core);
        assert!(
            out.contains("Modularity"),
            "the file says how it was grouped; got:\n{out}"
        );
        assert!(
            out.contains("rung "),
            "and which rung of the ladder it is; got:\n{out}"
        );
    }

    #[test]
    fn a_map_scoped_to_a_function_exports_that_map() {
        let mut core = CoreApp::new(create_demo_call_graph());
        let main = core.call_graph.find_function_by_name("main").unwrap().id;
        core.dispatch(crate::Command::SetMapScope(Some(main)));
        // The scoped clustering is computed off the tick loop.
        for _ in 0..50 {
            core.tick();
            if core.workspaces.current().unwrap().map_scope.is_some()
                && !core
                    .clustering_for(core.workspaces.current().unwrap())
                    .is_empty()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let out = export(&core, ExportFormat::Dot).expect("main has a neighbourhood");
        assert_eq!(
            out.filename, "main-clusters.dot",
            "a scoped map and the whole project's must not overwrite each other"
        );
        assert!(
            out.contents.contains("main — neighbourhood"),
            "the two look identical otherwise, so the file has to say which it is"
        );
    }

    #[test]
    fn every_exported_call_runs_between_two_exported_functions() {
        // The edges are built from the graph rather than from the layout, so
        // nothing stops them naming a function no box holds — which Graphviz
        // would happily render as a mystery node with no label.
        let core = CoreApp::new(create_demo_call_graph());
        let out = dot(&core);

        let declared: HashSet<&str> = out
            .lines()
            .filter_map(|line| line.trim().strip_suffix(';'))
            .filter(|line| line.contains("[label="))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        for line in out.lines() {
            let Some((left, right)) = line.trim().split_once(" -> ") else {
                continue;
            };
            let target = right.split_whitespace().next().unwrap_or_default();
            assert!(
                declared.contains(left),
                "edge from an undeclared node: {line}"
            );
            assert!(
                declared.contains(target),
                "edge to an undeclared node: {line}"
            );
        }
    }

    #[test]
    fn an_empty_view_exports_nothing() {
        // A rooted workspace with no root: the canvas is blank, and a blank
        // file is worse than being told there is nothing to save.
        let mut core = CoreApp::new(CallGraph::new());
        core.workspaces.current_mut().unwrap().view = ViewMode::Rooted;
        assert!(export(&core, ExportFormat::Dot).is_none());
    }

    #[test]
    fn labels_survive_the_characters_c_plus_plus_puts_in_names() {
        let mut graph = CallGraph::new();
        let caller = FunctionNode::new(
            "operator\"\"_kb".to_string(),
            "ns::operator\"\"_kb".to_string(),
            Location::new("src\\win.cpp".to_string(), 3, 0),
        );
        let callee = FunctionNode::new(
            "f".to_string(),
            "ns::f<int, char*>".to_string(),
            Location::new("src/x.cpp".to_string(), 9, 0),
        );
        let (from, to) = (caller.id, callee.id);
        graph.add_function(caller);
        graph.add_function(callee);
        graph.add_call(from, to, Location::new("src/x.cpp".to_string(), 4, 2));

        let mut core = CoreApp::new(graph);
        core.dispatch(crate::Command::StartGraphFromFunction(from));
        core.workspaces.current_mut().unwrap().view = ViewMode::Rooted;

        let out = export(&core, ExportFormat::Dot).unwrap();
        assert!(out.contents.contains(r#"label="operator\"\"_kb""#));
        assert!(
            out.contents.contains(r"src\\win.cpp"),
            "got:\n{}",
            out.contents
        );
        assert_eq!(
            out.filename, "operator-_kb.dot",
            "a filename keeps none of that"
        );
    }
}
