use std::error::Error;
use std::fmt;

/// Errors that can occur during layout computation
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    /// Invalid node index referenced
    InvalidNode(usize),

    /// Tree structure is invalid
    InvalidTree(String),

    /// Configuration error
    ConfigError(String),

    /// Empty tree provided
    EmptyTree,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::InvalidNode(id) => {
                write!(f, "Invalid node index: {}", id)
            }
            LayoutError::InvalidTree(msg) => {
                write!(f, "Invalid tree structure: {}", msg)
            }
            LayoutError::ConfigError(msg) => {
                write!(f, "Configuration error: {}", msg)
            }
            LayoutError::EmptyTree => {
                write!(f, "Empty tree provided")
            }
        }
    }
}

impl Error for LayoutError {}
