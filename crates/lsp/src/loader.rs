//! Background LSP loader task that initializes clangd, discovers files,
//! and provides a TUI ↔ LSP bridge for lazy call-hierarchy resolution.

use log::{debug, error, info};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::compile_commands;
use crate::{LspClient, LspRequest, LspResponse, LspService};
use model::lsp_status::{LspLoadPhase, LspUiMessage};

/// Runs the full LSP initialization sequence in the background:
///
/// 1. Spawns and initializes clangd
/// 2. Discovers source files via compile_commands.json
/// 3. Pre-loads documents into the language server
/// 4. Fetches initial workspace symbols
/// 5. Spins up a forwarder loop that bridges TUI requests to `LspService`
///
/// All progress is reported through `ui_tx`. Once the initial load is done,
/// the TUI can send `LspRequest`s through the channel pair sent via
/// `lsp_channels_tx`.
pub async fn lsp_loader_task(
    project_path: &str,
    ui_tx: mpsc::UnboundedSender<LspUiMessage>,
    lsp_channels_tx: mpsc::UnboundedSender<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("LSP loader: starting background initialization");
    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::SpawningServer))?;

    let (tx, rx) = mpsc::channel::<Value>(100);
    let mut lsp_client = LspClient::new(tx).await?;

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Initializing))?;

    let root_path = std::path::Path::new(project_path).canonicalize()?;
    let root_uri = lsp_types::Url::from_file_path(&root_path)
        .map_err(|_| "Failed to create URI from project path")?;

    let init_id = lsp_client.initialize(root_uri.clone()).await?;

    let mut init_response = None;
    let timeout = tokio::time::Duration::from_secs(10);
    let start_time = std::time::Instant::now();
    let mut rx_for_init = rx;

    while start_time.elapsed() < timeout && init_response.is_none() {
        match tokio::time::timeout(std::time::Duration::from_millis(100), rx_for_init.recv()).await
        {
            Ok(Some(message)) => {
                debug!(
                    "Init phase - received LSP message: {}",
                    serde_json::to_string_pretty(&message)?
                );
                if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
                    if id == init_id {
                        if let Some(error) = message.get("error") {
                            let msg = format!("LSP initialization failed: {:?}", error);
                            ui_tx
                                .send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
                            return Err(msg.into());
                        }
                        init_response = Some(message);
                        info!("LSP initialization complete");
                        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Initialized))?;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    if init_response.is_none() {
        let msg = "LSP initialization timed out".to_string();
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
        return Err(msg.into());
    }

    info!("Sending initialized notification...");
    lsp_client.send_initialized().await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::DiscoveringFiles))?;
    let source_files = compile_commands::parse_compile_commands(&root_path).await?;
    info!("Found {} source files", source_files.len());

    let mut lsp_service = LspService::new(lsp_client, rx_for_init).await?;

    let project_file_paths: Vec<String> = source_files
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect();
    lsp_service
        .set_project_files(project_file_paths.clone())
        .await?;
    info!(
        "Set {} project files for symbol filtering",
        project_file_paths.len()
    );

    let document_uris: Vec<lsp_types::Url> = source_files
        .iter()
        .filter_map(|path| lsp_types::Url::from_file_path(path).ok())
        .collect();

    if !document_uris.is_empty() {
        let total = document_uris.len();
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::PreloadingDocuments {
            done: 0,
            total,
        }))?;

        let _preload_request_id = lsp_service.preload_documents(document_uris.clone()).await?;

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::PreloadingDocuments {
            done: total,
            total,
        }))?;
    }

    let initial_request_id = Uuid::new_v4().to_string();
    lsp_service
        .request_workspace_symbols(initial_request_id.clone(), "".to_string())
        .await?;
    info!("Requested initial workspace symbols for overview");
    ui_tx.send(LspUiMessage::Progress(
        LspLoadPhase::LoadingWorkspaceSymbols { loaded: 0 },
    ))?;

    let symbol_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_millis(2000);
    let mut loaded_count: usize = 0;

    'symbol_wait: loop {
        match tokio::time::timeout_at(symbol_deadline, lsp_service.recv_response()).await {
            Ok(Some(LspResponse::WorkspaceSymbols { request_id, symbols }))
                if request_id == initial_request_id =>
            {
                info!("Received {} initial workspace symbols", symbols.len());
                for function_node in symbols.into_iter().take(200) {
                    let workspace_symbol = model::WorkspaceSymbolInfo {
                        name: function_node.name.clone(),
                        qualified_name: function_node.qualified_name.clone(),
                        kind: model::lsp_types::SymbolKind::FUNCTION,
                        location: function_node.definition_location.clone(),
                        container_name: None,
                    };
                    loaded_count += 1;
                    let _ = ui_tx.send(LspUiMessage::AddFunction(workspace_symbol));
                }
                ui_tx.send(LspUiMessage::Progress(
                    LspLoadPhase::LoadingWorkspaceSymbols {
                        loaded: loaded_count,
                    },
                ))?;
                break 'symbol_wait;
            }
            Ok(Some(_)) => continue, // response for a different request, keep waiting
            Ok(None) | Err(_) => break, // channel closed or deadline exceeded
        }
    }

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Completed))?;

    info!("LSP loader: starting lazy-loading TUI bridge");

    let (tui_request_tx, mut tui_request_rx) = mpsc::unbounded_channel::<LspRequest>();
    let (tui_response_tx, tui_response_rx) = mpsc::unbounded_channel::<LspResponse>();

    if let Err(e) = lsp_channels_tx.send((tui_response_rx, tui_request_tx)) {
        error!("Failed to send LSP channels: {}", e);
    }

    tokio::spawn(async move {
        let mut lsp_service = lsp_service;
        loop {
            tokio::select! {
                tui_request = tui_request_rx.recv() => {
                    if let Some(request) = tui_request {
                        log::debug!("Forwarding TUI request to LSP service: {:?}", request);
                        match request {
                            LspRequest::GetCallHierarchy { request_id, document_uri, position } => {
                                if let Err(e) = lsp_service.request_call_hierarchy(request_id.clone(), document_uri, position).await {
                                    log::error!("Failed to send call hierarchy request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::GetOutgoingCalls { request_id, call_hierarchy_item } => {
                                if let Err(e) = lsp_service.request_outgoing_calls(request_id.clone(), call_hierarchy_item).await {
                                    log::error!("Failed to send outgoing calls request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::GetIncomingCalls { request_id, call_hierarchy_item } => {
                                if let Err(e) = lsp_service.request_incoming_calls(request_id.clone(), call_hierarchy_item).await {
                                    log::error!("Failed to send incoming calls request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::FindReferences { request_id, document_uri, position } => {
                                if let Err(e) = lsp_service.request_references(request_id.clone(), document_uri, position).await {
                                    log::error!("Failed to send references request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::GetWorkspaceSymbols { request_id, query } => {
                                if let Err(e) = lsp_service.request_workspace_symbols(request_id.clone(), query).await {
                                    log::error!("Failed to send workspace symbols request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::FindReferencesWithSymbols { request_id, document_uri, position } => {
                                if let Err(e) = lsp_service.request_references_with_symbols(request_id.clone(), document_uri, position).await {
                                    log::error!("Failed to send enhanced references request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::PrepareCallHierarchy { request_id, document_uri, position } => {
                                if let Err(e) = lsp_service.request_prepare_call_hierarchy(request_id.clone(), document_uri, position).await {
                                    log::error!("Failed to send prepare call hierarchy request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::GetDocumentSymbols { request_id, document_uri } => {
                                if let Err(e) = lsp_service.request_document_symbols(request_id.clone(), document_uri).await {
                                    log::error!("Failed to send document symbols request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::PreloadDocuments { request_id, document_uris } => {
                                if let Err(e) = lsp_service.request_preload_documents(request_id.clone(), document_uris).await {
                                    log::error!("Failed to send preload documents request: {}", e);
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
                            LspRequest::SetProjectFiles { project_files } => {
                                if let Err(e) = lsp_service.set_project_files(project_files).await {
                                    log::error!("Failed to forward set project files: {}", e);
                                }
                            }
                            LspRequest::Shutdown => {
                                log::debug!("Received Shutdown from TUI, stopping forwarder");
                                break;
                            }
                        }
                    } else {
                        log::debug!("TUI request channel closed, stopping forwarder");
                        break;
                    }
                }

                response = lsp_service.recv_response() => {
                    match response {
                        Some(r) => {
                            log::trace!("Forwarding LSP response to TUI: {:?}", r);
                            if let Err(e) = tui_response_tx.send(r) {
                                log::error!("Failed to forward LSP response to TUI: {}", e);
                                break;
                            }
                        }
                        None => {
                            log::debug!("LSP service response channel closed, stopping forwarder");
                            break;
                        }
                    }
                }
            }
        }
        log::debug!("LSP-TUI forwarder task ended");
    });

    Ok(())
}
