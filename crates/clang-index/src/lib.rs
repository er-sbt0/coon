//! A call graph built by parsing every translation unit with libclang.
//!
//! The clangd backend asks a server that keeps an index of the whole project
//! resident, and on a large project that index — plus a preamble per open
//! document — is where the memory goes. This one keeps nothing resident: it
//! parses one translation unit at a time, records the functions defined in it
//! and the calls inside their bodies, and drops the AST before the next.
//!
//! libclang has no index that spans translation units, so a TU only knows the
//! calls written in it. That is enough for a function's *callees* — they are
//! all in the body of its definition — but a function's *callers* are spread
//! over every TU that calls it, so the graph is only right once every TU of the
//! build has been parsed. Which is why this is eager, and why the graph is
//! handed over whole rather than streamed: half of it would show a caller set
//! that depended on how far the parse had got.
//!
//! Functions are matched across TUs by clang's USR, which is stable across TUs,
//! tells overloads apart, and makes a `static` function's identity include its
//! file. The `SymbolId` the rest of the app sees is still built the usual way
//! from the definition, by [`FunctionNode::new`].

pub mod args;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use clang::diagnostic::Severity;
use clang::{Clang, Entity, EntityKind, EntityVisitResult, Index};
use model::{CallGraph, FunctionNode, Location};

pub use args::Job;

/// How many of a TU's error diagnostics go to the log. Enough to see what is
/// wrong with the arguments, not so many that one broken TU floods it.
const LOGGED_ERRORS_PER_TU: usize = 3;

/// What a run produced, and how much of the project it managed to read.
pub struct Report {
    pub graph: CallGraph,
    /// Translation units parsed, including those with errors.
    pub parsed: usize,
    /// Translation units libclang could not parse at all.
    pub failed: usize,
    /// Error and fatal diagnostics over every parsed TU. Nonzero means some
    /// bodies may have been read incompletely — usually a missing include.
    pub errors: usize,
    /// Calls whose callee has no definition in the project — libc, the system
    /// headers, or a function the build declares and never defines.
    pub external_calls: usize,
}

/// One function definition, as first seen.
struct Definition {
    name: String,
    qualified_name: String,
    location: Location,
}

/// One call, by USR on both ends.
struct Call {
    caller: String,
    callee: String,
    location: Location,
}

/// Everything collected so far, keyed by USR.
///
/// The definitions are a list in the order they were first seen — parse order,
/// then source order within a TU — rather than a map, because the graph is
/// built by walking them, and a `HashMap` walks in a different order every
/// run. That is the order the indexer already promises the graph is decided
/// by (the first TU to define a USR wins), so the build follows it too rather
/// than imposing a second order, by USR or by path, that only the build reads.
#[derive(Default)]
struct Collected {
    definitions: Vec<(String, Definition)>,
    defined: HashSet<String>,
    calls: Vec<Call>,
}

/// Parse every job in order and build the call graph of what they define.
///
/// `root` is the project directory: a function is part of the graph if its
/// definition lies under it, the same rule the clangd backend holds both its
/// discovery and its call replies to. `on_progress(done, total)` is called after
/// each TU.
///
/// The jobs are parsed in the order given, and the first TU to define a USR
/// is the one its node comes from — a header's inline function is defined in
/// every TU that includes it, and within one build they are the same code — so
/// a sorted job list gives the same graph every run.
pub fn index(
    root: &Path,
    jobs: &[Job],
    mut on_progress: impl FnMut(usize, usize),
) -> anyhow::Result<Report> {
    let clang = Clang::new().map_err(|e| anyhow::anyhow!("could not load libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    let mut scope = Scope::new(root);
    let mut collected = Collected::default();
    let mut parsed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    let started = Instant::now();

    for (done, job) in jobs.iter().enumerate() {
        let tu_started = Instant::now();
        let tu = index
            .parser(&job.file)
            .arguments(&job.arguments)
            .keep_going(true)
            .parse();
        match tu {
            Ok(tu) => {
                parsed += 1;
                let mut tu_errors = 0usize;
                for diagnostic in tu.get_diagnostics() {
                    if matches!(diagnostic.get_severity(), Severity::Error | Severity::Fatal) {
                        if tu_errors < LOGGED_ERRORS_PER_TU {
                            log::warn!("[clang-index] {}: {}", job.file.display(), diagnostic);
                        }
                        tu_errors += 1;
                    }
                }
                errors += tu_errors;
                let before = (collected.definitions.len(), collected.calls.len());
                scope.directory = job.directory.clone();
                walk_translation_unit(tu.get_entity(), &mut scope, &mut collected);
                log::debug!(
                    "[clang-index] {} in {:.0?}: {} new function(s), {} call(s), {} error(s)",
                    job.file.display(),
                    tu_started.elapsed(),
                    collected.definitions.len() - before.0,
                    collected.calls.len() - before.1,
                    tu_errors
                );
            }
            Err(e) => {
                failed += 1;
                log::warn!("[clang-index] could not parse {}: {e}", job.file.display());
            }
        }
        on_progress(done + 1, jobs.len());
    }

    let (graph, external_calls) = build_graph(collected);
    log::info!(
        "[clang-index] {} TU(s) parsed ({} failed, {} error diagnostic(s)) in {:.1?}: \
         {} function(s), {} edge(s), {} call(s) out of the project",
        parsed,
        failed,
        errors,
        started.elapsed(),
        graph.node_count(),
        graph.edge_count(),
        external_calls
    );
    Ok(Report {
        graph,
        parsed,
        failed,
        errors,
        external_calls,
    })
}

/// Turn what was collected into a graph, keeping only calls between two
/// functions the project defines.
fn build_graph(collected: Collected) -> (CallGraph, usize) {
    let mut graph = CallGraph::new();
    let mut ids = HashMap::with_capacity(collected.definitions.len());
    for (usr, definition) in collected.definitions {
        let id = graph.add_function(FunctionNode::new(
            definition.name,
            definition.qualified_name,
            definition.location,
        ));
        ids.insert(usr, id);
    }
    let mut external = 0usize;
    for call in collected.calls {
        match (ids.get(&call.caller), ids.get(&call.callee)) {
            (Some(&caller), Some(&callee)) => graph.add_call(caller, callee, call.location),
            _ => external += 1,
        }
    }
    (graph, external)
}

fn is_function(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::FunctionDecl
            | EntityKind::Method
            | EntityKind::Constructor
            | EntityKind::Destructor
            | EntityKind::ConversionFunction
            | EntityKind::FunctionTemplate
    )
}

/// Declarations that can hold a function definition inside them.
fn is_container(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Namespace
            | EntityKind::LinkageSpec
            | EntityKind::ClassDecl
            | EntityKind::StructDecl
            | EntityKind::UnionDecl
            | EntityKind::ClassTemplate
            | EntityKind::ClassTemplatePartialSpecialization
    )
}

fn walk_translation_unit(root: Entity<'_>, scope: &mut Scope, out: &mut Collected) {
    root.visit_children(|entity, _parent| {
        if entity
            .get_location()
            .is_some_and(|location| location.is_in_system_header())
        {
            return EntityVisitResult::Continue;
        }
        let kind = entity.get_kind();
        if is_function(kind) {
            if entity.is_definition() {
                record_definition(entity, scope, out);
            }
            EntityVisitResult::Continue
        } else if is_container(kind) {
            EntityVisitResult::Recurse
        } else {
            EntityVisitResult::Continue
        }
    });
}

/// Record a definition and every call in its body, unless an earlier TU has
/// already defined the same USR.
fn record_definition(entity: Entity<'_>, scope: &mut Scope, out: &mut Collected) {
    let Some(usr) = entity.get_usr().map(|usr| usr.0) else {
        return;
    };
    if out.defined.contains(&usr) {
        return;
    }
    let Some(location) = location_of(entity, scope) else {
        // Defined outside the project.
        return;
    };
    let Some(name) = entity.get_name() else {
        return;
    };
    let qualified_name = qualify(entity, &name);

    // A *reference* to a function, not only a call of it: that is what clangd's
    // `outgoingCalls` reports, so a function handed over as a callback —
    // `bus_subscribe(bus, on_error)` — is an edge in both backends. A call's
    // callee is itself a reference, so calls come along with it. A C++
    // construction is the one call with no reference under it, and is taken
    // from the `CallExpr` instead; a call inside a template whose arguments
    // depend on its parameters is not resolved yet, and names every overload
    // it could be.
    entity.visit_children(|child, _parent| {
        let callees = match child.get_kind() {
            EntityKind::DeclRefExpr | EntityKind::MemberRefExpr => {
                child.get_reference().into_iter().collect()
            }
            EntityKind::CallExpr => child
                .get_reference()
                .filter(|callee| callee.get_kind() == EntityKind::Constructor)
                .into_iter()
                .collect(),
            EntityKind::OverloadedDeclRef => {
                child.get_overloaded_declarations().unwrap_or_default()
            }
            _ => Vec::new(),
        };
        for callee in callees {
            if !is_function(callee.get_kind()) {
                continue;
            }
            // A call to `newImpl<false>` references the specialization, whose
            // USR is not the template's — and the template is the definition
            // that was recorded.
            let callee = callee.get_template().unwrap_or(callee);
            let Some(callee) = callee.get_usr() else {
                continue;
            };
            if let Some(location) = location_of(child, scope) {
                out.calls.push(Call {
                    caller: usr.clone(),
                    callee: callee.0,
                    location,
                });
            }
        }
        EntityVisitResult::Recurse
    });

    out.defined.insert(usr.clone());
    out.definitions.push((
        usr,
        Definition {
            name,
            qualified_name,
            location,
        },
    ));
}

/// `ns::Class::name` for C++, `name` for C — the same shape the clangd backend
/// builds from a symbol's container.
fn qualify(entity: Entity<'_>, name: &str) -> String {
    let mut parts = vec![name.to_string()];
    let mut parent = entity.get_semantic_parent();
    while let Some(p) = parent {
        if p.get_kind() == EntityKind::TranslationUnit {
            break;
        }
        if is_container(p.get_kind()) {
            if let Some(name) = p.get_name() {
                parts.push(name);
            }
        }
        parent = p.get_semantic_parent();
    }
    parts.reverse();
    parts.join("::")
}

/// Where `entity` is, 1-based as `model::Location` is and as libclang already
/// counts, or `None` if that is outside the project.
///
/// The file location rather than the spelling location: inside a macro
/// expansion that is where the macro was used, which is the line a reader looks
/// for the call on.
fn location_of(entity: Entity<'_>, scope: &mut Scope) -> Option<Location> {
    let location = entity.get_location()?.get_file_location();
    let file = location.file?;
    let file_path = scope.resolve(&file.get_path())?;
    Some(Location {
        file_path,
        line: location.line,
        column: location.column,
        length: None,
    })
}

/// The project root, and a cache of which files libclang names lie under it.
///
/// A path is canonicalised once, the first time it is seen: a TU names the same
/// few hundred headers over and over, and the database's `-I../include` spells
/// them through a `..` the root does not have.
///
/// libclang names a header the way the include path spelled it, so one found
/// through `-Iinclude` comes back as `include/…` — relative to the compile
/// command's directory, not to this process. Resolving it against the process
/// put every one of jemalloc's ~700 header-defined functions outside the
/// project.
struct Scope {
    root: PathBuf,
    /// The directory of the TU being walked, which a relative path is under.
    directory: PathBuf,
    files: HashMap<PathBuf, Option<String>>,
}

impl Scope {
    fn new(root: &Path) -> Self {
        Self {
            root: root.canonicalize().unwrap_or_else(|_| normalise(root)),
            directory: PathBuf::new(),
            files: HashMap::new(),
        }
    }

    /// The path to report for `path`, if it is project code.
    fn resolve(&mut self, path: &Path) -> Option<String> {
        let joined;
        let path = if path.is_relative() {
            joined = self.directory.join(path);
            joined.as_path()
        } else {
            path
        };
        if let Some(known) = self.files.get(path) {
            return known.clone();
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| normalise(path));
        let resolved = canonical
            .starts_with(&self.root)
            .then(|| canonical.to_string_lossy().into_owned());
        self.files.insert(path.to_path_buf(), resolved.clone());
        resolved
    }
}

/// `path` with `.` and `..` resolved lexically, for a file that cannot be
/// canonicalised.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
