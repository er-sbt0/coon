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

use logic::ClusterKey;
use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::layout::{
    compute_cluster_layout, compute_layout, GroupBox, LayoutNodeId, LayoutOrientation,
};
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

fn cluster_dot(core: &CoreApp) -> Option<String> {
    let ws = core.workspaces.current()?;
    let layout = compute_cluster_layout(
        &core.call_graph,
        core.clustering_for(ws),
        &ws.expanded_clusters,
        ws.direction,
        ws.orientation,
    );
    if layout.nodes.is_empty() {
        return None;
    }

    // Every element that got a position, named. Sorted by the id's own string
    // form, which is stable across runs where a HashMap iteration is not.
    let mut drawn: Vec<LayoutNodeId> = layout.nodes.keys().copied().collect();
    drawn.sort_by_key(|id| id.to_string());
    let name_of: HashMap<LayoutNodeId, String> = drawn
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, format!("n{i}")))
        .collect();

    // A meta-node is a closed community drawn as one box; the label and the
    // count come from the layout rather than from the clustering, because the
    // layout is what decided which communities the map shows at all.
    let meta_of: HashMap<ClusterKey, &crate::layout::MetaNode> =
        layout.metas.iter().map(|m| (m.key, m)).collect();
    let group_of: HashMap<ClusterKey, &GroupBox> =
        layout.groups.iter().map(|g| (g.key, g)).collect();

    // Anything listed inside a box is emitted by that box, so the top level is
    // whatever no box claimed.
    let nested: HashSet<LayoutNodeId> = layout
        .groups
        .iter()
        .flat_map(|g| g.members.iter().copied())
        .collect();

    let mut out = String::new();
    let scope = match ws.map_scope.and_then(|s| core.call_graph.get_function(&s)) {
        Some(func) => format!("{} — neighbourhood", func.qualified_name),
        None => "Whole project".to_string(),
    };
    header(
        &mut out,
        ws.orientation,
        &format!(
            "Cluster map: {scope}, {} communities",
            layout.groups.len() + layout.metas.len()
        ),
    );

    let mut next_cluster = 0usize;
    for group in &layout.groups {
        if nested.contains(&LayoutNodeId::Cluster(group.key)) {
            continue;
        }
        emit_group(
            &mut out,
            group,
            &group_of,
            &meta_of,
            &name_of,
            &core.call_graph,
            1,
            &mut next_cluster,
        );
    }
    for id in &drawn {
        if nested.contains(id) {
            continue;
        }
        emit_node(&mut out, *id, &meta_of, &name_of, &core.call_graph, 1);
    }

    out.push('\n');
    for edge in &layout.edges {
        // Both ends are always real nodes, never boxes: `compute_cluster_layout`
        // attributes a call to the innermost *drawn* element, and an open
        // cluster is a box precisely because its contents are drawn instead. So
        // no `lhead`/`ltail` is needed and `compound` stays off.
        let (Some(source), Some(target)) = (name_of.get(&edge.source), name_of.get(&edge.target))
        else {
            continue;
        };
        writeln!(
            out,
            "  {source} -> {target} [penwidth={:.1}, tooltip={}];",
            pen_width(edge.weight),
            quote(&count_of("call", edge.weight))
        )
        .ok();
    }

    out.push_str("}\n");
    Some(out)
}

#[allow(clippy::too_many_arguments)]
fn emit_group(
    out: &mut String,
    group: &GroupBox,
    groups: &HashMap<ClusterKey, &GroupBox>,
    metas: &HashMap<ClusterKey, &crate::layout::MetaNode>,
    names: &HashMap<LayoutNodeId, String>,
    graph: &CallGraph,
    depth: usize,
    next_cluster: &mut usize,
) {
    let pad = "  ".repeat(depth);
    // The `cluster_` prefix is not decoration: Graphviz draws a subgraph as a
    // labelled box only when its name starts with it.
    let id = *next_cluster;
    *next_cluster += 1;

    writeln!(out, "{pad}subgraph cluster_{id} {{").ok();
    writeln!(
        out,
        "{pad}  label={};",
        quote(&format!("{}  ({})", group.label, group.count))
    )
    .ok();
    writeln!(out, "{pad}  style=\"rounded\";").ok();

    for member in &group.members {
        match member {
            // A cluster key in `members` names a nested box exactly when the
            // walk opened it, and a meta-node otherwise — the two lists are
            // disjoint by construction, see `GroupBox::members`.
            LayoutNodeId::Cluster(key) if groups.contains_key(key) => emit_group(
                out,
                groups[key],
                groups,
                metas,
                names,
                graph,
                depth + 1,
                next_cluster,
            ),
            _ => emit_node(out, *member, metas, names, graph, depth + 1),
        }
    }

    writeln!(out, "{pad}}}").ok();
}

fn emit_node(
    out: &mut String,
    id: LayoutNodeId,
    metas: &HashMap<ClusterKey, &crate::layout::MetaNode>,
    names: &HashMap<LayoutNodeId, String>,
    graph: &CallGraph,
    depth: usize,
) {
    let Some(name) = names.get(&id) else {
        return;
    };
    let pad = "  ".repeat(depth);

    match id {
        LayoutNodeId::Symbol(symbol) => {
            let Some(func) = graph.get_function(&symbol) else {
                return;
            };
            let tooltip = format!(
                "{}\n{}:{}",
                func.qualified_name,
                func.definition_location.file_path,
                func.definition_location.line
            );
            writeln!(
                out,
                "{pad}{name} [label={}, tooltip={}];",
                quote(&func.name),
                quote(&tooltip)
            )
            .ok();
        }
        LayoutNodeId::Cluster(key) => {
            let Some(meta) = metas.get(&key) else {
                return;
            };
            // Dashed for the same reason a node hiding callees is: the picture
            // stops here, and the number says how much of it is missing.
            // A real newline: `quote` is what turns it into the `\n` a DOT
            // label breaks on. Writing the escape here would escape the
            // backslash on the way out and put it on the canvas verbatim.
            writeln!(
                out,
                "{pad}{name} [label={}, style=\"rounded,dashed\", tooltip={}];",
                quote(&format!("{}\n+{}", meta.label, meta.count)),
                quote(&count_of("function", meta.count))
            )
            .ok();
        }
    }
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
    fn a_cluster_map_exports_boxes_for_what_is_open_and_nodes_for_what_is_not() {
        let mut core = CoreApp::new(create_demo_call_graph());
        // A fresh workspace opens on the whole-program map with nothing open.
        assert_eq!(
            core.workspaces.current().unwrap().view,
            ViewMode::ClusterMap
        );

        let closed = dot(&core);
        assert!(
            !closed.contains("subgraph cluster_"),
            "nothing is open, so there are no boxes yet"
        );
        assert!(
            closed.contains("\\n+"),
            "every community is a single node carrying its size"
        );

        let key = core.active_clustering().top_level()[0];
        let key = core.active_clustering().cluster(key).unwrap().key;
        core.dispatch(crate::Command::ToggleClusterExpanded(key));

        let opened = dot(&core);
        assert!(
            opened.contains("subgraph cluster_0 {"),
            "an opened community is a DOT cluster subgraph; got:\n{opened}"
        );
    }

    #[test]
    fn cluster_edges_never_point_at_a_box() {
        // Graphviz cannot draw an edge to a subgraph without `compound` and an
        // `lhead`, and this export sets neither — which is only safe because
        // `compute_cluster_layout` attributes every call to a drawn node.
        let mut core = CoreApp::new(create_demo_call_graph());
        let keys: Vec<ClusterKey> = core.active_clustering().clusters().map(|c| c.key).collect();
        for key in keys {
            core.dispatch(crate::Command::ToggleClusterExpanded(key));
        }

        let ws = core.workspaces.current().unwrap();
        let layout = compute_cluster_layout(
            &core.call_graph,
            core.clustering_for(ws),
            &ws.expanded_clusters,
            ws.direction,
            ws.orientation,
        );
        for edge in &layout.edges {
            assert!(
                layout.nodes.contains_key(&edge.source) && layout.nodes.contains_key(&edge.target),
                "an aggregated edge reached something that has no position"
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
