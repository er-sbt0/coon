//! Load a project through the real bootstrap and print what came out.
//!
//! ```text
//! cargo run -p core --example load -- <project> [--flavour <file>] [--dump <out>] [--functions <out>] [--backend libclang|clangd] [--bodiless drop|project|all]
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

mod common;

use core::LoaderOptions;

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
            "--bodiless" => match args.next().map(|b| b.parse()) {
                Some(Ok(b)) => options.bodiless_callees = b,
                Some(Err(message)) => {
                    eprintln!("{message}");
                    std::process::exit(2);
                }
                None => {
                    eprintln!("--bodiless needs `drop`, `project` or `all`");
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

    let app = match common::load_settled(&project, backend, options).await {
        Ok(app) => app,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };

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
}
