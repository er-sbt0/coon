//! Shared frontend bootstrap: spawn the clangd loader and hand back a bridge.
//!
//! Every frontend needs the same wiring — a background `lsp_loader_task` plus
//! the channels it publishes — so it lives here rather than being duplicated
//! per binary.

use tokio::sync::{mpsc, oneshot};

use lsp::{LoaderOptions, LspRequest, LspResponse};
use model::lsp_status::LspUiMessage;

use crate::lsp_bridge::LspBridge;

/// Spawn the clangd loader for `project_path` and return a bridge wired to it.
///
/// `options` is what the frontend's launch flags decided — today, which build
/// of the project to analyse. It travels as one value so that a flag added to
/// one frontend costs the others nothing but a `Default`.
///
/// The loader runs detached; the returned `LspBridge` starts out with no
/// channels and picks them up via its one-shot receiver once clangd is ready,
/// so the caller can start rendering immediately.
///
/// The worker's replies do not reach the bridge directly. They pass through
/// a relay task that copies each one onto the channel the bridge reads and
/// then raises [`LspBridge::response_ready`], so a frontend awaiting that
/// signal ticks the moment an answer lands rather than at its next poll. The
/// relay is here rather than in `lsp` because the signal is the bridge's,
/// and `lsp` does not know a bridge exists; it costs one extra channel hop
/// per reply, which is nothing against the poll interval it saves.
pub fn spawn_lsp_bridge(project_path: &str, options: LoaderOptions) -> LspBridge {
    let (ui_msg_tx, ui_msg_rx) = mpsc::unbounded_channel::<LspUiMessage>();
    let (lsp_channels_tx, lsp_channels_rx) = oneshot::channel::<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>();
    let (relayed_tx, relayed_rx) = oneshot::channel();

    let project_path = project_path.to_string();
    tokio::spawn(async move {
        if let Err(e) =
            lsp::loader::lsp_loader_task(&project_path, options, ui_msg_tx, lsp_channels_tx).await
        {
            log::error!("LSP loader task failed: {}", e);
        }
    });

    let bridge = LspBridge::new_with_loader(ui_msg_rx, relayed_rx);
    let response_ready = bridge.response_ready();
    tokio::spawn(async move {
        let Ok((mut worker_rx, request_tx)) = lsp_channels_rx.await else {
            // The loader failed before it wired anything up; it has logged
            // why, and the bridge sees the same dropped sender it always did.
            return;
        };
        let (response_tx, response_rx) = mpsc::unbounded_channel::<LspResponse>();
        if relayed_tx.send((response_rx, request_tx)).is_err() {
            return;
        }
        while let Some(response) = worker_rx.recv().await {
            if response_tx.send(response).is_err() {
                break;
            }
            response_ready.notify_one();
        }
    });

    bridge
}

/// Check a `--flavour` before anything is spawned, so a frontend can refuse to
/// start rather than open onto a load that is going to fail.
///
/// Here beside the bootstrap for the same reason the bootstrap is here: a
/// frontend talks to `core`, and which builds a project holds is a question
/// about a load, not about clangd. The error is a finished message, listing the
/// files the user could have named instead.
pub async fn check_flavour(
    project_path: &std::path::Path,
    flavour: &std::path::Path,
) -> Result<(), String> {
    lsp::compile_commands::check_flavour(project_path, flavour).await
}
