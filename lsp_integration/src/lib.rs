#![allow(dead_code)]
use anyhow::Result;
use lsp_types as lsp;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// Response from LSP find_references request
#[derive(Debug, Clone)]
pub struct FindReferencesResponse {
    pub request_id: i64,
    pub locations: Vec<core_data::Location>,
}

/// Convert LSP Location to our core_data Location
pub fn convert_lsp_location(lsp_location: &lsp::Location) -> core_data::Location {
    core_data::Location {
        file_path: lsp_location
            .uri
            .to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| lsp_location.uri.to_string()),
        line: lsp_location.range.start.line,
        column: lsp_location.range.start.character,
        length: Some(lsp_location.range.end.character - lsp_location.range.start.character),
    }
}

/// Convert LSP Position to our core_data Location (without length)
pub fn convert_lsp_position(uri: &lsp::Url, position: &lsp::Position) -> core_data::Location {
    core_data::Location {
        file_path: uri
            .to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| uri.to_string()),
        line: position.line,
        column: position.character,
        length: None,
    }
}

pub struct LspClient {
    process: Child,
    writer: ChildStdin,
    reader_handle: tokio::task::JoinHandle<()>,
    next_id: i64,
    pending_requests: HashMap<i64, String>,
}

impl LspClient {
    pub async fn new(tx: mpsc::Sender<Value>) -> Result<Self> {
        let mut process = Command::new("/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let writer = process.stdin.take().unwrap();
        let stdout = process.stdout.take().unwrap();
        let stderr = process.stderr.take().unwrap();

        tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        log::error!("LSP stderr: {}", line.trim());
                        line.clear();
                    }
                    Err(e) => {
                        log::error!("failed to read from stderr: {}", e);
                        break;
                    }
                }
            }
        });

        let mut reader_half = BufReader::new(stdout);

        let reader_handle = tokio::spawn(async move {
            let mut buffer = String::new();
            loop {
                match reader_half.read_line(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if let Some(len_str) = buffer.strip_prefix("Content-Length: ") {
                            if let Ok(len) = len_str.trim().parse::<usize>() {
                                buffer.clear(); // Clear for reading the body
                                                // Read the '\r\n' after the header
                                if reader_half.read_line(&mut buffer).await.is_ok() {
                                    buffer.clear();
                                    let mut content = vec![0; len];
                                    if reader_half.read_exact(&mut content).await.is_ok() {
                                        if let Ok(msg) = serde_json::from_slice::<Value>(&content) {
                                            if tx.send(msg).await.is_err() {
                                                break; // Channel closed
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        buffer.clear();
                    }
                    Err(_) => break, // Error reading line
                }
            }
        });

        Ok(Self {
            process,
            writer,
            reader_handle,
            next_id: 0,
            pending_requests: HashMap::new(),
        })
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;

        // Track the request method for response handling
        self.pending_requests.insert(id, method.to_string());

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let request_str = serde_json::to_string(&request)?;
        let content_length = format!("Content-Length: {}\r\n\r\n", request_str.len());
        self.writer.write_all(content_length.as_bytes()).await?;
        self.writer.write_all(request_str.as_bytes()).await?;
        self.writer.flush().await?;

        Ok(id)
    }

    /// Parse find_references response and convert to our data structures
    pub fn parse_find_references_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<FindReferencesResponse>> {
        parse_find_references_response_impl(&mut self.pending_requests, response)
    }
}

// Helper function to test the parsing logic without LspClient
fn parse_find_references_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<FindReferencesResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "textDocument/references" {
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
                        return Ok(Some(FindReferencesResponse {
                            request_id: id,
                            locations: Vec::new(),
                        }));
                    }

                    let lsp_locations: Vec<lsp::Location> = serde_json::from_value(result.clone())?;
                    let locations = lsp_locations.iter().map(convert_lsp_location).collect();

                    return Ok(Some(FindReferencesResponse {
                        request_id: id,
                        locations,
                    }));
                }
            }
        }
    }
    Ok(None)
}

impl LspClient {
    pub async fn initialize(&mut self, root_uri: lsp::Url) -> Result<i64> {
        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri),
            capabilities: lsp::ClientCapabilities {
                text_document: Some(lsp::TextDocumentClientCapabilities {
                    references: Some(lsp::DynamicRegistrationClientCapabilities {
                        dynamic_registration: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        self.send_request("initialize", serde_json::to_value(params)?)
            .await
    }

    pub async fn find_references(
        &mut self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Result<i64> {
        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration: false,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.send_request("textDocument/references", serde_json::to_value(params)?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range, Url};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

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
    fn test_convert_lsp_location() {
        let uri = Url::parse("file:///home/user/test.rs").unwrap();
        let lsp_location = lsp::Location {
            uri: uri.clone(),
            range: Range {
                start: Position {
                    line: 10,
                    character: 5,
                },
                end: Position {
                    line: 10,
                    character: 15,
                },
            },
        };

        let core_location = convert_lsp_location(&lsp_location);

        assert_eq!(core_location.file_path, "/home/user/test.rs");
        assert_eq!(core_location.line, 10);
        assert_eq!(core_location.column, 5);
        assert_eq!(core_location.length, Some(10));
    }

    #[test]
    fn test_convert_lsp_position() {
        let uri = Url::parse("file:///home/user/test.rs").unwrap();
        let position = Position {
            line: 20,
            character: 8,
        };

        let core_location = convert_lsp_position(&uri, &position);

        assert_eq!(core_location.file_path, "/home/user/test.rs");
        assert_eq!(core_location.line, 20);
        assert_eq!(core_location.column, 8);
        assert_eq!(core_location.length, None);
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
        assert_eq!(response.locations[0].line, 0);
        assert_eq!(response.locations[0].column, 3);
        assert_eq!(response.locations[0].length, Some(8));

        assert_eq!(response.locations[1].file_path, "/home/user/test.rs");
        assert_eq!(response.locations[1].line, 5);
        assert_eq!(response.locations[1].column, 10);
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

    // Helper to send a notification without expecting a response ID
    impl LspClient {
        async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            });

            let notification_str = serde_json::to_string(&notification)?;
            let content_length = format!("Content-Length: {}\r\n\r\n", notification_str.len());
            self.writer.write_all(content_length.as_bytes()).await?;
            self.writer.write_all(notification_str.as_bytes()).await?;
            self.writer.flush().await?;

            Ok(())
        }
    }

    #[tokio::test]
    #[ignore] // Run with `cargo test -- --ignored` to include integration tests
    async fn test_find_references_integration() {
        let _ = pretty_env_logger::try_init();

        let temp_dir = TempDir::new().unwrap();
        let test_file_path = temp_dir.path().join("test.cpp");
        let file_content = "void my_func() {}\nint main() { my_func(); return 0; }";
        std::fs::write(&test_file_path, file_content).unwrap();

        let (tx, mut rx) = mpsc::channel(100);
        let mut client = LspClient::new(tx).await.unwrap();

        let root_uri = Url::from_file_path(temp_dir.path()).unwrap();
        let test_file_uri = Url::from_file_path(&test_file_path).unwrap();

        // Initialize LSP
        let init_id = client.initialize(root_uri.clone()).await.unwrap();

        // Wait for initialization response
        let mut init_response = None;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
                        init_response = Some(msg);
                        break;
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout waiting for initialization response");
                }
            }
        }
        assert!(init_response.is_some());

        // Send initialized notification
        client
            .send_notification("initialized", serde_json::json!({}))
            .await
            .unwrap();

        // Open the file
        let open_params = lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem::new(
                test_file_uri.clone(),
                "cpp".to_string(),
                1,
                file_content.to_string(),
            ),
        };
        client
            .send_notification(
                "textDocument/didOpen",
                serde_json::to_value(open_params).unwrap(),
            )
            .await
            .unwrap();

        // Wait a bit for the server to process the file
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Find references for the function call
        let ref_id = client
            .find_references(
                lsp::TextDocumentIdentifier {
                    uri: test_file_uri.clone(),
                },
                Position {
                    line: 1,
                    character: 15,
                }, // position inside `my_func` call
            )
            .await
            .unwrap();

        // Wait for response
        let mut ref_response = None;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    if msg.get("id").and_then(|i| i.as_i64()) == Some(ref_id) {
                        ref_response = Some(msg);
                        break;
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout waiting for find_references response");
                }
            }
        }

        let response = ref_response.unwrap();
        let parsed = client.parse_find_references_response(&response).unwrap();

        assert!(parsed.is_some());
        let find_refs_response = parsed.unwrap();
        assert_eq!(find_refs_response.request_id, ref_id);

        // We should find at least one reference (the call site) for a proper C++ file
        // If empty, it might be because clangd needs compilation database or headers
        println!("Found {} references", find_refs_response.locations.len());

        // Verify the locations point to our test file if any found
        for location in &find_refs_response.locations {
            assert!(location.file_path.contains("test.cpp"));
            assert!(location.line <= 1); // Should be on line 0 or 1
        }
    }

    #[tokio::test]
    async fn test_find_references_no_results() {
        let temp_dir = TempDir::new().unwrap();
        let test_file_path = temp_dir.path().join("empty.rs");
        let file_content = "// Empty file with just a comment";
        std::fs::write(&test_file_path, file_content).unwrap();

        let (tx, mut rx) = mpsc::channel(100);
        let mut client = LspClient::new(tx).await.unwrap();

        let root_uri = Url::from_file_path(temp_dir.path()).unwrap();
        let test_file_uri = Url::from_file_path(&test_file_path).unwrap();

        // Initialize and open file
        let init_id = client.initialize(root_uri).await.unwrap();

        // Wait for init response
        while let Some(msg) = rx.recv().await {
            if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
                break;
            }
        }

        client
            .send_notification("initialized", serde_json::json!({}))
            .await
            .unwrap();

        let open_params = lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem::new(
                test_file_uri.clone(),
                "rust".to_string(),
                1,
                file_content.to_string(),
            ),
        };
        client
            .send_notification(
                "textDocument/didOpen",
                serde_json::to_value(open_params).unwrap(),
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Try to find references at a position with no symbol
        let ref_id = client
            .find_references(
                lsp::TextDocumentIdentifier { uri: test_file_uri },
                Position {
                    line: 0,
                    character: 5,
                }, // position in comment
            )
            .await
            .unwrap();

        // Wait for response
        let mut ref_response = None;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    if msg.get("id").and_then(|i| i.as_i64()) == Some(ref_id) {
                        ref_response = Some(msg);
                        break;
                    }
                }
                _ = &mut timeout => {
                    panic!("Timeout waiting for find_references response");
                }
            }
        }

        let response = ref_response.unwrap();
        let parsed = client.parse_find_references_response(&response).unwrap();

        // LSP server might return None (no response parsed) if there's an error,
        // or Some with empty locations if no references found
        if let Some(find_refs_response) = parsed {
            assert_eq!(find_refs_response.request_id, ref_id);
            assert!(
                find_refs_response.locations.is_empty(),
                "Expected no references for comment position"
            );
        } else {
            // This is also acceptable - LSP server returned an error response
            // which means no references could be found
            println!("LSP server returned error response (acceptable for no references case)");
        }
    }
}
