use ::core::AppEvent;
use model::CallGraph;

use crate::status_message::StatusMessage;

/// Translate a `core::AppEvent` into a `StatusMessage` for display in the TUI.
///
/// Returns `None` for events that do not have a meaningful TUI status message
/// (e.g. `LayoutDirty`, which is a state-mutation signal, not a user-visible message).
pub fn app_event_to_status(event: &AppEvent, _graph: &CallGraph) -> Option<StatusMessage> {
    match event {
        // Graph data
        AppEvent::GraphSymbolsUpdated { count } => {
            Some(StatusMessage::FunctionsLoadedFromWorkspace { count: *count })
        }
        AppEvent::OutgoingCallsLoaded { .. } => Some(StatusMessage::OutgoingCallsLoaded),
        AppEvent::IncomingCallsLoaded { .. } => Some(StatusMessage::IncomingCallsLoaded),
        AppEvent::ReferencesFound { count, .. } => Some(StatusMessage::ReferencesFound {
            count: *count,
            name: None, // name lookup could use _graph if needed
        }),
        AppEvent::NoReferencesFound { .. } => Some(StatusMessage::NoReferencesFound {
            name: "function".to_string(),
        }),
        AppEvent::FunctionHasNoCallees => Some(StatusMessage::FunctionHasNoCallees),
        AppEvent::LayoutDirty => None,

        // LSP lifecycle
        AppEvent::LspProgress(_) => None,
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

/// Apply a slice of `AppEvent`s to the TUI's `status_message`.
///
/// Only the last non-None translation wins.
pub fn apply_event_to_status(
    status: &mut crate::status_message::StatusMessage,
    event: &AppEvent,
    graph: &CallGraph,
) {
    if let Some(msg) = app_event_to_status(event, graph) {
        *status = msg;
    }
}
