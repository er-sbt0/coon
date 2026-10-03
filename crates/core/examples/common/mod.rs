//! What the examples that load a project share: driving a load until the
//! graph has stopped growing.
//!
//! Not an example itself — cargo only builds `examples/*.rs` and
//! `examples/*/main.rs` — but declared as `mod common;` by the ones that
//! need it.

use std::time::{Duration, Instant};

use core::{Backend, CoreApp, LoaderOptions};
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

/// Load `project` with `backend` through the real bootstrap, ticked the way
/// the desktop backend ticks it, and hand back the app once the graph has
/// stopped growing.
pub async fn load_settled(
    project: &str,
    backend: Backend,
    options: LoaderOptions,
) -> Result<CoreApp, String> {
    if let Some(flavour) = &options.flavour {
        core::check_flavour(std::path::Path::new(project), flavour).await?;
    }

    let started = Instant::now();
    let mut app = CoreApp::new_with_lsp(
        CallGraph::new(),
        core::spawn_bridge(backend, project, options),
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
            return Err(format!("load failed: {reason}"));
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
            return Err(format!("load did not settle within {DEADLINE:?}"));
        }
    }

    println!(
        "{}: {} functions, {} edges in {:.1}s",
        project,
        app.call_graph.node_count(),
        app.call_graph.edge_count(),
        started.elapsed().as_secs_f64()
    );
    Ok(app)
}
