use anyhow::Result;
use lsp_types as lsp;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::types::{
    convert_lsp_location, make_qualified_name, DocumentSymbolResponse, FindReferencesResponse,
    HoverResponse, WorkspaceSymbolResponse,
};

// Helper function to test the parsing logic without LspClient
pub(crate) fn parse_find_references_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<FindReferencesResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        log::debug!("parse_find_references_response_impl: checking ID {}", id);
        if let Some(method) = pending_requests.get(&id) {
            log::debug!(
                "parse_find_references_response_impl: found method '{}' for ID {}",
                method,
                id
            );
        } else {
            log::debug!(
                "parse_find_references_response_impl: no pending request found for ID {}",
                id
            );
        }

        if let Some(method) = pending_requests.remove(&id) {
            log::debug!(
                "parse_find_references_response_impl: removed method '{}' for ID {}",
                method,
                id
            );
            if method == "textDocument/references" {
                log::debug!(
                    "parse_find_references_response_impl: method matches textDocument/references"
                );
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for find_references: {:?}", error);
                    // Return empty results for errors (like "no symbol found")
                    return Ok(Some(FindReferencesResponse {
                        request_id: id,
                        locations: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        log::debug!("parse_find_references_response_impl: result is null, returning empty response");
                        return Ok(Some(FindReferencesResponse {
                            request_id: id,
                            locations: Vec::new(),
                        }));
                    }

                    log::debug!(
                        "parse_find_references_response_impl: parsing result: {}",
                        result
                    );
                    let lsp_locations = Vec::<lsp::Location>::deserialize(result)?;
                    let locations: Vec<_> =
                        lsp_locations.iter().map(convert_lsp_location).collect();

                    log::debug!(
                        "parse_find_references_response_impl: successfully parsed {} locations",
                        locations.len()
                    );
                    return Ok(Some(FindReferencesResponse {
                        request_id: id,
                        locations,
                    }));
                }
            } else if method == "textDocument/references_enhanced" {
                log::debug!(
                    "parse_find_references_response_impl: method matches enhanced references"
                );
                // This is an enhanced reference request, but we process the base response here
                // The enhancement will be done separately
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for enhanced find_references: {:?}", error);
                    return Ok(Some(FindReferencesResponse {
                        request_id: id,
                        locations: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        log::debug!("parse_find_references_response_impl: enhanced result is null, returning empty response");
                        return Ok(Some(FindReferencesResponse {
                            request_id: id,
                            locations: Vec::new(),
                        }));
                    }

                    log::debug!(
                        "parse_find_references_response_impl: parsing enhanced result: {}",
                        result
                    );
                    let lsp_locations = Vec::<lsp::Location>::deserialize(result)?;
                    let locations: Vec<_> =
                        lsp_locations.iter().map(convert_lsp_location).collect();

                    log::debug!(
                        "parse_find_references_response_impl: successfully parsed {} enhanced locations",
                        locations.len()
                    );
                    return Ok(Some(FindReferencesResponse {
                        request_id: id,
                        locations,
                    }));
                }
            } else {
                log::debug!("parse_find_references_response_impl: method '{}' does not match 'textDocument/references'", method);
            }
        }
    } else {
        log::debug!("parse_find_references_response_impl: no ID found in response");
    }
    Ok(None)
}

// Helper function to parse workspace symbol responses
pub(crate) fn parse_workspace_symbol_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<WorkspaceSymbolResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "workspace/symbol" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for workspace/symbol: {:?}", error);
                    return Ok(Some(WorkspaceSymbolResponse {
                        request_id: id,
                        symbols: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(WorkspaceSymbolResponse {
                            request_id: id,
                            symbols: Vec::new(),
                        }));
                    }

                    let workspace_symbols = Vec::<lsp::WorkspaceSymbol>::deserialize(result)?;
                    let symbols = workspace_symbols
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
                                    line: 0, // WorkspaceLocation doesn't have range info
                                    column: 0,
                                    length: None,
                                },
                            };
                            model::WorkspaceSymbolInfo {
                                name: symbol.name.clone(),
                                qualified_name: make_qualified_name(
                                    &symbol.container_name,
                                    &symbol.name,
                                ),
                                kind: symbol.kind,
                                location,
                                container_name: symbol.container_name.clone(),
                            }
                        })
                        .collect();

                    return Ok(Some(WorkspaceSymbolResponse {
                        request_id: id,
                        symbols,
                    }));
                }
            }
        }
    }
    Ok(None)
}

// Helper function to parse document symbol responses
pub(crate) fn parse_document_symbol_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<DocumentSymbolResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "textDocument/documentSymbol" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for textDocument/documentSymbol: {:?}", error);
                    return Ok(Some(DocumentSymbolResponse {
                        request_id: id,
                        symbols: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(DocumentSymbolResponse {
                            request_id: id,
                            symbols: Vec::new(),
                        }));
                    }

                    // DocumentSymbol can return either DocumentSymbol[] or SymbolInformation[]
                    let symbols = if let Ok(doc_symbols) =
                        Vec::<lsp::DocumentSymbol>::deserialize(result)
                    {
                        // Convert DocumentSymbol to our format
                        doc_symbols
                            .into_iter()
                            .flat_map(|doc_symbol| {
                                convert_document_symbol_recursive(&doc_symbol, None)
                            })
                            .collect()
                    } else if let Ok(symbol_infos) =
                        Vec::<lsp::SymbolInformation>::deserialize(result)
                    {
                        // Convert SymbolInformation to our format
                        symbol_infos
                            .iter()
                            .map(|symbol| model::WorkspaceSymbolInfo {
                                name: symbol.name.clone(),
                                qualified_name: make_qualified_name(
                                    &symbol.container_name,
                                    &symbol.name,
                                ),
                                kind: symbol.kind,
                                location: convert_lsp_location(&symbol.location),
                                container_name: symbol.container_name.clone(),
                            })
                            .collect()
                    } else {
                        let error_msg = format!(
                            "Failed to parse documentSymbol result as either DocumentSymbol[] or SymbolInformation[]. Raw result: {:?}",
                            result
                        );
                        log::error!("{}", error_msg);
                        return Err(anyhow::anyhow!(error_msg));
                    };

                    return Ok(Some(DocumentSymbolResponse {
                        request_id: id,
                        symbols,
                    }));
                }
            }
        }
    }
    Ok(None)
}

pub(crate) fn parse_hover_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<HoverResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "textDocument/hover" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for textDocument/hover: {:?}", error);
                    return Ok(Some(HoverResponse {
                        request_id: id,
                        hover_info: None,
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(HoverResponse {
                            request_id: id,
                            hover_info: None,
                        }));
                    }

                    // Parse hover response
                    if let Ok(hover) = lsp_types::Hover::deserialize(result) {
                        // Extract text content from hover
                        let hover_text = match &hover.contents {
                            lsp_types::HoverContents::Scalar(marked_string) => {
                                extract_text_from_marked_string(marked_string)
                            }
                            lsp_types::HoverContents::Array(marked_strings) => marked_strings
                                .iter()
                                .map(extract_text_from_marked_string)
                                .collect::<Vec<_>>()
                                .join("\n"),
                            lsp_types::HoverContents::Markup(markup) => {
                                extract_text_from_markup(markup)
                            }
                        };

                        return Ok(Some(HoverResponse {
                            request_id: id,
                            hover_info: if hover_text.is_empty() {
                                None
                            } else {
                                Some(hover_text)
                            },
                        }));
                    } else {
                        log::warn!("Failed to parse hover response: {:?}", result);
                        return Ok(Some(HoverResponse {
                            request_id: id,
                            hover_info: None,
                        }));
                    }
                }
            }
        }
    }
    Ok(None)
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
