use model::{ReferenceSymbolKind, ReferencingSymbol};
use lsp_types::{DocumentSymbol, Position, Range, SymbolInformation, SymbolKind};

/// Find the innermost symbol containing the given position
pub fn find_containing_symbol<'a>(
    symbols: &'a [DocumentSymbol],
    position: &Position,
) -> Option<&'a DocumentSymbol> {
    for symbol in symbols {
        if position_in_range(position, &symbol.range) {
            // Check children first (innermost)
            if let Some(children) = &symbol.children {
                if let Some(child_symbol) = find_containing_symbol(children, position) {
                    return Some(child_symbol);
                }
            }
            // If no child contains it, this symbol does
            return Some(symbol);
        }
    }
    None
}

/// Find the innermost symbol containing the given position from SymbolInformation
pub fn find_containing_symbol_info<'a>(
    symbols: &'a [SymbolInformation],
    position: &Position,
) -> Option<&'a SymbolInformation> {
    for symbol in symbols {
        if position_in_range(position, &symbol.location.range) {
            return Some(symbol);
        }
    }
    None
}

/// Check if a position is within a range
fn position_in_range(position: &Position, range: &Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

/// Convert DocumentSymbol to ReferencingSymbol
pub fn document_symbol_to_referencing_symbol(symbol: &DocumentSymbol) -> ReferencingSymbol {
    let qualified_name = if let Some(detail) = &symbol.detail {
        if detail.is_empty() {
            symbol.name.clone()
        } else {
            format!("{}::{}", detail, symbol.name)
        }
    } else {
        symbol.name.clone()
    };

    ReferencingSymbol {
        name: symbol.name.clone(),
        qualified_name,
        kind: convert_lsp_symbol_kind(symbol.kind),
    }
}

/// Convert SymbolInformation to ReferencingSymbol
pub fn symbol_info_to_referencing_symbol(symbol: &SymbolInformation) -> ReferencingSymbol {
    let qualified_name = if let Some(container) = &symbol.container_name {
        if container.is_empty() {
            symbol.name.clone()
        } else {
            format!("{}::{}", container, symbol.name)
        }
    } else {
        symbol.name.clone()
    };

    ReferencingSymbol {
        name: symbol.name.clone(),
        qualified_name,
        kind: convert_lsp_symbol_kind(symbol.kind),
    }
}

/// Convert LSP SymbolKind to our ReferenceSymbolKind
fn convert_lsp_symbol_kind(kind: SymbolKind) -> ReferenceSymbolKind {
    match kind {
        SymbolKind::FUNCTION => ReferenceSymbolKind::Function,
        SymbolKind::METHOD => ReferenceSymbolKind::Method,
        SymbolKind::CONSTRUCTOR => ReferenceSymbolKind::Constructor,
        SymbolKind::VARIABLE => ReferenceSymbolKind::Variable,
        SymbolKind::FIELD => ReferenceSymbolKind::Field,
        SymbolKind::CLASS => ReferenceSymbolKind::Class,
        SymbolKind::STRUCT => ReferenceSymbolKind::Struct,
        SymbolKind::MODULE | SymbolKind::NAMESPACE => ReferenceSymbolKind::Module,
        _ => ReferenceSymbolKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    #[test]
    fn test_position_in_range() {
        let range = Range {
            start: Position {
                line: 5,
                character: 10,
            },
            end: Position {
                line: 8,
                character: 20,
            },
        };

        // Position inside range
        assert!(position_in_range(
            &Position {
                line: 6,
                character: 15
            },
            &range
        ));

        // Position at start
        assert!(position_in_range(
            &Position {
                line: 5,
                character: 10
            },
            &range
        ));

        // Position at end
        assert!(position_in_range(
            &Position {
                line: 8,
                character: 20
            },
            &range
        ));

        // Position before range
        assert!(!position_in_range(
            &Position {
                line: 4,
                character: 15
            },
            &range
        ));

        // Position after range
        assert!(!position_in_range(
            &Position {
                line: 9,
                character: 15
            },
            &range
        ));

        // Position on same line but before start
        assert!(!position_in_range(
            &Position {
                line: 5,
                character: 5
            },
            &range
        ));

        // Position on same line but after end
        assert!(!position_in_range(
            &Position {
                line: 8,
                character: 25
            },
            &range
        ));
    }

    #[test]
    fn test_convert_lsp_symbol_kind() {
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::FUNCTION),
            ReferenceSymbolKind::Function
        );
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::METHOD),
            ReferenceSymbolKind::Method
        );
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::CLASS),
            ReferenceSymbolKind::Class
        );
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::VARIABLE),
            ReferenceSymbolKind::Variable
        );
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::MODULE),
            ReferenceSymbolKind::Module
        );
        assert_eq!(
            convert_lsp_symbol_kind(SymbolKind::ENUM),
            ReferenceSymbolKind::Unknown
        );
    }
}
