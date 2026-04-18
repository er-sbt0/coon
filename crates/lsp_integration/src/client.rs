#![allow(dead_code)]
use anyhow::Result;
use lsp_types as lsp;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::parsing;
use crate::types::*;

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

    pub async fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
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
        let result =
            parsing::parse_find_references_response_impl(&mut self.pending_requests, response);
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
        parsing::parse_workspace_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse document symbol response and convert to our data structures
    pub fn parse_document_symbol_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<DocumentSymbolResponse>> {
        parsing::parse_document_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse hover response
    pub fn parse_hover_response(&mut self, response: &Value) -> Result<Option<HoverResponse>> {
        parsing::parse_hover_response_impl(&mut self.pending_requests, response)
    }

    /// Enhance references with symbol information
    pub async fn enhance_references(
        &mut self,
        references: Vec<model::Location>,
    ) -> Result<Vec<model::Reference>> {
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

                    model::ReferencingSymbol {
                        name: format!("{}:{}", filename, location.line),
                        qualified_name: format!(
                            "{}::{}:{}",
                            filename, location.line, location.column
                        ),
                        kind: model::ReferenceSymbolKind::Function,
                    }
                });

            enhanced_references.push(model::Reference {
                location,
                referencing_symbol: Some(referencing_symbol),
            });
        }

        Ok(enhanced_references)
    }

    /// Attempt to resolve the symbol at a given location
    async fn resolve_symbol_at_location(
        &mut self,
        location: &model::Location,
    ) -> Option<model::ReferencingSymbol> {
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
        Some(model::ReferencingSymbol {
            name: format!("caller_at_{}:{}", filename, location.line),
            qualified_name: format!(
                "{}::caller_at_{}:{}",
                filename, location.line, location.column
            ),
            kind: model::ReferenceSymbolKind::Function,
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
