mod call_hierarchy;
pub mod client;
pub mod compile_commands;
pub mod loader;
mod parsing;
mod service;
pub mod types;

// Re-export primary types for backward compatibility
pub use client::LspClient;
pub use service::{IndexProgress, LspRequest, LspResponse, LspService, OPEN_DOCUMENT_LIMIT};
pub use types::*;
