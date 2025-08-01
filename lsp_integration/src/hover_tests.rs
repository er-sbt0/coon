#[cfg(test)]
mod hover_tests {
    use crate::{
        extract_function_name_from_signature, extract_text_from_marked_string,
        extract_text_from_markup, parse_hover_response_impl,
    };
    use lsp_types::{HoverContents, MarkedString, MarkupContent, MarkupKind};
    use serde_json::{json, Value};
    use std::collections::HashMap;

    /// Test basic hover response parsing
    #[test]
    fn test_parse_hover_response_basic() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(1, "textDocument/hover".to_string());

        // Create a simple hover response with markdown content
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
        println!("Hover response parsing result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 1);
        assert!(hover.hover_info.is_some());

        let hover_info = hover.hover_info.unwrap();
        println!("Extracted hover info: '{}'", hover_info);
        assert!(hover_info.contains("foo"));
    }

    /// Test hover response with MarkedString array
    #[test]
    fn test_parse_hover_response_marked_string_array() {
        let mut pending_requests = HashMap::new();
        pending_requests.insert(2, "textDocument/hover".to_string());

        // Create hover response with MarkedString array (clangd style)
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
        println!("MarkedString array result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 2);
        assert!(hover.hover_info.is_some());

        let hover_info = hover.hover_info.unwrap();
        println!("Extracted hover info from array: '{}'", hover_info);
        assert!(hover_info.contains("foo"));
    }

    /// Test hover response with null result
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
        println!("Null result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 3);
        assert!(hover.hover_info.is_none());
    }

    /// Test hover response with error
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
        println!("Error result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 4);
        assert!(hover.hover_info.is_none());
    }

    /// Test function name extraction from C signatures
    #[test]
    fn test_extract_function_name_from_signature() {
        // Test various C function signature formats
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
            println!("Signature: '{}' -> {:?}", signature, result);
            assert_eq!(result, expected, "Failed for signature: '{}'", signature);
        }
    }

    /// Test text extraction from MarkupContent
    #[test]
    fn test_extract_text_from_markup() {
        // Test markdown content
        let markdown_content = MarkupContent {
            kind: MarkupKind::Markdown,
            value: "```c\nint foo(int x)\n```\nThis is a function".to_string(),
        };

        let result = extract_text_from_markup(&markdown_content);
        println!("Markdown extraction: '{}'", result);
        assert_eq!(result, "foo");

        // Test plain text content
        let plain_content = MarkupContent {
            kind: MarkupKind::PlainText,
            value: "Plain text description".to_string(),
        };

        let result = extract_text_from_markup(&plain_content);
        println!("Plain text extraction: '{}'", result);
        assert_eq!(result, "Plain text description");
    }

    /// Test text extraction from MarkedString
    #[test]
    fn test_extract_text_from_marked_string() {
        // Test language string
        let lang_string = MarkedString::LanguageString(lsp_types::LanguageString {
            language: "c".to_string(),
            value: "int main(int argc, char** argv)".to_string(),
        });

        let result = extract_text_from_marked_string(&lang_string);
        println!("Language string extraction: '{}'", result);
        assert_eq!(result, "main");

        // Test simple string
        let simple_string = MarkedString::String("Simple description".to_string());

        let result = extract_text_from_marked_string(&simple_string);
        println!("Simple string extraction: '{}'", result);
        assert_eq!(result, "Simple description");
    }

    /// Test complete hover flow simulation (simplified)
    #[test]
    fn test_complete_hover_flow_simplified() {
        println!("\n=== Testing Complete Hover Flow (Simplified) ===");

        // Simulate a hover request without creating a full LspClient
        let hover_request_id = 42i64;
        let mut pending_requests = HashMap::new();
        pending_requests.insert(hover_request_id, "textDocument/hover".to_string());

        // Simulate receiving a hover response
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

        println!(
            "Simulating hover response: {}",
            serde_json::to_string_pretty(&response).unwrap()
        );

        // Parse the response using the implementation function directly
        let result = parse_hover_response_impl(&mut pending_requests, &response);
        println!("Parse result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, hover_request_id);
        assert!(hover.hover_info.is_some());

        let hover_info = hover.hover_info.unwrap();
        println!("Final extracted hover info: '{}'", hover_info);
        assert_eq!(hover_info, "foo");

        println!("✅ Complete hover flow test passed!");
    }

    /// Test real-world clangd hover response format
    #[test]
    fn test_clangd_hover_response() {
        println!("\n=== Testing Clangd-style Hover Response ===");

        let mut pending_requests = HashMap::new();
        pending_requests.insert(5, "textDocument/hover".to_string());

        // This is closer to what clangd actually returns
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

        println!(
            "Clangd response: {}",
            serde_json::to_string_pretty(&response).unwrap()
        );

        let result = parse_hover_response_impl(&mut pending_requests, &response);
        println!("Clangd parse result: {:?}", result);

        assert!(result.is_ok());
        let hover_response = result.unwrap();
        assert!(hover_response.is_some());

        let hover = hover_response.unwrap();
        assert_eq!(hover.request_id, 5);
        assert!(hover.hover_info.is_some());

        let hover_info = hover.hover_info.unwrap();
        println!("Extracted from clangd response: '{}'", hover_info);
        assert_eq!(hover_info, "foo");

        println!("✅ Clangd hover response test passed!");
    }

    /// Test raw JSON parsing to see what clangd actually sends
    #[test]
    fn test_debug_raw_json_parsing() {
        println!("\n=== Debug Raw JSON Parsing ===");

        // Test parsing the exact structure clangd might send
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
        println!(
            "Parsed raw JSON: {}",
            serde_json::to_string_pretty(&value).unwrap()
        );

        // Try to parse as Hover
        if let Some(result) = value.get("result") {
            println!(
                "Result field: {}",
                serde_json::to_string_pretty(result).unwrap()
            );

            match serde_json::from_value::<lsp_types::Hover>(result.clone()) {
                Ok(hover) => {
                    println!("✅ Successfully parsed as Hover: {:?}", hover);

                    // Test content extraction
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

                    println!("Extracted text: '{}'", hover_text);
                    assert_eq!(hover_text, "foo");
                }
                Err(e) => {
                    println!("❌ Failed to parse as Hover: {:?}", e);
                    panic!("Could not parse hover response");
                }
            }
        }

        println!("✅ Raw JSON parsing test completed!");
    }
}
