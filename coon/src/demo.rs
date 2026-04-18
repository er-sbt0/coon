use model::{CallGraph, Diagnostic, DiagnosticSeverity, FunctionNode, Location};
use log::debug;

pub fn create_demo_call_graph() -> CallGraph {
    let mut graph = CallGraph::new();

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

    init_func.add_reference(Location::new("src/main.rs".to_string(), 5, 4));
    process_func.add_reference(Location::new("src/app.rs".to_string(), 15, 8));
    process_func.add_reference(Location::new("src/main.rs".to_string(), 8, 4));
    validate_func.add_reference(Location::new("src/data.rs".to_string(), 30, 12));
    helper_func.add_reference(Location::new("src/app.rs".to_string(), 20, 4));
    helper_func.add_reference(Location::new("src/data.rs".to_string(), 35, 8));
    helper_func.add_reference(Location::new("src/validation.rs".to_string(), 12, 16));

    let main_id = graph.add_function(main_func);
    let init_id = graph.add_function(init_func);
    let process_id = graph.add_function(process_func);
    let validate_id = graph.add_function(validate_func);
    let error_id = graph.add_function(error_prone_func);
    let helper_id = graph.add_function(helper_func);
    let cleanup_id = graph.add_function(cleanup_func);

    graph.add_call(
        main_id.clone(),
        init_id.clone(),
        Location::new("src/main.rs".to_string(), 5, 4),
    );
    graph.add_call(
        main_id.clone(),
        process_id.clone(),
        Location::new("src/main.rs".to_string(), 8, 4),
    );
    graph.add_call(
        init_id.clone(),
        helper_id.clone(),
        Location::new("src/app.rs".to_string(), 20, 4),
    );
    graph.add_call(
        process_id.clone(),
        validate_id.clone(),
        Location::new("src/data.rs".to_string(), 30, 12),
    );
    graph.add_call(
        process_id.clone(),
        error_id.clone(),
        Location::new("src/data.rs".to_string(), 32, 8),
    );
    graph.add_call(
        process_id.clone(),
        helper_id.clone(),
        Location::new("src/data.rs".to_string(), 35, 8),
    );
    graph.add_call(
        validate_id.clone(),
        helper_id.clone(),
        Location::new("src/validation.rs".to_string(), 12, 16),
    );
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

        assert_eq!(graph.nodes.len(), 7);
        assert!(graph.edges.len() > 5);
        assert!(graph.find_function_by_name("main").is_some());

        let error_func = graph.find_function_by_name("risky_operation").unwrap();
        assert!(!error_func.diagnostics.is_empty());

        let helper_func = graph.find_function_by_name("helper_function").unwrap();
        assert!(helper_func.references.len() > 1);
    }

    #[test]
    fn test_main_function_relationships() {
        let graph = create_demo_call_graph();
        let main_func = graph.find_function_by_name("main").unwrap();

        let callees = graph.get_callees(&main_func.id);
        assert!(callees.len() >= 3);

        let callers = graph.get_callers(&main_func.id);
        assert_eq!(callers.len(), 0);
    }

    #[test]
    fn test_helper_function_popularity() {
        let graph = create_demo_call_graph();
        let helper_func = graph.find_function_by_name("helper_function").unwrap();

        let callers = graph.get_callers(&helper_func.id);
        assert!(callers.len() >= 3);

        let callees = graph.get_callees(&helper_func.id);
        assert_eq!(callees.len(), 0);
    }
}
