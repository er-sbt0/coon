//! Shared frontend bootstrap: spawn the clangd loader and hand back a bridge.
//!
//! Every frontend needs the same wiring — a background `lsp_loader_task` plus
//! the channels it publishes — so it lives here rather than being duplicated
//! per binary.

use tokio::sync::{mpsc, oneshot};

use lsp::{LspRequest, LspResponse};
use model::lsp_status::LspUiMessage;

use crate::lsp_bridge::LspBridge;

/// Spawn the clangd loader for `project_path` and return a bridge wired to it.
///
/// The loader runs detached; the returned `LspBridge` starts out with no
/// channels and picks them up via its one-shot receiver once clangd is ready,
/// so the caller can start rendering immediately.
pub fn spawn_lsp_bridge(project_path: &str) -> LspBridge {
    let (ui_msg_tx, ui_msg_rx) = mpsc::unbounded_channel::<LspUiMessage>();
    let (lsp_channels_tx, lsp_channels_rx) = oneshot::channel::<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>();

    let project_path = project_path.to_string();
    tokio::spawn(async move {
        if let Err(e) =
            lsp::loader::lsp_loader_task(&project_path, ui_msg_tx, lsp_channels_tx).await
        {
            log::error!("LSP loader task failed: {}", e);
        }
    });

    LspBridge::new_with_loader(ui_msg_rx, lsp_channels_rx)
}
