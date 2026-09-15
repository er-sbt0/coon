//! Static client assets, embedded into the binary at compile time.
//!
//! Embedding (rather than serving `crates/web/static` from disk) means the
//! server works from any working directory and survives being installed as a
//! standalone binary. The desktop frontend embeds the same directory through
//! Tauri's own asset pipeline.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "static/"]
// The client's `CLAUDE.md` is guidance for whoever edits it, not an asset to
// serve or to carry in the binary.
#[exclude = "CLAUDE.md"]
struct Assets;

/// Serve an embedded asset, falling back to `index.html` for `/`.
pub async fn handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    // The client shipped with no cache validators at all — no
                    // `ETag`, no `Last-Modified`, no `Cache-Control` — which
                    // leaves a browser free to apply heuristic freshness and go
                    // on running an `app.js` from a previous run of the server.
                    // The symptom is a feature that is simply absent: no
                    // keybinding, no palette entry, no error. The assets live
                    // in the binary, so they change exactly when it does and
                    // there is nothing here worth caching across that.
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => {
            log::warn!("[web] asset not found: {}", path);
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
    }
}
