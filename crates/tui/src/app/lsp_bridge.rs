/// Re-export core's LspBridge types so existing TUI code continues to compile.
///
/// The implementation now lives in `crates/core/src/lsp_bridge.rs`.
pub use ::core::{LoadingState, LspBridge, PendingRequest};
