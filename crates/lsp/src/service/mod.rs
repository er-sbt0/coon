use crate::LspClient;
use anyhow::Result;
use lsp_types::{CallHierarchyItem, CallHierarchyOutgoingCall, DocumentSymbol, Position, Url};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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
    PrepareCallHierarchy {
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
    /// Close every document the server currently has open, freeing its
    /// per-document state.
    ///
    /// Fire-and-forget: `didClose` is a notification, and this travels the same
    /// channel as everything queued after it, so a reopen cannot overtake it.
    CloseDocuments,
    SetProjectFiles {
        project_files: Vec<String>,
    },
    Shutdown,
}

impl LspRequest {
    /// The id the worker's reply will carry, or `None` for the three requests
    /// that are answered by nothing.
    ///
    /// Exists for the failure path in `loader::run_tui_lsp_bridge`, which has
    /// to tell whoever is waiting that their request never went out — and by
    /// then the request itself has been moved into [`LspService::send`]. The
    /// id-less three are the ones nobody is waiting on: a close, a
    /// project-file list and the shutdown.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::PrepareCallHierarchy { request_id, .. }
            | Self::GetOutgoingCalls { request_id, .. }
            | Self::GetIncomingCalls { request_id, .. }
            | Self::FindReferences { request_id, .. }
            | Self::FindReferencesWithSymbols { request_id, .. }
            | Self::GetDocumentSymbols { request_id, .. }
            | Self::GetWorkspaceSymbols { request_id, .. }
            | Self::PreloadDocuments { request_id, .. } => Some(request_id),
            Self::CloseDocuments | Self::SetProjectFiles { .. } | Self::Shutdown => None,
        }
    }
}

/// How far clangd's background indexer has got.
///
/// `workspace/symbol` is answered from that index as it stands at the instant
/// the request arrives — clangd never blocks waiting for it — so a query sent
/// while this is anything but [`IndexProgress::Finished`] returns a sample of
/// the project rather than the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProgress {
    /// Indexing has begun; clangd has not said how much of it there is yet.
    Started,
    /// `done` of `total` translation units indexed.
    Indexing { done: usize, total: usize },
    /// The queue has drained. The index now covers the whole project.
    Finished,
}

/// Response types for LSP operations
#[derive(Debug, Clone)]
pub enum LspResponse {
    CallHierarchyPrepared {
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
    /// Unsolicited: clangd's background indexer moved. Carries no request id
    /// because nothing asked for it.
    IndexProgress(IndexProgress),
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

    /// Queue a request for the worker.
    ///
    /// This replaced eleven one-line methods — `request_outgoing_calls`,
    /// `request_workspace_symbols`, `preload_documents` and the rest — each of
    /// which took one variant's fields apart as parameters, put them straight
    /// back together and sent the result down this channel. Naming a variant
    /// twice bought nothing and cost the crate's one forwarder its shape:
    /// `loader::run_tui_lsp_bridge` takes an `LspRequest` off the frontend's
    /// channel, and it had to destructure it arm by arm purely to call the
    /// wrapper that rebuilt the identical value. With a single `send` it
    /// passes the request through whole, and a variant added to [`LspRequest`]
    /// costs neither a method here nor an arm there.
    ///
    /// Two of the eleven sent the *same* variant, which is the duplication read
    /// from the other end: `preload_documents` minted a request id and
    /// returned it, `request_preload_documents` took one that already existed.
    /// Minting belongs to whoever is going to match the reply against it, so
    /// it moved to the two call sites that wait for a `PreloadComplete`.
    ///
    /// Fire-and-forget from here: a request that has an answer is answered on
    /// the response channel [`Self::recv_response`] drains, matched by its
    /// `request_id`. The channel keeps order, so nothing queued after a
    /// [`LspRequest::CloseDocuments`] can overtake it.
    pub async fn send(&self, request: LspRequest) -> Result<()> {
        self.request_tx.send(request).await?;
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
