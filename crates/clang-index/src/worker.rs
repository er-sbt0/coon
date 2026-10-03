//! The other end of a worker process: parse what the parent sends, one TU at a
//! time, and send each result back.
//!
//! A worker is the app's own executable started again with [`ARGUMENT`] in
//! front of the project root, rather than a binary of its own. One executable
//! is what Tauri bundles, and a second would have to be shipped as a sidecar,
//! found next to the first at runtime and kept the same version as it; this
//! way the worker is the parent's own code by construction. The price is one
//! line at the top of every `main` that can index — [`run_if_worker`] — which
//! has to come before anything else the binary does, since a worker that went
//! on to open a window would be several windows.
//!
//! The protocol is a line of JSON each way. The worker announces itself with
//! [`READY`] once libclang has loaded, then answers each request with one
//! reply; stdin closing is the end of the run, and the worker exits. Nothing
//! else may be written to stdout, so a worker initialises no logging at all:
//! everything worth logging travels back in the reply and the parent logs it,
//! in job order. stderr is the parent's, for whatever libclang prints as it
//! dies.

use std::io::{BufRead, Write};
use std::path::Path;

use clang::{Clang, Index};
use serde::{Deserialize, Serialize};

use crate::{Outcome, Parser, Request};

/// The first argument that makes a process a worker.
pub const ARGUMENT: &str = "--coon-clang-index-worker";

/// The line a worker writes once it is ready for requests. A process that does
/// not write it in time is not a worker — a binary that forgot to call
/// [`run_if_worker`] and started being itself instead.
pub(crate) const READY: &str = "coon-clang-index-worker ready 1";

/// A worker's answer to one request.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Reply {
    pub(crate) index: usize,
    pub(crate) outcome: Outcome,
}

/// Become a worker and never return, if this process was started as one.
///
/// Call it first thing in `main` of every binary that indexes with libclang:
/// [`crate::Options::from_env`] starts workers by re-running the current
/// executable.
pub fn run_if_worker() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(ARGUMENT)) {
        return;
    }
    let Some(root) = args.next() else {
        eprintln!("{ARGUMENT} needs the project root");
        std::process::exit(2);
    };
    let code = match serve(Path::new(&root), |_| {}) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("clang-index worker: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// Answer requests from stdin until it closes.
///
/// `before_each` sees each file before it is parsed. It is there for tests,
/// which need a worker that crashes or hangs on a file of their choosing.
pub fn serve(root: &Path, mut before_each: impl FnMut(&Path)) -> anyhow::Result<()> {
    let clang = Clang::new().map_err(|e| anyhow::anyhow!("could not load libclang: {e}"))?;
    let index = Index::new(&clang, false, false);
    let mut parser = Parser::new(root);
    let mut out = std::io::stdout().lock();
    writeln!(out, "{READY}")?;
    out.flush()?;
    for line in std::io::stdin().lock().lines() {
        let request: Request = serde_json::from_str(&line?)?;
        before_each(&request.file);
        let reply = Reply {
            index: request.index,
            outcome: parser.parse(&index, &request),
        };
        serde_json::to_writer(&mut out, &reply)?;
        writeln!(out)?;
        out.flush()?;
    }
    Ok(())
}
