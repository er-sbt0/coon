use anyhow::Result;
use lsp_types as lsp;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::types::{
    convert_lsp_location, make_qualified_name, DocumentSymbolResponse, FindReferencesResponse,
    WorkspaceSymbolResponse,
};

// ---------------------------------------------------------------------------
// Generic LSP response parsing helpers
// ---------------------------------------------------------------------------

/// Core of LSP response parsing after ID extraction and method matching.
///
/// Handles the common pattern: check for error → check for null result →
/// deserialize → build typed response.  Callers that need multi-method
/// matching or custom deserialization can call this directly after doing
/// their own ID / method checks.
pub(crate) fn parse_lsp_result<T, R>(
    response: &Value,
    method_label: &str,
    build_result: impl FnOnce(T) -> R,
    build_empty: impl FnOnce() -> R,
) -> Result<Option<R>>
where
    T: for<'de> Deserialize<'de>,
{
    if let Some(error) = response.get("error") {
        log::warn!("LSP error for {}: {:?}", method_label, error);
        return Ok(Some(build_empty()));
    }

    match response.get("result") {
        Some(result) if !result.is_null() => match T::deserialize(result) {
            Ok(parsed) => Ok(Some(build_result(parsed))),
            Err(e) => {
                let msg = format!(
                    "Failed to parse {} result: {}. Raw result: {:?}",
                    method_label, e, result
                );
                log::error!("{}", msg);
                Err(anyhow::anyhow!(msg))
            }
        },
        _ => {
            log::warn!("No result in {} response", method_label);
            Ok(Some(build_empty()))
        }
    }
}

/// Full LSP response parser: extract ID, match a single method name,
/// remove from pending requests, then delegate to [`parse_lsp_result`].
///
/// Only removes the pending request entry when the method actually matches,
/// so other parsers can still try the same response in the legacy
/// detection path.
pub(crate) fn parse_lsp_response<T, R>(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
    method_name: &str,
    build_result: impl FnOnce(T) -> R,
    build_empty: impl FnOnce() -> R,
) -> Result<Option<R>>
where
    T: for<'de> Deserialize<'de>,
{
    let id = match response.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return Ok(None),
    };

    // Only remove from pending when the method matches (fixes legacy
    // detection path where multiple parsers try the same response).
    match pending_requests.get(&id) {
        Some(method) if method == method_name => {
            pending_requests.remove(&id);
        }
        _ => return Ok(None),
    }

    parse_lsp_result(response, method_name, build_result, build_empty)
}

// ---------------------------------------------------------------------------
// Newtype wrappers for custom deserialization
// ---------------------------------------------------------------------------

/// Handles both `DocumentSymbol[]` and legacy `SymbolInformation[]` response
/// shapes so the generic [`parse_lsp_response`] can be used directly.
struct RawDocumentSymbols(Vec<lsp::DocumentSymbol>);

impl<'de> Deserialize<'de> for RawDocumentSymbols {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if let Ok(doc_symbols) = Vec::<lsp::DocumentSymbol>::deserialize(&value) {
            Ok(RawDocumentSymbols(doc_symbols))
        } else if let Ok(symbol_infos) = Vec::<lsp::SymbolInformation>::deserialize(&value) {
            Ok(RawDocumentSymbols(convert_symbol_info_to_document_symbols(
                &symbol_infos,
            )))
        } else {
            Err(serde::de::Error::custom(
                "Failed to parse as either DocumentSymbol[] or SymbolInformation[]",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Concrete response parsers
// ---------------------------------------------------------------------------

pub(crate) fn parse_find_references_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<FindReferencesResponse>> {
    let id = match response.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return Ok(None),
    };

    match pending_requests.get(&id).map(|s| s.as_str()) {
        Some("textDocument/references") => {
            pending_requests.remove(&id);
        }
        _ => return Ok(None),
    }

    parse_lsp_result(
        response,
        "textDocument/references",
        |lsp_locations: Vec<lsp::Location>| FindReferencesResponse {
            locations: lsp_locations.iter().map(convert_lsp_location).collect(),
        },
        || FindReferencesResponse {
            locations: Vec::new(),
        },
    )
}

pub(crate) fn parse_workspace_symbol_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<WorkspaceSymbolResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "workspace/symbol",
        |workspace_symbols: Vec<lsp::WorkspaceSymbol>| WorkspaceSymbolResponse {
            symbols: parse_workspace_symbols_from_result(workspace_symbols),
        },
        || WorkspaceSymbolResponse {
            symbols: Vec::new(),
        },
    )
}

pub(crate) fn parse_document_symbol_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<DocumentSymbolResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "textDocument/documentSymbol",
        |RawDocumentSymbols(doc_symbols)| {
            let symbols = doc_symbols
                .into_iter()
                .flat_map(|doc_symbol| convert_document_symbol_recursive(&doc_symbol, None))
                .collect();
            DocumentSymbolResponse { symbols }
        },
        || DocumentSymbolResponse {
            symbols: Vec::new(),
        },
    )
}

// ---------------------------------------------------------------------------
// Shared document-symbol deserialization helpers
// ---------------------------------------------------------------------------

/// Deserialize a `documentSymbol` **result** value (not the full response
/// envelope) into `Vec<lsp::DocumentSymbol>`.  Handles the two shapes
/// clangd may return: `DocumentSymbol[]` (preferred) or the legacy
/// `SymbolInformation[]` (converted into `DocumentSymbol` for a uniform
/// return type).
pub(crate) fn parse_document_symbols_from_result(
    result: &Value,
) -> Result<Vec<lsp::DocumentSymbol>> {
    if let Ok(doc_symbols) = Vec::<lsp::DocumentSymbol>::deserialize(result) {
        Ok(doc_symbols)
    } else if let Ok(symbol_infos) = Vec::<lsp::SymbolInformation>::deserialize(result) {
        Ok(convert_symbol_info_to_document_symbols(&symbol_infos))
    } else {
        let msg = format!(
            "Failed to parse documentSymbol result as either DocumentSymbol[] or SymbolInformation[]. Raw result: {:?}",
            result
        );
        log::error!("{}", msg);
        Err(anyhow::anyhow!(msg))
    }
}

/// Convert legacy `SymbolInformation[]` into `DocumentSymbol[]`.
///
/// This is the single canonical implementation — used by both the
/// `LspClient` parsing path and the `LspService` response handlers.
pub(crate) fn convert_symbol_info_to_document_symbols(
    symbol_infos: &[lsp::SymbolInformation],
) -> Vec<lsp::DocumentSymbol> {
    symbol_infos
        .iter()
        .map(|info| lsp::DocumentSymbol {
            name: info.name.clone(),
            detail: info.container_name.clone(),
            kind: info.kind,
            tags: info.tags.clone(),
            #[allow(deprecated)]
            deprecated: info.deprecated,
            range: info.location.range,
            selection_range: info.location.range,
            children: None,
        })
        .collect()
}

/// Deserialize a workspace/symbol **result** value into
/// `Vec<model::WorkspaceSymbolInfo>`.  This is the canonical conversion
/// from the raw LSP JSON to our model type, used by both the `LspClient`
/// parsing path and the `LspService` response handlers.
pub(crate) fn parse_workspace_symbols_from_result(
    workspace_symbols: Vec<lsp::WorkspaceSymbol>,
) -> Vec<model::WorkspaceSymbolInfo> {
    workspace_symbols
        .iter()
        .map(|symbol| {
            let location = match &symbol.location {
                lsp::OneOf::Left(location) => convert_lsp_location(location),
                lsp::OneOf::Right(workspace_location) => model::Location {
                    file_path: workspace_location
                        .uri
                        .to_file_path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| workspace_location.uri.to_string()),
                    line: 0,
                    column: 0,
                    length: None,
                },
            };
            model::WorkspaceSymbolInfo {
                name: symbol.name.clone(),
                qualified_name: make_qualified_name(&symbol.container_name, &symbol.name),
                kind: symbol.kind,
                location,
                container_name: symbol.container_name.clone(),
            }
        })
        .collect()
}

// Helper to recursively convert DocumentSymbol to model::WorkspaceSymbolInfo
pub(crate) fn convert_document_symbol_recursive(
    doc_symbol: &lsp::DocumentSymbol,
    container: Option<&str>,
) -> Vec<model::WorkspaceSymbolInfo> {
    let mut results = Vec::new();

    let container_name = container.map(|s| s.to_string());
    // Convert the current symbol
    let symbol_info = model::WorkspaceSymbolInfo {
        name: doc_symbol.name.clone(),
        qualified_name: make_qualified_name(&container_name, &doc_symbol.name),
        kind: doc_symbol.kind,
        location: model::Location {
            file_path: "".to_string(), // Will be filled in by caller
            line: doc_symbol.selection_range.start.line,
            column: doc_symbol.selection_range.start.character,
            length: Some(
                doc_symbol.selection_range.end.character
                    - doc_symbol.selection_range.start.character,
            ),
        },
        container_name,
    };
    results.push(symbol_info);

    // Recursively process children
    if let Some(children) = &doc_symbol.children {
        for child in children {
            let mut child_symbols =
                convert_document_symbol_recursive(child, Some(&doc_symbol.name));
            results.append(&mut child_symbols);
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range, Url};
    use serde_json::Value;

    // Helper to create a mock LSP response
    fn create_mock_find_references_response(id: i64, locations: Vec<lsp::Location>) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": locations
        })
    }

    // Helper to create a mock error response
    fn create_mock_error_response(id: i64, code: i32, message: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message
            }
        })
    }

    // Helper to create a mock empty response
    fn create_mock_empty_response(id: i64) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": null
        })
    }

    #[test]
    fn test_parse_find_references_response_with_results() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(42, "textDocument/references".to_string());

        // Create a mock client structure for parsing only
        let mut client_data = (pending_requests,);

        let uri = Url::parse("file:///home/user/test.rs").unwrap();
        let mock_locations = vec![
            lsp::Location {
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 11,
                    },
                },
            },
            lsp::Location {
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: 5,
                        character: 10,
                    },
                    end: Position {
                        line: 5,
                        character: 18,
                    },
                },
            },
        ];

        let mock_response = create_mock_find_references_response(42, mock_locations);

        // Test the parsing function directly
        let result =
            parse_find_references_response_impl(&mut client_data.0, &mock_response).unwrap();

        assert!(result.is_some());
        let response = result.unwrap();
        assert_eq!(response.locations.len(), 2);

        assert_eq!(response.locations[0].file_path, "/home/user/test.rs");
        assert_eq!(response.locations[0].line, 1);
        assert_eq!(response.locations[0].column, 4);
        assert_eq!(response.locations[0].length, Some(8));

        assert_eq!(response.locations[1].file_path, "/home/user/test.rs");
        assert_eq!(response.locations[1].line, 6);
        assert_eq!(response.locations[1].column, 11);
        assert_eq!(response.locations[1].length, Some(8));
    }

    #[test]
    fn test_parse_find_references_response_empty() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(42, "textDocument/references".to_string());

        let mock_response = create_mock_empty_response(42);
        let result =
            parse_find_references_response_impl(&mut pending_requests, &mock_response).unwrap();

        assert!(result.is_some());
        let response = result.unwrap();
        assert_eq!(response.locations.len(), 0);
    }

    #[test]
    fn test_parse_find_references_response_wrong_method() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(42, "textDocument/definition".to_string());

        let mock_response = create_mock_empty_response(42);
        let result =
            parse_find_references_response_impl(&mut pending_requests, &mock_response).unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_parse_find_references_response_error() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(42, "textDocument/references".to_string());

        let mock_response = create_mock_error_response(42, -32602, "No symbol found");
        let result =
            parse_find_references_response_impl(&mut pending_requests, &mock_response).unwrap();

        assert!(result.is_some());
        let response = result.unwrap();
        assert_eq!(response.locations.len(), 0);
    }

    #[test]
    fn test_parse_find_references_response_no_pending_request() {
        let mut pending_requests = HashMap::new();
        // No pending request for this ID

        let mock_response = create_mock_empty_response(42);
        let result =
            parse_find_references_response_impl(&mut pending_requests, &mock_response).unwrap();

        assert!(result.is_none());
    }
}
