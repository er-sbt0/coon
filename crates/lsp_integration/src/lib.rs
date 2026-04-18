mod call_hierarchy;
pub mod client;
pub mod compile_commands;
pub mod loader;
pub mod parsing;
mod service;
pub mod types;

// Re-export primary types for backward compatibility
pub use client::LspClient;
pub use parsing::{
    extract_function_name_from_signature, extract_text_from_marked_string,
    extract_text_from_markup, parse_hover_response_impl,
};
pub use service::{LspRequest, LspResponse, LspService};
pub use types::*;
