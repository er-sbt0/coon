//! A call graph built by parsing every translation unit with libclang.
//!
//! The clangd backend asks a server that keeps an index of the whole project
//! resident, and on a large project that index — plus a preamble per open
//! document — is where the memory goes. This one keeps nothing resident: a
//! parser reads one translation unit at a time, records the functions defined
//! in it and the calls inside their bodies, and drops the AST before the next.
//! The parsers are worker processes (see `pool`), one AST each, so a crash or
//! a hang in libclang costs one TU rather than the app; and since the result
//! of every TU is merged in job order (see `Merge`), how many there are and
//! which finishes first change nothing in the graph.
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
//!
//! What a TU came to is kept on disk (see `cache`), so reopening a project, or
//! refreshing it after an edit, parses only the TUs whose inputs changed.

pub mod args;
mod cache;
mod pool;
pub mod worker;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime};

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
    /// Of `parsed`, the TUs whose result came from the cache rather than a
    /// parse.
    pub cached: usize,
}

/// One function definition, as first seen.
struct Definition {
    name: String,
    qualified_name: String,
    location: Site,
}

/// One call, by USR on both ends, every string a number in the run's
/// [`Strings`].
struct Call {
    caller: u32,
    callee: u32,
    location: Site,
}

/// A place in the source: the file as a number into whichever string table the
/// value travels with, then line and column, 1-based as `model::Location` is.
///
/// A tuple so that on the wire it is `[file,line,column]`: there is one per
/// call, and field names would be most of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Site(u32, u32, u32);

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
    /// Where to cache the result, when the run caches: the parse then walks
    /// every body, not only those its parser has not met (see `cache`).
    store: Option<cache::Store>,
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
    /// Every USR and file path the walk below names, once each, which the
    /// numbers in it index. A header's path is otherwise written out once per
    /// call made in it and a hot callee's USR once per call of it; most of
    /// what a TU sends was those repeats. Emptied by [`Strings::adopt`], once
    /// the numbers have been made the run's.
    strings: Vec<String>,
    /// The definitions whose bodies this parse walked, in source order.
    walked: Vec<Walked>,
    elapsed_ms: u64,
    /// Whether the result was cached, when it was asked to be.
    #[serde(default)]
    cached: Cached,
}

/// What became of a request to cache a result.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) enum Cached {
    #[default]
    NotAsked,
    Stored,
    /// Why not: errors in the parse, a file that changed under it, a write
    /// that failed.
    NotStored(String),
}

/// One definition and the calls in its body.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Walked {
    usr: u32,
    name: String,
    qualified_name: String,
    location: Site,
    /// Callee USR and where the call is written, in source order.
    calls: Vec<(u32, Site)>,
}

/// Strings given numbers in the order first met, each kept once.
///
/// A parse numbers what it names in a table of its own, since it cannot know
/// the run's; the parent folds each into the run's table as it arrives
/// ([`Strings::adopt`]), so a result waiting its turn in the merge holds
/// numbers rather than copies of strings every other result also holds.
#[derive(Default)]
struct Strings {
    numbers: HashMap<String, u32>,
}

impl Strings {
    fn number(&mut self, text: &str) -> u32 {
        if let Some(&n) = self.numbers.get(text) {
            return n;
        }
        let n = self.numbers.len() as u32;
        self.numbers.insert(text.to_owned(), n);
        n
    }

    /// Renumber a result from its own table into this one, and drop its table.
    ///
    /// `None` for a result naming a number its table does not have. Only a
    /// broken worker sends one, and it is checked rather than indexed because
    /// a panic here would take down the process the workers exist to protect.
    fn adopt(&mut self, parsed: &mut Parsed) -> Option<()> {
        let map: Vec<u32> = parsed.strings.iter().map(|s| self.number(s)).collect();
        parsed.strings = Vec::new();
        let renumber = |n: &mut u32| -> Option<()> {
            *n = *map.get(*n as usize)?;
            Some(())
        };
        for walked in &mut parsed.walked {
            renumber(&mut walked.usr)?;
            renumber(&mut walked.location.0)?;
            for (callee, location) in &mut walked.calls {
                renumber(callee)?;
                renumber(&mut location.0)?;
            }
        }
        Some(())
    }

    /// Every string, indexed by its number.
    fn by_number(&self) -> Vec<&str> {
        let mut out = vec![""; self.numbers.len()];
        for (text, &n) in &self.numbers {
            out[n as usize] = text;
        }
        out
    }
}

/// A parse's own string table, as it fills.
#[derive(Default)]
struct LocalStrings {
    table: Strings,
    list: Vec<String>,
}

impl LocalStrings {
    fn number(&mut self, text: &str) -> u32 {
        let n = self.table.number(text);
        if n as usize == self.list.len() {
            self.list.push(text.to_owned());
        }
        n
    }
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
/// first: results are merged in job order, by the same rule a serial run
/// follows, each as soon as every job before it is in (see [`Merge`]). A
/// worker that crashes or runs past `options.timeout` costs the TU it was on,
/// counted in `failed`, and is replaced. Cancelling `cancel` kills the
/// workers and returns an error.
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

    let mut merge = Merge::new(jobs, &flavours);
    let first = (options.workers > 0 && !jobs.is_empty())
        .then(|| pool::start(options, root))
        .flatten();
    let cache = if !options.cache {
        None
    } else if let Some(worker) = &first {
        Some(cache::Cache::open(root, &worker.version))
    } else {
        cache::libclang_version().map(|version| cache::Cache::open(root, &version))
    };
    let mut plan = Plan {
        jobs,
        flavours: &flavours,
        stores: Vec::new(),
        todo: (0..jobs.len()).collect(),
    };
    let mut hits = 0;
    if let Some(cache) = &cache {
        let mut files = cache::Files::default();
        plan.stores = jobs.iter().map(|job| Some(cache.store_for(job))).collect();
        plan.todo.clear();
        for (i, store) in plan.stores.iter_mut().enumerate() {
            match store.as_ref().and_then(|s| cache.load(s, &mut files)) {
                Some(parsed) => {
                    *store = None;
                    hits += 1;
                    merge.arrive(i, Outcome::Parsed(parsed));
                    on_progress(hits, jobs.len());
                }
                None => plan.todo.push(i),
            }
        }
        log::info!(
            "[clang-index] {hits} of {} TU(s) unchanged since cached in {}; {} to parse",
            jobs.len(),
            cache.dir().display(),
            plan.todo.len()
        );
    }

    let mut progress = |done: usize, _: usize| on_progress(hits + done, jobs.len());
    let workers = options.workers.min(plan.todo.len());
    let first = first.filter(|_| workers > 0);
    let in_workers = first.is_some();
    if let Some(first) = first {
        pool::run(
            root,
            &plan,
            first,
            workers,
            options,
            cancel,
            &mut progress,
            &mut |i, outcome| merge.arrive(i, outcome),
        )?;
    }
    if !in_workers && !plan.todo.is_empty() {
        in_process(root, &plan, cancel, &mut progress, &mut |i, outcome| {
            merge.arrive(i, outcome)
        })?;
    }
    Ok(merge.finish(numbers.len(), hits, started))
}

/// What a run has to parse, and how to ask for each.
pub(crate) struct Plan<'a> {
    jobs: &'a [Job],
    flavours: &'a [usize],
    /// Per job, where its result is to be cached: empty when the run does not
    /// cache, `None` for a job the cache already answered.
    stores: Vec<Option<cache::Store>>,
    /// The jobs to parse, in increasing order — every job, less the cache's.
    todo: Vec<usize>,
}

impl Plan<'_> {
    fn caching(&self) -> bool {
        !self.stores.is_empty()
    }

    /// The request for job `i`, without the arguments already refused — unless
    /// the run caches.
    ///
    /// See `args::rejected_argument`. Once refused, an argument is left out of
    /// every TU handed out afterwards, so libclang does not report it again
    /// for every file. Leaving it out changes nothing but the diagnostic:
    /// libclang ignored it anyway. But which arguments have been refused by
    /// the time a job goes out depends on which jobs went before it, and a
    /// cached result is keyed by the arguments it was parsed with, so a run
    /// that caches sends every job whole: the key is then a function of the
    /// job alone, and the merge reports each refused argument once regardless.
    pub(crate) fn request(&self, i: usize, rejected: &[String]) -> Request {
        let job = &self.jobs[i];
        let strip = if self.caching() { &[][..] } else { rejected };
        Request {
            index: i,
            file: job.file.clone(),
            directory: job.directory.clone(),
            arguments: job
                .arguments
                .iter()
                .filter(|a| !strip.contains(a))
                .cloned()
                .collect(),
            flavour: self.flavours[i],
            store: self.stores.get(i).cloned().flatten(),
        }
    }
}

/// Parse every job of the plan here, one after another, handing each outcome
/// to `sink`.
fn in_process(
    root: &Path,
    plan: &Plan<'_>,
    cancel: &Cancel,
    on_progress: &mut impl FnMut(usize, usize),
    sink: &mut impl FnMut(usize, Outcome),
) -> anyhow::Result<()> {
    let clang = Clang::new().map_err(|e| anyhow::anyhow!("could not load libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    let mut parser = Parser::new(root);
    let mut rejected: Vec<String> = Vec::new();
    for (done, &i) in plan.todo.iter().enumerate() {
        if cancel.is_cancelled() {
            anyhow::bail!("the index run was cancelled");
        }
        let outcome = parser.parse(&index, &plan.request(i, &rejected));
        if let Outcome::Parsed(parsed) = &outcome {
            for (argument, _) in &parsed.rejected {
                if !rejected.contains(argument) {
                    rejected.push(argument.clone());
                }
            }
        }
        sink(i, outcome);
        on_progress(done + 1, plan.todo.len());
    }
    Ok(())
}

/// Every TU's result folded into one, in job order.
///
/// This is where the serial rule is applied, whoever did the parsing: a body
/// is kept from the first job of its flavour, in job order, to walk it, and a
/// node from the first job of any flavour. A parser walks a definition the
/// first time *it* meets it in a flavour, so with several parsers one body may
/// be walked by more than one of them; but each is handed jobs in increasing
/// order, so the lowest job to define it is always among those that report it,
/// and the rest are dropped here.
///
/// Results are folded as they arrive, not once the run is over. A result
/// ahead of the next in job order waits in `waiting` until the ones before it
/// are in, and then they go in together; so what the parent holds is the
/// merged graph so far plus the results that overtook a slower TU, rather
/// than every result of the run. That matters because with N parsers a
/// header's inline bodies are walked up to N times per flavour, and all but
/// one of those walks are dropped here: holding them all to the end held the
/// duplicates too.
struct Merge<'a> {
    jobs: &'a [Job],
    flavours: &'a [usize],
    strings: Strings,
    /// Results that came in ahead of `next`, by job index.
    waiting: BTreeMap<usize, Outcome>,
    /// The first job not yet folded in.
    next: usize,
    definitions: Vec<(u32, Definition)>,
    /// Per USR, the flavours whose body of it has been kept.
    defined: HashMap<u32, Vec<usize>>,
    /// Per USR and flavour, the lowest job whose result to arrive so far
    /// carries its body.
    lowest: HashMap<(u32, usize), usize>,
    calls: Vec<Call>,
    parsed: usize,
    failed: usize,
    errors: usize,
    tus_with_errors: usize,
    rejected: Vec<String>,
    /// Results written to the cache, and the first reason one was not.
    stored: usize,
    not_stored: usize,
    first_not_stored: Option<String>,
}

impl<'a> Merge<'a> {
    fn new(jobs: &'a [Job], flavours: &'a [usize]) -> Self {
        Self {
            jobs,
            flavours,
            strings: Strings::default(),
            waiting: BTreeMap::new(),
            next: 0,
            definitions: Vec::new(),
            defined: HashMap::new(),
            lowest: HashMap::new(),
            calls: Vec::new(),
            parsed: 0,
            failed: 0,
            errors: 0,
            tus_with_errors: 0,
            rejected: Vec::new(),
            stored: 0,
            not_stored: 0,
            first_not_stored: None,
        }
    }

    /// Take job `i`'s outcome, in whatever order they come.
    ///
    /// A body some lower job has already brought in is dropped on arrival
    /// rather than when its turn comes: the fold would skip it anyway, since
    /// the lower job is folded first. This is what keeps a cached result
    /// waiting behind a TU still being parsed from holding every header body
    /// its neighbours already hold — a cache hit is complete, so after an edit
    /// to one file the hits past it would otherwise wait whole (~40 MB more at
    /// the peak on jemalloc, edit to `src/ckh.c`).
    fn arrive(&mut self, i: usize, mut outcome: Outcome) {
        if let Outcome::Parsed(parsed) = &mut outcome {
            if self.strings.adopt(parsed).is_none() {
                outcome = Outcome::Failed("its result named strings it did not send".into());
            }
        }
        if let Outcome::Parsed(parsed) = &mut outcome {
            let flavour = self.flavours[i];
            let lowest = &mut self.lowest;
            parsed.walked.retain(|walked| {
                let first = lowest.entry((walked.usr, flavour)).or_insert(i);
                *first = (*first).min(i);
                *first == i
            });
        }
        self.waiting.insert(i, outcome);
        while let Some(outcome) = self.waiting.remove(&self.next) {
            self.fold(self.next, outcome);
            self.next += 1;
        }
    }

    fn fold(&mut self, i: usize, outcome: Outcome) {
        let (job, flavour) = (&self.jobs[i], self.flavours[i]);
        let result = match outcome {
            Outcome::Parsed(result) => result,
            Outcome::Failed(reason) => {
                self.failed += 1;
                log::warn!(
                    "[clang-index] could not parse {}: {reason}",
                    job.file.display()
                );
                return;
            }
        };
        self.parsed += 1;
        match result.cached {
            Cached::NotAsked => {}
            Cached::Stored => self.stored += 1,
            Cached::NotStored(reason) => {
                log::debug!("[clang-index] {} not cached: {reason}", job.file.display());
                self.not_stored += 1;
                self.first_not_stored.get_or_insert(reason);
            }
        }
        for (argument, text) in result.rejected {
            if !self.rejected.contains(&argument) {
                log::info!(
                    "[clang-index] libclang ignores {argument} ({text}); left out from here on"
                );
                self.rejected.push(argument);
            }
        }
        for sample in &result.error_samples {
            log::warn!("[clang-index] {}: {sample}", job.file.display());
        }
        self.errors += result.errors;
        if result.errors > 0 {
            self.tus_with_errors += 1;
        }
        let before = (self.definitions.len(), self.calls.len());
        for walked in result.walked {
            let kept = self.defined.entry(walked.usr).or_default();
            if kept.contains(&flavour) {
                continue;
            }
            let first = kept.is_empty();
            kept.push(flavour);
            self.calls
                .extend(walked.calls.into_iter().map(|(callee, location)| Call {
                    caller: walked.usr,
                    callee,
                    location,
                }));
            if first {
                self.definitions.push((
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
            self.definitions.len() - before.0,
            self.calls.len() - before.1,
            result.errors
        );
    }

    fn finish(self, flavour_count: usize, cached: usize, started: Instant) -> Report {
        debug_assert!(
            self.waiting.is_empty() && self.next == self.jobs.len(),
            "every job's outcome is in before the merge finishes"
        );
        if flavour_count > 1 {
            let shared = self.defined.values().filter(|f| f.len() > 1).count();
            log::info!(
                "[clang-index] {} build flavour(s) in one graph: {} function(s) defined under \
                 more than one, their calls unioned",
                flavour_count,
                shared
            );
        }
        if self.stored + self.not_stored > 0 {
            log::info!(
                "[clang-index] {} result(s) cached, {} not{}",
                self.stored,
                self.not_stored,
                self.first_not_stored
                    .as_deref()
                    .map(|r| format!(" (the first because {r})"))
                    .unwrap_or_default()
            );
        }
        drop(self.defined);
        drop(self.lowest);
        let (graph, external_calls) = build_graph(self.definitions, self.calls, &self.strings);
        log::info!(
            "[clang-index] {} TU(s) read ({} from the cache, {} failed, {} with {} error \
             diagnostic(s)) in {:.1?}: {} function(s), {} edge(s), {} call(s) out of the project",
            self.parsed,
            cached,
            self.failed,
            self.tus_with_errors,
            self.errors,
            started.elapsed(),
            graph.node_count(),
            graph.edge_count(),
            external_calls
        );
        Report {
            graph,
            parsed: self.parsed,
            failed: self.failed,
            errors: self.errors,
            tus_with_errors: self.tus_with_errors,
            external_calls,
            rejected_arguments: self.rejected,
            cached,
        }
    }
}

/// What one parser — this process, or one worker — carries from one TU to the
/// next: where the project is, and which bodies it has already walked.
///
/// `walked` holds, per USR, the flavours whose definition of it this parser
/// has walked: a later TU of one of those skips the body, which is what keeps
/// a header's inline functions from being walked once per includer, while a TU
/// of another flavour walks it again for the calls that build makes.
///
/// A request that is to be cached is walked whole regardless (see `cache`),
/// and what this parser had already walked is cut from the reply afterwards
/// rather than skipped: the entry needs every body, the merge only the ones
/// no earlier job of this parser sent.
pub(crate) struct Parser {
    scope: Scope,
    walked: HashMap<String, Vec<usize>>,
    files: cache::Files,
}

impl Parser {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            scope: Scope::new(root),
            walked: HashMap::new(),
            files: cache::Files::default(),
        }
    }

    pub(crate) fn parse(&mut self, index: &Index<'_>, request: &Request) -> Outcome {
        let started = Instant::now();
        let started_at = SystemTime::now();
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
            complete: request.store.as_ref().map(|_| HashSet::new()),
            strings: LocalStrings::default(),
            out: Vec::new(),
            known: Vec::new(),
        };
        walk_translation_unit(tu.get_entity(), &mut walk);
        let (strings, walked, known) = (walk.strings.list, walk.out, walk.known);
        let mut parsed = Parsed {
            errors,
            error_samples,
            rejected,
            strings,
            walked,
            elapsed_ms: started.elapsed().as_millis() as u64,
            cached: Cached::NotAsked,
        };
        if let Some(store) = &request.store {
            parsed.cached = if errors > 0 {
                Cached::NotStored(format!("{errors} error(s) in the parse"))
            } else {
                let inputs = cache::inclusions(&tu, &request.directory);
                match cache::save(store, &inputs, started_at, &parsed, &mut self.files) {
                    Ok(()) => Cached::Stored,
                    Err(reason) => Cached::NotStored(reason),
                }
            };
            // The bodies an earlier job of this parser already sent, which the
            // merge would drop. The strings only they named stay in the table,
            // unreferenced; renumbering a few extra costs less than a second
            // table.
            let mut known = known.into_iter();
            parsed.walked.retain(|_| !known.next().unwrap_or(false));
        }
        Outcome::Parsed(parsed)
    }
}

/// One TU's walk: the parser's state, and what this TU adds.
struct Walk<'a> {
    scope: &'a mut Scope,
    seen: &'a mut HashMap<String, Vec<usize>>,
    /// The flavour of the TU being walked.
    flavour: usize,
    /// When the result is to be cached, the USRs walked in this TU: every
    /// body is walked once, whether or not `seen` has it.
    complete: Option<HashSet<String>>,
    /// What `out` names, numbered.
    strings: LocalStrings,
    out: Vec<Walked>,
    /// Per entry of `out`, whether `seen` already had it — only ever true
    /// when `complete` is set, since otherwise it would not have been walked.
    known: Vec<bool>,
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
            cached: self.cached,
        }
    }
}

/// Turn what was collected into a graph, keeping only calls between two
/// functions the project defines.
fn build_graph(
    definitions: Vec<(u32, Definition)>,
    calls: Vec<Call>,
    strings: &Strings,
) -> (CallGraph, usize) {
    let text = strings.by_number();
    let location = |site: Site| Location {
        file_path: text[site.0 as usize].to_owned(),
        line: site.1,
        column: site.2,
        length: None,
    };
    let mut graph = CallGraph::new();
    let mut ids = vec![None; text.len()];
    for (usr, definition) in definitions {
        let id = graph.add_function(FunctionNode::new(
            definition.name,
            definition.qualified_name,
            location(definition.location),
        ));
        ids[usr as usize] = Some(id);
    }
    // A call site met twice — a header body walked under two flavours — is one
    // edge, because `add_call` keys an edge by its site; the external count is
    // held to the same rule, so it does not grow with the number of flavours.
    let mut external = HashSet::new();
    for call in calls {
        match (ids[call.caller as usize], ids[call.callee as usize]) {
            (Some(caller), Some(callee)) => graph.add_call(caller, callee, location(call.location)),
            _ => {
                external.insert((call.caller, call.callee, call.location));
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
/// flavour is walked again, since it may be different code there. A walk to be
/// cached records every definition once, whatever the parser has seen.
fn record_definition(entity: Entity<'_>, walk: &mut Walk<'_>) {
    let Some(usr) = entity.get_usr().map(|usr| usr.0) else {
        return;
    };
    let known = walk
        .seen
        .get(&usr)
        .is_some_and(|flavours| flavours.contains(&walk.flavour));
    match &mut walk.complete {
        Some(this_tu) => {
            if this_tu.contains(&usr) {
                return;
            }
        }
        None if known => return,
        None => {}
    }
    let Some(location) = location_of(entity, walk.scope, &mut walk.strings) else {
        // Defined outside the project.
        return;
    };
    let Some(name) = entity.get_name() else {
        return;
    };
    let qualified_name = qualify(entity, &name);
    let scope = &mut *walk.scope;
    let strings = &mut walk.strings;
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
            if let Some(location) = location_of(child, scope, strings) {
                calls.push((strings.number(&callee.0), location));
            }
        }
        EntityVisitResult::Recurse
    });

    let number = walk.strings.number(&usr);
    if let Some(this_tu) = &mut walk.complete {
        this_tu.insert(usr.clone());
    }
    if !known {
        walk.seen.entry(usr).or_default().push(walk.flavour);
    }
    walk.known.push(known);
    walk.out.push(Walked {
        usr: number,
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
fn location_of(entity: Entity<'_>, scope: &mut Scope, strings: &mut LocalStrings) -> Option<Site> {
    let location = entity.get_location()?.get_file_location();
    let file = location.file?;
    let file_path = scope.resolve(&file.get_path())?;
    Some(Site(
        strings.number(file_path),
        location.line,
        location.column,
    ))
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
    fn resolve(&mut self, path: &Path) -> Option<&str> {
        let joined;
        let path = if path.is_relative() {
            joined = self.directory.join(path);
            joined.as_path()
        } else {
            path
        };
        if !self.files.contains_key(path) {
            let canonical = path.canonicalize().unwrap_or_else(|_| normalise(path));
            let resolved = canonical
                .starts_with(&self.root)
                .then(|| canonical.to_string_lossy().into_owned());
            self.files.insert(path.to_path_buf(), resolved);
        }
        self.files[path].as_deref()
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
