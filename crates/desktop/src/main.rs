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
use tokio::sync::Mutex;

use coon_core::{
    create_demo_call_graph, snapshot_of, spawn_lsp_bridge, AppEvent, Command, CoreApp, EventRouter,
    ServerMsg,
};
use model::CallGraph;

/// Event channel the client listens on. Mirrors the web frontend's socket.
const CHANNEL: &str = "coon://message";

/// Poll interval for `CoreApp::tick`, matching the web frontend's cadence.
const TICK_MS: u64 = 100;

#[derive(Parser)]
#[command(
    name = "coon-desktop",
    about = "COON - C/C++ call graph visualizer (desktop)"
)]
struct Args {
    /// Path to the C/C++ project directory (omit to run with demo data)
    project_path: Option<String>,
}

/// Shared backend state. One window, one `CoreApp` — unlike the web frontend
/// there is no multi-client question to answer.
struct Backend {
    core: Arc<Mutex<CoreApp>>,
    router: Arc<Mutex<EventRouter>>,
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
            let msg = snapshot_of(&*self.core.lock().await);
            emit(app, &msg);
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
    let msg = snapshot_of(&*backend.core.lock().await);
    emit(&app, &msg);
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
            log::info!("[desktop] starting LSP loader for {}", path);
            CoreApp::new_with_lsp(CallGraph::new(), spawn_lsp_bridge(path))
        }
        None => {
            log::info!("[desktop] no project path given — using demo data");
            CoreApp::new(create_demo_call_graph())
        }
    };

    let backend = Backend {
        core: Arc::new(Mutex::new(core)),
        router: Arc::new(Mutex::new(EventRouter::new())),
    };

    tauri::Builder::default()
        .manage(backend)
        .invoke_handler(tauri::generate_handler![dispatch, request_snapshot])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
                loop {
                    interval.tick().await;
                    let backend = handle.state::<Backend>();
                    let events = { backend.core.lock().await.tick() };
                    if events.iter().any(|e| !matches!(e, AppEvent::LspProgress(_))) {
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
