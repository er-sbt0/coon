//! A call graph built by parsing every translation unit with libclang.
//!
//! The clangd backend asks a server that keeps an index of the whole project
//! resident, and on a large project that index — plus a preamble per open
//! document — is where the memory goes. This one keeps nothing resident: a
//! parser reads one translation unit at a time, records the functions defined
//! in it and the calls inside their bodies, and drops the AST before the next.
//! The parsers are worker processes (see `pool`), one AST each, so a crash or
//! a hang in libclang costs one TU rather than the app; and since the result
//! of every TU is merged in job order once all are in (see `merge`), how many
//! there are and which finishes first change nothing in the graph.
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
//! body is walked once per *flavour* (see `compdb::FlavourKey`)
//! and the calls of every flavour land on the one node.

pub mod args;
mod pool;
pub mod worker;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use clang::diagnostic::Severity;
use clang::{Clang, Entity, EntityKind, EntityVisitResult, Index};
use model::{CallGraph, FunctionNode, Location, ParseSummary};
use serde::{Deserialize, Serialize};

pub use args::Job;
pub use pool::{Cancel, Options, WorkerCommand};

/// How many of a TU's error diagnostics go to the log. Enough to see what is
/// wrong with the arguments, not so many that one broken TU floods it.
const LOGGED_ERRORS_PER_TU: usize = 3;

/// What a run produced, and how much of the project it managed to read.
pub struct Report {
    pub graph: CallGraph,
    /// Translation units parsed, including those with errors.
    pub parsed: usize,
    /// Translation units libclang could not parse at all — and, out of
    /// process, those whose worker crashed or ran out of time on them.
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

/// One translation unit to parse, as handed to whoever parses it: a [`Job`]
/// with the arguments already refused left out, and its flavour as a number.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Request {
    /// The job's place in the run, which is what results are merged by.
    index: usize,
    file: PathBuf,
    directory: PathBuf,
    arguments: Vec<String>,
    /// The job's flavour, numbered in the order the run first meets each one.
    flavour: usize,
}

/// What parsing one translation unit produced, before it is merged with the
/// rest. This is what a worker process sends back, so it is plain data.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Parsed(Parsed),
    /// Why the TU was not read: libclang's error, or what became of the worker.
    Failed(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Parsed {
    errors: usize,
    /// The first [`LOGGED_ERRORS_PER_TU`] of them, for the log.
    error_samples: Vec<String>,
    /// Each argument libclang refused, with the diagnostic that said so.
    rejected: Vec<(String, String)>,
    /// The definitions whose bodies this parse walked, in source order.
    walked: Vec<Walked>,
    elapsed_ms: u64,
}

/// One definition and the calls in its body.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Walked {
    usr: String,
    name: String,
    qualified_name: String,
    location: Location,
    /// Callee USR and where the call is written, in source order.
    calls: Vec<(String, Location)>,
}

/// Parse every job in order, in this process, and build the call graph of what
/// they define.
///
/// `root` is the project directory: a function is part of the graph if its
/// definition lies under it, the same rule the clangd backend holds both its
/// discovery and its call replies to. `on_progress(done, total)` is called after
/// each TU.
///
/// The first TU in job order to define a USR is the one its node's location
/// comes from, so a sorted job list gives the same graph every run. Its calls
/// are those of the first TU of each flavour to define it — a header's inline
/// function is defined in every TU that includes it, and within one build they
/// are the same code.
///
/// This is [`index_with`] at zero workers: what the tests use, since a test
/// binary cannot be a worker, and what a run falls back to when no worker
/// starts.
pub fn index(
    root: &Path,
    jobs: &[Job],
    on_progress: impl FnMut(usize, usize),
) -> anyhow::Result<Report> {
    let options = Options {
        workers: 0,
        ..Options::default()
    };
    index_with(root, jobs, &options, &Cancel::default(), on_progress)
}

/// [`index`], with the parsing spread over `options.workers` worker processes
/// (see [`worker`]), or done in this process at zero.
///
/// The graph does not depend on the worker count or on which TU finishes
/// first: every TU's result is held until the run ends and merged in job
/// order, by the same rule a serial run follows. A worker that crashes or runs
/// past `options.timeout` costs the TU it was on, counted in `failed`, and is
/// replaced. Cancelling `cancel` kills the workers and returns an error.
pub fn index_with(
    root: &Path,
    jobs: &[Job],
    options: &Options,
    cancel: &Cancel,
    mut on_progress: impl FnMut(usize, usize),
) -> anyhow::Result<Report> {
    let started = Instant::now();
    // Flavours numbered in the order first met, so a request carries a number
    // rather than a copy of the key.
    let mut numbers: HashMap<&compdb::FlavourKey, usize> = HashMap::new();
    let flavours: Vec<usize> = jobs
        .iter()
        .map(|job| {
            let next = numbers.len();
            *numbers.entry(&job.flavour).or_insert(next)
        })
        .collect();

    let workers = options.workers.min(jobs.len());
    let outcomes = if workers == 0 {
        None
    } else {
        pool::run(
            root,
            jobs,
            &flavours,
            workers,
            options,
            cancel,
            &mut on_progress,
        )?
    };
    let outcomes = match outcomes {
        Some(outcomes) => outcomes,
        None => in_process(root, jobs, &flavours, cancel, &mut on_progress)?,
    };
    Ok(merge(jobs, &flavours, numbers.len(), outcomes, started))
}

/// Parse every job here, one after another.
fn in_process(
    root: &Path,
    jobs: &[Job],
    flavours: &[usize],
    cancel: &Cancel,
    on_progress: &mut impl FnMut(usize, usize),
) -> anyhow::Result<Vec<Outcome>> {
    let clang = Clang::new().map_err(|e| anyhow::anyhow!("could not load libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    let mut parser = Parser::new(root);
    let mut rejected: Vec<String> = Vec::new();
    let mut outcomes = Vec::with_capacity(jobs.len());
    for (i, job) in jobs.iter().enumerate() {
        if cancel.is_cancelled() {
            anyhow::bail!("the index run was cancelled");
        }
        let outcome = parser.parse(&index, &request(i, job, flavours[i], &rejected));
        if let Outcome::Parsed(parsed) = &outcome {
            for (argument, _) in &parsed.rejected {
                if !rejected.contains(argument) {
                    rejected.push(argument.clone());
                }
            }
        }
        outcomes.push(outcome);
        on_progress(i + 1, jobs.len());
    }
    Ok(outcomes)
}

/// The request for job `i`, without the arguments already refused.
///
/// See `args::rejected_argument`. Once refused, an argument is left out of
/// every TU handed out afterwards, so a flag the whole build passes is reported
/// once rather than once per file. Leaving it out changes nothing but the
/// diagnostic: libclang ignored it anyway.
pub(crate) fn request(i: usize, job: &Job, flavour: usize, rejected: &[String]) -> Request {
    Request {
        index: i,
        file: job.file.clone(),
        directory: job.directory.clone(),
        arguments: job
            .arguments
            .iter()
            .filter(|a| !rejected.contains(a))
            .cloned()
            .collect(),
        flavour,
    }
}

/// Fold every TU's result into one report, in job order.
///
/// This is where the serial rule is applied, whoever did the parsing: a body
/// is kept from the first job of its flavour, in job order, to walk it, and a
/// node from the first job of any flavour. A parser walks a definition the
/// first time *it* meets it in a flavour, so with several parsers one body may
/// be walked by more than one of them; but each is handed jobs in increasing
/// order, so the lowest job to define it is always among those that report it,
/// and the rest are dropped here.
fn merge(
    jobs: &[Job],
    flavours: &[usize],
    flavour_count: usize,
    outcomes: Vec<Outcome>,
    started: Instant,
) -> Report {
    let mut definitions: Vec<(String, Definition)> = Vec::new();
    // Per USR, the flavours whose body of it has been kept.
    let mut defined: HashMap<String, Vec<usize>> = HashMap::new();
    let mut calls: Vec<Call> = Vec::new();
    let mut parsed = 0usize;
    let mut failed = 0usize;
    let mut errors = 0usize;
    let mut tus_with_errors = 0usize;
    let mut rejected: Vec<String> = Vec::new();

    for ((job, &flavour), outcome) in jobs.iter().zip(flavours).zip(outcomes) {
        let result = match outcome {
            Outcome::Parsed(result) => result,
            Outcome::Failed(reason) => {
                failed += 1;
                log::warn!(
                    "[clang-index] could not parse {}: {reason}",
                    job.file.display()
                );
                continue;
            }
        };
        parsed += 1;
        for (argument, text) in result.rejected {
            if !rejected.contains(&argument) {
                log::info!(
                    "[clang-index] libclang ignores {argument} ({text}); left out from here on"
                );
                rejected.push(argument);
            }
        }
        for sample in &result.error_samples {
            log::warn!("[clang-index] {}: {sample}", job.file.display());
        }
        errors += result.errors;
        if result.errors > 0 {
            tus_with_errors += 1;
        }
        let before = (definitions.len(), calls.len());
        for walked in result.walked {
            let kept = defined.entry(walked.usr.clone()).or_default();
            if kept.contains(&flavour) {
                continue;
            }
            let first = kept.is_empty();
            kept.push(flavour);
            calls.extend(walked.calls.into_iter().map(|(callee, location)| Call {
                caller: walked.usr.clone(),
                callee,
                location,
            }));
            if first {
                definitions.push((
                    walked.usr,
                    Definition {
                        name: walked.name,
                        qualified_name: walked.qualified_name,
                        location: walked.location,
                    },
                ));
            }
        }
        log::debug!(
            "[clang-index] {} in {}ms: {} new function(s), {} call(s), {} error(s)",
            job.file.display(),
            result.elapsed_ms,
            definitions.len() - before.0,
            calls.len() - before.1,
            result.errors
        );
    }

    if flavour_count > 1 {
        let shared = defined.values().filter(|f| f.len() > 1).count();
        log::info!(
            "[clang-index] {} build flavour(s) in one graph: {} function(s) defined under more \
             than one, their calls unioned",
            flavour_count,
            shared
        );
    }
    let (graph, external_calls) = build_graph(definitions, calls);
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
    Report {
        graph,
        parsed,
        failed,
        errors,
        tus_with_errors,
        external_calls,
        rejected_arguments: rejected,
    }
}

/// What one parser — this process, or one worker — carries from one TU to the
/// next: where the project is, and which bodies it has already walked.
///
/// `walked` holds, per USR, the flavours whose definition of it this parser
/// has walked: a later TU of one of those skips the body, which is what keeps
/// a header's inline functions from being walked once per includer, while a TU
/// of another flavour walks it again for the calls that build makes.
pub(crate) struct Parser {
    scope: Scope,
    walked: HashMap<String, Vec<usize>>,
}

impl Parser {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            scope: Scope::new(root),
            walked: HashMap::new(),
        }
    }

    pub(crate) fn parse(&mut self, index: &Index<'_>, request: &Request) -> Outcome {
        let started = Instant::now();
        let tu = index
            .parser(&request.file)
            .arguments(&request.arguments)
            .keep_going(true)
            .parse();
        let tu = match tu {
            Ok(tu) => tu,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let mut errors = 0usize;
        let mut error_samples = Vec::new();
        let mut rejected = Vec::new();
        for diagnostic in tu.get_diagnostics() {
            if matches!(diagnostic.get_severity(), Severity::Error | Severity::Fatal) {
                if diagnostic.get_location().get_file_location().file.is_none() {
                    let text = diagnostic.get_text();
                    if let Some(argument) = args::rejected_argument(&text, &request.arguments) {
                        rejected.push((argument, text));
                        continue;
                    }
                }
                if errors < LOGGED_ERRORS_PER_TU {
                    error_samples.push(diagnostic.to_string());
                }
                errors += 1;
            }
        }
        self.scope.directory = request.directory.clone();
        let mut walk = Walk {
            scope: &mut self.scope,
            seen: &mut self.walked,
            flavour: request.flavour,
            out: Vec::new(),
        };
        walk_translation_unit(tu.get_entity(), &mut walk);
        Outcome::Parsed(Parsed {
            errors,
            error_samples,
            rejected,
            walked: walk.out,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// One TU's walk: the parser's state, and what this TU adds.
struct Walk<'a> {
    scope: &'a mut Scope,
    seen: &'a mut HashMap<String, Vec<usize>>,
    /// The flavour of the TU being walked.
    flavour: usize,
    out: Vec<Walked>,
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
fn build_graph(definitions: Vec<(String, Definition)>, calls: Vec<Call>) -> (CallGraph, usize) {
    let mut graph = CallGraph::new();
    let mut ids = HashMap::with_capacity(definitions.len());
    for (usr, definition) in definitions {
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
    for call in calls {
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

fn walk_translation_unit(root: Entity<'_>, walk: &mut Walk<'_>) {
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
                record_definition(entity, walk);
            }
            EntityVisitResult::Continue
        } else if is_container(kind) {
            EntityVisitResult::Recurse
        } else {
            EntityVisitResult::Continue
        }
    });
}

/// Record a definition and every call in its body, unless this parser has
/// already walked the same USR in the same flavour. One walked in another
/// flavour is walked again, since it may be different code there.
fn record_definition(entity: Entity<'_>, walk: &mut Walk<'_>) {
    let Some(usr) = entity.get_usr().map(|usr| usr.0) else {
        return;
    };
    if walk
        .seen
        .get(&usr)
        .is_some_and(|flavours| flavours.contains(&walk.flavour))
    {
        return;
    }
    let Some(location) = location_of(entity, walk.scope) else {
        // Defined outside the project.
        return;
    };
    let Some(name) = entity.get_name() else {
        return;
    };
    let qualified_name = qualify(entity, &name);
    let scope = &mut *walk.scope;
    let mut calls = Vec::new();

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
                calls.push((callee.0, location));
            }
        }
        EntityVisitResult::Recurse
    });

    walk.seen.entry(usr.clone()).or_default().push(walk.flavour);
    walk.out.push(Walked {
        usr,
        name,
        qualified_name,
        location,
        calls,
    });
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
