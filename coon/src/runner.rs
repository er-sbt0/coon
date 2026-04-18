use log::info;
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

pub async fn run_tui(call_graph: CallGraph) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting TUI interface...");
    let mut tui_app = TuiApp::new(call_graph)?;
    tui_app.run()?;
    info!("TUI exited. Goodbye!");
    Ok(())
}

pub async fn run_tui_async_lsp(
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
