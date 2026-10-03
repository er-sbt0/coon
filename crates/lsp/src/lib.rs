mod call_hierarchy;
pub mod clangd_database;
pub mod client;
pub mod loader;
mod parsing;
mod service;
pub mod types;

// Re-export primary types for backward compatibility
pub use client::LspClient;
pub use loader::LoaderOptions;
pub use service::{
    clangd_rss_budget_bytes, open_document_limit, IndexProgress, DISCOVERY_CHUNK, LspRequest, LspResponse, LspService,
    OPEN_DOCUMENT_LIMIT,
};
pub use types::*;
