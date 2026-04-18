use model::{CallGraph, DiagnosticSeverity, FunctionNode, SymbolId};

/// Function filtering operations
#[derive(Debug)]
pub struct FunctionFilter<'a> {
    graph: &'a CallGraph,
}

impl<'a> FunctionFilter<'a> {
    pub fn new(graph: &'a CallGraph) -> Self {
        Self { graph }
    }

    /// Filter functions by name pattern (simple substring match)
    pub fn filter_by_name(&self, pattern: &str) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| func.name.contains(pattern) || func.qualified_name.contains(pattern))
            .collect()
    }

    /// Filter functions by file path pattern
    pub fn filter_by_file(&self, pattern: &str) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| func.definition_location.file_path.contains(pattern))
            .collect()
    }

    /// Filter functions that have diagnostics of specific severity
    pub fn filter_by_diagnostic_severity(
        &self,
        severity: DiagnosticSeverity,
    ) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| {
                func.diagnostics
                    .iter()
                    .any(|diag| diag.severity == severity)
            })
            .collect()
    }

    /// Filter functions that have any diagnostics
    pub fn filter_with_diagnostics(&self) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| !func.diagnostics.is_empty())
            .collect()
    }

    /// Filter functions that are called by a specific function
    pub fn filter_callees_of(&self, caller_id: &SymbolId) -> Vec<&FunctionNode> {
        self.graph.get_callees(caller_id)
    }

    /// Filter functions that call a specific function
    pub fn filter_callers_of(&self, callee_id: &SymbolId) -> Vec<&FunctionNode> {
        self.graph.get_callers(callee_id)
    }

    /// Filter functions that have no callers (entry points)
    pub fn filter_entry_points(&self) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| self.graph.get_callers(&func.id).is_empty())
            .collect()
    }

    /// Filter functions that have no callees (leaf functions)
    pub fn filter_leaf_functions(&self) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| self.graph.get_callees(&func.id).is_empty())
            .collect()
    }

    /// Filter functions by minimum number of references
    pub fn filter_by_reference_count(&self, min_refs: usize) -> Vec<&FunctionNode> {
        self.graph
            .nodes
            .values()
            .filter(|func| func.references.len() >= min_refs)
            .collect()
    }

    /// Combine multiple filters with AND logic
    pub fn combine_filters<F>(&self, filters: Vec<F>) -> Vec<&FunctionNode>
    where
        F: Fn(&FunctionNode) -> bool,
    {
        self.graph
            .nodes
            .values()
            .filter(|func| filters.iter().all(|filter| filter(func)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Diagnostic, FunctionNode, Location};

    fn create_test_graph_with_diagnostics() -> CallGraph {
        let mut graph = CallGraph::new();

        let mut func_with_error = FunctionNode::new(
            "func_with_error".to_string(),
            "mod::func_with_error".to_string(),
            Location::new("error.rs".to_string(), 1, 0),
        );
        func_with_error.add_diagnostic(Diagnostic {
            location: Location::new("error.rs".to_string(), 2, 4),
            severity: DiagnosticSeverity::Error,
            message: "Compilation error".to_string(),
            code: Some("E001".to_string()),
        });

        let func_clean = FunctionNode::new(
            "clean_func".to_string(),
            "mod::clean_func".to_string(),
            Location::new("clean.rs".to_string(), 1, 0),
        );

        let id1 = graph.add_function(func_with_error);
        let id2 = graph.add_function(func_clean);

        // Add a call relationship
        graph.add_call(id1, id2, Location::new("error.rs".to_string(), 3, 4));

        graph
    }

    #[test]
    fn test_filter_by_name() {
        let graph = create_test_graph_with_diagnostics();
        let filter = FunctionFilter::new(&graph);

        let results = filter.filter_by_name("error");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "func_with_error");
    }

    #[test]
    fn test_filter_by_file() {
        let graph = create_test_graph_with_diagnostics();
        let filter = FunctionFilter::new(&graph);

        let results = filter.filter_by_file("error.rs");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "func_with_error");
    }

    #[test]
    fn test_filter_by_diagnostic_severity() {
        let graph = create_test_graph_with_diagnostics();
        let filter = FunctionFilter::new(&graph);

        let results = filter.filter_by_diagnostic_severity(DiagnosticSeverity::Error);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "func_with_error");
    }

    #[test]
    fn test_filter_with_diagnostics() {
        let graph = create_test_graph_with_diagnostics();
        let filter = FunctionFilter::new(&graph);

        let results = filter.filter_with_diagnostics();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "func_with_error");
    }

    #[test]
    fn test_filter_entry_points_and_leaf_functions() {
        let graph = create_test_graph_with_diagnostics();
        let filter = FunctionFilter::new(&graph);

        let entry_points = filter.filter_entry_points();
        assert_eq!(entry_points.len(), 1);
        assert_eq!(entry_points[0].name, "func_with_error");

        let leaf_functions = filter.filter_leaf_functions();
        assert_eq!(leaf_functions.len(), 1);
        assert_eq!(leaf_functions[0].name, "clean_func");
    }
}
