pub mod graph;
pub mod lazy_graph;
pub mod lsp_status;
pub mod symbols;

// Re-export for convenience
pub use lsp_types;

// Re-export all public types for backward compatibility
pub use graph::*;
pub use lazy_graph::*;
pub use symbols::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_id_creation() {
        let id1 = SymbolId::new();
        let id2 = SymbolId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_function_node_creation() {
        let location = Location::new("test.rs".to_string(), 10, 5);
        let function = FunctionNode::new(
            "test_func".to_string(),
            "my_mod::test_func".to_string(),
            location.clone(),
        );

        assert_eq!(function.name, "test_func");
        assert_eq!(function.qualified_name, "my_mod::test_func");
        assert_eq!(function.definition_location, location);
        assert!(function.references.is_empty());
        assert!(function.diagnostics.is_empty());
    }

    #[test]
    fn test_enhanced_references() {
        let mut function = FunctionNode::new(
            "target_func".to_string(),
            "my_mod::target_func".to_string(),
            Location::new("test.rs".to_string(), 10, 5),
        );

        // Add a simple reference (backward compatibility)
        function.add_reference(Location::new("caller.rs".to_string(), 5, 10));

        // Add an enhanced reference
        let referencing_symbol = ReferencingSymbol {
            name: "caller_func".to_string(),
            qualified_name: "my_mod::caller_func".to_string(),
            kind: ReferenceSymbolKind::Function,
        };
        function.add_reference_with_symbol(
            Location::new("caller.rs".to_string(), 8, 15),
            Some(referencing_symbol),
        );

        assert_eq!(function.references.len(), 2);
        assert_eq!(function.get_referencing_function_names().len(), 1);
        assert_eq!(function.get_referencing_function_names()[0], "caller_func");
        assert_eq!(function.get_reference_locations().len(), 2);
    }

    #[test]
    fn test_call_graph_operations() {
        let mut graph = CallGraph::new();

        let func1 = FunctionNode::new(
            "func1".to_string(),
            "func1".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        let func2 = FunctionNode::new(
            "func2".to_string(),
            "func2".to_string(),
            Location::new("test.rs".to_string(), 5, 0),
        );

        let id1 = graph.add_function(func1);
        let id2 = graph.add_function(func2);

        graph.add_call(
            id1.clone(),
            id2.clone(),
            Location::new("test.rs".to_string(), 2, 4),
        );

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);

        let callees = graph.get_callees(&id1);
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].name, "func2");

        let callers = graph.get_callers(&id2);
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].name, "func1");
    }
}
