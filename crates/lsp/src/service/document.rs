use super::worker::LspWorkerState;
use anyhow::Result;
use lsp_types::Url;

pub(super) async fn ensure_document_opened(
    state: &mut LspWorkerState,
    document_uri: &Url,
) -> Result<()> {
    if state.opened_documents.contains(document_uri) {
        log::debug!("Document already opened: {}", document_uri);
        return Ok(());
    }

    log::info!("Opening document in LSP server: {}", document_uri);

    let file_path = document_uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Invalid file URI: {}", document_uri))?;

    let content = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path.display(), e))?;

    log::debug!(
        "Read file content: {} bytes from {}",
        content.len(),
        file_path.display()
    );

    let language_id = match file_path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("c") => "c",
        Some("cpp") | Some("cxx") | Some("cc") => "cpp",
        Some("h") | Some("hpp") | Some("hxx") => "c",
        Some("py") => "python",
        Some("js") => "javascript",
        Some("ts") => "typescript",
        Some("java") => "java",
        Some("go") => "go",
        _ => "plaintext",
    };

    log::info!(
        "Sending didOpen notification: uri={}, language_id={}, version=1",
        document_uri,
        language_id
    );

    state
        .client
        .did_open(document_uri.clone(), language_id, 1, content)
        .await?;
    state.opened_documents.insert(document_uri.clone());

    log::info!(
        "Successfully opened document: {} (total opened: {})",
        document_uri,
        state.opened_documents.len()
    );

    Ok(())
}

pub(super) fn find_containing_symbol<'a>(
    symbols: &'a [lsp_types::DocumentSymbol],
    position: &lsp_types::Position,
) -> Option<&'a lsp_types::DocumentSymbol> {
    for symbol in symbols {
        if position_in_range(position, &symbol.range) {
            if let Some(children) = &symbol.children {
                if let Some(child_symbol) = find_containing_symbol(children, position) {
                    return Some(child_symbol);
                }
            }
            return Some(symbol);
        }
    }
    None
}

pub(super) fn position_in_range(position: &lsp_types::Position, range: &lsp_types::Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

pub(super) fn get_qualified_symbol_name(symbol: &lsp_types::DocumentSymbol) -> String {
    if let Some(detail) = &symbol.detail {
        if !detail.is_empty() {
            format!("{}::{}", detail, symbol.name)
        } else {
            format!("::{}", symbol.name)
        }
    } else {
        format!("::{}", symbol.name)
    }
}

pub(super) fn convert_lsp_symbol_kind(kind: lsp_types::SymbolKind) -> model::ReferenceSymbolKind {
    match kind {
        lsp_types::SymbolKind::FUNCTION => model::ReferenceSymbolKind::Function,
        lsp_types::SymbolKind::METHOD => model::ReferenceSymbolKind::Method,
        lsp_types::SymbolKind::CONSTRUCTOR => model::ReferenceSymbolKind::Constructor,
        lsp_types::SymbolKind::VARIABLE => model::ReferenceSymbolKind::Variable,
        _ => model::ReferenceSymbolKind::Function,
    }
}
