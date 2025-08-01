use anyhow::Result;
use lsp_types::{CallHierarchyItem, DocumentSymbol, Position, Url};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::LspClient;
use core_data;

/// Async LSP service that handles background requests
pub struct LspService {
    request_tx: mpsc::Sender<LspRequest>,
    response_rx: mpsc::Receiver<LspResponse>,
    worker_handle: Option<JoinHandle<()>>,
}

/// Internal state for the LSP worker
struct LspWorkerState {
    client: LspClient,
    service_requests: HashMap<i64, String>, // Maps LSP ID to service request ID
    opened_documents: HashSet<Url>,
    project_files: HashSet<String>, // Set of project file paths for filtering
    document_symbols_cache: HashMap<String, Vec<lsp_types::DocumentSymbol>>, // Cache document symbols by file path
    pending_enhanced_requests: HashMap<String, EnhancedRequestInfo>, // Track enhanced requests needing symbol resolution
    enhanced_lsp_requests: HashSet<i64>, // Track which LSP request IDs are enhanced references
}

/// Information about pending enhanced requests
#[derive(Debug, Clone)]
struct EnhancedRequestInfo {
    request_id: String,
    locations: Vec<core_data::Location>,
    pending_symbol_requests: HashSet<String>, // File paths we're waiting for symbols
}

/// Request types for LSP operations
#[derive(Debug, Clone)]
pub enum LspRequest {
    GetCallHierarchy {
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
    References {
        request_id: String,
        locations: Vec<core_data::Location>,
    },
    ReferencesWithSymbols {
        request_id: String,
        references: Vec<core_data::Reference>,
    },
    DocumentSymbols {
        request_id: String,
        symbols: Vec<DocumentSymbol>,
    },
    WorkspaceSymbols {
        request_id: String,
        symbols: Vec<core_data::FunctionNode>,
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

        // Start background worker
        let worker_handle = tokio::spawn(async move {
            Self::worker_loop(client, request_rx, response_tx, lsp_message_rx).await;
        });

        Ok(Self {
            request_tx,
            response_rx,
            worker_handle: Some(worker_handle),
        })
    }

    /// Background worker that processes LSP requests
    async fn worker_loop(
        client: LspClient,
        mut request_rx: mpsc::Receiver<LspRequest>,
        response_tx: mpsc::Sender<LspResponse>,
        mut lsp_message_rx: mpsc::Receiver<Value>,
    ) {
        let mut state = LspWorkerState {
            client,
            service_requests: HashMap::new(),
            opened_documents: HashSet::new(),
            project_files: HashSet::new(),
            document_symbols_cache: HashMap::new(),
            pending_enhanced_requests: HashMap::new(),
            enhanced_lsp_requests: HashSet::new(),
        };

        loop {
            tokio::select! {
                // Handle incoming requests
                request = request_rx.recv() => {
                    match request {
                        Some(LspRequest::GetCallHierarchy { request_id, document_uri, position }) => {
                            Self::handle_call_hierarchy_request(&mut state, &response_tx, request_id, document_uri, position).await;
                        }
                        Some(LspRequest::FindReferences { request_id, document_uri, position }) => {
                            Self::handle_references_request(&mut state, &response_tx, request_id, document_uri, position).await;
                        }
                        Some(LspRequest::FindReferencesWithSymbols { request_id, document_uri, position }) => {
                            Self::handle_references_with_symbols_request(&mut state, &response_tx, request_id, document_uri, position).await;
                        }
                        Some(LspRequest::GetDocumentSymbols { request_id, document_uri }) => {
                            Self::handle_document_symbols_request(&mut state, &response_tx, request_id, document_uri).await;
                        }
                        Some(LspRequest::GetWorkspaceSymbols { request_id, query }) => {
                            Self::handle_workspace_symbols_request(&mut state, &response_tx, request_id, query).await;
                        }
                        Some(LspRequest::PreloadDocuments { request_id, document_uris }) => {
                            Self::handle_preload_documents(&mut state, &response_tx, request_id, document_uris).await;
                        }
                        Some(LspRequest::SetProjectFiles { project_files }) => {
                            state.project_files = project_files.into_iter().collect();
                            log::info!("Updated project files: {} files", state.project_files.len());
                        }
                        Some(LspRequest::Shutdown) | None => {
                            break;
                        }
                    }
                }

                // Handle LSP responses
                message = lsp_message_rx.recv() => {
                    if let Some(message) = message {
                        Self::handle_lsp_message(message, &mut state, &response_tx).await;
                    }
                }
            }
        }
    }

    /// Handle preloading multiple documents
    async fn handle_preload_documents(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        document_uris: Vec<Url>,
    ) {
        let mut loaded_count = 0;
        let mut failed_count = 0;

        for uri in document_uris {
            match Self::ensure_document_opened(state, &uri).await {
                Ok(()) => {
                    loaded_count += 1;
                    log::debug!("Preloaded document: {}", uri);
                }
                Err(e) => {
                    failed_count += 1;
                    log::warn!("Failed to preload document {}: {}", uri, e);
                }
            }
        }

        let _ = response_tx
            .send(LspResponse::PreloadComplete {
                request_id,
                loaded_count,
                failed_count,
            })
            .await;
    }

    /// Handle call hierarchy request with document opening
    async fn handle_call_hierarchy_request(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) {
        log::info!(
            "Handling call hierarchy request: uri={}, position={}:{}, request_id={}",
            document_uri,
            position.line,
            position.character,
            request_id
        );

        // Ensure document is opened
        if let Err(e) = Self::ensure_document_opened(state, &document_uri).await {
            log::error!("Failed to open document for call hierarchy: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to open document: {}", e),
                })
                .await;
            return;
        }

        let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
        match state
            .client
            .prepare_call_hierarchy(text_document, position)
            .await
        {
            Ok(lsp_request_id) => {
                log::info!(
                    "Sent call hierarchy request to LSP server: lsp_request_id={}",
                    lsp_request_id
                );
                state.service_requests.insert(lsp_request_id, request_id);
            }
            Err(e) => {
                log::error!("Failed to send call hierarchy request: {}", e);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: format!("Failed to request call hierarchy: {}", e),
                    })
                    .await;
            }
        }
    }

    /// Handle references request with document opening
    async fn handle_references_request(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) {
        log::info!(
            "Handling references request: uri={}, position={}:{}, request_id={}",
            document_uri,
            position.line,
            position.character,
            request_id
        );

        // Ensure document is opened
        if let Err(e) = Self::ensure_document_opened(state, &document_uri).await {
            log::error!("Failed to open document for references: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to open document: {}", e),
                })
                .await;
            return;
        }

        let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
        match state.client.find_references(text_document, position).await {
            Ok(lsp_request_id) => {
                log::info!(
                    "Sent references request to LSP server: lsp_request_id={}",
                    lsp_request_id
                );
                state.service_requests.insert(lsp_request_id, request_id);
            }
            Err(e) => {
                log::error!("Failed to send references request: {}", e);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: format!("Failed to request references: {}", e),
                    })
                    .await;
            }
        }
    }

    /// Handle enhanced references request with symbol information
    async fn handle_references_with_symbols_request(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        document_uri: Url,
        position: Position,
    ) {
        log::info!(
            "Handling enhanced references request: uri={}, position={}:{}, request_id={}",
            document_uri,
            position.line,
            position.character,
            request_id
        );

        // Ensure document is opened
        if let Err(e) = Self::ensure_document_opened(state, &document_uri).await {
            log::error!("Failed to open document for enhanced references: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to open document: {}", e),
                })
                .await;
            return;
        }

        let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
        match state
            .client
            .find_references_with_symbols(text_document, position)
            .await
        {
            Ok(lsp_request_id) => {
                log::info!(
                    "Sent enhanced references request to LSP server: lsp_request_id={}",
                    lsp_request_id
                );
                state.service_requests.insert(lsp_request_id, request_id);
                state.enhanced_lsp_requests.insert(lsp_request_id); // Track as enhanced
            }
            Err(e) => {
                log::error!("Failed to send enhanced references request: {}", e);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: format!("Failed to request enhanced references: {}", e),
                    })
                    .await;
            }
        }
    }

    /// Handle document symbols request with document opening
    async fn handle_document_symbols_request(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        document_uri: Url,
    ) {
        log::info!(
            "Handling document symbols request: uri={}, request_id={}",
            document_uri,
            request_id
        );

        // Ensure document is opened
        if let Err(e) = Self::ensure_document_opened(state, &document_uri).await {
            log::error!("Failed to open document for document symbols: {}", e);
            let _ = response_tx
                .send(LspResponse::Error {
                    request_id,
                    error: format!("Failed to open document: {}", e),
                })
                .await;
            return;
        }

        let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
        match state.client.document_symbol(text_document).await {
            Ok(lsp_request_id) => {
                log::info!(
                    "Sent document symbols request to LSP server: lsp_request_id={}",
                    lsp_request_id
                );
                state.service_requests.insert(lsp_request_id, request_id);
            }
            Err(e) => {
                log::error!("Failed to send document symbols request: {}", e);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: format!("Failed to request document symbols: {}", e),
                    })
                    .await;
            }
        }
    }

    async fn handle_workspace_symbols_request(
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
        request_id: String,
        query: String,
    ) {
        log::info!(
            "Handling workspace symbols request: query='{}', request_id={}",
            query,
            request_id
        );

        match state.client.workspace_symbol(&query).await {
            Ok(lsp_request_id) => {
                log::info!(
                    "Sent workspace symbols request to LSP server: lsp_request_id={}",
                    lsp_request_id
                );
                state.service_requests.insert(lsp_request_id, request_id);
            }
            Err(e) => {
                log::error!("Failed to send workspace symbols request: {}", e);
                let _ = response_tx
                    .send(LspResponse::Error {
                        request_id,
                        error: format!("Failed to request workspace symbols: {}", e),
                    })
                    .await;
            }
        }
    }

    /// Ensure a document is opened in the LSP server
    async fn ensure_document_opened(state: &mut LspWorkerState, document_uri: &Url) -> Result<()> {
        if state.opened_documents.contains(document_uri) {
            log::debug!("Document already opened: {}", document_uri);
            return Ok(()); // Already opened
        }

        log::info!("Opening document in LSP server: {}", document_uri);

        // Convert URI to file path and read content
        let file_path = document_uri
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Invalid file URI: {}", document_uri))?;

        let content = std::fs::read_to_string(&file_path)
            .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path.display(), e))?;

        log::debug!(
            "Read file content: {} bytes from {}",
            content.len(),
            file_path.display()
        );

        // Determine language ID from file extension
        let language_id = match file_path.extension().and_then(|ext| ext.to_str()) {
            Some("rs") => "rust",
            Some("c") => "c",
            Some("cpp") | Some("cxx") | Some("cc") => "cpp",
            Some("h") | Some("hpp") | Some("hxx") => "c",
            Some("py") => "python",
            Some("js") => "javascript",
            Some("ts") => "typescript",
            Some("java") => "java",
            Some("go") => "go",
            _ => "plaintext",
        };

        log::info!(
            "Sending didOpen notification: uri={}, language_id={}, version=1",
            document_uri,
            language_id
        );

        // Open the document
        state
            .client
            .did_open(document_uri.clone(), language_id, 1, content)
            .await?;
        state.opened_documents.insert(document_uri.clone());

        log::info!(
            "Successfully opened document: {} (total opened: {})",
            document_uri,
            state.opened_documents.len()
        );

        // Give the server a moment to process the document
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Ok(())
    }

    /// Handle incoming LSP messages and convert them to responses
    async fn handle_lsp_message(
        message: Value,
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
    ) {
        if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
            if let Some(request_id) = state.service_requests.remove(&id) {
                // Try to parse different response types
                if let Ok(Some(call_hierarchy_response)) =
                    state.client.parse_prepare_call_hierarchy_response(&message)
                {
                    // Remove from enhanced tracking now that we know it's not a references request
                    state.enhanced_lsp_requests.remove(&id);

                    log::info!(
                        "LSP Call Hierarchy Response: found {} items for request {}",
                        call_hierarchy_response.items.len(),
                        request_id
                    );

                    // Log each call hierarchy item
                    for (i, item) in call_hierarchy_response.items.iter().enumerate() {
                        log::info!("  Call hierarchy item {}: name='{}', kind={:?}, uri={}, range={}:{}-{}:{}", 
                            i, item.name, item.kind, item.uri,
                            item.range.start.line, item.range.start.character,
                            item.range.end.line, item.range.end.character);
                    }

                    let _ = response_tx
                        .send(LspResponse::CallHierarchy {
                            request_id,
                            items: call_hierarchy_response.items,
                        })
                        .await;
                } else if Self::is_references_response(&message) {
                    // Check if this was an enhanced references request
                    if Self::was_enhanced_references_request(&message, state) {
                        // Parse enhanced references with symbol information
                        let enhanced_references = Self::parse_enhanced_references_response(
                            &message,
                            &request_id,
                            state,
                            response_tx,
                        )
                        .await;

                        // Only send response if we got results immediately
                        if !enhanced_references.is_empty() {
                            log::info!(
                                "LSP Enhanced References Response: found {} references for request {}",
                                enhanced_references.len(),
                                request_id
                            );

                            // Log each enhanced reference
                            for (i, ref_info) in enhanced_references.iter().enumerate() {
                                if let Some(symbol) = &ref_info.referencing_symbol {
                                    log::info!(
                                        "  Enhanced Reference {}: {}:{}:{} (from {}::{})",
                                        i,
                                        ref_info.location.file_path,
                                        ref_info.location.line,
                                        ref_info.location.column,
                                        symbol.qualified_name,
                                        symbol.name
                                    );
                                } else {
                                    log::info!(
                                        "  Enhanced Reference {}: {}:{}:{} (no symbol info)",
                                        i,
                                        ref_info.location.file_path,
                                        ref_info.location.line,
                                        ref_info.location.column
                                    );
                                }
                            }

                            let _ = response_tx
                                .send(LspResponse::ReferencesWithSymbols {
                                    request_id,
                                    references: enhanced_references,
                                })
                                .await;
                        }
                        // If empty, the response will be sent later when symbols arrive

                        // Remove from enhanced tracking after processing enhanced references
                        state.enhanced_lsp_requests.remove(&id);
                    } else {
                        // Parse regular references response
                        let locations = Self::parse_references_response_content(&message);

                        log::info!(
                            "LSP References Response: found {} references for request {}",
                            locations.len(),
                            request_id
                        );

                        // Log each reference location
                        for (i, loc) in locations.iter().enumerate() {
                            log::info!(
                                "  Reference {}: {}:{}:{}",
                                i,
                                loc.file_path,
                                loc.line,
                                loc.column
                            );
                        }

                        let _ = response_tx
                            .send(LspResponse::References {
                                request_id,
                                locations,
                            })
                            .await;

                        // Remove from enhanced tracking after processing regular references
                        state.enhanced_lsp_requests.remove(&id);
                    }
                } else if let Ok(Some(hover_response)) = state.client.parse_hover_response(&message)
                {
                    log::debug!(
                        "LSP Hover Response for request {}: hover_info={:?}",
                        request_id,
                        hover_response.hover_info
                    );

                    // Check if this is a hover request for enhanced references
                    if request_id.starts_with("hover_for_") {
                        log::debug!("Raw hover response message for {}: {}", request_id, message);

                        Self::handle_hover_for_enhanced_references(
                            hover_response,
                            &request_id,
                            state,
                            response_tx,
                        )
                        .await;
                    }

                    // Remove from enhanced tracking after processing hover
                    state.enhanced_lsp_requests.remove(&id);
                } else if let Ok(Some(document_symbols_response)) =
                    state.client.parse_document_symbol_response(&message)
                {
                    log::info!(
                        "LSP Document Symbols Response: found {} symbols for request {}",
                        document_symbols_response.symbols.len(),
                        request_id
                    );

                    // Log each document symbol
                    for (i, sym) in document_symbols_response.symbols.iter().enumerate() {
                        log::info!("  Document symbol {}: name='{}', kind={:?}, container={:?}, location={}:{}:{}", 
                            i, sym.name, sym.kind,
                            sym.container_name.as_deref().unwrap_or("None"),
                            sym.location.file_path, sym.location.line, sym.location.column);
                    }

                    // Check if this is a document symbols request for enhanced references
                    if request_id.starts_with("document_symbols_for_") {
                        let enhanced_request_id = request_id
                            .strip_prefix("document_symbols_for_")
                            .unwrap()
                            .to_string();

                        // TODO: Extract file path from the request or response to cache symbols
                        // For now, we'll try to determine it from the symbols themselves
                        if let Some(first_symbol) = document_symbols_response.symbols.first() {
                            let file_path = first_symbol.location.file_path.clone();

                            // Convert to proper DocumentSymbol format and cache
                            let doc_symbols: Vec<lsp_types::DocumentSymbol> =
                                document_symbols_response
                                    .symbols
                                    .iter()
                                    .map(|sym| lsp_types::DocumentSymbol {
                                        name: sym.name.clone(),
                                        detail: sym.container_name.clone(),
                                        kind: sym.kind,
                                        tags: None,
                                        #[allow(deprecated)]
                                        deprecated: Some(false),
                                        range: lsp_types::Range {
                                            start: lsp_types::Position {
                                                line: sym.location.line.saturating_sub(1),
                                                character: sym.location.column.saturating_sub(1),
                                            },
                                            end: lsp_types::Position {
                                                line: sym.location.line,
                                                character: sym.location.column + 10, // Estimate
                                            },
                                        },
                                        selection_range: lsp_types::Range {
                                            start: lsp_types::Position {
                                                line: sym.location.line.saturating_sub(1),
                                                character: sym.location.column.saturating_sub(1),
                                            },
                                            end: lsp_types::Position {
                                                line: sym.location.line,
                                                character: sym.location.column
                                                    + sym.name.len() as u32,
                                            },
                                        },
                                        children: None,
                                    })
                                    .collect();

                            // Cache the symbols
                            state
                                .document_symbols_cache
                                .insert(file_path.clone(), doc_symbols);
                            log::debug!(
                                "Cached {} document symbols for file: {}",
                                document_symbols_response.symbols.len(),
                                file_path
                            );

                            // Check if this completes an enhanced references request
                            if let Some(pending) = state
                                .pending_enhanced_requests
                                .get_mut(&enhanced_request_id)
                            {
                                pending.pending_symbol_requests.remove(&file_path);

                                // If no more pending requests, complete the enhanced references
                                if pending.pending_symbol_requests.is_empty() {
                                    log::info!(
                                        "Completing enhanced references request: {}",
                                        enhanced_request_id
                                    );

                                    // Group locations by file for enhancement
                                    let mut locations_by_file: std::collections::HashMap<
                                        String,
                                        Vec<core_data::Location>,
                                    > = std::collections::HashMap::new();
                                    for location in &pending.locations {
                                        locations_by_file
                                            .entry(location.file_path.clone())
                                            .or_insert_with(Vec::new)
                                            .push(location.clone());
                                    }

                                    let enhanced_refs =
                                        Self::enhance_references_with_cached_symbols(
                                            &pending.locations,
                                            &locations_by_file,
                                            &state.document_symbols_cache,
                                        );

                                    log::info!(
                                        "Enhanced {} references with symbol information",
                                        enhanced_refs.len()
                                    );

                                    // Send the completed response
                                    let _ = response_tx
                                        .send(LspResponse::ReferencesWithSymbols {
                                            request_id: enhanced_request_id.clone(),
                                            references: enhanced_refs,
                                        })
                                        .await;

                                    // Clean up
                                    state.pending_enhanced_requests.remove(&enhanced_request_id);
                                }
                            }
                        }

                        // Don't send a separate DocumentSymbols response for enhanced requests
                    } else {
                        // Regular document symbols request - send normal response
                        let symbols = document_symbols_response
                            .symbols
                            .into_iter()
                            .map(|sym| DocumentSymbol {
                                name: sym.name,
                                detail: sym.container_name,
                                kind: sym.kind,
                                tags: None,
                                #[allow(deprecated)]
                                deprecated: Some(false),
                                range: lsp_types::Range::default(), // Will need proper conversion
                                selection_range: lsp_types::Range::default(),
                                children: None,
                            })
                            .collect();

                        let _ = response_tx
                            .send(LspResponse::DocumentSymbols {
                                request_id,
                                symbols,
                            })
                            .await;
                    }

                    // Remove from enhanced tracking after processing document symbols
                    state.enhanced_lsp_requests.remove(&id);
                } else if request_id.starts_with("document_symbol_for_") {
                    // Handle document symbol response for enhanced references
                    log::debug!(
                        "Processing document symbol response for enhanced references: {}",
                        request_id
                    );

                    if let Some(result) = message.get("result") {
                        if !result.is_null() {
                            match serde_json::from_value::<Vec<lsp_types::DocumentSymbol>>(
                                result.clone(),
                            ) {
                                Ok(document_symbols) => {
                                    log::info!(
                                        "Successfully parsed document symbols for enhanced references request {}: found {} symbols",
                                        request_id,
                                        document_symbols.len()
                                    );

                                    // Extract the base request ID from the document symbol request ID
                                    if let Some(base_request_id) =
                                        request_id.strip_prefix("document_symbol_for_")
                                    {
                                        Self::handle_document_symbols_for_enhanced_references(
                                            base_request_id,
                                            &document_symbols,
                                            state,
                                            response_tx,
                                        )
                                        .await;
                                    }
                                }
                                Err(_) => {
                                    // Try parsing as SymbolInformation (what clangd often returns)
                                    match serde_json::from_value::<Vec<lsp_types::SymbolInformation>>(
                                        result.clone(),
                                    ) {
                                        Ok(symbol_infos) => {
                                            log::info!(
                                                "Successfully parsed symbol information for enhanced references request {}: found {} symbols",
                                                request_id,
                                                symbol_infos.len()
                                            );

                                            // Convert SymbolInformation to DocumentSymbol format
                                            let document_symbols =
                                                Self::convert_symbol_info_to_document_symbols(
                                                    &symbol_infos,
                                                );

                                            // Extract the base request ID from the document symbol request ID
                                            if let Some(base_request_id) =
                                                request_id.strip_prefix("document_symbol_for_")
                                            {
                                                Self::handle_document_symbols_for_enhanced_references(
                                                    base_request_id,
                                                    &document_symbols,
                                                    state,
                                                    response_tx,
                                                ).await;
                                            }
                                        }
                                        Err(e) => {
                                            log::error!("Error parsing document symbols or symbol information for enhanced references {}: {:?}", request_id, e);
                                            log::debug!(
                                                "Raw response: {}",
                                                serde_json::to_string_pretty(result)
                                                    .unwrap_or_else(
                                                        |_| "failed to serialize".to_string()
                                                    )
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            log::warn!("Document symbols response was null for enhanced references request: {}", request_id);
                        }
                    }

                    // Remove from enhanced tracking after processing document symbols
                    state.enhanced_lsp_requests.remove(&id);
                } else if let Some(result) = message.get("result") {
                    // Parse workspace symbols response directly
                    if !result.is_null() {
                        if let Ok(workspace_symbols) = serde_json::from_value::<
                            Vec<lsp_types::WorkspaceSymbol>,
                        >(result.clone())
                        {
                            log::info!(
                                "LSP Workspace Symbols Response: found {} symbols for request {}",
                                workspace_symbols.len(),
                                request_id
                            );

                            // Convert to our WorkspaceSymbolInfo format
                            let symbols: Vec<crate::WorkspaceSymbolInfo> = workspace_symbols
                                .iter()
                                .map(|symbol| {
                                    let location = match &symbol.location {
                                        lsp_types::OneOf::Left(location) => {
                                            crate::convert_lsp_location(location)
                                        }
                                        lsp_types::OneOf::Right(workspace_location) => {
                                            core_data::Location {
                                                file_path: workspace_location
                                                    .uri
                                                    .to_file_path()
                                                    .map(|p| p.to_string_lossy().to_string())
                                                    .unwrap_or_else(|_| {
                                                        workspace_location.uri.to_string()
                                                    }),
                                                line: 0, // WorkspaceLocation doesn't have range info
                                                column: 0,
                                                length: None,
                                            }
                                        }
                                    };
                                    crate::WorkspaceSymbolInfo {
                                        name: symbol.name.clone(),
                                        kind: symbol.kind,
                                        location,
                                        container_name: symbol.container_name.clone(),
                                    }
                                })
                                .collect();

                            // Log each workspace symbol
                            for (i, sym) in symbols.iter().enumerate() {
                                log::info!("  Workspace symbol {}: name='{}', kind={:?}, container={:?}, location={}:{}:{}", 
                                    i, sym.name, sym.kind,
                                    sym.container_name.as_deref().unwrap_or("None"),
                                    sym.location.file_path, sym.location.line, sym.location.column);
                            }

                            // Filter to only function symbols from project files and convert to FunctionNode
                            let function_symbols: Vec<core_data::FunctionNode> = symbols
                                .into_iter()
                                .filter(|sym| {
                                    // Check if it's a function/method type
                                    let is_function = matches!(
                                        sym.kind,
                                        lsp_types::SymbolKind::FUNCTION
                                            | lsp_types::SymbolKind::METHOD
                                            | lsp_types::SymbolKind::CONSTRUCTOR
                                    );

                                    // Check if it's from a project file
                                    let is_project_file = if state.project_files.is_empty() {
                                        // If no project files set, include all symbols (fallback behavior)
                                        true
                                    } else {
                                        // Check if the symbol's file path matches any project file
                                        state.project_files.iter().any(|project_file| {
                                            sym.location.file_path.contains(project_file)
                                                || project_file.contains(&sym.location.file_path)
                                        })
                                    };

                                    is_function && is_project_file
                                })
                                .map(|sym| {
                                    let qualified_name =
                                        if let Some(container) = &sym.container_name {
                                            format!("{}::{}", container, sym.name)
                                        } else {
                                            sym.name.clone()
                                        };
                                    core_data::FunctionNode::new(
                                        sym.name,
                                        qualified_name,
                                        sym.location,
                                    )
                                })
                                .collect();

                            log::info!("Filtered to {} function symbols", function_symbols.len());

                            let _ = response_tx
                                .send(LspResponse::WorkspaceSymbols {
                                    request_id,
                                    symbols: function_symbols,
                                })
                                .await;
                        } else {
                            log::warn!(
                                "Failed to parse workspace symbols response for request {}",
                                request_id
                            );
                        }
                    } else {
                        log::info!(
                            "Empty workspace symbols response for request {}",
                            request_id
                        );
                        let _ = response_tx
                            .send(LspResponse::WorkspaceSymbols {
                                request_id,
                                symbols: Vec::new(),
                            })
                            .await;
                    }

                    // Remove from enhanced tracking after processing workspace symbols
                    state.enhanced_lsp_requests.remove(&id);
                } else if message.get("error").is_some() {
                    let error_msg = message
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown LSP error");

                    log::error!(
                        "LSP Error Response for request {}: {}",
                        request_id,
                        error_msg
                    );
                    log::debug!("Full LSP error message: {}", message);

                    let _ = response_tx
                        .send(LspResponse::Error {
                            request_id,
                            error: error_msg.to_string(),
                        })
                        .await;

                    // Remove from enhanced tracking after error response
                    state.enhanced_lsp_requests.remove(&id);
                } else {
                    log::warn!(
                        "Unrecognized LSP response for request {}: {}",
                        request_id,
                        message
                    );

                    // Remove from enhanced tracking for unrecognized responses
                    state.enhanced_lsp_requests.remove(&id);
                }
            }
        }
    }

    /// Request call hierarchy for a symbol
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

    /// Try to receive a response without blocking
    pub fn try_recv_response(&mut self) -> Option<LspResponse> {
        self.response_rx.try_recv().ok()
    }

    /// Shutdown the service
    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.request_tx.send(LspRequest::Shutdown).await;

        if let Some(handle) = self.worker_handle.take() {
            handle.await?;
        }

        Ok(())
    }

    /// Check if a response is a references response by looking at the request method in service_requests
    fn is_references_response(response: &Value) -> bool {
        if let Some(result) = response.get("result") {
            // Check if result is an array of locations (textDocument/references response format)
            if let Ok(_locations) =
                serde_json::from_value::<Vec<lsp_types::Location>>(result.clone())
            {
                // Even empty arrays are valid references responses
                return true;
            }
        }
        // Also handle null results (valid but empty references response)
        response.get("result").map_or(false, |r| r.is_null())
    }

    /// Parse the content of a references response
    fn parse_references_response_content(response: &Value) -> Vec<core_data::Location> {
        if let Some(result) = response.get("result") {
            if result.is_null() {
                log::debug!("References response has null result, returning empty vec");
                return Vec::new();
            }

            match serde_json::from_value::<Vec<lsp_types::Location>>(result.clone()) {
                Ok(lsp_locations) => {
                    log::debug!("Successfully parsed {} LSP locations", lsp_locations.len());
                    lsp_locations
                        .iter()
                        .map(crate::convert_lsp_location)
                        .collect()
                }
                Err(e) => {
                    log::error!("Failed to parse references response: {}", e);
                    Vec::new()
                }
            }
        } else {
            log::debug!("References response has no result field");
            Vec::new()
        }
    }

    /// Check if this was an enhanced references request
    fn was_enhanced_references_request(message: &Value, state: &LspWorkerState) -> bool {
        // Check if this request ID was marked as enhanced
        if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
            let is_enhanced = state.enhanced_lsp_requests.contains(&id);
            log::debug!(
                "Checking if request {} is enhanced: {} (tracked enhanced requests: {:?})",
                id,
                is_enhanced,
                state.enhanced_lsp_requests
            );
            return is_enhanced;
        }
        log::debug!("No request ID found in message for enhanced check");
        false
    }

    /// Parse enhanced references response with symbol information
    async fn parse_enhanced_references_response(
        message: &Value,
        service_request_id: &str,
        state: &mut LspWorkerState,
        _response_tx: &mpsc::Sender<LspResponse>,
    ) -> Vec<core_data::Reference> {
        // First get the basic locations
        let locations = Self::parse_references_response_content(message);
        let _lsp_request_id = message.get("id").and_then(|i| i.as_i64());

        log::info!(
            "Enhancing {} reference locations with symbol information using hover requests",
            locations.len()
        );

        if locations.is_empty() {
            return Vec::new();
        }

        log::debug!("Using service request ID: {}", service_request_id);
        log::debug!("Using service request ID: {}", service_request_id);

        // Store the pending request info
        state.pending_enhanced_requests.insert(
            service_request_id.to_string(),
            EnhancedRequestInfo {
                request_id: service_request_id.to_string(),
                locations: locations.clone(),
                pending_symbol_requests: locations
                    .iter()
                    .enumerate()
                    .map(|(i, _)| i.to_string())
                    .collect(),
            },
        );

        // Send document symbol requests for each unique file containing references
        let mut files_to_analyze: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for location in locations.iter() {
            files_to_analyze.insert(location.file_path.clone());
        }

        log::info!(
            "Need to analyze {} unique files for symbol information",
            files_to_analyze.len()
        );

        for file_path in files_to_analyze {
            if let Ok(document_uri) = lsp_types::Url::from_file_path(&file_path) {
                let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };

                match state.client.document_symbol(text_document).await {
                    Ok(lsp_request_id) => {
                        // Track this as a document symbol request for the enhanced references
                        state.service_requests.insert(
                            lsp_request_id,
                            format!("document_symbol_for_{}", service_request_id),
                        );
                        state.enhanced_lsp_requests.insert(lsp_request_id);

                        log::debug!(
                            "Sent document symbol request for {} (lsp_request_id: {})",
                            file_path,
                            lsp_request_id
                        );
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to send document symbol request for {}: {:?}",
                            file_path,
                            e
                        );
                    }
                }
            } else {
                log::error!("Failed to convert file path to URI: {}", file_path);
            }
        }

        // Return empty for now, the real response will be sent when document symbol responses arrive
        Vec::new()
    }

    /// Enhance references using cached document symbols
    fn enhance_references_with_cached_symbols(
        _locations: &[core_data::Location],
        locations_by_file: &std::collections::HashMap<String, Vec<core_data::Location>>,
        symbols_cache: &HashMap<String, Vec<lsp_types::DocumentSymbol>>,
    ) -> Vec<core_data::Reference> {
        let mut enhanced_references = Vec::new();

        for (file_path, file_locations) in locations_by_file {
            let file_symbols = symbols_cache.get(file_path);

            for location in file_locations {
                let position = lsp_types::Position {
                    line: location.line.saturating_sub(1), // Convert to 0-indexed
                    character: location.column.saturating_sub(1),
                };

                let referencing_symbol = if let Some(symbols) = file_symbols {
                    crate::utils::find_containing_symbol(symbols, &position)
                } else {
                    None
                };

                enhanced_references.push(core_data::Reference {
                    location: location.clone(),
                    referencing_symbol,
                });
            }
        }

        enhanced_references
    }

    /// Handle hover response for enhanced references
    async fn handle_hover_for_enhanced_references(
        hover_response: crate::HoverResponse,
        request_id: &str,
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
    ) {
        // Parse the request ID format: "hover_for_{service_request_id}_{index}"
        if let Some(remaining) = request_id.strip_prefix("hover_for_") {
            if let Some(last_underscore) = remaining.rfind('_') {
                let service_request_id = &remaining[..last_underscore];
                let index_str = &remaining[last_underscore + 1..];

                if let Ok(index) = index_str.parse::<usize>() {
                    log::debug!(
                        "Processing hover response for service_request_id={}, index={}",
                        service_request_id,
                        index
                    );

                    // Get the pending enhanced request
                    if let Some(pending) =
                        state.pending_enhanced_requests.get_mut(service_request_id)
                    {
                        // Remove this index from pending
                        pending.pending_symbol_requests.remove(index_str);

                        // Extract symbol name from hover info if available
                        let symbol_name = if let Some(hover_info) = &hover_response.hover_info {
                            crate::extract_function_name_from_signature(hover_info)
                        } else {
                            None
                        };

                        log::debug!(
                            "Extracted symbol name for index {}: {:?}",
                            index,
                            symbol_name
                        );

                        // Check if all hover requests are complete
                        if pending.pending_symbol_requests.is_empty() {
                            // Build the enhanced references with symbol information
                            let mut enhanced_refs = Vec::new();

                            for (i, location) in pending.locations.iter().enumerate() {
                                let referencing_symbol = if i == index && symbol_name.is_some() {
                                    Some(core_data::ReferencingSymbol {
                                        name: symbol_name.clone().unwrap(),
                                        qualified_name: symbol_name.clone().unwrap(), // For now, same as name
                                        kind: core_data::ReferenceSymbolKind::Function, // Default to function
                                    })
                                } else {
                                    None
                                };

                                enhanced_refs.push(core_data::Reference {
                                    location: location.clone(),
                                    referencing_symbol,
                                });
                            }

                            log::info!(
                                "Completed enhanced references for {}: {} references with symbol info",
                                service_request_id,
                                enhanced_refs.len()
                            );

                            // Send the completed response
                            let _ = response_tx
                                .send(LspResponse::ReferencesWithSymbols {
                                    request_id: service_request_id.to_string(),
                                    references: enhanced_refs,
                                })
                                .await;

                            // Clean up
                            state.pending_enhanced_requests.remove(service_request_id);
                        } else {
                            log::debug!(
                                "Still waiting for {} more hover responses for {}",
                                pending.pending_symbol_requests.len(),
                                service_request_id
                            );
                        }
                    } else {
                        log::warn!(
                            "No pending enhanced request found for service_request_id: {}",
                            service_request_id
                        );
                    }
                } else {
                    log::warn!(
                        "Failed to parse index from hover request ID: {}",
                        request_id
                    );
                }
            } else {
                log::warn!("Invalid hover request ID format: {}", request_id);
            }
        } else {
            log::warn!(
                "Hover request ID does not start with 'hover_for_': {}",
                request_id
            );
        }
    }

    /// Handle document symbols for enhanced references
    async fn handle_document_symbols_for_enhanced_references(
        base_request_id: &str,
        document_symbols: &[lsp_types::DocumentSymbol],
        state: &mut LspWorkerState,
        response_tx: &mpsc::Sender<LspResponse>,
    ) {
        log::debug!(
            "Processing document symbols for enhanced references request: {}",
            base_request_id
        );

        // Get the pending request info
        if let Some(pending_request) = state.pending_enhanced_requests.get(base_request_id) {
            let mut enhanced_references = Vec::new();

            for location in &pending_request.locations {
                // Convert to LSP position
                let position = lsp_types::Position {
                    line: location.line.saturating_sub(1), // Convert to 0-indexed
                    character: location.column.saturating_sub(1),
                };

                // Find the containing symbol for this reference location
                if let Some(containing_symbol) =
                    Self::find_containing_symbol(document_symbols, &position)
                {
                    let reference = core_data::Reference {
                        location: location.clone(),
                        referencing_symbol: Some(core_data::ReferencingSymbol {
                            name: containing_symbol.name.clone(),
                            qualified_name: Self::get_qualified_symbol_name(containing_symbol),
                            kind: Self::convert_lsp_symbol_kind(containing_symbol.kind),
                        }),
                    };

                    log::debug!(
                        "Found containing symbol '{}' for reference at {}:{}:{}",
                        containing_symbol.name,
                        location.file_path,
                        location.line,
                        location.column
                    );

                    enhanced_references.push(reference);
                } else {
                    // No containing symbol found, create reference without symbol info
                    let reference = core_data::Reference {
                        location: location.clone(),
                        referencing_symbol: None,
                    };

                    log::debug!(
                        "No containing symbol found for reference at {}:{}:{}",
                        location.file_path,
                        location.line,
                        location.column
                    );

                    enhanced_references.push(reference);
                }
            }

            // Send the enhanced references response
            let response = LspResponse::ReferencesWithSymbols {
                request_id: base_request_id.to_string(),
                references: enhanced_references,
            };

            if let Err(e) = response_tx.send(response).await {
                log::error!("Failed to send enhanced references response: {:?}", e);
            } else {
                log::info!(
                    "Sent enhanced references response for request {}",
                    base_request_id
                );
            }

            // Clean up
            state.pending_enhanced_requests.remove(base_request_id);
        } else {
            log::warn!(
                "No pending request found for enhanced references: {}",
                base_request_id
            );
        }
    }

    /// Find the innermost symbol containing the given position
    fn find_containing_symbol<'a>(
        symbols: &'a [lsp_types::DocumentSymbol],
        position: &lsp_types::Position,
    ) -> Option<&'a lsp_types::DocumentSymbol> {
        for symbol in symbols {
            if Self::position_in_range(position, &symbol.range) {
                // Check children first (innermost)
                if let Some(children) = &symbol.children {
                    if let Some(child_symbol) = Self::find_containing_symbol(children, position) {
                        return Some(child_symbol);
                    }
                }
                // If no child contains it, this symbol does
                return Some(symbol);
            }
        }
        None
    }

    /// Check if a position is within a range
    fn position_in_range(position: &lsp_types::Position, range: &lsp_types::Range) -> bool {
        (position.line > range.start.line
            || (position.line == range.start.line && position.character >= range.start.character))
            && (position.line < range.end.line
                || (position.line == range.end.line && position.character <= range.end.character))
    }

    /// Get qualified name for a symbol
    fn get_qualified_symbol_name(symbol: &lsp_types::DocumentSymbol) -> String {
        if let Some(detail) = &symbol.detail {
            if !detail.is_empty() {
                format!("{}::{}", detail, symbol.name)
            } else {
                format!("::{}", symbol.name)
            }
        } else {
            format!("::{}", symbol.name)
        }
    }

    /// Convert LSP symbol kind to our enum
    fn convert_lsp_symbol_kind(kind: lsp_types::SymbolKind) -> core_data::ReferenceSymbolKind {
        match kind {
            lsp_types::SymbolKind::FUNCTION => core_data::ReferenceSymbolKind::Function,
            lsp_types::SymbolKind::METHOD => core_data::ReferenceSymbolKind::Method,
            lsp_types::SymbolKind::CONSTRUCTOR => core_data::ReferenceSymbolKind::Constructor,
            lsp_types::SymbolKind::VARIABLE => core_data::ReferenceSymbolKind::Variable,
            _ => core_data::ReferenceSymbolKind::Function, // Default fallback
        }
    }

    /// Convert SymbolInformation to DocumentSymbol format for consistency
    fn convert_symbol_info_to_document_symbols(
        symbol_infos: &[lsp_types::SymbolInformation],
    ) -> Vec<lsp_types::DocumentSymbol> {
        symbol_infos
            .iter()
            .map(|info| lsp_types::DocumentSymbol {
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
}

impl Drop for LspService {
    fn drop(&mut self) {
        if let Some(handle) = self.worker_handle.take() {
            handle.abort();
        }
    }
}
