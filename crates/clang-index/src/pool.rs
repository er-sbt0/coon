//! The parent end of the worker processes.
//!
//! libclang runs in a process of its own because of what it can do to the one
//! it runs in. A crash on a strange TU — an assertion, a stack overflow on a
//! deeply nested macro — takes the whole process with it, which in-process was
//! the app; a parse that never ends could not be stopped, nor could a run the
//! user had moved on from. And the `clang` crate allows one `Clang` per
//! process, so threads would not have parsed in parallel anyway: processes
//! are the only parallelism there is.
//!
//! Each worker gets a thread here that hands it one job at a time from a
//! shared counter, so every worker sees an increasing run of job indices —
//! which is what lets `Merge` reproduce the serial graph exactly (see there).
//! A worker that dies, or that is still on one TU after the timeout, is
//! killed; the TU counts as failed and the next job goes to a fresh worker.
//!
//! A worker dies with its parent three ways: it is killed when its handle is
//! dropped, it exits when stdin closes, and on Linux the kernel kills it if
//! the thread that started it goes, which is the one that still works when the
//! parent was killed rather than shut down. That thread is the one that owns
//! the worker for the whole run, so it never goes first.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::worker::{Reply, ARGUMENT, READY};
use crate::{Outcome, Plan, Request};

/// A parse that takes longer than this is assumed never to finish. The
/// slowest TU of jemalloc takes well under a second; this is room for a
/// generated file a hundred times that, not a target.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The most workers a run starts unasked. Each holds one AST, and memory is
/// the reason this backend exists; past eight the parse is mostly waiting on
/// the slowest TU anyway.
const DEFAULT_MAX_WORKERS: usize = 8;

/// What one more worker is allowed to cost, for sizing the default against
/// the memory the machine has free. Measured on jemalloc a worker's own pages
/// peak at 50–80 MB, and its RSS at ~120 MB of which ~70 MB is libclang,
/// mapped from the one file and shared by every worker; its proportional
/// share comes to ~60 MB. This is several times that, for the AST of a large
/// C++ TU, which is where a worker's private pages go.
const WORKER_MEMORY: u64 = 256 << 20;

/// How long a new worker has to load libclang and say it is ready.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a thread waiting on a worker looks up to see if the run has been
/// cancelled.
const POLL: Duration = Duration::from_millis(50);

/// How a run is spread over processes.
#[derive(Debug, Clone)]
pub struct Options {
    /// Worker processes to parse in. Zero parses in this process, serially,
    /// as the backend did before it had workers.
    pub workers: usize,
    /// How long one TU may take before its worker is killed.
    pub timeout: Duration,
    /// What to run as a worker.
    pub worker: WorkerCommand,
    /// Whether to keep each TU's result on disk and reuse it while its inputs
    /// are unchanged (see `cache`). Off by default, which is what the tests
    /// get; [`Options::from_env`] turns it on.
    pub cache: bool,
    /// Which calls to a function with no body in the build become edges.
    /// Read only once every TU is in, so it is not part of a cache key: one
    /// cached parse answers all three.
    pub bodiless_callees: model::BodilessCallees,
}

/// A program and the arguments that make it a worker. The project root is
/// appended to them.
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl WorkerCommand {
    /// This executable, re-run in worker mode. Its `main` must call
    /// [`crate::worker::run_if_worker`] first.
    pub fn current_exe() -> Self {
        Self {
            program: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("coon-desktop")),
            args: vec![ARGUMENT.into()],
        }
    }
}

impl Default for Options {
    /// One worker per core, up to [`DEFAULT_MAX_WORKERS`] and to as many as
    /// fit in the memory available at [`WORKER_MEMORY`] each, but at least
    /// one; each this executable re-run.
    fn default() -> Self {
        Self {
            workers: default_workers(),
            timeout: DEFAULT_TIMEOUT,
            worker: WorkerCommand::current_exe(),
            cache: false,
            bodiless_callees: model::BodilessCallees::default(),
        }
    }
}

impl Options {
    /// The defaults, with the cache on unless `COON_INDEX_CACHE=0`, and
    /// `COON_INDEX_JOBS` (worker count; 0 parses in this process) and
    /// `COON_INDEX_TIMEOUT` (seconds per TU) applied.
    pub fn from_env() -> Self {
        let mut options = Self {
            cache: env_number("COON_INDEX_CACHE") != Some(0),
            ..Self::default()
        };
        if let Some(workers) = env_number("COON_INDEX_JOBS") {
            options.workers = workers;
        }
        if let Some(seconds) = env_number("COON_INDEX_TIMEOUT") {
            options.timeout = Duration::from_secs(seconds as u64);
        }
        options
    }
}

/// See [`Options::default`]. The memory check is what keeps a machine that is
/// already short from being pushed into swap by a parse it did not size: eight
/// workers summed to ~400 MB proportional (~900 MB RSS) on jemalloc against
/// ~120 MB for one. It only ever lowers the count; `COON_INDEX_JOBS` overrides
/// it either way.
fn default_workers() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let wanted = cores.min(DEFAULT_MAX_WORKERS);
    let Some(available) = available_memory() else {
        return wanted;
    };
    let fit = usize::try_from(available / WORKER_MEMORY)
        .unwrap_or(usize::MAX)
        .max(1);
    if fit < wanted {
        log::info!(
            "[clang-index] {} MB of memory available: {fit} worker(s) rather than {wanted}",
            available >> 20
        );
    }
    wanted.min(fit)
}

/// `MemAvailable` from `/proc/meminfo`: the kernel's estimate of what can be
/// allocated without swapping, page cache included, which free memory is not.
#[cfg(target_os = "linux")]
fn available_memory() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(not(target_os = "linux"))]
fn available_memory() -> Option<u64> {
    None
}

fn env_number(name: &str) -> Option<usize> {
    let value = std::env::var(name).ok()?;
    match value.trim().parse() {
        Ok(n) => Some(n),
        Err(_) => {
            log::warn!("[clang-index] {name}={value} is not a number; ignored");
            None
        }
    }
}

/// Stops a run from another thread. Clones share the one flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What the worker threads share: the next job to hand out, as a place in the
/// plan's `todo`, and the arguments libclang has refused so far, which no job
/// handed out after is sent unless the run caches (see `Plan::request`).
struct Dispatch {
    next: usize,
    rejected: Vec<String>,
}

/// The first worker of a run, or `None` if this binary cannot be one — found
/// out once, before the run, rather than once per job; the caller then parses
/// in process rather than not at all. Started ahead of the plan, because the
/// libclang version it reports is part of the cache's keys.
pub(crate) fn start(options: &Options, root: &Path) -> Option<Worker> {
    match Worker::spawn(&options.worker, root) {
        Ok(worker) => Some(worker),
        Err(e) => {
            log::error!(
                "[clang-index] could not start a worker ({} {:?}): {e:#}; parsing in this \
                 process instead",
                options.worker.program.display(),
                options.worker.args
            );
            None
        }
    }
}

/// Parse the plan's jobs in `workers` processes, `first` among them, handing
/// each TU's outcome to `sink` as it comes in — in no particular order, and
/// every job exactly once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    root: &Path,
    plan: &Plan<'_>,
    first: Worker,
    workers: usize,
    options: &Options,
    cancel: &Cancel,
    on_progress: &mut impl FnMut(usize, usize),
    sink: &mut impl FnMut(usize, Outcome),
) -> anyhow::Result<()> {
    log::info!(
        "[clang-index] parsing {} TU(s) in {} worker process(es)",
        plan.todo.len(),
        workers
    );

    let dispatch = Mutex::new(Dispatch {
        next: 0,
        rejected: Vec::new(),
    });
    let (results_tx, results_rx) = mpsc::channel::<(usize, Outcome)>();
    let mut delivered = vec![false; plan.jobs.len()];

    std::thread::scope(|threads| {
        let mut first = Some(first);
        for _ in 0..workers {
            let worker = first.take();
            let results = results_tx.clone();
            let dispatch = &dispatch;
            threads.spawn(move || feed(root, plan, options, cancel, dispatch, results, worker));
        }
        drop(results_tx);

        let mut done = 0;
        while done < plan.todo.len() {
            match results_rx.recv_timeout(POLL) {
                Ok((i, outcome)) => {
                    if let Outcome::Parsed(parsed) = &outcome {
                        let mut dispatch = dispatch.lock().unwrap();
                        for (argument, _) in &parsed.rejected {
                            if !dispatch.rejected.contains(argument) {
                                dispatch.rejected.push(argument.clone());
                            }
                        }
                    }
                    delivered[i] = true;
                    sink(i, outcome);
                    done += 1;
                    on_progress(done, plan.todo.len());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every thread has stopped: the run was cancelled.
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    if cancel.is_cancelled() {
        anyhow::bail!("the index run was cancelled");
    }
    for &i in plan.todo.iter().filter(|&&i| !delivered[i]) {
        sink(i, Outcome::Failed("no worker returned it".into()));
    }
    Ok(())
}

/// One worker's thread: take the next job, have the worker parse it, send the
/// outcome on, until the jobs run out or the run is cancelled.
fn feed(
    root: &Path,
    plan: &Plan<'_>,
    options: &Options,
    cancel: &Cancel,
    dispatch: &Mutex<Dispatch>,
    results: mpsc::Sender<(usize, Outcome)>,
    mut worker: Option<Worker>,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let request = {
            let mut dispatch = dispatch.lock().unwrap();
            let Some(&i) = plan.todo.get(dispatch.next) else {
                return;
            };
            dispatch.next += 1;
            plan.request(i, &dispatch.rejected)
        };
        let file = &plan.jobs[request.index].file;
        let live = match worker.take() {
            Some(live) => Ok(live),
            None => Worker::spawn(&options.worker, root),
        };
        let outcome = match live {
            Err(e) => Outcome::Failed(format!("could not start a worker: {e:#}")),
            Ok(mut live) => match live.parse(&request, options.timeout, cancel) {
                Ok(outcome) => {
                    worker = Some(live);
                    outcome
                }
                Err(Fault::Cancelled) => return,
                Err(Fault::Died(how)) => {
                    log::warn!(
                        "[clang-index] the worker parsing {} died ({how}); that TU counts as \
                         failed and the next goes to a new worker",
                        file.display()
                    );
                    Outcome::Failed(format!("the worker parsing it died ({how})"))
                }
                Err(Fault::TimedOut) => {
                    log::warn!(
                        "[clang-index] {} was still parsing after {:?}; its worker was killed \
                         and that TU counts as failed",
                        file.display(),
                        options.timeout
                    );
                    Outcome::Failed(format!("timed out after {:?}", options.timeout))
                }
            },
        };
        if results.send((request.index, outcome)).is_err() {
            return;
        }
    }
}

/// Why a worker gave no reply.
enum Fault {
    Died(String),
    TimedOut,
    Cancelled,
}

/// A running worker process. Dropping it kills the process.
pub(crate) struct Worker {
    /// The libclang version it loaded, as it said when it started.
    pub(crate) version: String,
    child: Child,
    stdin: ChildStdin,
    /// The worker's stdout, a line at a time, read on a thread of its own so
    /// that waiting for a reply can time out.
    lines: mpsc::Receiver<String>,
}

impl Worker {
    fn spawn(command: &WorkerCommand, root: &Path) -> anyhow::Result<Self> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: prctl is async-signal-safe and touches nothing of the
            // parent's, which is all that may run between fork and exec.
            unsafe {
                process.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
        }
        let mut child = process.spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (lines_tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            version: String::new(),
            child,
            stdin,
            lines,
        };
        match worker.lines.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(line) if line.starts_with(READY) => {
                worker.version = line[READY.len()..].trim().to_owned();
                Ok(worker)
            }
            Ok(line) => anyhow::bail!("it did not answer as a worker: {line:?}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                anyhow::bail!("it did not answer as a worker within {HANDSHAKE_TIMEOUT:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("it exited ({})", worker.exit_status())
            }
        }
    }

    fn parse(
        &mut self,
        request: &Request,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Outcome, Fault> {
        let mut line = serde_json::to_vec(request).expect("a request serialises");
        line.push(b'\n');
        if self
            .stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
            .is_err()
        {
            return Err(Fault::Died(self.exit_status()));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.lines.recv_timeout(POLL) {
                Ok(line) => match serde_json::from_str::<Reply>(&line) {
                    Ok(reply) if reply.index == request.index => return Ok(reply.outcome),
                    _ => log::warn!("[clang-index] a worker wrote something else: {line}"),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        return Err(Fault::Cancelled);
                    }
                    if Instant::now() >= deadline {
                        return Err(Fault::TimedOut);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Fault::Died(self.exit_status()))
                }
            }
        }
    }

    /// How the process ended, once it has: its stdout closing comes first.
    fn exit_status(&mut self) -> String {
        match self.child.wait() {
            Ok(status) => status.to_string(),
            Err(e) => format!("status unknown: {e}"),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
