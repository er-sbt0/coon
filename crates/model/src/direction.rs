use serde::{Deserialize, Serialize};

/// Direction for building a call hierarchy tree.
///
/// This is a domain concept used by both `core` and `tui`.
/// Defined here in `model` so all crates can reference it without depending on `core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CallDirection {
    /// Show who this function calls (callees / outgoing)
    Outgoing,
    /// Show who calls this function (callers / incoming)
    Incoming,
}
