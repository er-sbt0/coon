//! Shared frontend bootstrap: spawn the clangd loader and hand back a bridge.
//!
//! Every frontend needs the same wiring — a background `lsp_loader_task` plus
//! the channels it publishes — so it lives here rather than being duplicated
//! per binary.

use tokio::sync::{mpsc, oneshot};

use lsp::{LoaderOptions, LspRequest, LspResponse};
use model::lsp_status::{LspLoadPhase, LspUiMessage};

use crate::lsp_bridge::LspBridge;

/// Spawn the clangd loader for `project_path` and return a bridge wired to it.
///
/// `options` is what the frontend's launch flags decided — today, which build
/// of the project to analyse. It travels as one value so that a flag added to
/// one frontend costs the others nothing but a `Default`.
///
/// The loader runs detached; the returned `LspBridge` starts out with no
/// channels and picks them up via its one-shot receiver once clangd is ready,
/// so the caller can start rendering immediately.
///
/// The worker's replies do not reach the bridge directly. They pass through
/// a relay task that copies each one onto the channel the bridge reads and
/// then raises [`LspBridge::response_ready`], so a frontend awaiting that
/// signal ticks the moment an answer lands rather than at its next poll. The
/// relay is here rather than in `lsp` because the signal is the bridge's,
/// and `lsp` does not know a bridge exists; it costs one extra channel hop
/// per reply, which is nothing against the poll interval it saves.
pub fn spawn_lsp_bridge(project_path: &str, options: LoaderOptions) -> LspBridge {
    let (ui_msg_tx, ui_msg_rx) = mpsc::unbounded_channel::<LspUiMessage>();
    let (lsp_channels_tx, lsp_channels_rx) = oneshot::channel::<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>();
    let (relayed_tx, relayed_rx) = oneshot::channel();

    let project_path = project_path.to_string();
    tokio::spawn(async move {
        if let Err(e) =
            lsp::loader::lsp_loader_task(&project_path, options, ui_msg_tx, lsp_channels_tx).await
        {
            log::error!("LSP loader task failed: {}", e);
        }
    });

    let bridge = LspBridge::new_with_loader(ui_msg_rx, relayed_rx);
    let response_ready = bridge.response_ready();
    tokio::spawn(async move {
        let Ok((mut worker_rx, request_tx)) = lsp_channels_rx.await else {
            // The loader failed before it wired anything up; it has logged
            // why, and the bridge sees the same dropped sender it always did.
            return;
        };
        let (response_tx, response_rx) = mpsc::unbounded_channel::<LspResponse>();
        if relayed_tx.send((response_rx, request_tx)).is_err() {
            return;
        }
        while let Some(response) = worker_rx.recv().await {
            if response_tx.send(response).is_err() {
                break;
            }
            response_ready.notify_one();
        }
    });

    bridge
}

/// Which analysis backend builds the call graph of a project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// clangd over LSP: symbols first, edges fetched on demand and by the
    /// warmup.
    Clangd,
    /// libclang, every translation unit parsed up front. Holds one AST at a
    /// time, where clangd holds an index of the whole project. The default.
    #[default]
    Libclang,
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "clangd" => Ok(Self::Clangd),
            "libclang" => Ok(Self::Libclang),
            other => Err(format!(
                "`{other}` is not a backend; expected `clangd` or `libclang`"
            )),
        }
    }
}

/// Spawn the loader `backend` names and return the bridge wired to it.
pub fn spawn_bridge(backend: Backend, project_path: &str, options: LoaderOptions) -> LspBridge {
    match backend {
        Backend::Clangd => spawn_lsp_bridge(project_path, options),
        Backend::Libclang => spawn_libclang_bridge(project_path, options),
    }
}

/// Index `project_path` with libclang and return a bridge that will receive the
/// graph.
///
/// The alternative to [`spawn_lsp_bridge`] for projects where clangd's resident
/// index is more memory than the machine has. Every translation unit of the
/// build `options` names is parsed, one at a time on a blocking thread, and the
/// finished graph is handed over in one [`LspUiMessage::AddGraph`] followed by
/// `Completed` — never a partial one, because a function's callers are only
/// known once every TU that could call it has been read (see `clang_index`).
/// Progress goes out as `IndexingProject`, one step per TU, which is what the
/// status bar already shows for clangd's own indexing.
///
/// The bridge has no worker: there is nothing to ask afterwards, since the
/// graph arrives with all its edges.
pub fn spawn_libclang_bridge(project_path: &str, options: LoaderOptions) -> LspBridge {
    let (ui_msg_tx, ui_msg_rx) = mpsc::unbounded_channel::<LspUiMessage>();
    let project_path = std::path::PathBuf::from(project_path);
    tokio::spawn(async move {
        if let Err(e) = libclang_loader_task(project_path, options, ui_msg_tx.clone()).await {
            log::error!("libclang loader failed: {e:#}");
            let _ = ui_msg_tx.send(LspUiMessage::Progress(LspLoadPhase::Failed(format!(
                "{e:#}"
            ))));
        }
    });
    LspBridge::new_with_status(ui_msg_rx)
}

async fn libclang_loader_task(
    project_path: std::path::PathBuf,
    options: LoaderOptions,
    ui_msg_tx: mpsc::UnboundedSender<LspUiMessage>,
) -> anyhow::Result<()> {
    use lsp::compile_commands;

    let _ = ui_msg_tx.send(LspUiMessage::Progress(LspLoadPhase::DiscoveringFiles));
    let entries = compile_commands::read_entries(&project_path)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no compile_commands.json, which the libclang backend needs",
                project_path.display()
            )
        })?;
    let flavours = compile_commands::group_by_flavour(entries);
    for line in compile_commands::describe_flavours(&flavours, &project_path) {
        log::info!("[libclang] flavour: {line}");
    }
    let entries = match &options.flavour {
        Some(wanted) => {
            compile_commands::select_flavour(&flavours, &project_path, wanted)
                .await
                .map_err(|e| anyhow::anyhow!(e))?
                .entries
                .clone()
        }
        None => flavours.into_iter().flat_map(|f| f.entries).collect(),
    };
    let mut jobs: Vec<clang_index::Job> = entries.iter().map(clang_index::Job::from_entry).collect();
    // Sorted so the first TU to define a header's inline function is the same
    // one every run.
    jobs.sort_by(|a, b| a.file.cmp(&b.file).then_with(|| a.arguments.cmp(&b.arguments)));
    log::info!(
        "[libclang] indexing {} translation unit(s) of {}",
        jobs.len(),
        project_path.display()
    );

    let progress_tx = ui_msg_tx.clone();
    let report = tokio::task::spawn_blocking(move || {
        clang_index::index(&project_path, &jobs, |done, total| {
            let _ = progress_tx.send(LspUiMessage::Progress(LspLoadPhase::IndexingProject {
                done,
                total,
            }));
        })
    })
    .await??;

    let _ = ui_msg_tx.send(LspUiMessage::AddGraph(Box::new(report.graph)));
    let _ = ui_msg_tx.send(LspUiMessage::Progress(LspLoadPhase::Completed));
    Ok(())
}

/// Check a `--flavour` before anything is spawned, so a frontend can refuse to
/// start rather than open onto a load that is going to fail.
///
/// Here beside the bootstrap for the same reason the bootstrap is here: a
/// frontend talks to `core`, and which builds a project holds is a question
/// about a load, not about clangd. The error is a finished message, listing the
/// files the user could have named instead.
pub async fn check_flavour(
    project_path: &std::path::Path,
    flavour: &std::path::Path,
) -> Result<(), String> {
    lsp::compile_commands::check_flavour(project_path, flavour).await
}
