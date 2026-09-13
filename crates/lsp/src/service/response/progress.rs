//! clangd → us: `$/progress`, and the request that has to be answered before
//! any of it is sent.

use super::super::worker::LspWorkerState;
use super::super::{IndexProgress, LspResponse};
use serde_json::Value;
use tokio::sync::mpsc;

/// The work-done token clangd reports its background indexer under.
///
/// Hard-coded in clangd (`ClangdLSPServer::onBackgroundIndexProgress`), and the
/// only token it creates, but matching on it keeps a future second token — a
/// rename, say — from being read as indexing.
const BACKGROUND_INDEX_TOKEN: &str = "backgroundIndexProgress";

/// Answer `window/workDoneProgress/create`.
///
/// clangd sends this once, before the first `$/progress` for a token, and holds
/// the reporting until it is answered. There is nothing to decide — the reply
/// is an acknowledgement — but it is not optional.
pub(super) async fn handle_work_done_progress_create(message: Value, state: &mut LspWorkerState) {
    let Some(id) = message.get("id").cloned() else {
        log::warn!("LSP worker: window/workDoneProgress/create arrived with no id");
        return;
    };
    let token = message
        .pointer("/params/token")
        .and_then(|t| t.as_str())
        .unwrap_or("?");
    log::info!("LSP worker: clangd created progress token {token:?}, acknowledging");
    if let Err(e) = state.client.respond_ok(id).await {
        log::warn!("LSP worker: failed to acknowledge progress token {token:?}: {e}");
    }
}

/// Turn a `$/progress` notification for the background index into an
/// [`IndexProgress`], and forward it.
///
/// Progress for any other token is someone else's business and is dropped.
pub(super) async fn handle_progress(message: Value, response_tx: &mpsc::Sender<LspResponse>) {
    let token = message.pointer("/params/token").and_then(|t| t.as_str());
    if token != Some(BACKGROUND_INDEX_TOKEN) {
        log::trace!("LSP worker: $/progress for token {token:?}, ignored");
        return;
    }

    let Some(value) = message.pointer("/params/value") else {
        return;
    };

    let progress = match value.get("kind").and_then(|k| k.as_str()) {
        Some("begin") => IndexProgress::Started,
        // clangd's report message is the fraction itself — "37/217" — and is
        // the only place the totals appear; `percentage` is that same fraction
        // already rounded to an integer. Parse the message and fall back to
        // "started" rather than inventing a denominator.
        Some("report") => value
            .get("message")
            .and_then(|m| m.as_str())
            .and_then(parse_fraction)
            .map_or(IndexProgress::Started, |(done, total)| {
                IndexProgress::Indexing { done, total }
            }),
        Some("end") => IndexProgress::Finished,
        other => {
            log::debug!("LSP worker: unrecognised index progress kind {other:?}");
            return;
        }
    };

    log::debug!("LSP worker: background index {progress:?}");
    let _ = response_tx.send(LspResponse::IndexProgress(progress)).await;
}

/// Read clangd's `"37/217"` progress message.
fn parse_fraction(message: &str) -> Option<(usize, usize)> {
    let (done, total) = message.split_once('/')?;
    Some((done.trim().parse().ok()?, total.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_message_is_a_fraction() {
        assert_eq!(parse_fraction("37/217"), Some((37, 217)));
        assert_eq!(parse_fraction("0/1"), Some((0, 1)));
    }

    #[test]
    fn anything_else_is_not() {
        assert_eq!(parse_fraction("indexing"), None);
        assert_eq!(parse_fraction("37"), None);
        assert_eq!(parse_fraction("37/"), None);
    }
}
