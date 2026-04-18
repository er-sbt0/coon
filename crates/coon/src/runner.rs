use log::{error, info};
use tokio::sync::mpsc;

use lsp_integration::{LspRequest, LspResponse};
use model::lsp_status::LspUiMessage;
use model::CallGraph;
use tui_ui::TuiApp;

pub async fn run_with_demo_data() -> Result<(), Box<dyn std::error::Error>> {
    info!("Creating demo call graph...");
    let call_graph = crate::demo::create_demo_call_graph();

    info!(
        "Demo call graph created with {} functions and {} call relationships",
        call_graph.nodes.len(),
        call_graph.edges.len()
    );

    run_tui(call_graph).await
}

pub async fn run_with_lsp(project_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting LSP loader and TUI asynchronously...");

    let (ui_msg_tx, ui_msg_rx) = mpsc::unbounded_channel::<LspUiMessage>();
    let (lsp_channels_tx, lsp_channels_rx) = mpsc::unbounded_channel::<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>();

    let project_path_string = project_path.to_string();
    tokio::spawn(async move {
        if let Err(e) = lsp_integration::loader::lsp_loader_task(
            &project_path_string,
            ui_msg_tx,
            lsp_channels_tx,
        )
        .await
        {
            error!("LSP loader task failed: {}", e);
        }
    });

    run_tui_async_lsp(ui_msg_rx, lsp_channels_rx).await
}

async fn run_tui(call_graph: CallGraph) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting TUI interface...");
    let mut tui_app = TuiApp::new(call_graph)?;
    tui_app.run()?;
    info!("TUI exited. Goodbye!");
    Ok(())
}

async fn run_tui_async_lsp(
    lsp_rx: mpsc::UnboundedReceiver<LspUiMessage>,
    lsp_channels_rx: mpsc::UnboundedReceiver<(
        mpsc::UnboundedReceiver<LspResponse>,
        mpsc::UnboundedSender<LspRequest>,
    )>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting TUI interface with async LSP loading...");
    let call_graph = CallGraph::new();
    let mut tui_app = TuiApp::new_with_lsp_async(call_graph, lsp_rx, lsp_channels_rx)?;
    tui_app.run()?;
    info!("TUI exited. Goodbye!");
    Ok(())
}
