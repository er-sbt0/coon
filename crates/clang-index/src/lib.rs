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
//!
//! A USR names a function, not a body. Within one build a header's inline
//! function is the same code in every TU that includes it, so its body is
//! walked once; but a database is often several builds, and the same function
//! under `-DJEMALLOC_UNIT_TEST` can call what the library build does not. So a
//! body is walked once per *flavour* (see `lsp::compile_commands::FlavourKey`)
//! and the calls of every flavour land on the one node.

pub mod args;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use clang::diagnostic::Severity;
use clang::{Clang, Entity, EntityKind, EntityVisitResult, Index};
use model::{CallGraph, FunctionNode, Location, ParseSummary};

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
    /// Parsed TUs with at least one of those errors — how much of the build is
    /// in doubt, where `errors` may be one header counted once per includer.
    pub tus_with_errors: usize,
    /// Calls whose callee has no definition in the project — libc, the system
    /// headers, or a function the build declares and never defines. Counted per
    /// call site, so a header's call that two flavours both make counts once,
    /// just as it is one edge when its callee is in the project.
    pub external_calls: usize,
    /// Arguments libclang refused, in the order first met: gcc flags clang does
    /// not have. Not counted in `errors`, since the code was read regardless.
    pub rejected_arguments: Vec<String>,
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
/// `defined` holds, per USR, the flavours whose definition of it has been
/// walked: a later TU of one of those skips the body, which is what keeps a
/// header's inline functions from being walked once per includer, while a TU of
/// another flavour walks it again for the calls that build makes.
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
    defined: HashMap<String, Vec<usize>>,
    calls: Vec<Call>,
    /// The flavour of the TU being walked, as an index into the run's flavours.
    flavour: usize,
}

/// Parse every job in order and build the call graph of what they define.
///
/// `root` is the project directory: a function is part of the graph if its
/// definition lies under it, the same rule the clangd backend holds both its
/// discovery and its call replies to. `on_progress(done, total)` is called after
/// each TU.
///
/// The jobs are parsed in the order given, and the first TU to define a USR
/// is the one its node's location comes from, so a sorted job list gives the
/// same graph every run. Its calls are those of the first TU of each flavour
/// to define it — a header's inline function is defined in every TU that
/// includes it, and within one build they are the same code.
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
    let mut tus_with_errors = 0usize;
    // See `args::rejected_argument`. Once refused, an argument is left out of
    // every later TU, so a flag the whole build passes is reported once rather
    // than once per file.
    let mut rejected: Vec<String> = Vec::new();
    let started = Instant::now();
    // Flavours numbered in the order first met, so `Collected::defined` holds
    // an index rather than a copy of the key per function.
    let mut flavours: HashMap<&lsp::compile_commands::FlavourKey, usize> = HashMap::new();

    for (done, job) in jobs.iter().enumerate() {
        let tu_started = Instant::now();
        let arguments: Vec<&String> = job
            .arguments
            .iter()
            .filter(|a| !rejected.contains(a))
            .collect();
        let tu = index
            .parser(&job.file)
            .arguments(&arguments)
            .keep_going(true)
            .parse();
        match tu {
            Ok(tu) => {
                parsed += 1;
                let mut tu_errors = 0usize;
                for diagnostic in tu.get_diagnostics() {
                    if matches!(diagnostic.get_severity(), Severity::Error | Severity::Fatal) {
                        if diagnostic.get_location().get_file_location().file.is_none() {
                            if let Some(argument) =
                                args::rejected_argument(&diagnostic.get_text(), &job.arguments)
                            {
                                log::info!(
                                    "[clang-index] libclang ignores {argument} ({}); \
                                     left out from here on",
                                    diagnostic.get_text()
                                );
                                if !rejected.contains(&argument) {
                                    rejected.push(argument);
                                }
                                continue;
                            }
                        }
                        if tu_errors < LOGGED_ERRORS_PER_TU {
                            log::warn!("[clang-index] {}: {}", job.file.display(), diagnostic);
                        }
                        tu_errors += 1;
                    }
                }
                errors += tu_errors;
                if tu_errors > 0 {
                    tus_with_errors += 1;
                }
                let before = (collected.definitions.len(), collected.calls.len());
                scope.directory = job.directory.clone();
                let next = flavours.len();
                collected.flavour = *flavours.entry(&job.flavour).or_insert(next);
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

    if flavours.len() > 1 {
        let shared = collected.defined.values().filter(|f| f.len() > 1).count();
        log::info!(
            "[clang-index] {} build flavour(s) in one graph: {} function(s) defined under more \
             than one, their calls unioned",
            flavours.len(),
            shared
        );
    }
    let (graph, external_calls) = build_graph(collected);
    log::info!(
        "[clang-index] {} TU(s) parsed ({} failed, {} with {} error diagnostic(s)) in {:.1?}: \
         {} function(s), {} edge(s), {} call(s) out of the project",
        parsed,
        failed,
        tus_with_errors,
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
        tus_with_errors,
        external_calls,
        rejected_arguments: rejected,
    })
}

impl Report {
    /// The per-TU tallies a frontend shows, without the graph.
    pub fn summary(&self) -> ParseSummary {
        ParseSummary {
            parsed: self.parsed,
            failed: self.failed,
            with_errors: self.tus_with_errors,
            external_calls: self.external_calls,
            rejected_arguments: self.rejected_arguments.clone(),
        }
    }
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
    // A call site met twice — a header body walked under two flavours — is one
    // edge, because `add_call` keys an edge by its site; the external count is
    // held to the same rule, so it does not grow with the number of flavours.
    let mut external = HashSet::new();
    for call in collected.calls {
        match (ids.get(&call.caller), ids.get(&call.callee)) {
            (Some(&caller), Some(&callee)) => graph.add_call(caller, callee, call.location),
            _ => {
                external.insert((
                    call.caller,
                    call.callee,
                    call.location.file_path,
                    call.location.line,
                    call.location.column,
                ));
            }
        }
    }
    (graph, external.len())
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

/// Record a definition and every call in its body, unless an earlier TU of the
/// same flavour has already defined the same USR. An earlier TU of another
/// flavour only means the definition is known: the body is still walked, since
/// it may be different code there.
fn record_definition(entity: Entity<'_>, scope: &mut Scope, out: &mut Collected) {
    let Some(usr) = entity.get_usr().map(|usr| usr.0) else {
        return;
    };
    let known = match out.defined.get(&usr) {
        Some(flavours) if flavours.contains(&out.flavour) => return,
        Some(_) => true,
        None => false,
    };
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

    if known {
        if let Some(flavours) = out.defined.get_mut(&usr) {
            flavours.push(out.flavour);
        }
        return;
    }
    out.defined.insert(usr.clone(), vec![out.flavour]);
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
pub(crate) fn normalise(path: &Path) -> PathBuf {
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
