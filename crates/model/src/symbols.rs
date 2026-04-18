use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for symbols in the call graph
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub Uuid);

impl SymbolId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SymbolId {
    fn default() -> Self {
        Self::new()
    }
}

/// Location information for symbols
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub length: Option<u32>,
}

impl Location {
    pub fn new(file_path: String, line: u32, column: u32) -> Self {
        Self {
            file_path,
            line,
            column,
            length: None,
        }
    }

    pub fn with_length(mut self, length: u32) -> Self {
        self.length = Some(length);
        self
    }
}

/// Enhanced reference information that includes symbol context
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub location: Location,
    pub referencing_symbol: Option<ReferencingSymbol>,
}

/// Information about the symbol that is making a reference
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferencingSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: ReferenceSymbolKind,
}

/// Types of symbols that can make references
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReferenceSymbolKind {
    Function,
    Method,
    Constructor,
    Variable,
    Field,
    Class,
    Struct,
    Module,
    Unknown,
}

/// Diagnostic information
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub location: Location,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_id_creation() {
        let id1 = SymbolId::new();
        let id2 = SymbolId::new();
        assert_ne!(id1, id2);
    }
}
