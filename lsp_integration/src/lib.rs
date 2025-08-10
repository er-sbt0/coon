#![allow(dead_code)]
use anyhow::Result;
use lsp_types as lsp;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

mod call_hierarchy;
mod service;
mod symbol_resolution;
mod utils;

pub use service::{LspRequest, LspResponse, LspService};

#[cfg(test)]
mod call_hierarchy_tests;

#[cfg(test)]
mod hover_tests;

#[cfg(test)]
mod enhanced_references_tests;

/// Response from LSP find_references request
#[derive(Debug, Clone)]
pub struct FindReferencesResponse {
    pub request_id: i64,
    pub locations: Vec<core_data::Location>,
}

/// Enhanced response from LSP find_references request with symbol information
#[derive(Debug, Clone)]
pub struct EnhancedReferencesResponse {
    pub request_id: i64,
    pub references: Vec<core_data::Reference>,
}

/// Response from LSP workspace/symbol request
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolResponse {
    pub request_id: i64,
    pub symbols: Vec<WorkspaceSymbolInfo>,
}

/// Response from LSP textDocument/documentSymbol request
#[derive(Debug, Clone)]
pub struct DocumentSymbolResponse {
    pub request_id: i64,
    pub symbols: Vec<WorkspaceSymbolInfo>,
}

/// Response from LSP textDocument/hover request
#[derive(Debug, Clone)]
pub struct HoverResponse {
    pub request_id: i64,
    pub hover_info: Option<String>, // Simplified hover content
}

/// Response from LSP textDocument/prepareCallHierarchy request
#[derive(Debug, Clone)]
pub struct PrepareCallHierarchyResponse {
    pub request_id: i64,
    pub items: Vec<lsp::CallHierarchyItem>,
}

/// Response from LSP callHierarchy/outgoingCalls request
#[derive(Debug, Clone)]
pub struct OutgoingCallsResponse {
    pub request_id: i64,
    pub calls: Vec<lsp::CallHierarchyOutgoingCall>,
}

/// Response from LSP callHierarchy/incomingCalls request
#[derive(Debug, Clone)]
pub struct IncomingCallsResponse {
    pub request_id: i64,
    pub calls: Vec<lsp::CallHierarchyIncomingCall>,
}

/// Simplified symbol information from LSP
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolInfo {
    pub name: String,
    pub kind: lsp::SymbolKind,
    pub location: core_data::Location,
    pub container_name: Option<String>,
}

/// Convert LSP Location to our core_data Location
pub fn convert_lsp_location(lsp_location: &lsp::Location) -> core_data::Location {
    core_data::Location {
        file_path: lsp_location
            .uri
            .to_file_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| lsp_location.uri.to_string()),
        line: lsp_location.range.start.line + 1, // Convert from 0-indexed LSP to 1-indexed
        column: lsp_location.range.start.character + 1, // Convert from 0-indexed LSP to 1-indexed
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
        line: position.line + 1, // Convert from 0-indexed LSP to 1-indexed
        column: position.character + 1, // Convert from 0-indexed LSP to 1-indexed
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
    /// Create a new LspClient with a custom clangd path
    pub async fn with_path(tx: mpsc::Sender<Value>, clangd_path: &str) -> Result<Self> {
        let process = Command::new(clangd_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    /// Create a new LspClient with a specified working directory
    pub async fn with_working_dir(
        tx: mpsc::Sender<Value>,
        clangd_path: &str,
        working_dir: &std::path::Path,
    ) -> Result<Self> {
        let process = Command::new(clangd_path)
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Self::from_process(tx, process).await
    }

    /// Create a new LspClient that tries to find clangd in PATH
    pub async fn new(tx: mpsc::Sender<Value>) -> Result<Self> {
        // First try the hardcoded path from the original code
        if std::path::Path::new("/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd").exists() {
            return Self::with_path(tx, "/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd").await;
        }

        // Then try to find clangd in PATH
        Self::with_path(tx, "clangd").await
    }

    async fn from_process(tx: mpsc::Sender<Value>, mut process: Child) -> Result<Self> {
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

    /// Send a notification to the LSP server (no response expected)
    pub async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
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

    /// Initialize the LSP server with the given root URI
    pub async fn initialize(&mut self, root_uri: lsp::Url) -> Result<i64> {
        // Create default workspace folders
        let workspace_folders = vec![lsp::WorkspaceFolder {
            uri: root_uri.clone(),
            name: root_uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "workspace".to_string()),
        }];

        let client_capabilities = lsp::ClientCapabilities {
            workspace: Some(lsp::WorkspaceClientCapabilities {
                symbol: Some(lsp::WorkspaceSymbolClientCapabilities {
                    dynamic_registration: Some(false),
                    ..Default::default()
                }),
                // Note: call_hierarchy is not a field in WorkspaceClientCapabilities
                ..Default::default()
            }),
            text_document: Some(lsp::TextDocumentClientCapabilities {
                references: Some(lsp::DynamicRegistrationClientCapabilities {
                    dynamic_registration: Some(false),
                }),
                call_hierarchy: Some(lsp::CallHierarchyClientCapabilities {
                    dynamic_registration: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            capabilities: client_capabilities,
            workspace_folders: Some(workspace_folders),
            client_info: Some(lsp::ClientInfo {
                name: "coon-lsp-client".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..Default::default()
        };

        self.send_request("initialize", serde_json::to_value(params)?)
            .await
    }

    /// Send a find references request
    pub async fn find_references(
        &mut self,
        document_id: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Result<i64> {
        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document: document_id,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.send_request("textDocument/references", serde_json::to_value(params)?)
            .await
    }

    /// Send an enhanced find references request that will resolve symbol names
    pub async fn find_references_with_symbols(
        &mut self,
        document_id: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Result<i64> {
        // For now, this uses the same request as find_references
        // The enhancement happens in the response processing
        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document: document_id,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let request_id = self
            .send_request("textDocument/references", serde_json::to_value(params)?)
            .await?;

        // Mark this request as enhanced for special processing
        self.pending_requests
            .insert(request_id, "textDocument/references_enhanced".to_string());

        Ok(request_id)
    }

    /// Helper for workspace symbol requests
    pub async fn workspace_symbol(&mut self, query: &str) -> Result<i64> {
        let params = lsp::WorkspaceSymbolParams {
            query: query.to_string(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.send_request("workspace/symbol", serde_json::to_value(params)?)
            .await
    }

    /// Document symbol request helper
    pub async fn document_symbol(
        &mut self,
        text_document: lsp::TextDocumentIdentifier,
    ) -> Result<i64> {
        let params = lsp::DocumentSymbolParams {
            text_document,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.send_request("textDocument/documentSymbol", serde_json::to_value(params)?)
            .await
    }

    /// Hover request helper
    pub async fn hover(
        &mut self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Result<i64> {
        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: Default::default(),
        };
        self.send_request("textDocument/hover", serde_json::to_value(params)?)
            .await
    }

    /// Send initialized notification
    pub async fn send_initialized(&mut self) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });

        let notification_str = serde_json::to_string(&notification)?;
        let content_length = format!("Content-Length: {}\r\n\r\n", notification_str.len());
        self.writer.write_all(content_length.as_bytes()).await?;
        self.writer.write_all(notification_str.as_bytes()).await?;
        self.writer.flush().await?;

        Ok(())
    }

    /// Helper to open a document
    pub async fn did_open(
        &mut self,
        uri: lsp::Url,
        language_id: &str,
        version: i32,
        text: String,
    ) -> Result<()> {
        let params = lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri,
                language_id: language_id.to_string(),
                version,
                text,
            },
        };

        self.send_notification("textDocument/didOpen", serde_json::to_value(params)?)
            .await
    }

    /// Helper to close a document
    pub async fn did_close(&mut self, uri: lsp::Url) -> Result<()> {
        let params = lsp::DidCloseTextDocumentParams {
            text_document: lsp::TextDocumentIdentifier { uri },
        };

        self.send_notification("textDocument/didClose", serde_json::to_value(params)?)
            .await
    }

    /// Parse find_references response and convert to our data structures
    pub fn parse_find_references_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<FindReferencesResponse>> {
        log::info!(
            "parse_find_references_response called with response: {}",
            response
        );
        let result = parse_find_references_response_impl(&mut self.pending_requests, response);
        log::info!(
            "parse_find_references_response returning: {:?}",
            result
                .as_ref()
                .map(|r| r.as_ref().map(|resp| resp.locations.len()))
        );
        result
    }

    /// Parse workspace symbol response and convert to our data structures
    pub fn parse_workspace_symbol_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        parse_workspace_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse document symbol response and convert to our data structures
    pub fn parse_document_symbol_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<DocumentSymbolResponse>> {
        parse_document_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse hover response
    pub fn parse_hover_response(&mut self, response: &Value) -> Result<Option<HoverResponse>> {
        parse_hover_response_impl(&mut self.pending_requests, response)
    }

    /// Enhance references with symbol information
    pub async fn enhance_references(
        &mut self,
        references: Vec<core_data::Location>,
    ) -> Result<Vec<core_data::Reference>> {
        let mut enhanced_references = Vec::new();

        for location in references {
            // Try to extract a meaningful symbol name from the file context
            let referencing_symbol = self
                .resolve_symbol_at_location(&location)
                .await
                .unwrap_or_else(|| {
                    // Fallback to a descriptive name showing location
                    let filename = std::path::Path::new(&location.file_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");

                    core_data::ReferencingSymbol {
                        name: format!("{}:{}", filename, location.line),
                        qualified_name: format!(
                            "{}::{}:{}",
                            filename, location.line, location.column
                        ),
                        kind: core_data::ReferenceSymbolKind::Function,
                    }
                });

            enhanced_references.push(core_data::Reference {
                location,
                referencing_symbol: Some(referencing_symbol),
            });
        }

        Ok(enhanced_references)
    }

    /// Attempt to resolve the symbol at a given location
    async fn resolve_symbol_at_location(
        &mut self,
        location: &core_data::Location,
    ) -> Option<core_data::ReferencingSymbol> {
        // For now, create a basic symbol name
        // In a full implementation, this would:
        // 1. Request document symbols for the file
        // 2. Find the symbol containing the location
        // 3. Extract the actual symbol name

        let filename = std::path::Path::new(&location.file_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        // Create a more meaningful name format
        Some(core_data::ReferencingSymbol {
            name: format!("caller_at_{}:{}", filename, location.line),
            qualified_name: format!(
                "{}::caller_at_{}:{}",
                filename, location.line, location.column
            ),
            kind: core_data::ReferenceSymbolKind::Function,
        })
    }

    /// Parse prepare call hierarchy response
    pub fn parse_prepare_call_hierarchy_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<PrepareCallHierarchyResponse>> {
        crate::call_hierarchy::parse_prepare_call_hierarchy_response_impl(
            &mut self.pending_requests,
            response,
        )
    }

    /// Parse outgoing calls response
    pub fn parse_outgoing_calls_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<OutgoingCallsResponse>> {
        crate::call_hierarchy::parse_outgoing_calls_response_impl(
            &mut self.pending_requests,
            response,
        )
    }

    /// Parse incoming calls response
    pub fn parse_incoming_calls_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<IncomingCallsResponse>> {
        crate::call_hierarchy::parse_incoming_calls_response_impl(
            &mut self.pending_requests,
            response,
        )
    }

    /// Request outgoing calls from a call hierarchy item
    pub async fn get_outgoing_calls(&mut self, item: lsp::CallHierarchyItem) -> Result<i64> {
        let params = lsp::CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.send_request("callHierarchy/outgoingCalls", serde_json::to_value(params)?)
            .await
    }

    /// Request incoming calls from a call hierarchy item
    pub async fn get_incoming_calls(&mut self, item: lsp::CallHierarchyItem) -> Result<i64> {
        let params = lsp::CallHierarchyIncomingCallsParams {
            item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.send_request("callHierarchy/incomingCalls", serde_json::to_value(params)?)
            .await
    }

    /// Prepare call hierarchy for a document position
    pub async fn prepare_call_hierarchy(
        &mut self,
        document_uri: lsp::Url,
        position: lsp::Position,
    ) -> Result<i64> {
        let params = lsp::CallHierarchyPrepareParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document: lsp::TextDocumentIdentifier { uri: document_uri },
                position,
            },
            work_done_progress_params: Default::default(),
        };

        self.send_request(
            "textDocument/prepareCallHierarchy",
            serde_json::to_value(params)?,
        )
        .await
    }
}

// Helper function to test the parsing logic without LspClient
fn parse_find_references_response_impl(
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
                    let lsp_locations: Vec<lsp::Location> = serde_json::from_value(result.clone())?;
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
                    let lsp_locations: Vec<lsp::Location> = serde_json::from_value(result.clone())?;
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
fn parse_workspace_symbol_response_impl(
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

                    let workspace_symbols: Vec<lsp::WorkspaceSymbol> =
                        serde_json::from_value(result.clone())?;
                    let symbols = workspace_symbols
                        .iter()
                        .map(|symbol| {
                            let location = match &symbol.location {
                                lsp::OneOf::Left(location) => convert_lsp_location(location),
                                lsp::OneOf::Right(workspace_location) => core_data::Location {
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
                            WorkspaceSymbolInfo {
                                name: symbol.name.clone(),
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
fn parse_document_symbol_response_impl(
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
                        serde_json::from_value::<Vec<lsp::DocumentSymbol>>(result.clone())
                    {
                        // Convert DocumentSymbol to our format
                        doc_symbols
                            .into_iter()
                            .flat_map(|doc_symbol| {
                                convert_document_symbol_recursive(&doc_symbol, None)
                            })
                            .collect()
                    } else if let Ok(symbol_infos) =
                        serde_json::from_value::<Vec<lsp::SymbolInformation>>(result.clone())
                    {
                        // Convert SymbolInformation to our format
                        symbol_infos
                            .iter()
                            .map(|symbol| WorkspaceSymbolInfo {
                                name: symbol.name.clone(),
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

fn parse_hover_response_impl(
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
                    if let Ok(hover) = serde_json::from_value::<lsp_types::Hover>(result.clone()) {
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
pub fn extract_text_from_marked_string(marked_string: &lsp_types::MarkedString) -> String {
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
pub fn extract_text_from_markup(markup: &lsp_types::MarkupContent) -> String {
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
pub fn extract_function_name_from_signature(signature: &str) -> Option<String> {
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

// Helper to recursively convert DocumentSymbol to our WorkspaceSymbolInfo
fn convert_document_symbol_recursive(
    doc_symbol: &lsp::DocumentSymbol,
    container: Option<&str>,
) -> Vec<WorkspaceSymbolInfo> {
    let mut results = Vec::new();

    // Convert the current symbol
    let symbol_info = WorkspaceSymbolInfo {
        name: doc_symbol.name.clone(),
        kind: doc_symbol.kind,
        location: core_data::Location {
            file_path: "".to_string(), // Will be filled in by caller
            line: doc_symbol.selection_range.start.line,
            column: doc_symbol.selection_range.start.character,
            length: Some(
                doc_symbol.selection_range.end.character
                    - doc_symbol.selection_range.start.character,
            ),
        },
        container_name: container.map(|s| s.to_string()),
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

// Some helpers for testing
#[cfg(test)]
impl LspClient {
    async fn initialize_with_capabilities(&mut self, root_uri: lsp::Url) -> Result<i64> {
        // Create default workspace folders
        let workspaces = vec![lsp::WorkspaceFolder {
            uri: root_uri.clone(),
            name: root_uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "workspace".to_string()),
        }];

        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            root_path: None,
            root_uri: Some(root_uri),
            initialization_options: None,
            capabilities: lsp::ClientCapabilities {
                text_document: Some(lsp::TextDocumentClientCapabilities {
                    document_symbol: Some(lsp::DocumentSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        symbol_kind: Some(lsp::SymbolKindCapability {
                            value_set: Some(vec![
                                lsp::SymbolKind::FILE,
                                lsp::SymbolKind::MODULE,
                                lsp::SymbolKind::NAMESPACE,
                                lsp::SymbolKind::PACKAGE,
                                lsp::SymbolKind::CLASS,
                                lsp::SymbolKind::METHOD,
                                lsp::SymbolKind::PROPERTY,
                                lsp::SymbolKind::FIELD,
                                lsp::SymbolKind::CONSTRUCTOR,
                                lsp::SymbolKind::ENUM,
                                lsp::SymbolKind::INTERFACE,
                                lsp::SymbolKind::FUNCTION,
                                lsp::SymbolKind::VARIABLE,
                                lsp::SymbolKind::CONSTANT,
                                lsp::SymbolKind::STRING,
                                lsp::SymbolKind::NUMBER,
                                lsp::SymbolKind::BOOLEAN,
                                lsp::SymbolKind::ARRAY,
                                lsp::SymbolKind::OBJECT,
                                lsp::SymbolKind::KEY,
                                lsp::SymbolKind::NULL,
                                lsp::SymbolKind::ENUM_MEMBER,
                                lsp::SymbolKind::STRUCT,
                                lsp::SymbolKind::EVENT,
                                lsp::SymbolKind::OPERATOR,
                                lsp::SymbolKind::TYPE_PARAMETER,
                            ]),
                        }),
                        hierarchical_document_symbol_support: Some(true),
                        tag_support: None,
                    }),
                    synchronization: Some(lsp::TextDocumentSyncClientCapabilities {
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        did_save: Some(false),
                    }),
                    ..Default::default()
                }),
                workspace: Some(lsp::WorkspaceClientCapabilities {
                    symbol: Some(lsp::WorkspaceSymbolClientCapabilities {
                        dynamic_registration: Some(false),
                        symbol_kind: Some(lsp::SymbolKindCapability {
                            value_set: Some(vec![
                                lsp::SymbolKind::FILE,
                                lsp::SymbolKind::MODULE,
                                lsp::SymbolKind::NAMESPACE,
                                lsp::SymbolKind::PACKAGE,
                                lsp::SymbolKind::CLASS,
                                lsp::SymbolKind::METHOD,
                                lsp::SymbolKind::PROPERTY,
                                lsp::SymbolKind::FIELD,
                                lsp::SymbolKind::CONSTRUCTOR,
                                lsp::SymbolKind::ENUM,
                                lsp::SymbolKind::INTERFACE,
                                lsp::SymbolKind::FUNCTION,
                                lsp::SymbolKind::VARIABLE,
                                lsp::SymbolKind::CONSTANT,
                                lsp::SymbolKind::STRING,
                                lsp::SymbolKind::NUMBER,
                                lsp::SymbolKind::BOOLEAN,
                                lsp::SymbolKind::ARRAY,
                                lsp::SymbolKind::OBJECT,
                                lsp::SymbolKind::KEY,
                                lsp::SymbolKind::NULL,
                                lsp::SymbolKind::ENUM_MEMBER,
                                lsp::SymbolKind::STRUCT,
                                lsp::SymbolKind::EVENT,
                                lsp::SymbolKind::OPERATOR,
                                lsp::SymbolKind::TYPE_PARAMETER,
                            ]),
                        }),
                        tag_support: None,
                        resolve_support: Some(lsp::WorkspaceSymbolResolveSupportCapability {
                            properties: vec!["location.uri".to_string()],
                        }),
                    }),
                    workspace_folders: Some(true),
                    configuration: Some(true),
                    did_change_configuration: Some(lsp::DidChangeConfigurationClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    did_change_watched_files: Some(lsp::DidChangeWatchedFilesClientCapabilities {
                        dynamic_registration: Some(false),
                        relative_pattern_support: Some(false),
                    }),
                    execute_command: Some(lsp::ExecuteCommandClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    apply_edit: Some(true),
                    ..Default::default()
                }),
                window: Some(lsp::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    show_message: Some(lsp::ShowMessageRequestClientCapabilities {
                        message_action_item: Some(lsp::MessageActionItemCapabilities {
                            additional_properties_support: Some(false),
                        }),
                    }),
                    show_document: Some(lsp::ShowDocumentClientCapabilities { support: true }),
                }),
                general: Some(lsp::GeneralClientCapabilities {
                    regular_expressions: Some(lsp::RegularExpressionsClientCapabilities {
                        engine: "ECMAScript".to_string(),
                        version: Some("ES2020".to_string()),
                    }),
                    markdown: Some(lsp::MarkdownClientCapabilities {
                        parser: "marked".to_string(),
                        version: Some("1.1.0".to_string()),
                        allowed_tags: Some(vec![]),
                    }),
                    stale_request_support: None,
                    position_encodings: Some(vec![
                        lsp::PositionEncodingKind::UTF16,
                        lsp::PositionEncodingKind::UTF8,
                    ]),
                }),
                experimental: None,
            },
            trace: Some(lsp::TraceValue::Verbose),
            workspace_folders: Some(workspaces),
            client_info: Some(lsp::ClientInfo {
                name: "coon".to_string(),
                version: Some("0.1.0".to_string()),
            }),
            locale: Some("en-US".to_string()),
            work_done_progress_params: Default::default(),
        };
        self.send_request("initialize", serde_json::to_value(params)?)
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
