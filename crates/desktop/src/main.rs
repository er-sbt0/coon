//! COON desktop frontend.
//!
//! A native window hosting the same client the web frontend serves. The
//! backend speaks `core::protocol` over Tauri IPC instead of a WebSocket:
//! commands arrive through `invoke`, and `ServerMsg`s go out as events on the
//! `coon://message` channel. Because the message shapes are identical, the
//! JavaScript in `crates/web/static` is shared verbatim — only `transport.js`
//! differs at runtime, and it picks its pipe by feature-detecting Tauri.

// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Mutex;

use coon_core::{
    create_demo_call_graph, snapshot_of, spawn_lsp_bridge, AppEvent, Command, CoreApp, EventRouter,
    LoaderOptions, ServerMsg,
};
use model::CallGraph;

/// Event channel the client listens on. Mirrors the web frontend's socket.
const CHANNEL: &str = "coon://message";

/// Poll interval for `CoreApp::tick`, matching the web frontend's cadence.
///
/// The ceiling on how long anything waits to be noticed, not the pace the
/// app runs at: a clangd reply wakes the tick loop through
/// `LspBridge::response_ready` the moment it lands, and this interval is
/// what catches everything else — loader progress, timeouts, a finished
/// clustering.
const TICK_MS: u64 = 100;

#[derive(Parser)]
#[command(
    name = "coon-desktop",
    about = "COON - C/C++ call graph visualizer (desktop)"
)]
struct Args {
    /// Path to the C/C++ project directory (omit to run with demo data)
    project_path: Option<String>,

    /// A source file whose compile command names the build to analyse.
    ///
    /// A project's compilation database is usually several builds at once —
    /// the library, the unit tests, the integration tests — and the same
    /// header compiled under different defines declares different functions,
    /// so a call graph of all of them at once is a graph of no build in
    /// particular. Naming one file picks the build that file is compiled as;
    /// the project becomes every entry sharing its defines. Absolute, or
    /// relative to the project directory. Omitted, every entry is analysed and
    /// the builds on offer are listed in the log.
    #[arg(long, value_name = "FILE")]
    flavour: Option<std::path::PathBuf>,
}

/// Shared backend state. One window, one `CoreApp` — unlike the web frontend
/// there is no multi-client question to answer.
struct Backend {
    core: Arc<Mutex<CoreApp>>,
    router: Arc<Mutex<EventRouter>>,
    /// The project being analysed, so the export dialog opens where the user is
    /// already looking. `None` in demo mode, where there is no such directory.
    project_dir: Option<std::path::PathBuf>,
}

impl Backend {
    /// Translate a batch of `AppEvent`s into client messages and emit them.
    async fn pump(&self, app: &AppHandle, events: Vec<AppEvent>) {
        let routed = { self.router.lock().await.route(&events) };

        for event in routed.forward {
            log::debug!("[desktop] forwarding event to client: {:?}", event);
            emit(app, &ServerMsg::Event { event });
        }

        if routed.resnapshot {
            // Full snapshot rather than positions only: a Layout-only message
            // can't tell the client which nodes to add or remove.
            log::debug!("[desktop] sending snapshot");
            // The guard is held across the emit on purpose: the message borrows
            // the app rather than copying the call graph out of it.
            let core = self.core.lock().await;
            emit(app, &snapshot_of(&core));
        }
    }
}

fn emit(app: &AppHandle, msg: &ServerMsg) {
    if let Err(e) = app.emit(CHANNEL, msg) {
        log::warn!("[desktop] failed to emit message to client: {}", e);
    }
}

/// Send the client a full snapshot. Called once on startup by `transport.js`,
/// which has no connection event to hang the initial fetch off.
#[tauri::command]
async fn request_snapshot(app: AppHandle, backend: State<'_, Backend>) -> Result<(), String> {
    log::debug!("[desktop] snapshot requested by client");
    let core = backend.core.lock().await;
    emit(&app, &snapshot_of(&core));
    Ok(())
}

/// Dispatch a domain command and emit whatever it changed.
#[tauri::command]
async fn dispatch(
    command: Command,
    app: AppHandle,
    backend: State<'_, Backend>,
) -> Result<(), String> {
    log::info!("[desktop] command received: {:?}", command);
    let events = { backend.core.lock().await.dispatch(command) };
    log::debug!("[desktop] dispatch produced {} event(s)", events.len());
    backend.pump(&app, events).await;
    Ok(())
}

/// Ask where the export goes, write it there, and answer with the path.
///
/// `Ok(None)` means the user cancelled, which is not a failure and must not be
/// reported as one.
///
/// The browser build hands the file to the download machinery instead (see
/// `browserSave` in `transport.js`), because that is the only place a page can
/// put a file. A desktop app can ask, and asking is the answer here: picking a
/// directory on the user's behalf went to whatever XDG calls "downloads", which
/// on a machine with `XDG_DOWNLOAD_DIR="$HOME/"` is the home directory — so
/// three exports landed in `~` and read as a key that did nothing.
///
/// The dialog is opened from Rust rather than from `transport.js` on purpose:
/// a plugin's JavaScript bindings come as an npm package, and reaching for one
/// would cost this client the property that it is a plain script with no build
/// step. The pipe stays `invoke`, exactly as it was.
///
/// It opens on the project being analysed — the directory the export describes
/// is the one the user is thinking about — falling back to downloads when coon
/// is running on demo data.
///
/// `filename` is built by `core::export`, which already slugs it, but it
/// arrives here across an IPC boundary, so it is checked again rather than
/// trusted: a name is a name, never a path. The dialog would contain it either
/// way; this keeps a bad one from ever reaching the dialog.
#[tauri::command]
async fn save_export(
    app: AppHandle,
    backend: State<'_, Backend>,
    filename: String,
    contents: String,
) -> Result<Option<String>, String> {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.starts_with('.')
    {
        return Err(format!("`{filename}` is not a filename"));
    }

    let mut dialog = app
        .dialog()
        .file()
        .set_title("Export graph")
        .set_file_name(&filename)
        .add_filter("Graphviz DOT", &["dot"]);
    if let Some(dir) = backend
        .project_dir
        .clone()
        .or_else(|| app.path().download_dir().ok())
    {
        dialog = dialog.set_directory(dir);
    }

    // `save_file` answers on a callback, and the command has to hold the reply
    // until it does — the client is waiting on this promise to know what to put
    // on the status line.
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog.save_file(move |chosen| {
        let _ = tx.send(chosen);
    });
    let Some(chosen) = rx
        .await
        .map_err(|e| format!("the save dialog closed unexpectedly: {e}"))?
    else {
        log::info!("[desktop] export cancelled at the save dialog");
        return Ok(None);
    };

    let path = chosen
        .into_path()
        .map_err(|e| format!("that is not a path this can write to: {e}"))?;
    std::fs::write(&path, contents)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    log::info!("[desktop] exported to {}", path.display());
    Ok(Some(path.display().to_string()))
}

fn main() {
    if let Err(e) = coon_core::logging::init_logging() {
        eprintln!("Failed to initialize logging: {}", e);
    }

    let args = Args::parse();

    // Drive Tauri and our LSP tasks from a single runtime, so the loader
    // spawned below shares a reactor with the tick loop and the IPC commands.
    let runtime = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());
    let _guard = runtime.enter();

    let core = match args.project_path.as_deref() {
        Some(path) => {
            if !std::path::Path::new(path).exists() {
                eprintln!("Error: Project path '{}' does not exist", path);
                std::process::exit(1);
            }
            // Checked here rather than left to the loader: a mistyped flavour
            // would otherwise open a window onto a load that failed, with the
            // list of what could have been typed buried in the log file.
            if let Some(flavour) = args.flavour.as_deref() {
                if let Err(message) = runtime.block_on(coon_core::check_flavour(
                    std::path::Path::new(path),
                    flavour,
                )) {
                    eprintln!("Error: {message}");
                    std::process::exit(1);
                }
            }
            log::info!("[desktop] starting LSP loader for {}", path);
            let options = LoaderOptions {
                flavour: args.flavour.clone(),
            };
            CoreApp::new_with_lsp(CallGraph::new(), spawn_lsp_bridge(path, options))
        }
        None => {
            if args.flavour.is_some() {
                eprintln!("Error: --flavour needs a project path; demo data has no builds");
                std::process::exit(1);
            }
            log::info!("[desktop] no project path given — using demo data");
            CoreApp::new(create_demo_call_graph())
        }
    };

    // Taken before the app is locked away: the tick loop below awaits it
    // between ticks, and it cannot take the mutex to ask.
    let response_ready = core.lsp.response_ready();

    let backend = Backend {
        core: Arc::new(Mutex::new(core)),
        router: Arc::new(Mutex::new(EventRouter::new())),
        project_dir: args.project_path.as_deref().map(std::path::PathBuf::from),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(backend)
        .invoke_handler(tauri::generate_handler![
            dispatch,
            request_snapshot,
            save_export
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
                // A run of reply-driven ticks must not be followed by a burst
                // of interval ticks making up for the ones they pre-empted.
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    // Whichever comes first. A reply between two interval
                    // ticks used to wait for the second; during the edge
                    // warmup that wait was most of the step — see
                    // `LspBridge::response_ready`.
                    tokio::select! {
                        _ = interval.tick() => {}
                        _ = response_ready.notified() => {}
                    }
                    let backend = handle.state::<Backend>();
                    let events = { backend.core.lock().await.tick() };
                    if events
                        .iter()
                        .any(|e| !matches!(e, AppEvent::LspProgress(_)))
                    {
                        log::debug!("[desktop] tick produced {} event(s)", events.len());
                    }
                    backend.pump(&handle, events).await;
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running COON desktop");
}
