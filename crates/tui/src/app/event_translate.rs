use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ::core::AppEvent;

use crate::graph_view::GraphViewState;
use crate::status_message::StatusMessage;

use super::App;

/// Translate a `core::AppEvent` into a `StatusMessage` for display in the TUI.
///
/// Returns `None` for events that do not have a meaningful TUI status message
/// (e.g. `LayoutDirty`, which is a state-mutation signal, not a user-visible message).
pub fn app_event_to_status(event: &AppEvent) -> Option<StatusMessage> {
    match event {
        // Graph data
        AppEvent::GraphSymbolsUpdated { count } => {
            Some(StatusMessage::FunctionsLoadedFromWorkspace { count: *count })
        }
        AppEvent::OutgoingCallsLoaded { .. } => Some(StatusMessage::OutgoingCallsLoaded),
        AppEvent::IncomingCallsLoaded { .. } => Some(StatusMessage::IncomingCallsLoaded),
        AppEvent::ReferencesFound { count, .. } => Some(StatusMessage::ReferencesFound {
            count: *count,
            name: None,
        }),
        AppEvent::NoReferencesFound { .. } => Some(StatusMessage::NoReferencesFound {
            name: "function".to_string(),
        }),
        AppEvent::FunctionHasNoCallees => Some(StatusMessage::FunctionHasNoCallees),
        AppEvent::LayoutDirty => None,
        // Nothing in the TUI can ask for a map, scoped or otherwise, so nothing
        // here can be told it was refused.
        AppEvent::FunctionHasNoCommunities => None,
        // The TUI draws no cluster map; Phase 5 gives the map a canvas, and
        // until there is one a recount is nothing the status line can report.
        AppEvent::ClusteringUpdated { .. } => None,

        // Export. `GraphExported` is deliberately not translated here: the
        // status line has to name the file that was written, and which file
        // that is only becomes known in `apply_events`, where it is written.
        AppEvent::GraphExported { .. } => None,
        AppEvent::NothingToExport => Some(StatusMessage::NothingToExport),

        // LSP lifecycle
        AppEvent::LspProgress(_) => None,
        // Like the load phase, the warmup has its own bar above the graph,
        // which reads the progress off `CoreApp` directly. Routing it through
        // the transient status line as well would overwrite whatever the user
        // last did, ten times a second, for the whole warmup.
        AppEvent::ResolvingCalls { .. } => None,
        AppEvent::LspRequestTimedOut => Some(StatusMessage::LspRequestTimedOut),
        AppEvent::LoadingCalls { direction } => Some(StatusMessage::LoadingCalls {
            direction: direction.clone(),
        }),
        AppEvent::FindingReferences { name } => {
            Some(StatusMessage::FindingReferences { name: name.clone() })
        }
        AppEvent::RefreshingWorkspaceSymbols => Some(StatusMessage::RefreshingWorkspaceSymbols),
        AppEvent::WorkspaceSymbolsLoaded { count } => {
            Some(StatusMessage::WorkspaceSymbolsLoaded { count: *count })
        }

        // Workspace lifecycle
        AppEvent::WorkspaceCreated { id, .. } => Some(StatusMessage::CreatedWorkspace { id: *id }),
        AppEvent::WorkspaceClosed => Some(StatusMessage::WorkspaceClosed),
        AppEvent::WorkspaceSwitched { name, .. } => {
            Some(StatusMessage::SwitchedToWorkspace { name: name.clone() })
        }
        AppEvent::WorkspaceRenamed { name } => {
            Some(StatusMessage::RenamedWorkspace { name: name.clone() })
        }
        AppEvent::CannotCloseLastWorkspace => Some(StatusMessage::CannotCloseLastWorkspace),
        AppEvent::InvalidWorkspaceIndex => Some(StatusMessage::InvalidWorkspaceIndex),

        // Errors
        AppEvent::InvalidFilePath => Some(StatusMessage::InvalidFilePath),
        AppEvent::FailedToCreateUri => Some(StatusMessage::FailedToCreateUri),
        AppEvent::FailedToSendRequest => Some(StatusMessage::FailedToSendRequest),
        AppEvent::NoLspChannel => Some(StatusMessage::NoLspChannel),
        AppEvent::LspError(error) => Some(StatusMessage::LspError {
            error: error.clone(),
        }),
    }
}

/// Apply a single `AppEvent` to the TUI status message.
///
/// Only updates if the event has a displayable translation.
pub fn apply_event_to_status(status: &mut crate::status_message::StatusMessage, event: &AppEvent) {
    if let Some(msg) = app_event_to_status(event) {
        *status = msg;
    }
}

/// Write one export into `dir`, and say what happened.
///
/// Split out from the event handling so the path logic is testable without a
/// test having to move the process's working directory out from under every
/// other test in the binary.
///
/// The status message names the full path rather than the bare filename: the
/// TUI writes into whatever directory it was started in, and "Exported
/// main.dot" leaves the user hunting for it.
fn save_export(dir: &Path, filename: &str, contents: &str) -> StatusMessage {
    let path = dir.join(filename);
    match std::fs::write(&path, contents) {
        Ok(()) => StatusMessage::GraphExported {
            path: path.display().to_string(),
        },
        Err(e) => StatusMessage::Error {
            error: format!("could not write {}: {e}", path.display()),
        },
    }
}

/// Apply a batch of `AppEvent`s to the full `App` state.
///
/// Handles view-state mutations (create / remove `GraphViewState`, mark layout
/// dirty, sync direction) and then updates the status message for any event
/// that has a displayable translation.
pub fn apply_events(app: &mut App, events: Vec<AppEvent>) {
    for event in &events {
        match event {
            AppEvent::WorkspaceCreated { id, .. } => {
                // Create a new view state, seeding its root from the domain workspace.
                let root = app
                    .core
                    .workspaces
                    .workspaces
                    .iter()
                    .find(|w| w.id == *id)
                    .and_then(|w| w.root_symbol());
                let mut vs = GraphViewState::new(&app.layout_settings);
                if let Some(sym) = root {
                    vs.set_root(sym);
                }
                app.view_states.insert(*id, vs);
            }
            AppEvent::WorkspaceClosed => {
                // Remove view_states for workspaces that no longer exist.
                let live: HashSet<usize> = app
                    .core
                    .workspaces
                    .workspaces
                    .iter()
                    .map(|w| w.id)
                    .collect();
                app.view_states.retain(|id, _| live.contains(id));
            }
            AppEvent::GraphExported { filename, contents } => {
                // `core` builds the file and stops; where it goes is the
                // frontend's to decide, and for a terminal program that is the
                // directory it was started in.
                let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                app.status_message = save_export(&dir, filename, contents);
            }
            AppEvent::LayoutDirty => {
                // Sync direction from the domain workspace into the view state
                // and mark it dirty so the renderer recomputes the layout.
                if let Some(core_ws) = app.core.workspaces.current() {
                    let dir = core_ws.direction();
                    let id = core_ws.id;
                    if let Some(vs) = app.view_states.get_mut(&id) {
                        vs.direction = dir;
                        vs.mark_layout_dirty();
                    }
                }
            }
            _ => {}
        }
        // Update status message (last non-None wins).
        apply_event_to_status(&mut app.status_message, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, since the export deliberately writes to
    /// a real path rather than somewhere injectable.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("coon-export-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_export_lands_on_disk_and_the_status_line_says_where() {
        let dir = scratch("ok");
        let status = save_export(&dir, "main.dot", "digraph coon {}\n");

        assert_eq!(
            std::fs::read_to_string(dir.join("main.dot")).unwrap(),
            "digraph coon {}\n"
        );
        match status {
            StatusMessage::GraphExported { path } => assert!(
                path.ends_with("main.dot") && path.contains("coon-export-test-ok"),
                "the whole path, not the bare name: {path}"
            ),
            other => panic!("expected an export, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_is_reported_rather_than_swallowed() {
        let dir = scratch("fail").join("no-such-directory");
        let status = save_export(&dir, "main.dot", "digraph coon {}");

        assert!(
            matches!(status, StatusMessage::Error { .. }),
            "a silent failure looks exactly like a successful export: {status:?}"
        );
    }

    #[test]
    fn nothing_to_export_reaches_the_status_line() {
        assert_eq!(
            app_event_to_status(&AppEvent::NothingToExport),
            Some(StatusMessage::NothingToExport)
        );
        // The export itself does not, because only `apply_events` knows the
        // path it ended up at.
        assert_eq!(
            app_event_to_status(&AppEvent::GraphExported {
                filename: "x.dot".into(),
                contents: String::new(),
            }),
            None
        );
    }
}
