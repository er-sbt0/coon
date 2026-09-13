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
                // Ask for the nested form. Without this clangd falls back to
                // the flat `SymbolInformation[]`, where every namespace and
                // class is a sibling spanning its whole body — so looking up
                // which symbol encloses a position lands on the outermost
                // namespace instead of the function, and there is no tree to
                // read a scope out of.
                document_symbol: Some(lsp::DocumentSymbolClientCapabilities {
                    dynamic_registration: Some(false),
                    hierarchical_document_symbol_support: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            // Ask to be told about long-running server work. clangd gates its
            // background-index reporting on this one flag: without it
            // `BackgroundIndexProgress` stays `Unsupported` and not a single
            // `$/progress` is sent, which leaves the loader nothing to wait on
            // and no way to know whether `workspace/symbol` is being answered
            // from a finished index or from a tenth of one.
            window: Some(lsp::WindowClientCapabilities {
                work_done_progress: Some(true),
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

    /// Send an enhanced find references request that will resolve symbol names.
    /// Identical wire request to `find_references`; the enhanced/plain discrimination
    /// is carried by `RequestType` in the worker, not by the method string here.
    pub async fn find_references_with_symbols(
        &mut self,
        document_id: lsp::TextDocumentIdentifier,
        position: lsp::Position,
    ) -> Result<i64> {
        self.find_references(document_id, position).await
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
