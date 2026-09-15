//! Axum server frontend. **Deprecated** — kept working, no longer developed;
//! `coon --web <PORT>` still serves the client over a WebSocket, but the
//! desktop app is the frontend now.
//!
//! The Rust in this crate is what is deprecated, not `static/`: those assets
//! are the *shared* client, and `desktop` ships them verbatim as its
//! `frontendDist`. Client work belongs there as much as it ever did — see
//! `transport.js`, which is the only file that knows which pipe it is on.

pub mod assets;
pub mod protocol;
pub mod routes;
pub mod session;

pub use routes::{router, AppState};
