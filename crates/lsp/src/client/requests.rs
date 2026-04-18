use anyhow::Result;
use lsp_types as lsp;

use super::LspClient;

impl LspClient {
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
        self.send_notification("initialized", serde_json::json!({}))
            .await
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
    pub(crate) async fn initialize_with_capabilities(&mut self, root_uri: lsp::Url) -> Result<i64> {
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
