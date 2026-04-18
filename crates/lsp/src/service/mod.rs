use crate::LspClient;
use anyhow::Result;
use lsp_types::{CallHierarchyItem, CallHierarchyOutgoingCall, DocumentSymbol, Position, Url};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

mod document;
mod request;
mod response;
mod worker;

/// Async LSP service that handles background requests
pub struct LspService {
    request_tx: mpsc::Sender<LspRequest>,
    response_rx: mpsc::Receiver<LspResponse>,
    worker_handle: Option<JoinHandle<()>>,
}

/// Request types for LSP operations
#[derive(Debug, Clone)]
pub enum LspRequest {
    GetCallHierarchy {
        request_id: String,
        document_uri: Url,
        position: Position,
    },
    GetOutgoingCalls {
        request_id: String,
        call_hierarchy_item: lsp_types::CallHierarchyItem,
    },
    GetIncomingCalls {
        request_id: String,
        call_hierarchy_item: lsp_types::CallHierarchyItem,
    },
    PrepareCallHierarchy {
        request_id: String,
        document_uri: Url,
        position: Position,
    },
    FindReferences {
        request_id: String,
        document_uri: Url,
        position: Position,
    },
    FindReferencesWithSymbols {
        request_id: String,
        document_uri: Url,
        position: Position,
    },
    GetDocumentSymbols {
        request_id: String,
        document_uri: Url,
    },
    GetWorkspaceSymbols {
        request_id: String,
        query: String,
    },
    PreloadDocuments {
        request_id: String,
        document_uris: Vec<Url>,
    },
    SetProjectFiles {
        project_files: Vec<String>,
    },
    Shutdown,
}

/// Response types for LSP operations
#[derive(Debug, Clone)]
pub enum LspResponse {
    CallHierarchy {
        request_id: String,
        items: Vec<CallHierarchyItem>,
    },
    OutgoingCalls {
        request_id: String,
        calls: Vec<CallHierarchyOutgoingCall>,
    },
    IncomingCalls {
        request_id: String,
        calls: Vec<lsp_types::CallHierarchyIncomingCall>,
    },
    CallHierarchyPrepared {
        request_id: String,
        items: Vec<lsp_types::CallHierarchyItem>,
    },
    References {
        request_id: String,
        locations: Vec<model::Location>,
    },
    ReferencesWithSymbols {
        request_id: String,
        references: Vec<model::Reference>,
    },
    DocumentSymbols {
        request_id: String,
        symbols: Vec<DocumentSymbol>,
    },
    WorkspaceSymbols {
        request_id: String,
        symbols: Vec<model::FunctionNode>,
    },
    PreloadComplete {
        request_id: String,
        loaded_count: usize,
        failed_count: usize,
    },
    Error {
        request_id: String,
        error: String,
    },
}

impl LspService {
    /// Create a new LSP service with background worker
    pub async fn new(client: LspClient, lsp_message_rx: mpsc::Receiver<Value>) -> Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<LspRequest>(100);
        let (response_tx, response_rx) = mpsc::channel::<LspResponse>(100);

        let worker_handle = tokio::spawn(async move {
            worker::run_loop(client, request_rx, response_tx, lsp_message_rx).await;
        });

        Ok(Self {
            request_tx,
            response_rx,
            worker_handle: Some(worker_handle),
        })
    }

    /// Request call hierarchy for a symbol
    pub async fn request_call_hierarchy(
        &self,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::GetCallHierarchy {
                request_id,
                document_uri,
                position,
            })
            .await?;
        Ok(())
    }

    /// Request references for a symbol
    pub async fn request_references(
        &self,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::FindReferences {
                request_id,
                document_uri,
                position,
            })
            .await?;
        Ok(())
    }

    /// Request enhanced references with symbol information
    pub async fn request_references_with_symbols(
        &self,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::FindReferencesWithSymbols {
                request_id,
                document_uri,
                position,
            })
            .await?;
        Ok(())
    }

    /// Request document symbols
    pub async fn request_document_symbols(
        &self,
        request_id: String,
        document_uri: Url,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::GetDocumentSymbols {
                request_id,
                document_uri,
            })
            .await?;
        Ok(())
    }

    /// Request workspace symbols for project refresh
    pub async fn request_workspace_symbols(&self, request_id: String, query: String) -> Result<()> {
        self.request_tx
            .send(LspRequest::GetWorkspaceSymbols { request_id, query })
            .await?;
        Ok(())
    }

    /// Request outgoing calls for a call hierarchy item
    pub async fn request_outgoing_calls(
        &self,
        request_id: String,
        call_hierarchy_item: lsp_types::CallHierarchyItem,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::GetOutgoingCalls {
                request_id,
                call_hierarchy_item,
            })
            .await?;
        Ok(())
    }

    pub async fn request_incoming_calls(
        &self,
        request_id: String,
        call_hierarchy_item: lsp_types::CallHierarchyItem,
    ) -> Result<()> {
        self.request_tx
            .send(LspRequest::GetIncomingCalls {
                request_id,
                call_hierarchy_item,
            })
            .await?;
        Ok(())
    }

    /// Preload multiple documents to avoid "non-added document" errors
    pub async fn preload_documents(&self, document_uris: Vec<Url>) -> Result<String> {
        let request_id = Uuid::new_v4().to_string();
        self.request_tx
            .send(LspRequest::PreloadDocuments {
                request_id: request_id.clone(),
                document_uris,
            })
            .await?;
        Ok(request_id)
    }

    /// Set project files for symbol filtering
    pub async fn set_project_files(&self, project_files: Vec<String>) -> Result<()> {
        self.request_tx
            .send(LspRequest::SetProjectFiles { project_files })
            .await?;
        Ok(())
    }

    /// Receive the next response, awaiting until one arrives or the channel closes.
    pub async fn recv_response(&mut self) -> Option<LspResponse> {
        self.response_rx.recv().await
    }

    /// Shutdown the service
    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.request_tx.send(LspRequest::Shutdown).await;

        if let Some(handle) = self.worker_handle.take() {
            handle.await?;
        }

        Ok(())
    }
}

impl Drop for LspService {
    fn drop(&mut self) {
        if let Some(handle) = self.worker_handle.take() {
            handle.abort();
        }
    }
}
