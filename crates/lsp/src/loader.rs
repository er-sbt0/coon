//! Background LSP loader task that initializes clangd, discovers files,
//! and provides a TUI ↔ LSP bridge for lazy call-hierarchy resolution.

use log::{debug, error, info};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::compile_commands;
use crate::{IndexProgress, LspClient, LspRequest, LspResponse, LspService};
use model::lsp_status::{LspLoadPhase, LspUiMessage};

/// How long to wait for the initial `workspace/symbol` reply before giving up
/// and wiring the bridge anyway.
///
/// Blocking here costs nothing the user can feel — there are no symbols to click
/// until this lands. If the deadline does expire, the reply is not lost: it is
/// forwarded to the frontend through the bridge below, which ingests workspace
/// symbols whether or not it issued the request.
const INITIAL_SYMBOL_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// How long to wait for clangd's background indexer to *announce itself* before
/// concluding that it is not going to.
///
/// A warm `.cache/clangd/index` can leave nothing to do, and a clangd built
/// without background indexing never reports at all; neither is an error, and
/// neither should cost the full [`INDEX_WAIT_TIMEOUT`] before the first query
/// goes out.
const INDEX_START_GRACE: tokio::time::Duration = tokio::time::Duration::from_secs(2);

/// How long the indexer has to stay quiet after reporting itself done before
/// that is believed.
///
/// clangd reports `end` whenever its queue drains, and the preload feeds that
/// queue 217 `didOpen`s in a row — so an `end` can arrive between batches with
/// most of the project still to go. Waiting out a quiet window turns "the queue
/// is empty right now" into "there is no more work", and costs a second only on
/// the path that was about to declare victory anyway.
const INDEX_SETTLE: tokio::time::Duration = tokio::time::Duration::from_secs(1);

/// The ceiling on waiting for a running index to finish.
///
/// `workspace/symbol` is answered from the index as it stands the instant the
/// request arrives — clangd never blocks on the indexer — so querying early
/// returns whatever fraction of the project has been indexed so far. That is
/// where the graph's run-to-run size variance came from.
///
/// The wait is capped rather than unbounded because nothing is interactive
/// until it ends: the bridge is wired below it. When the cap expires the query
/// goes out against a partial index anyway, for a fast first paint, and
/// `run_tui_lsp_bridge` repeats it the moment indexing does finish.
const INDEX_WAIT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

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
    lsp_channels_tx: oneshot::Sender<(
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
                if log::log_enabled!(log::Level::Debug) {
                    debug!(
                        "Init phase - received LSP message: {}",
                        serde_json::to_string_pretty(&message).unwrap_or_default()
                    );
                }
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
    // clangd loads compile_commands.json in reaction to `initialized`, and a
    // `didOpen` that overtakes it is compiled with fallback flags instead of
    // the file's real ones. This pause is only about that ordering — what the
    // *symbol query* waits for is the index, below.
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
        .send(LspRequest::SetProjectFiles {
            project_files: project_file_paths.clone(),
        })
        .await?;
    info!(
        "Set {} project files for symbol filtering",
        project_file_paths.len()
    );

    let document_uris: Vec<lsp_types::Url> = source_files
        .iter()
        .filter_map(|path| lsp_types::Url::from_file_path(path).ok())
        .collect();

    // Whether the last thing clangd said about its indexer was that it had gone
    // idle. Tracked from here rather than only inside `await_background_index`,
    // because every wait below drains the same channel: a `$/progress` that
    // lands while one of the others is reading is gone, and waiting afterwards
    // for an announcement that has already been made waits forever.
    let mut index_idle = false;

    if !document_uris.is_empty() {
        let total = document_uris.len();
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::PreloadingDocuments {
            done: 0,
            total,
        }))?;

        // The id is minted here, not by the service, because this is the end
        // that matches it against the `PreloadComplete` below.
        let preload_request_id = Uuid::new_v4().to_string();
        lsp_service
            .send(LspRequest::PreloadDocuments {
                request_id: preload_request_id.clone(),
                document_uris: document_uris.clone(),
            })
            .await?;

        // Wait for the service worker to confirm all didOpen notifications were sent.
        let preload_timeout = tokio::time::Duration::from_secs(30);
        let preload_deadline = tokio::time::Instant::now() + preload_timeout;
        loop {
            match tokio::time::timeout_at(preload_deadline, lsp_service.recv_response()).await {
                Ok(Some(LspResponse::PreloadComplete {
                    request_id,
                    loaded_count,
                    failed_count,
                })) if request_id == preload_request_id => {
                    info!(
                        "Preload complete: {} loaded, {} failed",
                        loaded_count, failed_count
                    );
                    break;
                }
                Ok(Some(LspResponse::IndexProgress(progress))) => {
                    index_idle = progress == IndexProgress::Finished;
                    continue;
                }
                Ok(Some(_)) => continue, // unrelated response, keep waiting
                Ok(None) => {
                    let msg = "LSP service channel closed during preload".to_string();
                    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
                    return Err(msg.into());
                }
                Err(_) => {
                    log::warn!(
                        "Preload timed out after {:?}; continuing anyway",
                        preload_timeout
                    );
                    break;
                }
            }
        }
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::PreloadingDocuments {
            done: total,
            total,
        }))?;
    }

    // The point of everything above was to get clangd indexing. Wait for it to
    // finish before asking it anything, so the answer describes the project.
    //
    // Whether the query below is the one that gets a complete answer: read here
    // rather than after the wait for symbols, because that wait can itself see
    // the index finish — too late for this query, which has already gone out.
    let queried_a_finished_index =
        await_background_index(&mut lsp_service, &ui_tx, index_idle).await;

    // The preload existed to make clangd load the compilation database and index
    // the project; the index is now built, so it has done its job. Closing here
    // rather than after the query is deliberate on two counts. Holding every
    // document open costs a live AST worker and a resident preamble each — on a
    // 217-file tree the difference between hundreds of megabytes and several
    // gigabytes — and buys nothing, since `workspace/symbol` is answered from
    // the index and every position-based request reopens its own file through
    // `ensure_document_opened`. And clangd answers `workspace/symbol` from the
    // static index *merged with a dynamic index of whatever is open*, whose
    // contents depend on which of the open files have finished building their
    // ASTs — timing that `backgroundIndexProgress` says nothing about. Querying
    // with nothing open is what makes the answer the same twice running.
    if let Err(e) = lsp_service.send(LspRequest::CloseDocuments).await {
        log::warn!("Failed to close documents before the symbol sweep: {}", e);
    }

    let initial_request_id = Uuid::new_v4().to_string();
    lsp_service
        .send(LspRequest::GetWorkspaceSymbols {
            request_id: initial_request_id.clone(),
            query: String::new(),
        })
        .await?;
    info!("Requested initial workspace symbols for overview");
    ui_tx.send(LspUiMessage::Progress(
        LspLoadPhase::LoadingWorkspaceSymbols { loaded: 0 },
    ))?;

    let symbol_deadline = tokio::time::Instant::now() + INITIAL_SYMBOL_TIMEOUT;
    let mut loaded_count: usize = 0;

    'symbol_wait: loop {
        match tokio::time::timeout_at(symbol_deadline, lsp_service.recv_response()).await {
            Ok(Some(LspResponse::WorkspaceSymbols {
                request_id,
                symbols,
            })) if request_id == initial_request_id => {
                info!("Received {} initial workspace symbols", symbols.len());
                for function_node in symbols {
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
            Ok(Some(LspResponse::IndexProgress(progress))) => {
                index_idle = progress == IndexProgress::Finished;
                continue;
            }
            Ok(Some(_)) => continue, // response for a different request, keep waiting
            Ok(None) => break,       // channel closed
            Err(_) => {
                log::warn!(
                    "Initial workspace symbols did not arrive within {:?}; continuing. \
                     A late reply will still reach the frontend via the bridge.",
                    INITIAL_SYMBOL_TIMEOUT
                );
                break;
            }
        }
    }

    // The index went idle while the symbols above were in flight, so that reply
    // describes a project that was still being indexed. Ask again — the answer
    // appends, because `CoreApp` ingests workspace symbols whether or not it
    // issued the request. `requery_on_index` stays set either way: this `end`
    // may have been another lull, and the bridge asks again on the next one.
    let requery_on_index = !queried_a_finished_index;
    if requery_on_index && index_idle {
        let request_id = Uuid::new_v4().to_string();
        info!("Background index finished during the symbol wait; re-querying ({request_id})");
        if let Err(e) = lsp_service
            .send(LspRequest::GetWorkspaceSymbols {
                request_id,
                query: String::new(),
            })
            .await
        {
            log::error!("Failed to re-query workspace symbols: {}", e);
        }
    }

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Completed))?;

    info!("LSP loader: starting lazy-loading TUI bridge");

    let (tui_request_tx, tui_request_rx) = mpsc::unbounded_channel::<LspRequest>();
    let (tui_response_tx, tui_response_rx) = mpsc::unbounded_channel::<LspResponse>();

    if lsp_channels_tx
        .send((tui_response_rx, tui_request_tx))
        .is_err()
    {
        error!("Failed to send LSP channels: receiver was dropped");
    }

    run_tui_lsp_bridge(
        lsp_service,
        tui_request_rx,
        tui_response_tx,
        requery_on_index,
    )
    .await;

    Ok(())
}

/// Block until clangd's background indexer has drained, so the discovery query
/// that follows is answered from a finished index rather than from however much
/// of one exists at that instant.
///
/// `already_idle` carries what the preload wait overheard: it drains the same
/// channel, so an `end` it saw is an `end` this will never see.
///
/// Returns whether the index actually finished. It does not on two paths, and
/// they are not the same: the budget can run out on a big cold tree, and — on a
/// clangd with no background indexer — nothing is ever reported at all. Only
/// the first leaves a partial answer behind, but neither is distinguishable
/// from here, so both are reported as unfinished and the caller arranges to ask
/// again.
async fn await_background_index(
    lsp_service: &mut LspService,
    ui_tx: &mpsc::UnboundedSender<LspUiMessage>,
    already_idle: bool,
) -> bool {
    let started_waiting = tokio::time::Instant::now();
    let ceiling = started_waiting + INDEX_WAIT_TIMEOUT;
    let mut saw_progress = already_idle;
    // Whether the last word was `end`. Not the same as finished: the queue
    // drains between batches, so what makes it final is the quiet that follows.
    let mut idle = already_idle;
    let mut deadline = started_waiting
        + if already_idle {
            INDEX_SETTLE
        } else {
            // Nothing has been heard at all yet, and on a clangd that does not
            // report, nothing ever will be.
            INDEX_START_GRACE
        };

    loop {
        match tokio::time::timeout_at(deadline, lsp_service.recv_response()).await {
            Ok(Some(LspResponse::IndexProgress(progress))) => {
                saw_progress = true;
                match progress {
                    IndexProgress::Finished => {
                        idle = true;
                        deadline = tokio::time::Instant::now() + INDEX_SETTLE;
                    }
                    IndexProgress::Started => {
                        idle = false;
                        deadline = ceiling;
                        let _ = ui_tx.send(LspUiMessage::Progress(LspLoadPhase::IndexingProject {
                            done: 0,
                            total: 0,
                        }));
                    }
                    IndexProgress::Indexing { done, total } => {
                        idle = false;
                        deadline = ceiling;
                        let _ = ui_tx.send(LspUiMessage::Progress(LspLoadPhase::IndexingProject {
                            done,
                            total,
                        }));
                    }
                }
            }
            Ok(Some(_)) => continue, // a straggler from the preload; keep waiting
            Ok(None) => {
                log::warn!("LSP service channel closed while waiting for the background index");
                return idle;
            }
            // Quiet since the last `end` — that one was the real one.
            Err(_) if idle => {
                info!(
                    "Background index finished after {:?}; workspace/symbol now sees the whole project",
                    started_waiting.elapsed()
                );
                return true;
            }
            Err(_) if saw_progress => {
                log::warn!(
                    "Background index still running after {:?}; querying it partially built. \
                     The query will be repeated when indexing finishes.",
                    INDEX_WAIT_TIMEOUT
                );
                return false;
            }
            Err(_) => {
                info!(
                    "clangd reported no indexing within {:?} — nothing to index, or a build \
                     without a background indexer. Continuing.",
                    INDEX_START_GRACE
                );
                return false;
            }
        }
    }
}

/// Bridges TUI ↔ LSP indefinitely: forwards `LspRequest`s to `lsp_service` and pipes
/// responses back through `tui_response_tx`. Owns the service and shuts it down on exit.
///
/// `requery_on_index` says the discovery query above went out against an index
/// that was not finished, so its answer describes part of the project. When
/// clangd reports the index done, the query is repeated; `CoreApp` ingests
/// workspace symbols whether or not it issued the request, so the second reply
/// lands as an append rather than a replacement.
async fn run_tui_lsp_bridge(
    mut lsp_service: LspService,
    mut tui_request_rx: mpsc::UnboundedReceiver<LspRequest>,
    tui_response_tx: mpsc::UnboundedSender<LspResponse>,
    mut requery_on_index: bool,
) {
    loop {
        tokio::select! {
            tui_request = tui_request_rx.recv() => {
                if let Some(request) = tui_request {
                    log::debug!("Forwarding TUI request to LSP service: {:?}", request);
                    match request {
                        // The one request the forwarder acts on rather than
                        // forwards: the service is shut down below, on the way
                        // out of this loop.
                        LspRequest::Shutdown => {
                            log::debug!("Received Shutdown from TUI, stopping forwarder");
                            break;
                        }
                        // Everything else goes through untouched. This used to
                        // be eight arms that destructured a request only to
                        // hand its fields to the `LspService` method that put
                        // the same variant back together, plus two more for the
                        // requests nothing replies to.
                        request => {
                            // Read before the send, which takes the request:
                            // it is how the waiter downstream is told that
                            // their request never went out. `None` means
                            // nobody is waiting on this one.
                            let request_id = request.request_id().map(str::to_owned);
                            if let Err(e) = lsp_service.send(request).await {
                                log::error!("LSP request failed: {}", e);
                                if let Some(request_id) = request_id {
                                    let _ = tui_response_tx.send(LspResponse::Error {
                                        request_id,
                                        error: format!("Request failed: {}", e),
                                    });
                                }
                            }
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
                        if requery_on_index
                            && matches!(r, LspResponse::IndexProgress(IndexProgress::Finished))
                        {
                            requery_on_index = false;
                            let request_id = Uuid::new_v4().to_string();
                            info!(
                                "Background index finished; re-querying workspace symbols ({})",
                                request_id
                            );
                            if let Err(e) = lsp_service
                                .send(LspRequest::GetWorkspaceSymbols {
                                    request_id,
                                    query: String::new(),
                                })
                                .await
                            {
                                log::error!("Failed to re-query workspace symbols: {}", e);
                            }
                        }
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
    log::debug!("LSP-TUI forwarder loop ended, shutting down LSP service");

    if let Err(e) = lsp_service.shutdown().await {
        log::error!("Error during LSP service shutdown: {}", e);
    }
    info!("LSP loader: graceful shutdown complete");
}
