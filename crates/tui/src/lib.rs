//! Ratatui terminal frontend. **Deprecated** — kept working, no longer
//! developed; new frontend work goes to `desktop` and the client it shares with
//! `web/static`. It is still a member of the workspace and still tested, and
//! `coon`'s layout flags still drive it, but a feature that lands in the
//! desktop app is not expected to land here.
//!
//! Nothing here is `#[deprecated]` as an attribute: the only caller is `coon`,
//! which is deprecated alongside it, so the annotation would buy warnings in
//! code that already carries the notice and nowhere else.

pub mod actions;
pub mod app;
pub mod graph_adapter;
pub mod graph_view;
pub mod key_map;
pub mod rendering;
pub mod search_bar;
pub mod status_message;
pub mod tui;

pub use actions::Action;
pub use app::{App, LoadingState, PendingRequest};
pub use graph_adapter::{CallDirection, CallGraphAdapter};
pub use graph_view::{GraphView, GraphViewState, LayoutSettings};
pub use rendering::ui;
pub use search_bar::{SearchBar, SearchBarState};
pub use status_message::StatusMessage;
pub use tui::TuiApp;
