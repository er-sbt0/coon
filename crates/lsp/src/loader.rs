//! Background LSP loader task that initializes clangd, discovers files,
//! and provides a TUI ↔ LSP bridge for lazy call-hierarchy resolution.

use log::{debug, error, info};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::compile_commands;
use crate::{IndexProgress, LspClient, LspRequest, LspResponse, LspService};
use model::lsp_status::{LspLoadPhase, LspUiMessage};
use std::path::{Path, PathBuf};

/// How long clangd has to answer `initialize`.
///
/// Nothing else can start until it does — the compile database is not read,
/// no document is opened, and the bridge is not wired — so an unanswered
/// `initialize` is a dead server rather than a slow one, and the budget only
/// has to be long enough to cover a cold process start.
const INIT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(10);

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
/// clangd reports `end` whenever its indexing queue drains, and it works
/// through the compilation database in batches — so an `end` can arrive
/// between two of them with most of the project still to go. Waiting out a
/// quiet window turns "the queue is empty right now" into "there is no more
/// work", and costs a second only on the path that was about to declare
/// victory anyway.
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

/// How long to wait for the preloaded file's `documentSymbol` reply before
/// querying anyway.
///
/// That reply is a synchronisation point, not a result anyone reads — see the
/// call site — so losing it costs determinism rather than correctness, and the
/// wait is bounded like every other one here.
const PRELOAD_SYNC_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

/// What a frontend gets to choose about a load.
///
/// One value rather than a parameter per setting, because a frontend that
/// grows a second launch flag should not make every caller of
/// [`lsp_loader_task`] — including the two deprecated ones — change shape
/// again.
#[derive(Debug, Clone, Default)]
pub struct LoaderOptions {
    /// A source file naming the build to analyse: the project becomes every
    /// entry of the compilation database compiled with the same defines.
    ///
    /// `None` hands clangd the database as it stands, which is every build at
    /// once. See [`compile_commands::Flavour`] for why that is a choice and
    /// not a default.
    pub flavour: Option<PathBuf>,
}

/// What [`choose_build`] settled on: the project's files, and what clangd has
/// to be told to see the same project.
struct Build {
    source_files: Vec<PathBuf>,
    clangd_args: Vec<String>,
}

/// Decide which build of the project this load is of.
///
/// A call graph is of one build. The same header compiled under two define
/// sets declares different functions — jemalloc's tests see `jet_*` where its
/// library sees `je_*` — and clangd answers `outgoingCalls` out of whichever
/// view of a header was published last, so with every build in one database a
/// call into the library appears or vanishes depending on which translation
/// unit happened to be opened most recently. Handing clangd one flavour's
/// database leaves exactly one view per header, and the answers stop moving.
///
/// Without a flavour nothing changes from what this always did — the whole
/// database, whatever is in it — but the choices are logged, because the flag
/// is useless to a user who cannot see what there is to pass it. Nothing is
/// guessed: on jemalloc the tests outnumber the library two to one, so
/// "the biggest" would pick the build almost nobody means.
async fn choose_build(root_path: &Path, options: &LoaderOptions) -> Result<Build, String> {
    let entries = compile_commands::read_entries(root_path)
        .await
        .map_err(|e| e.to_string())?;

    let Some(entries) = entries else {
        if let Some(wanted) = &options.flavour {
            return Err(format!(
                "--flavour {} was given, but {} has no compile_commands.json, \
                 so there are no builds to choose between",
                wanted.display(),
                root_path.display()
            ));
        }
        let mut source_files = compile_commands::parse_compile_commands(root_path)
            .await
            .map_err(|e| e.to_string())?;
        source_files.sort();
        return Ok(Build {
            source_files,
            clangd_args: Vec::new(),
        });
    };

    let total = entries.len();
    let flavours = compile_commands::group_by_flavour(entries);

    let Some(wanted) = &options.flavour else {
        if flavours.len() > 1 {
            info!(
                "This project is {} builds in one compilation database; analysing all of them. \
                 Pass --flavour <file> to analyse one:\n  {}",
                flavours.len(),
                compile_commands::describe_flavours(&flavours, root_path).join("\n  ")
            );
        }
        let entries: Vec<compile_commands::CompileCommand> =
            flavours.into_iter().flat_map(|f| f.entries).collect();
        return Ok(Build {
            source_files: compile_commands::canonical_files(&entries).await,
            clangd_args: Vec::new(),
        });
    };

    let flavour = compile_commands::select_flavour(&flavours, root_path, wanted).await?;
    // The database this flavour gets is written inside the project, so clangd
    // keeps a set of index shards per flavour and finds them again next time.
    // The first run of a flavour therefore indexes from cold, which is what
    // `INDEX_WAIT_TIMEOUT` and the re-query on a later `end` are there for.
    let dir = compile_commands::write_flavour_database(root_path, flavour)
        .await
        .map_err(|e| format!("could not write this flavour's compilation database: {e}"))?;
    info!(
        "Analysing the build of {}: {} of the database's {total} entries, defined by {}",
        wanted.display(),
        flavour.entries.len(),
        flavour.key.defines().join(" ")
    );

    Ok(Build {
        source_files: compile_commands::canonical_files(&flavour.entries).await,
        clangd_args: vec![format!("--compile-commands-dir={}", dir.display())],
    })
}

/// Runs the full LSP initialization sequence in the background:
///
/// 1. Spawns and initializes clangd
/// 2. Picks the build to analyse and discovers its files via
///    compile_commands.json
/// 3. Opens exactly one of them, which is what makes clangd read the
///    compilation database and start indexing
/// 4. Fetches initial workspace symbols
/// 5. Spins up a forwarder loop that bridges TUI requests to `LspService`
///
/// All progress is reported through `ui_tx`. Once the initial load is done,
/// the TUI can send `LspRequest`s through the channel pair sent via
/// `lsp_channels_tx`.
pub async fn lsp_loader_task(
    project_path: &str,
    options: LoaderOptions,
    ui_tx: mpsc::UnboundedSender<LspUiMessage>,
    lsp_channels_tx: oneshot::Sender<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("LSP loader: starting background initialization");

    let root_path = std::path::Path::new(project_path).canonicalize()?;
    let root_uri = lsp_types::Url::from_file_path(&root_path)
        .map_err(|_| "Failed to create URI from project path")?;

    // Before the server, not after it: which build this is decides what clangd
    // is spawned with, and a `--compile-commands-dir` cannot be handed to a
    // process that is already running.
    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::DiscoveringFiles))?;
    let Build {
        source_files,
        clangd_args,
    } = match choose_build(&root_path, &options).await {
        Ok(build) => build,
        Err(msg) => {
            ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
            return Err(msg.into());
        }
    };
    info!("Found {} source files", source_files.len());

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::SpawningServer))?;

    let (tx, rx) = mpsc::channel::<Value>(100);
    let mut lsp_client = LspClient::new_with_args(tx, &clangd_args).await?;

    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Initializing))?;

    let init_id = lsp_client.initialize(root_uri.clone()).await?;

    // Wait for the reply to `initialize`, and only for that. clangd starts
    // talking before it answers — `window/workDoneProgress/create`, the first
    // `$/progress` — so this has to keep reading and discarding until the id
    // matches, which is why it is a loop and not a single `timeout`. One
    // deadline covers the whole wait, as it does for every other wait in this
    // file: polling in 100ms slices re-armed a timer per slice and spun for
    // the full budget on a server that had simply gone quiet, and the budget
    // it enforced was the loop's own `elapsed()` check rather than the timer.
    let init_deadline = tokio::time::Instant::now() + INIT_TIMEOUT;
    let mut rx_for_init = rx;

    loop {
        match tokio::time::timeout_at(init_deadline, rx_for_init.recv()).await {
            Ok(Some(message)) => {
                if log::log_enabled!(log::Level::Debug) {
                    debug!(
                        "Init phase - received LSP message: {}",
                        serde_json::to_string_pretty(&message).unwrap_or_default()
                    );
                }
                // Anything that is not the answer to `initialize` is startup
                // chatter: drop it and keep waiting. The `method` check is not
                // redundant with the id — `window/workDoneProgress/create` is
                // a *request from the server*, so it carries an id of
                // clangd's own choosing out of a counter that starts where
                // ours does, and a numeric collision would otherwise read as
                // a successful initialize. A response never names a method.
                if message.get("method").is_some()
                    || message.get("id").and_then(|i| i.as_i64()) != Some(init_id)
                {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let msg = format!("LSP initialization failed: {:?}", error);
                    ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
                    return Err(msg.into());
                }
                info!("LSP initialization complete");
                ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Initialized))?;
                break;
            }
            // The channel closed, so clangd died before it answered. Waiting
            // out the rest of the deadline would only delay saying so.
            Ok(None) => {
                let msg = "LSP server closed the connection during initialization".to_string();
                ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
                return Err(msg.into());
            }
            Err(_) => {
                let msg = "LSP initialization timed out".to_string();
                ui_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(msg.clone())))?;
                return Err(msg.into());
            }
        }
    }

    info!("Sending initialized notification...");
    lsp_client.send_initialized().await?;
    // clangd loads compile_commands.json in reaction to `initialized`, and a
    // `didOpen` that overtakes it is compiled with fallback flags instead of
    // the file's real ones. This pause is only about that ordering — what the
    // *symbol query* waits for is the index, below.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let mut lsp_service = LspService::new(lsp_client, rx_for_init).await?;

    // The root, not the list of files just discovered: a compilation database
    // holds translation units, and the functions a C project defines in its
    // headers belong to it just as much — jemalloc has ~736 of them. What the
    // filter is for is the other direction, libc and the system headers, and
    // those are under nobody's root. See `service::project`.
    lsp_service
        .send(LspRequest::SetProjectRoot {
            root: root_path.clone(),
        })
        .await?;

    // Exactly one document is opened, and it is the same one every run.
    //
    // clangd answers `workspace/symbol` from its static index *merged with a
    // dynamic index of whatever has been open*: building a document's preamble
    // publishes that document's view of every header it includes, last writer
    // per header wins, and `didClose` does not retract it — a view measured
    // still standing eight seconds after the close. So the answer is decided by
    // which files this process has ever had open, and in what order their
    // preambles landed.
    //
    // Opening the whole compilation database made that a lottery: 217
    // `didOpen`s racing the LRU in `service::document`, which closed most of
    // them again before their preambles had published, so a different subset
    // won the headers each run. On jemalloc the headers have two views — its
    // 149 test TUs compile with the `jet_*` defines, its 68 library TUs with
    // `je_*` — and one clangd process answered 4,041 project functions with a
    // library file open against 4,594 with a test file open. Two processes
    // given the same open history answered identically, symbol for symbol.
    //
    // One `didOpen` is all the compilation database needs: clangd reads
    // `compile_commands.json` to compile that file and hands every entry in it
    // to the background indexer, which is the only job the preload ever had.
    let preload_uri = source_files
        .iter()
        .find_map(|path| lsp_types::Url::from_file_path(path).ok());

    // Whether the last thing clangd said about its indexer was that it had gone
    // idle. Tracked from here rather than only inside `await_background_index`,
    // because every wait below drains the same channel: a `$/progress` that
    // lands while one of the others is reading is gone, and waiting afterwards
    // for an announcement that has already been made waits forever.
    let mut index_idle = false;

    if let Some(preload_uri) = &preload_uri {
        ui_tx.send(LspUiMessage::Progress(LspLoadPhase::PreloadingDocuments {
            done: 0,
            total: 1,
        }))?;
        info!("Opening {preload_uri} to make clangd load the compilation database");

        // The id is minted here, not by the service, because this is the end
        // that matches it against the `PreloadComplete` below.
        let preload_request_id = Uuid::new_v4().to_string();
        lsp_service
            .send(LspRequest::PreloadDocuments {
                request_id: preload_request_id.clone(),
                document_uris: vec![preload_uri.clone()],
            })
            .await?;

        // Wait for the service worker to confirm the didOpen notification was sent.
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
            done: 1,
            total: 1,
        }))?;
    } else {
        log::warn!("No source file to open: clangd will index nothing and the query will be empty");
    }

    // The point of everything above was to get clangd indexing. Wait for it to
    // finish before asking it anything, so the answer describes the project.
    //
    // Whether the query below is the one that gets a complete answer: read here
    // rather than after the wait for symbols, because that wait can itself see
    // the index finish — too late for this query, which has already gone out.
    let queried_a_finished_index =
        await_background_index(&mut lsp_service, &ui_tx, index_idle).await;

    // Sync on the preloaded file before asking anything about the project.
    //
    // `PreloadComplete` above says the `didOpen` was *sent*; it says nothing
    // about when clangd gets round to building that file's preamble and
    // publishing its view of the headers into the dynamic index the query below
    // is answered from. A `textDocument/documentSymbol` is answered from the
    // file's AST, which is built on top of that preamble, so its reply is the
    // first moment the open history is settled. Nothing reads the symbols it
    // returns: it is a barrier, not a result.
    //
    // Nothing is closed first, and closing would not help if it were: a
    // `didClose` does not retract what the preamble published, so the view is
    // in the merged index either way. One open document is one AST worker and
    // one resident preamble, far inside `OPEN_DOCUMENT_LIMIT`, so it stays
    // open and the graph is seeded from a clangd whose dynamic index holds
    // exactly one file's view.
    if let Some(preload_uri) = &preload_uri {
        await_document_sync(&mut lsp_service, preload_uri, &mut index_idle).await;
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

/// Wait for `uri`'s `documentSymbol` reply, which lands once clangd has built
/// that file's AST — and therefore once its preamble has published its view of
/// every header it includes into the merged index the discovery query reads.
///
/// Keeps `index_idle` current for the reason every wait in this file does: they
/// all drain the one response channel, so a `$/progress` read by this one is
/// gone from the next.
///
/// Every way of not getting the reply is a warning and a shrug. The query still
/// goes out; what is lost is the guarantee that two runs ask the same clangd.
async fn await_document_sync(
    lsp_service: &mut LspService,
    uri: &lsp_types::Url,
    index_idle: &mut bool,
) {
    let request_id = Uuid::new_v4().to_string();
    if let Err(e) = lsp_service
        .send(LspRequest::GetDocumentSymbols {
            request_id: request_id.clone(),
            document_uri: uri.clone(),
        })
        .await
    {
        log::warn!("Failed to ask {uri} for its document symbols: {e}");
        return;
    }

    let deadline = tokio::time::Instant::now() + PRELOAD_SYNC_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, lsp_service.recv_response()).await {
            Ok(Some(LspResponse::DocumentSymbols {
                request_id: id,
                symbols,
            })) if id == request_id => {
                info!(
                    "Preloaded document parsed ({} top-level symbol(s)); its view of the headers \
                     is in the index",
                    symbols.len()
                );
                return;
            }
            Ok(Some(LspResponse::Error {
                request_id: id,
                error,
            })) if id == request_id => {
                log::warn!("Could not parse the preloaded document: {error}");
                return;
            }
            Ok(Some(LspResponse::IndexProgress(progress))) => {
                *index_idle = progress == IndexProgress::Finished;
            }
            Ok(Some(_)) => continue, // a reply to something else; keep waiting
            Ok(None) => {
                log::warn!("LSP service channel closed while parsing the preloaded document");
                return;
            }
            Err(_) => {
                log::warn!(
                    "The preloaded document was not parsed within {:?}; querying anyway",
                    PRELOAD_SYNC_TIMEOUT
                );
                return;
            }
        }
    }
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
