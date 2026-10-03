//! Load a project through the real bootstrap and print what came out.
//!
//! ```text
//! cargo run -p core --example load -- <project> [--flavour <file>] [--dump <out>] [--functions <out>] [--backend libclang|clangd]
//! ```
//!
//! The desktop app needs a display and the deprecated CLI deliberately has no
//! `--flavour`, so this is how a load is measured: it is the same
//! `spawn_lsp_bridge` into the same `CoreApp`, ticked the way the desktop
//! backend ticks it, and it exits as soon as the graph has stopped growing.
//! Two runs of it are how "the same project answers the same twice" is checked:
//! the counts answer whether they agree, and `--dump` answers where they do
//! not, being one sorted `caller -> callee` line per call site. `--functions`
//! is the same for the nodes, one `name @ file:line` per function, which is
//! what a `SymbolId` is made of — so two backends agreeing on it agree on
//! identity, not only on names.

use std::time::{Duration, Instant};

use core::{CoreApp, LoaderOptions};
use model::lsp_status::LspLoadPhase;
use model::CallGraph;

/// How long to keep ticking after the symbols land before deciding the edge
/// warmup is never going to start.
///
/// `warmup_progress()` is `None` both before the warmup begins and after it
/// drains, so "not running" only means "finished" once it has been seen
/// running — or once nothing has happened for a while.
const WARMUP_GRACE: Duration = Duration::from_secs(5);

/// The ceiling on the whole load, so a stuck clangd fails the harness rather
/// than hanging it.
const DEADLINE: Duration = Duration::from_secs(600);

fn main() {
    // A libclang worker is this executable started again; it must not get as
    // far as loading anything itself.
    core::run_if_index_worker();
    run();
}

#[tokio::main]
async fn run() {
    if let Err(e) = core::logging::init_logging() {
        eprintln!("failed to initialize logging: {e}");
    }

    let mut args = std::env::args().skip(1);
    let Some(project) = args.next() else {
        eprintln!("usage: cargo run -p core --example load -- <project> [--flavour <file>]");
        std::process::exit(2);
    };
    let mut options = LoaderOptions::default();
    let mut dump: Option<std::path::PathBuf> = None;
    let mut functions: Option<std::path::PathBuf> = None;
    let mut backend = core::Backend::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--flavour" => match args.next() {
                Some(file) => options.flavour = Some(file.into()),
                None => {
                    eprintln!("--flavour needs a file");
                    std::process::exit(2);
                }
            },
            "--dump" => match args.next() {
                Some(file) => dump = Some(file.into()),
                None => {
                    eprintln!("--dump needs a file");
                    std::process::exit(2);
                }
            },
            "--functions" => match args.next() {
                Some(file) => functions = Some(file.into()),
                None => {
                    eprintln!("--functions needs a file");
                    std::process::exit(2);
                }
            },
            "--backend" => match args.next().map(|b| b.parse()) {
                Some(Ok(b)) => backend = b,
                Some(Err(message)) => {
                    eprintln!("{message}");
                    std::process::exit(2);
                }
                None => {
                    eprintln!("--backend needs `clangd` or `libclang`");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("unexpected argument `{other}`");
                std::process::exit(2);
            }
        }
    }

    if let Some(flavour) = &options.flavour {
        if let Err(message) = core::check_flavour(std::path::Path::new(&project), flavour).await {
            eprintln!("{message}");
            std::process::exit(1);
        }
    }

    let started = Instant::now();
    let mut app = CoreApp::new_with_lsp(
        CallGraph::new(),
        core::spawn_bridge(backend, &project, options.clone()),
    );
    let response_ready = app.lsp.response_ready();

    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut saw_warmup = false;
    let mut reported = 0usize;
    let mut symbols_landed: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = response_ready.notified() => {}
        }
        app.tick();

        if let LspLoadPhase::Failed(reason) = app.lsp.status() {
            eprintln!("load failed: {reason}");
            std::process::exit(1);
        }

        match app.warmup_progress() {
            Some((done, total)) => {
                saw_warmup = true;
                if done >= reported + 500 {
                    reported = done - done % 500;
                    println!("  resolving calls {done}/{total}");
                }
            }
            None => {
                let done = matches!(app.lsp.status(), LspLoadPhase::Completed);
                let landed = *symbols_landed.get_or_insert_with(Instant::now);
                if done && (saw_warmup || landed.elapsed() > WARMUP_GRACE) {
                    break;
                }
                if !done {
                    symbols_landed = None;
                }
            }
        }

        if started.elapsed() > DEADLINE {
            eprintln!("load did not settle within {DEADLINE:?}");
            std::process::exit(1);
        }
    }

    if let Some(path) = dump {
        let mut lines: Vec<String> = app
            .call_graph
            .edges()
            .map(|edge| {
                let name = |id| {
                    app.call_graph
                        .get_function(id)
                        .map(|f| f.qualified_name.as_str())
                        .unwrap_or("?")
                };
                format!(
                    "{} -> {} @ {}:{}",
                    name(&edge.caller),
                    name(&edge.callee),
                    edge.call_location.file_path,
                    edge.call_location.line
                )
            })
            .collect();
        lines.sort();
        if let Err(e) = std::fs::write(&path, lines.join("\n")) {
            eprintln!("could not write {}: {e}", path.display());
        }
    }

    if let Some(path) = functions {
        let mut lines: Vec<String> = app
            .call_graph
            .functions()
            .map(|f| {
                format!(
                    "{} @ {}:{}",
                    f.qualified_name, f.definition_location.file_path, f.definition_location.line
                )
            })
            .collect();
        lines.sort();
        if let Err(e) = std::fs::write(&path, lines.join("\n")) {
            eprintln!("could not write {}: {e}", path.display());
        }
    }

    println!(
        "{}: {} functions, {} edges in {:.1}s",
        project,
        app.call_graph.node_count(),
        app.call_graph.edge_count(),
        started.elapsed().as_secs_f64()
    );
}
