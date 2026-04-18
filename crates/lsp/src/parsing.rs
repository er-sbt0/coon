use anyhow::Result;
use lsp_types as lsp;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::types::{
    convert_lsp_location, make_qualified_name, DocumentSymbolResponse, FindReferencesResponse,
    HoverResponse, WorkspaceSymbolResponse,
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
    id: i64,
    response: &Value,
    method_label: &str,
    build_result: impl FnOnce(i64, T) -> R,
    build_empty: impl FnOnce(i64) -> R,
) -> Result<Option<R>>
where
    T: for<'de> Deserialize<'de>,
{
    if let Some(error) = response.get("error") {
        log::warn!("LSP error for {}: {:?}", method_label, error);
        return Ok(Some(build_empty(id)));
    }

    match response.get("result") {
        Some(result) if !result.is_null() => match T::deserialize(result) {
            Ok(parsed) => Ok(Some(build_result(id, parsed))),
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
            Ok(Some(build_empty(id)))
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
    build_result: impl FnOnce(i64, T) -> R,
    build_empty: impl FnOnce(i64) -> R,
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

    parse_lsp_result(id, response, method_name, build_result, build_empty)
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

    // Matches both normal and enhanced reference requests.
    match pending_requests.get(&id).map(|s| s.as_str()) {
        Some("textDocument/references") | Some("textDocument/references_enhanced") => {
            pending_requests.remove(&id);
        }
        _ => return Ok(None),
    }

    parse_lsp_result(
        id,
        response,
        "textDocument/references",
        |id, lsp_locations: Vec<lsp::Location>| FindReferencesResponse {
            request_id: id,
            locations: lsp_locations.iter().map(convert_lsp_location).collect(),
        },
        |id| FindReferencesResponse {
            request_id: id,
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
        |id, workspace_symbols: Vec<lsp::WorkspaceSymbol>| WorkspaceSymbolResponse {
            request_id: id,
            symbols: parse_workspace_symbols_from_result(workspace_symbols),
        },
        |id| WorkspaceSymbolResponse {
            request_id: id,
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
        |id, RawDocumentSymbols(doc_symbols)| {
            let symbols = doc_symbols
                .into_iter()
                .flat_map(|doc_symbol| convert_document_symbol_recursive(&doc_symbol, None))
                .collect();
            DocumentSymbolResponse {
                request_id: id,
                symbols,
            }
        },
        |id| DocumentSymbolResponse {
            request_id: id,
            symbols: Vec::new(),
        },
    )
}

pub(crate) fn parse_hover_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<HoverResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "textDocument/hover",
        |id, hover: lsp_types::Hover| {
            let hover_text = match &hover.contents {
                lsp_types::HoverContents::Scalar(marked_string) => {
                    extract_text_from_marked_string(marked_string)
                }
                lsp_types::HoverContents::Array(marked_strings) => marked_strings
                    .iter()
                    .map(extract_text_from_marked_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
                lsp_types::HoverContents::Markup(markup) => extract_text_from_markup(markup),
            };
            HoverResponse {
                request_id: id,
                hover_info: if hover_text.is_empty() {
                    None
                } else {
                    Some(hover_text)
                },
            }
        },
        |id| HoverResponse {
            request_id: id,
            hover_info: None,
        },
    )
}

// Helper to extract text from MarkedString
pub(crate) fn extract_text_from_marked_string(marked_string: &lsp_types::MarkedString) -> String {
    match marked_string {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(lang_string) => {
            // Try to extract function name from the code
            if let Some(name) = extract_function_name_from_signature(&lang_string.value) {
                name
            } else {
                lang_string.value.clone()
            }
        }
    }
}

// Helper to extract text from MarkupContent
pub(crate) fn extract_text_from_markup(markup: &lsp_types::MarkupContent) -> String {
    match markup.kind {
        lsp_types::MarkupKind::PlainText => markup.value.clone(),
        lsp_types::MarkupKind::Markdown => {
            // For markdown, try to extract function names from code blocks
            // Look for patterns like: int foo(int x) or void bar()
            let content = &markup.value;

            // Try to find function signatures in the markdown
            if let Some(line) = content.lines().find(|line| {
                line.contains('(') && line.contains(')') && !line.trim().starts_with('#')
            }) {
                // Extract function name from signature
                if let Some(name) = extract_function_name_from_signature(line) {
                    return name;
                }
            }

            // Fallback to first non-empty line
            content
                .lines()
                .find(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
                .unwrap_or(content)
                .trim()
                .to_string()
        }
    }
}

// Helper to extract function name from C/C++ function signature
pub(crate) fn extract_function_name_from_signature(signature: &str) -> Option<String> {
    // Look for pattern: [return_type] function_name([params])
    let trimmed = signature.trim();

    // Find the opening parenthesis
    if let Some(paren_pos) = trimmed.find('(') {
        let before_paren = &trimmed[..paren_pos].trim();

        // Split by whitespace and take the last word as function name
        if let Some(func_name) = before_paren.split_whitespace().last() {
            // Remove any pointer indicators or other decorators
            let clean_name = func_name.trim_start_matches('*').trim();
            if !clean_name.is_empty() {
                return Some(clean_name.to_string());
            }
        }
    }

    None
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
    use lsp_types::{HoverContents, MarkedString, MarkupContent, MarkupKind, Position, Range, Url};
    use serde_json::{json, Value};

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
        assert_eq!(response.request_id, 42);
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
        assert_eq!(response.request_id, 42);
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
        assert_eq!(response.request_id, 42);
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

    // ── Hover / parsing helper tests (moved from tests/hover_tests.rs) ──

    #[test]
    fn test_parse_hover_response_basic() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(1, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "contents": {
                    "kind": "markdown",
                    "value": "```c\nint foo(int x)\n```\nFunction foo defined at main.c:6"
                },
                "range": {
                    "start": {"line": 5, "character": 4},
                    "end": {"line": 5, "character": 7}
                }
            }
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 1);
        assert!(hover.hover_info.is_some());
        assert!(hover.hover_info.unwrap().contains("foo"));
    }

    #[test]
    fn test_parse_hover_response_marked_string_array() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(2, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "contents": [
                    {"language": "c", "value": "int foo(int x)"},
                    "Function foo returns an integer"
                ]
            }
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 2);
        assert!(hover.hover_info.is_some());
        assert!(hover.hover_info.unwrap().contains("foo"));
    }

    #[test]
    fn test_parse_hover_response_null_result() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(3, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": null
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 3);
        assert!(hover.hover_info.is_none());
    }

    #[test]
    fn test_parse_hover_response_error() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(4, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "error": {
                "code": -32601,
                "message": "Method not found"
            }
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 4);
        assert!(hover.hover_info.is_none());
    }

    #[test]
    fn test_extract_function_name_from_signature() {
        let test_cases = vec![
            ("int foo(int x)", Some("foo".to_string())),
            ("void bar()", Some("bar".to_string())),
            (
                "static int my_func(const char* str, int len)",
                Some("my_func".to_string()),
            ),
            (
                "unsigned long long calculate_hash(void)",
                Some("calculate_hash".to_string()),
            ),
            ("int* get_pointer()", Some("get_pointer".to_string())),
            ("const char* get_name(void)", Some("get_name".to_string())),
            (
                "struct Point create_point(int x, int y)",
                Some("create_point".to_string()),
            ),
            ("invalid signature without parentheses", None),
            ("", None),
            ("()", None),
        ];

        for (signature, expected) in test_cases {
            let result = extract_function_name_from_signature(signature);
            assert_eq!(result, expected, "Failed for signature: '{}'", signature);
        }
    }

    #[test]
    fn test_extract_text_from_markup() {
        let markdown_content = MarkupContent {
            kind: MarkupKind::Markdown,
            value: "```c\nint foo(int x)\n```\nThis is a function".to_string(),
        };
        let result = extract_text_from_markup(&markdown_content);
        assert_eq!(result, "foo");

        let plain_content = MarkupContent {
            kind: MarkupKind::PlainText,
            value: "Plain text description".to_string(),
        };
        let result = extract_text_from_markup(&plain_content);
        assert_eq!(result, "Plain text description");
    }

    #[test]
    fn test_extract_text_from_marked_string() {
        let lang_string = MarkedString::LanguageString(lsp_types::LanguageString {
            language: "c".to_string(),
            value: "int main(int argc, char** argv)".to_string(),
        });
        let result = extract_text_from_marked_string(&lang_string);
        assert_eq!(result, "main");

        let simple_string = MarkedString::String("Simple description".to_string());
        let result = extract_text_from_marked_string(&simple_string);
        assert_eq!(result, "Simple description");
    }

    #[test]
    fn test_complete_hover_flow_simplified() {
        let hover_request_id = 42i64;
        let mut pending_requests = HashMap::new();
        pending_requests.insert(hover_request_id, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": hover_request_id,
            "result": {
                "contents": {
                    "kind": "markdown",
                    "value": "```c\nint foo(int x)\n```\n\nFunction `foo` takes an integer parameter and returns an integer.\n\nDefined in main.c at line 6."
                },
                "range": {
                    "start": {"line": 5, "character": 4},
                    "end": {"line": 5, "character": 7}
                }
            }
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, hover_request_id);
        assert!(hover.hover_info.is_some());
        assert_eq!(hover.hover_info.unwrap(), "foo");
    }

    #[test]
    fn test_clangd_hover_response() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(5, "textDocument/hover".to_string());

        let response = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "result": {
                "contents": {
                    "kind": "markdown",
                    "value": "### function `foo`\n\n```cpp\nint foo(int x)\n```\n\n---\nDeclared in `/home/user/main.c:6`"
                },
                "range": {
                    "start": {"line": 14, "character": 2},
                    "end": {"line": 14, "character": 5}
                }
            }
        });

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 5);
        assert!(hover.hover_info.is_some());
        assert_eq!(hover.hover_info.unwrap(), "foo");
    }

    #[test]
    fn test_debug_raw_json_parsing() {
        let raw_json = r#"{
            "jsonrpc": "2.0",
            "id": 4,
            "result": {
                "contents": {
                    "kind": "markdown",
                    "value": "```c\nint foo(int x)\n```"
                }
            }
        }"#;

        let value: Value = serde_json::from_str(raw_json).unwrap();

        if let Some(result) = value.get("result") {
            match serde_json::from_value::<lsp_types::Hover>(result.clone()) {
                Ok(hover) => {
                    let hover_text = match &hover.contents {
                        HoverContents::Scalar(marked_string) => {
                            extract_text_from_marked_string(marked_string)
                        }
                        HoverContents::Array(marked_strings) => marked_strings
                            .iter()
                            .map(extract_text_from_marked_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        HoverContents::Markup(markup) => extract_text_from_markup(markup),
                    };
                    assert_eq!(hover_text, "foo");
                }
                Err(e) => {
                    panic!("Could not parse hover response: {:?}", e);
                }
            }
        }
    }
}
