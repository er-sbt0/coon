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
//! letting the copy drift. So the export carries shape and dash — the parts
//! that mean something — and leaves the colours to whoever renders it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

use crate::app::CoreApp;
use crate::layout::{compute_cluster_layout, compute_layout, LayoutOrientation};
use crate::protocol::layout_options;
use crate::workspace::{CoreWorkspace, ViewMode};

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
    let contents = match ws.view() {
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
    let root = ws.root_symbol()?;
    let graph = &core.call_graph;

    // The same call that builds the canvas, so the file holds the subgraph the
    // user is looking at rather than a second interpretation of the same
    // settings. Its keys are exactly the visible nodes.
    let positions = compute_layout(graph, Some(root), ws.direction(), &layout_options(core));
    if positions.is_empty() {
        return None;
    }

    // `positions` is a HashMap, so an order has to be imposed or the same graph
    // exports to a different file every time and no two are diffable.
    let mut visible: Vec<SymbolId> = positions.keys().copied().collect();
    visible.sort_by_cached_key(|id| sort_key(graph, id));
    let index_of: HashMap<SymbolId, usize> =
        visible.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let mut out = String::new();
    let root_label = graph
        .get_function(&root)
        .map(|f| f.qualified_name.as_str())
        .unwrap_or("unknown");
    let direction = match ws.direction() {
        CallDirection::Outgoing => "callees",
        CallDirection::Incoming => "callers",
    };

    header(
        &mut out,
        ws.orientation(),
        &format!(
            "{root_label} — {direction}, {} levels, {} functions",
            ws.depth(),
            visible.len()
        ),
        false,
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
        let parent = match ws.direction() {
            CallDirection::Outgoing => edge.caller,
            CallDirection::Incoming => edge.callee,
        };
        if ws.collapsed().contains(&parent) {
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
    let children = match ws.direction() {
        CallDirection::Outgoing => graph.callee_ids(id),
        CallDirection::Incoming => graph.caller_ids(id),
    };
    if children.is_empty() {
        return 0;
    }
    if ws.collapsed().contains(id) {
        return children.len();
    }
    children
        .iter()
        .filter(|child| !visible.contains_key(*child))
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
        core.map_frame(),
        core.map_level_for(ws),
        ws.direction(),
        ws.orientation(),
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
    let scope = match ws
        .map_scope()
        .and_then(|s| core.call_graph.get_function(&s))
    {
        Some(func) => format!("{} — neighbourhood", func.qualified_name),
        None => "Whole project".to_string(),
    };
    // The grain goes in the title beside the method, because the same method
    // at a different γ or inflation is a different map and the file is the
    // only thing left saying which one this was.
    let settings = ws.cluster_settings();
    header(
        &mut out,
        ws.orientation(),
        &format!(
            "Cluster map: {scope}, {} clusters ({} at {} {}, rung {} of {})",
            boxes.len(),
            settings.method().label(),
            settings.method().grain_spec().label,
            settings.grain(),
            core.map_level_for(ws) + 1,
            clustering.level_count(),
        ),
        true,
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
    // Every line at one width, with the count of calls in the tooltip rather
    // than in the ink. The pen used to scale with the count on a log curve, to
    // match a client that scaled its own; the client stopped (8408323) because
    // a weighted map read as a different kind of picture from the graph it
    // summarises. That argument is stronger here, not weaker: these are
    // function-to-function calls, the very edges `rooted_dot` writes plain, so
    // the variance made two exports of one project two kinds of drawing. And
    // the count is not lost for want of a hover — DOT is text, so it is legible
    // in the source, becomes a tooltip in a rendered SVG, and comes back as an
    // attribute through `networkx` or `gvpr`, which a line width never did.
    //
    // Calls that stay inside a box are written one per function pair, since
    // opening the box is what the file is for. Calls that cross between two
    // boxes are folded into one line per pair of boxes, drawn border to
    // border, because that is the one line the canvas draws: `aggregate_calls`
    // in `layout` sums the calls between two boxes into one edge, and an
    // export of that map with thirty lines fanning between the same two boxes
    // was a drawing of something the screen never showed. The line is anchored
    // on the pair of functions that call each other the most across it, so
    // `dot` routes it where the traffic is, and the count it carries is the
    // sum. What this gives up is the function-level record of who calls whom
    // across a border — a tool reading the file back gets the pair of boxes
    // and the total, not the thirty calls.
    let box_of: HashMap<SymbolId, usize> = members_of
        .iter()
        .enumerate()
        .flat_map(|(index, members)| members.iter().map(move |m| (*m, index)))
        .collect();
    // Per ordered pair of boxes: the calls crossing it, and the heaviest
    // function pair among them. `call_edges` is sorted by name, so among
    // equally heavy pairs the first by name is the anchor.
    struct Crossing {
        total: usize,
        anchor: (SymbolId, SymbolId, usize),
    }
    let mut between: BTreeMap<(usize, usize), Crossing> = BTreeMap::new();
    for edge in call_edges(&core.call_graph, &name_of, ws.direction()) {
        let (Some(&from), Some(&to)) = (box_of.get(&edge.0), box_of.get(&edge.1)) else {
            continue;
        };
        if from != to {
            let crossing = between.entry((from, to)).or_insert(Crossing {
                total: 0,
                anchor: edge,
            });
            crossing.total += edge.2;
            if edge.2 > crossing.anchor.2 {
                crossing.anchor = edge;
            }
            continue;
        }
        writeln!(
            &mut out,
            "  {} -> {} [tooltip={}];",
            name_of[&edge.0],
            name_of[&edge.1],
            quote(&count_of("call", edge.2))
        )
        .ok();
    }
    for ((from, to), crossing) in &between {
        writeln!(
            &mut out,
            "  {} -> {} [ltail=cluster_{from}, lhead=cluster_{to}, tooltip={}];",
            name_of[&crossing.anchor.0],
            name_of[&crossing.anchor.1],
            quote(&count_of("call", crossing.total))
        )
        .ok();
    }

    out.push_str("}\n");
    Some(out)
}

/// Every call between two exported functions, as (from, to, how many), sorted.
///
/// [`CallGraph::edges`] yields exactly this multiplicity — one entry per call
/// site — and `rooted_dot` above walks it. This one walks the functions
/// instead, the way `logic::cluster` and `layout` do, because `exported` is a
/// rung of a clustering rather than the whole graph: a map scoped to one
/// function's neighbourhood holds a few dozen symbols out of a project's
/// thousands, and `callee_ids` per exported function then costs the node count
/// plus those functions' own calls, where `edges()` would read every call site
/// in the project to find the few that land inside.
///
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
        for callee in graph.callee_ids(caller) {
            if !exported.contains_key(callee) || callee == caller {
                continue;
            }
            let pair = match direction {
                CallDirection::Outgoing => (*caller, *callee),
                CallDirection::Incoming => (*callee, *caller),
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

/// `compound` lets an edge end on a cluster's border (`ltail`/`lhead`)
/// rather than on a node inside it, which is how the map draws one line
/// between two boxes; a rooted export has no clusters and leaves it off.
fn header(out: &mut String, orientation: LayoutOrientation, label: &str, compound: bool) {
    let rankdir = match orientation {
        LayoutOrientation::LeftRight => "LR",
        LayoutOrientation::TopDown => "TB",
    };
    out.push_str("// Exported by COON. Render with: dot -Tsvg <this file> -o graph.svg\n");
    out.push_str("digraph coon {\n");
    writeln!(
        out,
        "  graph [rankdir={rankdir}, labelloc=\"t\", label={}, fontname=\"Helvetica\", fontsize=12{}];",
        quote(label),
        if compound { ", compound=true" } else { "" }
    )
    .ok();
    out.push_str("  node [shape=box, style=rounded, fontname=\"Helvetica\", fontsize=11];\n");
    out.push_str("  edge [arrowsize=0.7];\n\n");
}

/// Sort functions into an order two runs agree on. The name first so the file
/// reads sensibly, the location and then the id to break the ties C++ hands out
/// freely — jemalloc has nine distinct `thd_start`s.
///
/// It borrows out of the graph rather than cloning two `String`s, and the
/// caller reaches it through `sort_by_cached_key`, which asks for the key once
/// per function instead of once per comparison. `&str` compares byte for byte
/// exactly as `String` did, so the order is the same one.
fn sort_key<'a>(graph: &'a CallGraph, id: &SymbolId) -> (&'a str, &'a str, u32, uuid::Uuid) {
    match graph.get_function(id) {
        Some(func) => (
            func.qualified_name.as_str(),
            func.definition_location.file_path.as_str(),
            func.definition_location.line,
            id.0,
        ),
        None => ("", "", 0, id.0),
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
    let subject = match ws.view() {
        ViewMode::ClusterMap => match ws
            .map_scope()
            .and_then(|s| core.call_graph.get_function(&s))
        {
            Some(func) => format!("{}-clusters", func.name),
            None => "clusters".to_string(),
        },
        ViewMode::Rooted => ws
            .root_symbol()
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
    // One pass: a run of illegal characters becomes a single `-`, because
    // `operator<<` should read `operator-` and not `operator---`. Collapsing
    // afterwards means rescanning and reallocating the whole string once per
    // doubled dash, which for `std::map<std::string, int>::at` is several
    // passes to reach a fixed point the writer already knew.
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "graph".to_string()
    } else {
        trimmed.to_string()
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
        ws.set_root(id);
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
        let main = core.workspaces.current().unwrap().root_symbol().unwrap();
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

    /// The order is not merely deterministic, it is a *particular* order —
    /// qualified name, then where the definition is — and the file is diffed
    /// against the last one somebody kept. Pinning the whole list is what
    /// catches a cheaper sort that is stable but no longer alphabetical.
    #[test]
    fn the_export_lists_functions_in_name_order() {
        let mut core = rooted_at("main");
        core.dispatch(crate::Command::SetGraphDepth(crate::layout::MAX_DEPTH));

        assert_eq!(
            labels(&dot(&core)),
            vec![
                "buf_append",
                "buf_free",
                "buf_init",
                "buf_reserve",
                "config_free",
                "config_get_int",
                "config_get_str",
                "config_load",
                "config_parse_line",
                "conn_close",
                "crc32_update",
                "handle_client",
                "hash_key",
                "index_insert",
                "index_lookup",
                "index_rehash",
                "index_remove",
                "install_signal_handlers",
                "log_close",
                "log_init",
                "log_rotate",
                "log_write",
                "main",
                "oom_die",
                "parse_args",
                "parse_integer",
                "parse_request",
                "parse_token",
                "print_usage",
                "request_free",
                "response_write_error",
                "response_write_ok",
                "response_write_value",
                "run_server",
                "server_accept_loop",
                "server_start",
                "server_stop",
                "shutdown_gracefully",
                "socket_accept",
                "socket_close",
                "socket_listen",
                "socket_read",
                "socket_set_nonblocking",
                "socket_write",
                "store_close",
                "store_compact",
                "store_delete",
                "store_get",
                "store_open",
                "store_put",
                "wal_append",
                "wal_replay",
                "wal_sync",
                "wal_truncate",
                "xmalloc",
                "xrealloc",
            ]
        );
    }

    /// C++ hands out the same qualified name freely, so the order past it is
    /// load-bearing too: file, then line. Only `name` reaches the label, so
    /// three `ns::dup`s are told apart by the tooltip.
    #[test]
    fn functions_sharing_a_name_are_ordered_by_where_they_are_defined() {
        let mut graph = CallGraph::new();
        let root = FunctionNode::new(
            "aaa_root".to_string(),
            "aaa_root".to_string(),
            Location::new("root.c".to_string(), 1, 0),
        );
        let from = root.id;
        graph.add_function(root);
        for (file, line) in [("b.c", 1), ("a.c", 9), ("a.c", 2)] {
            let dup = FunctionNode::new(
                "dup".to_string(),
                "ns::dup".to_string(),
                Location::new(file.to_string(), line, 0),
            );
            let to = dup.id;
            graph.add_function(dup);
            graph.add_call(from, to, Location::new("root.c".to_string(), 2, 0));
        }

        let mut core = CoreApp::new(graph);
        core.dispatch(crate::Command::StartGraphFromFunction(from));
        core.workspaces
            .current_mut()
            .unwrap()
            .set_view(ViewMode::Rooted);

        assert_eq!(
            tooltips(&dot(&core)),
            vec![
                "aaa_root\\nroot.c:1",
                "ns::dup\\na.c:2",
                "ns::dup\\na.c:9",
                "ns::dup\\nb.c:1",
            ]
        );
    }

    /// The `label=` of every node declaration, in the order the file declares
    /// them — which is the order `rooted_dot` sorted the visible symbols into.
    fn labels(out: &str) -> Vec<&str> {
        field(out, "[label=\"")
    }

    fn tooltips(out: &str) -> Vec<&str> {
        field(out, ", tooltip=\"")
    }

    fn field<'a>(out: &'a str, marker: &str) -> Vec<&'a str> {
        out.lines()
            .filter_map(|line| line.trim().strip_prefix('n'))
            .filter_map(|line| line.split_once(marker))
            .map(|(_, rest)| rest.split('"').next().unwrap_or_default())
            .collect()
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
            core.workspaces.current().unwrap().view(),
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
        let functions = |out: &str| out.matches("[label=").count();
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

    /// What a cluster-map edge stands for is written in words, not drawn as
    /// ink. The pen width used to scale with the count to match a client that
    /// has since gone back to one width for every edge, and these are the same
    /// function-to-function calls a rooted export writes plain.
    #[test]
    fn a_map_edge_carries_its_count_in_the_tooltip_not_in_its_width() {
        let core = CoreApp::new(create_demo_call_graph());
        let out = dot(&core);

        assert!(
            !out.contains("penwidth"),
            "a weighted line makes the map a different kind of drawing from a \
             rooted export of the same calls; got:\n{out}"
        );
        assert!(
            out.contains("[tooltip=\"1 call\"]"),
            "and the count it used to carry is still on the edge; got:\n{out}"
        );

        // A pair called twice, so the tooltip is reporting a number rather
        // than printing a constant.
        let mut graph = CallGraph::new();
        let caller = FunctionNode::new(
            "a".to_string(),
            "a".to_string(),
            Location::new("a.c".to_string(), 1, 0),
        );
        let callee = FunctionNode::new(
            "b".to_string(),
            "b".to_string(),
            Location::new("b.c".to_string(), 1, 0),
        );
        let (from, to) = (caller.id, callee.id);
        graph.add_function(caller);
        graph.add_function(callee);
        graph.add_call(from, to, Location::new("a.c".to_string(), 2, 0));
        graph.add_call(from, to, Location::new("a.c".to_string(), 3, 0));

        let twice = dot(&CoreApp::new(graph));
        assert!(
            twice.contains("tooltip=\"2 calls\"]"),
            "two call sites, one line, and the file says which; got:\n{twice}"
        );
        assert!(!twice.contains("penwidth"), "still no ink for it:\n{twice}");
    }

    #[test]
    fn a_map_scoped_to_a_function_exports_that_map() {
        let mut core = CoreApp::new(create_demo_call_graph());
        let main = core.call_graph.find_function_by_name("main").unwrap().id;
        core.dispatch(crate::Command::SetMapScope(Some(main)));
        // The scoped clustering is computed off the tick loop.
        for _ in 0..50 {
            core.tick();
            if core.workspaces.current().unwrap().map_scope().is_some()
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

    /// The map draws one line between two boxes however many calls cross
    /// between them, and so does its export: every plain line stays inside
    /// one box, and every line between boxes is border to border and unique
    /// to its pair.
    #[test]
    fn two_boxes_share_one_line_however_many_calls_cross() {
        let core = CoreApp::new(create_demo_call_graph());
        let out = dot(&core);
        assert!(
            out.contains("compound=true"),
            "border-to-border edges need it"
        );

        // Which cluster subgraph each node is declared in.
        let mut box_of: HashMap<&str, usize> = HashMap::new();
        let mut current = None;
        for line in out.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("subgraph cluster_") {
                current = rest.split_whitespace().next().and_then(|n| n.parse().ok());
            } else if line == "}" {
                current = None;
            } else if let (Some(id), Some(index)) = (
                line.split_whitespace()
                    .next()
                    .filter(|_| line.contains("[label=")),
                current,
            ) {
                box_of.insert(id, index);
            }
        }

        let mut pairs: HashSet<(usize, usize)> = HashSet::new();
        let mut crossing = 0;
        for line in out.lines() {
            let Some((left, right)) = line.trim().split_once(" -> ") else {
                continue;
            };
            let target = right.split_whitespace().next().unwrap();
            let (from, to) = (box_of[left], box_of[target]);
            if right.contains("ltail=") {
                crossing += 1;
                assert!(
                    right.contains(&format!("ltail=cluster_{from}, lhead=cluster_{to}"))
                        && from != to,
                    "a border line is anchored in the boxes it names: {line}"
                );
                assert!(
                    pairs.insert((from, to)),
                    "two lines between the same pair of boxes: {line}"
                );
            } else {
                assert_eq!(from, to, "a plain line crosses between boxes: {line}");
            }
        }
        assert!(
            crossing > 0,
            "the demo's boxes call each other; got:\n{out}"
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
        core.workspaces
            .current_mut()
            .unwrap()
            .set_view(ViewMode::Rooted);
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
        core.workspaces
            .current_mut()
            .unwrap()
            .set_view(ViewMode::Rooted);

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

    /// A run of illegal characters is one dash however long it is, and the
    /// dashes a run leaves at either end are not part of the name. The
    /// interesting case is three or more in a row: the collapse has to reach a
    /// fixed point, not merely halve.
    #[test]
    fn slug_collapses_runs_of_separators() {
        assert_eq!(slug("std::map<int, char>::at"), "std-map-int-char-at");
        assert_eq!(slug("a <<< b"), "a-b");
        assert_eq!(slug("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(slug("<<<>>>"), "graph", "nothing legal left to name it");
        assert_eq!(
            slug("keeps_word.chars"),
            "keeps_word.chars",
            "`_` and `.` are legal in a filename and survive"
        );
    }
}
