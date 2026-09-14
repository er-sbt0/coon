use std::collections::HashSet;

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
pub fn apply_event_to_status(
    status: &mut crate::status_message::StatusMessage,
    event: &AppEvent,
) {
    if let Some(msg) = app_event_to_status(event) {
        *status = msg;
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
                    .and_then(|w| w.root_symbol);
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
            AppEvent::LayoutDirty => {
                // Sync direction from the domain workspace into the view state
                // and mark it dirty so the renderer recomputes the layout.
                if let Some(core_ws) = app.core.workspaces.current() {
                    let dir = core_ws.direction;
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
