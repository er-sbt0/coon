use log::{debug, error, info};
use serde_json::Value;
use std::env;
use std::path::Path;
use tokio::sync::mpsc;
use uuid::Uuid;

use core_data::logging;
use core_data::{CallGraph, Diagnostic, DiagnosticSeverity, FunctionNode, Location};
use lsp_integration::{LspClient, LspRequest, LspResponse, LspService};
use tui_ui::TuiApp;

mod compile_commands;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize file-based logging
    logging::init_logging().map_err(|e| format!("Failed to initialize logging: {}", e))?;

    let args: Vec<String> = env::args().collect();

    match args.len() {
        1 => {
            // No arguments - run with demo data
            info!("No arguments provided. Running with demo data...");
            run_with_demo_data().await?;
        }
        2 => {
            // Single argument - project path
            let project_path = &args[1];
            if Path::new(project_path).exists() {
                info!("Starting LSP integration for project: {}", project_path);
                run_with_lsp(project_path).await?;
            } else {
                error!("Error: Project path '{}' does not exist", project_path);
                print_usage();
                std::process::exit(1);
            }
        }
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_usage() {
    println!("Usage:");
    println!(
        "  {} [project_path]",
        env::args().next().unwrap_or("coon".to_string())
    );
    println!();
    println!("Options:");
    println!("  project_path    Path to the project directory for LSP analysis");
    println!("                  If not provided, runs with demo data");
    println!();
    println!("Examples:");
    println!(
        "  {}                    # Run with demo data",
        env::args().next().unwrap_or("coon".to_string())
    );
    println!(
        "  {} /path/to/project   # Analyze a real project",
        env::args().next().unwrap_or("coon".to_string())
    );
}

async fn run_with_demo_data() -> Result<(), Box<dyn std::error::Error>> {
    info!("Creating demo call graph...");
    let call_graph = create_demo_call_graph();

    info!(
        "Demo call graph created with {} functions and {} call relationships",
        call_graph.nodes.len(),
        call_graph.edges.len()
    );

    run_tui(call_graph).await
}

async fn run_with_lsp(project_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting LSP client for lazy loading...");

    // Create channel for LSP communication
    let (tx, rx) = mpsc::channel::<Value>(100);

    // Start LSP client
    let mut lsp_client = LspClient::new(tx).await?;

    info!("LSP client started. Initializing LSP...");

    // Initialize LSP
    let root_path = std::path::Path::new(project_path).canonicalize()?;
    let root_uri = lsp_types::Url::from_file_path(&root_path)
        .map_err(|_| "Failed to create URI from project path")?;

    let init_id = lsp_client.initialize(root_uri.clone()).await?;

    // Wait for initialization response
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
                            return Err(format!("LSP initialization failed: {:?}", error).into());
                        }
                        init_response = Some(message);
                        info!("LSP initialization complete");
                    }
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    let _init_result = init_response.ok_or("LSP initialization timed out")?;

    // Send initialized notification
    info!("Sending initialized notification...");
    lsp_client.send_initialized().await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Discover source files from compile_commands.json or fallback to directory walking
    let source_files = compile_commands::parse_compile_commands(&root_path)?;
    info!("Found {} source files", source_files.len());

    // Create a lazy call graph that will load relationships on demand
    let mut lazy_call_graph = core_data::LazyCallGraph::new();

    // Create LSP service for all LSP operations
    let mut lsp_service = LspService::new(lsp_client, rx_for_init).await?;

    // Set project files for symbol filtering
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

    // Preload documents from compile_commands.json to avoid "non-added document" errors
    let document_uris: Vec<lsp_types::Url> = source_files
        .iter()
        .filter_map(|path| lsp_types::Url::from_file_path(path).ok())
        .collect();

    if !document_uris.is_empty() {
        info!(
            "Preloading {} documents to avoid LSP errors...",
            document_uris.len()
        );
        let _preload_request_id = lsp_service.preload_documents(document_uris.clone()).await?;

        // Give some time for preloading to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Use only workspace symbols for fast startup (lazy loading approach)
    let initial_request_id = Uuid::new_v4().to_string();
    lsp_service
        .request_workspace_symbols(initial_request_id.clone(), "".to_string())
        .await?;
    info!("Requested initial workspace symbols for overview");

    // Give time for initial symbols loading
    let symbol_timeout = tokio::time::Duration::from_millis(2000); // Increased timeout for workspace symbols
    let symbol_start = std::time::Instant::now();
    let mut initial_symbols_loaded = false;

    while symbol_start.elapsed() < symbol_timeout && !initial_symbols_loaded {
        match lsp_service.try_recv_response() {
            Some(lsp_response) => {
                if let LspResponse::WorkspaceSymbols {
                    request_id,
                    symbols,
                } = lsp_response
                {
                    if request_id == initial_request_id {
                        info!("Received {} initial workspace symbols", symbols.len());

                        // Add function symbols to lazy call graph
                        let mut function_count = 0;
                        for function_node in symbols.into_iter().take(100) {
                            // Increased limit since lazy loading is faster
                            info!(
                                "Adding function to lazy call graph: '{}' from {}:{}:{}",
                                function_node.qualified_name,
                                function_node.definition_location.file_path,
                                function_node.definition_location.line,
                                function_node.definition_location.column
                            );

                            // Convert to workspace symbol info
                            let workspace_symbol = core_data::WorkspaceSymbolInfo {
                                name: function_node.name.clone(),
                                qualified_name: function_node.qualified_name.clone(),
                                kind: core_data::lsp_types::SymbolKind::FUNCTION,
                                location: function_node.definition_location.clone(),
                                container_name: None,
                            };

                            lazy_call_graph.add_function_from_workspace_symbol(workspace_symbol);
                            function_count += 1;
                        }

                        info!(
                            "Added {} functions to lazy call graph from workspace symbols",
                            function_count
                        );
                        initial_symbols_loaded = true;
                        break;
                    }
                }
            }
            None => {
                // No response yet, continue waiting
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    if !initial_symbols_loaded {
        info!("Initial workspace symbols loading timed out - proceeding with document symbols we found");
    }

    // If no functions were found, add a placeholder
    if lazy_call_graph.nodes.is_empty() {
        let project_name = std::path::Path::new(project_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown");

        let message = if source_files.is_empty() {
            format!("Project: {} (no source files found)", project_name)
        } else {
            format!(
                "Project: {} (click to explore {} source files)",
                project_name,
                source_files.len()
            )
        };

        let project_symbol = core_data::WorkspaceSymbolInfo {
            name: message.clone(),
            qualified_name: format!("project::{}", project_path),
            kind: core_data::lsp_types::SymbolKind::MODULE,
            location: core_data::Location::new(project_path.to_string(), 1, 0),
            container_name: None,
        };
        lazy_call_graph.add_function_from_workspace_symbol(project_symbol);
    }

    info!(
        "Starting TUI with {} initial functions. Additional data will load on demand...",
        lazy_call_graph.nodes.len()
    );

    // Convert to old CallGraph format for backward compatibility during migration
    let call_graph = lazy_call_graph.to_call_graph();

    // Start TUI with lazy loading
    run_tui_with_lazy_lsp(call_graph, lsp_service).await
}

async fn run_tui(call_graph: CallGraph) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting TUI interface...");

    let mut tui_app = TuiApp::new(call_graph)?;
    tui_app.run()?;

    info!("TUI exited. Goodbye!");
    Ok(())
}

async fn run_tui_with_lazy_lsp(
    call_graph: CallGraph,
    mut lsp_service: LspService,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::sync::mpsc;

    info!("Starting lazy-loading TUI interface...");

    // Create channels for TUI communication
    let (tui_request_tx, mut tui_request_rx) = mpsc::unbounded_channel();
    let (tui_response_tx, tui_response_rx) = mpsc::unbounded_channel();

    // Spawn a task to handle the communication between TUI and LSP service
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // Forward TUI requests to LSP service
                tui_request = tui_request_rx.recv() => {
                    if let Some(request) = tui_request {
                        log::info!("Forwarding TUI request to LSP service: {:?}", request);
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
                            _ => {
                                log::warn!("Unhandled LSP request type");
                            }
                        }
                    } else {
                        log::info!("TUI request channel closed, stopping forwarder");
                        break; // TUI channel closed
                    }
                }

                // Poll for LSP responses and forward them to TUI
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {
                    // Collect all available responses
                    let mut response_count = 0;
                    while let Some(response) = lsp_service.try_recv_response() {
                        log::info!("Forwarding LSP response to TUI: {:?}", response);
                        if let Err(e) = tui_response_tx.send(response) {
                            log::error!("Failed to forward LSP response to TUI: {}", e);
                            break;
                        }
                        response_count += 1;
                    }
                    if response_count > 0 {
                        log::info!("Forwarded {} LSP responses to TUI", response_count);
                    }
                }
            }
        }
        log::info!("LSP-TUI forwarder task ended");
    });

    // Create TUI app with channels
    let mut tui_app = TuiApp::new(call_graph)?;
    tui_app.set_lsp_channels(tui_response_rx, tui_request_tx);

    // Use the full TUI implementation with all features
    tui_app.run()?;

    info!("Lazy-loading TUI exited. Goodbye!");
    Ok(())
}

fn create_demo_call_graph() -> CallGraph {
    let mut graph = CallGraph::new();

    // Create some demo functions with various characteristics
    let main_func = FunctionNode::new(
        "main".to_string(),
        "main".to_string(),
        Location::new("src/main.rs".to_string(), 1, 0),
    );

    let mut init_func = FunctionNode::new(
        "initialize".to_string(),
        "app::initialize".to_string(),
        Location::new("src/app.rs".to_string(), 10, 0),
    );

    let mut process_func = FunctionNode::new(
        "process_data".to_string(),
        "data::process_data".to_string(),
        Location::new("src/data.rs".to_string(), 25, 0),
    );

    let mut validate_func = FunctionNode::new(
        "validate_input".to_string(),
        "validation::validate_input".to_string(),
        Location::new("src/validation.rs".to_string(), 5, 0),
    );

    let mut error_prone_func = FunctionNode::new(
        "risky_operation".to_string(),
        "unsafe_module::risky_operation".to_string(),
        Location::new("src/unsafe_module.rs".to_string(), 15, 0),
    );

    let mut helper_func = FunctionNode::new(
        "helper_function".to_string(),
        "utils::helper_function".to_string(),
        Location::new("src/utils.rs".to_string(), 30, 0),
    );

    let cleanup_func = FunctionNode::new(
        "cleanup".to_string(),
        "cleanup::cleanup".to_string(),
        Location::new("src/cleanup.rs".to_string(), 8, 0),
    );

    // Add some diagnostics to make it more interesting
    error_prone_func.add_diagnostic(Diagnostic {
        location: Location::new("src/unsafe_module.rs".to_string(), 17, 4),
        severity: DiagnosticSeverity::Error,
        message: "Potential null pointer dereference".to_string(),
        code: Some("E0001".to_string()),
    });

    error_prone_func.add_diagnostic(Diagnostic {
        location: Location::new("src/unsafe_module.rs".to_string(), 20, 8),
        severity: DiagnosticSeverity::Warning,
        message: "Unused variable 'temp'".to_string(),
        code: Some("W0042".to_string()),
    });

    validate_func.add_diagnostic(Diagnostic {
        location: Location::new("src/validation.rs".to_string(), 8, 12),
        severity: DiagnosticSeverity::Warning,
        message: "Consider using more specific error types".to_string(),
        code: Some("W0100".to_string()),
    });

    helper_func.add_diagnostic(Diagnostic {
        location: Location::new("src/utils.rs".to_string(), 35, 0),
        severity: DiagnosticSeverity::Information,
        message: "This function could be optimized".to_string(),
        code: Some("I0005".to_string()),
    });

    // Add some references to functions
    init_func.add_reference(Location::new("src/main.rs".to_string(), 5, 4));
    process_func.add_reference(Location::new("src/app.rs".to_string(), 15, 8));
    process_func.add_reference(Location::new("src/main.rs".to_string(), 8, 4));
    validate_func.add_reference(Location::new("src/data.rs".to_string(), 30, 12));
    helper_func.add_reference(Location::new("src/app.rs".to_string(), 20, 4));
    helper_func.add_reference(Location::new("src/data.rs".to_string(), 35, 8));
    helper_func.add_reference(Location::new("src/validation.rs".to_string(), 12, 16));

    // Add functions to graph and get their IDs
    let main_id = graph.add_function(main_func);
    let init_id = graph.add_function(init_func);
    let process_id = graph.add_function(process_func);
    let validate_id = graph.add_function(validate_func);
    let error_id = graph.add_function(error_prone_func);
    let helper_id = graph.add_function(helper_func);
    let cleanup_id = graph.add_function(cleanup_func);

    // Create call relationships
    // main -> initialize
    graph.add_call(
        main_id.clone(),
        init_id.clone(),
        Location::new("src/main.rs".to_string(), 5, 4),
    );

    // main -> process_data
    graph.add_call(
        main_id.clone(),
        process_id.clone(),
        Location::new("src/main.rs".to_string(), 8, 4),
    );

    // initialize -> helper_function
    graph.add_call(
        init_id.clone(),
        helper_id.clone(),
        Location::new("src/app.rs".to_string(), 20, 4),
    );

    // process_data -> validate_input
    graph.add_call(
        process_id.clone(),
        validate_id.clone(),
        Location::new("src/data.rs".to_string(), 30, 12),
    );

    // process_data -> risky_operation
    graph.add_call(
        process_id.clone(),
        error_id.clone(),
        Location::new("src/data.rs".to_string(), 32, 8),
    );

    // process_data -> helper_function
    graph.add_call(
        process_id.clone(),
        helper_id.clone(),
        Location::new("src/data.rs".to_string(), 35, 8),
    );

    // validate_input -> helper_function
    graph.add_call(
        validate_id.clone(),
        helper_id.clone(),
        Location::new("src/validation.rs".to_string(), 12, 16),
    );

    // main -> cleanup (called at the end)
    graph.add_call(
        main_id.clone(),
        cleanup_id.clone(),
        Location::new("src/main.rs".to_string(), 15, 4),
    );

    debug!("Demo call graph structure:");
    debug!("  main");
    debug!("  ├─ initialize");
    debug!("  │  └─ helper_function");
    debug!("  ├─ process_data");
    debug!("  │  ├─ validate_input");
    debug!("  │  │  └─ helper_function");
    debug!("  │  ├─ risky_operation (has errors!)");
    debug!("  │  └─ helper_function");
    debug!("  └─ cleanup");

    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_graph_creation() {
        let graph = create_demo_call_graph();

        // Should have 7 functions
        assert_eq!(graph.nodes.len(), 7);

        // Should have multiple call relationships
        assert!(graph.edges.len() > 5);

        // Main function should exist
        assert!(graph.find_function_by_name("main").is_some());

        // Error-prone function should have diagnostics
        let error_func = graph.find_function_by_name("risky_operation").unwrap();
        assert!(!error_func.diagnostics.is_empty());

        // Helper function should have multiple references
        let helper_func = graph.find_function_by_name("helper_function").unwrap();
        assert!(helper_func.references.len() > 1);
    }

    #[test]
    fn test_main_function_relationships() {
        let graph = create_demo_call_graph();
        let main_func = graph.find_function_by_name("main").unwrap();

        // Main should call multiple functions
        let callees = graph.get_callees(&main_func.id);
        assert!(callees.len() >= 3); // initialize, process_data, cleanup

        // Main should have no callers (entry point)
        let callers = graph.get_callers(&main_func.id);
        assert_eq!(callers.len(), 0);
    }

    #[test]
    fn test_helper_function_popularity() {
        let graph = create_demo_call_graph();
        let helper_func = graph.find_function_by_name("helper_function").unwrap();

        // Helper should be called by multiple functions
        let callers = graph.get_callers(&helper_func.id);
        assert!(callers.len() >= 3);

        // Helper should have no callees (leaf function)
        let callees = graph.get_callees(&helper_func.id);
        assert_eq!(callees.len(), 0);
    }
}
