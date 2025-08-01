use log::{debug, error, info};
use serde_json::Value;
use std::env;
use std::path::Path;
use tokio::sync::mpsc;

use core_data::logging;
use core_data::{CallGraph, Diagnostic, DiagnosticSeverity, FunctionNode, Location};
use lsp_integration::LspClient;
use tui_ui::TuiApp;

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
    info!("Starting LSP client...");

    // Create channel for LSP communication
    let (tx, mut rx) = mpsc::channel::<Value>(100);

    // Start LSP client
    let _lsp_client = LspClient::new(tx).await?;

    info!("LSP client started. Building call graph...");

    // For now, we'll start with a minimal graph and could extend this
    // to actually process LSP responses in a real implementation
    let mut call_graph = CallGraph::new();

    // Add a placeholder function to show the project
    let project_func = FunctionNode::new(
        format!(
            "Project: {}",
            Path::new(project_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
        ),
        format!("project::{}", project_path),
        Location::new(project_path.to_string(), 1, 0),
    );
    call_graph.add_function(project_func);

    // Listen for a few LSP messages (in a real implementation, this would be more sophisticated)
    let mut message_count = 0;
    while let Ok(message) = rx.try_recv() {
        message_count += 1;
        debug!(
            "Received LSP message {}: {}",
            message_count,
            serde_json::to_string_pretty(&message)?
        );

        // For demo purposes, just collect a few messages
        if message_count >= 3 {
            break;
        }
    }

    info!(
        "Call graph built with {} functions. Starting TUI...",
        call_graph.nodes.len()
    );
    run_tui(call_graph).await
}

async fn run_tui(call_graph: CallGraph) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting TUI interface...");

    let mut tui_app = TuiApp::new(call_graph)?;
    tui_app.run()?;

    info!("TUI exited. Goodbye!");
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
