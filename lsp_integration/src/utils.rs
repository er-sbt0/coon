use std::path::Path;

// Helper function to find clangd executable
pub fn find_clangd() -> Option<String> {
    // Check common locations for clangd
    let locations = vec![
        "/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd",
        "/usr/bin/clangd",
        "/usr/local/bin/clangd",
        "clangd", // In PATH
    ];

    for location in locations {
        // If it's a full path, check if it exists
        if Path::new(location).exists() && !location.contains("/") {
            return Some(location.to_string());
        }

        // If it's just a name, check if it's in PATH
        if !location.contains("/") {
            if let Ok(output) = std::process::Command::new("which").arg(location).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() {
                        return Some(location.to_string());
                    }
                }
            }
        }
    }

    // Look for clangd with version suffixes
    for version in 12..=18 {
        let binary = format!("clangd-{}", version);
        if let Ok(output) = std::process::Command::new("which").arg(&binary).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(binary);
                }
            }
        }
    }

    None
}

/// Find the innermost symbol containing the given position
pub fn find_containing_symbol(
    symbols: &[lsp_types::DocumentSymbol],
    position: &lsp_types::Position,
) -> Option<model::ReferencingSymbol> {
    for symbol in symbols {
        if position_in_range(position, &symbol.range) {
            // Check children first (innermost)
            if let Some(children) = &symbol.children {
                if let Some(child_symbol) = find_containing_symbol(children, position) {
                    return Some(child_symbol);
                }
            }
            // If no child contains it, this symbol does
            return Some(document_symbol_to_referencing_symbol(symbol));
        }
    }
    None
}

fn position_in_range(position: &lsp_types::Position, range: &lsp_types::Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

pub fn document_symbol_to_referencing_symbol(
    symbol: &lsp_types::DocumentSymbol,
) -> model::ReferencingSymbol {
    model::ReferencingSymbol {
        name: symbol.name.clone(),
        qualified_name: symbol.detail.as_deref().unwrap_or(&symbol.name).to_string(),
        kind: convert_lsp_symbol_kind(symbol.kind),
    }
}

fn convert_lsp_symbol_kind(kind: lsp_types::SymbolKind) -> model::ReferenceSymbolKind {
    match kind {
        lsp_types::SymbolKind::FUNCTION => model::ReferenceSymbolKind::Function,
        lsp_types::SymbolKind::METHOD => model::ReferenceSymbolKind::Method,
        lsp_types::SymbolKind::CONSTRUCTOR => model::ReferenceSymbolKind::Constructor,
        lsp_types::SymbolKind::VARIABLE => model::ReferenceSymbolKind::Variable,
        _ => model::ReferenceSymbolKind::Function, // Default fallback
    }
}
