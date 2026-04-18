use crate::{filters::FunctionFilter, path::PathAnalyzer, GraphTraversal};
use model::{CallGraph, DiagnosticSeverity, FunctionNode, SymbolId};
use std::collections::HashMap;

/// High-level query interface combining multiple analysis operations
#[derive(Debug)]
pub struct GraphQueryEngine<'a> {
    graph: &'a CallGraph,
    pub traversal: GraphTraversal<'a>,
    pub filter: FunctionFilter<'a>,
    pub path_analyzer: PathAnalyzer<'a>,
}

impl<'a> GraphQueryEngine<'a> {
    pub fn new(graph: &'a CallGraph) -> Self {
        Self {
            graph,
            traversal: GraphTraversal::new(graph),
            filter: FunctionFilter::new(graph),
            path_analyzer: PathAnalyzer::new(graph),
        }
    }

    /// Find functions that match a query string (name or file pattern)
    pub fn search_functions(&self, query: &str) -> Vec<&FunctionNode> {
        let mut results = self.filter.filter_by_name(query);
        results.extend(self.filter.filter_by_file(query));
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results.dedup_by(|a, b| a.id == b.id);
        results
    }

    /// Get comprehensive function information
    pub fn get_function_info(&self, function_id: &SymbolId) -> Option<FunctionInfo> {
        let function = self.graph.get_function(function_id)?;

        let callers = self.graph.get_callers(function_id);
        let callees = self.graph.get_callees(function_id);
        let reachable_count = self.traversal.find_reachable_from(function_id).len();
        let can_reach_count = self.traversal.find_can_reach(function_id).len();

        Some(FunctionInfo {
            function: function.clone(),
            caller_count: callers.len(),
            callee_count: callees.len(),
            reachable_function_count: reachable_count,
            can_reach_function_count: can_reach_count,
            has_diagnostics: !function.diagnostics.is_empty(),
            error_count: function
                .diagnostics
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::ERROR)
                .count(),
            warning_count: function
                .diagnostics
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::WARNING)
                .count(),
        })
    }

    /// Find impact analysis: what functions are affected if this function changes
    pub fn analyze_impact(&self, function_id: &SymbolId, max_depth: usize) -> ImpactAnalysis {
        let affected_functions = self.traversal.find_can_reach(function_id);
        let depth_analysis = self.path_analyzer.analyze_call_depth(function_id);

        let directly_affected = self.graph.get_callers(function_id);
        let total_affected = affected_functions.len();

        ImpactAnalysis {
            target_function: *function_id,
            directly_affected_count: directly_affected.len(),
            total_affected_count: total_affected,
            affected_functions,
            depth_distribution: depth_analysis,
            max_depth_analyzed: max_depth,
        }
    }

    /// Find dependency analysis: what functions does this function depend on
    pub fn analyze_dependencies(
        &self,
        function_id: &SymbolId,
        max_depth: usize,
    ) -> DependencyAnalysis {
        let dependencies = self.traversal.find_reachable_from(function_id);
        let path_analysis = self
            .path_analyzer
            .analyze_paths_from(function_id, max_depth);

        let direct_dependencies = self.graph.get_callees(function_id);

        DependencyAnalysis {
            source_function: *function_id,
            direct_dependency_count: direct_dependencies.len(),
            total_dependency_count: dependencies.len(),
            dependencies,
            path_analysis,
            max_depth_analyzed: max_depth,
        }
    }

    /// Find potential problem areas (functions with high complexity or many issues)
    pub fn find_problem_areas(&self) -> ProblemAreas {
        let functions_with_errors = self
            .filter
            .filter_by_diagnostic_severity(DiagnosticSeverity::ERROR);
        let functions_with_warnings = self
            .filter
            .filter_by_diagnostic_severity(DiagnosticSeverity::WARNING);
        let highly_connected = self.find_highly_connected_functions(5); // functions with 5+ connections
        let entry_points = self.filter.filter_entry_points();
        let leaf_functions = self.filter.filter_leaf_functions();

        ProblemAreas {
            functions_with_errors: functions_with_errors.len(),
            functions_with_warnings: functions_with_warnings.len(),
            highly_connected_functions: highly_connected.len(),
            entry_point_count: entry_points.len(),
            leaf_function_count: leaf_functions.len(),
            error_functions: functions_with_errors.into_iter().map(|f| f.id).collect(),
            warning_functions: functions_with_warnings.into_iter().map(|f| f.id).collect(),
            complex_functions: highly_connected.into_iter().map(|f| f.id).collect(),
        }
    }

    /// Find functions with high connectivity (many callers + callees)
    fn find_highly_connected_functions(&self, min_connections: usize) -> Vec<&FunctionNode> {
        self.graph
            .functions()
            .filter(|func| {
                let caller_count = self.graph.get_callers(&func.id).len();
                let callee_count = self.graph.get_callees(&func.id).len();
                caller_count + callee_count >= min_connections
            })
            .collect()
    }

    /// Get graph statistics
    pub fn get_graph_stats(&self) -> GraphStatistics {
        let total_functions = self.graph.node_count();
        let total_edges = self.graph.edge_count();
        let entry_points = self.filter.filter_entry_points().len();
        let leaf_functions = self.filter.filter_leaf_functions().len();
        let functions_with_diagnostics = self.filter.filter_with_diagnostics().len();

        let avg_calls_per_function = if total_functions > 0 {
            total_edges as f64 / total_functions as f64
        } else {
            0.0
        };

        GraphStatistics {
            total_functions,
            total_call_relationships: total_edges,
            entry_point_count: entry_points,
            leaf_function_count: leaf_functions,
            functions_with_diagnostics,
            average_calls_per_function: avg_calls_per_function,
        }
    }
}

/// Comprehensive information about a single function
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub function: FunctionNode,
    pub caller_count: usize,
    pub callee_count: usize,
    pub reachable_function_count: usize,
    pub can_reach_function_count: usize,
    pub has_diagnostics: bool,
    pub error_count: usize,
    pub warning_count: usize,
}

/// Results of impact analysis
#[derive(Debug, Clone)]
pub struct ImpactAnalysis {
    pub target_function: SymbolId,
    pub directly_affected_count: usize,
    pub total_affected_count: usize,
    pub affected_functions: Vec<SymbolId>,
    pub depth_distribution: HashMap<SymbolId, usize>,
    pub max_depth_analyzed: usize,
}

/// Results of dependency analysis
#[derive(Debug, Clone)]
pub struct DependencyAnalysis {
    pub source_function: SymbolId,
    pub direct_dependency_count: usize,
    pub total_dependency_count: usize,
    pub dependencies: Vec<SymbolId>,
    pub path_analysis: crate::path::PathAnalysisResult,
    pub max_depth_analyzed: usize,
}

/// Analysis of potential problem areas in the codebase
#[derive(Debug, Clone)]
pub struct ProblemAreas {
    pub functions_with_errors: usize,
    pub functions_with_warnings: usize,
    pub highly_connected_functions: usize,
    pub entry_point_count: usize,
    pub leaf_function_count: usize,
    pub error_functions: Vec<SymbolId>,
    pub warning_functions: Vec<SymbolId>,
    pub complex_functions: Vec<SymbolId>,
}

/// Overall graph statistics
#[derive(Debug, Clone)]
pub struct GraphStatistics {
    pub total_functions: usize,
    pub total_call_relationships: usize,
    pub entry_point_count: usize,
    pub leaf_function_count: usize,
    pub functions_with_diagnostics: usize,
    pub average_calls_per_function: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{Diagnostic, FunctionNode, Location};

    fn create_complex_test_graph() -> CallGraph {
        let mut graph = CallGraph::new();

        // Create multiple functions with various relationships
        let main_func = FunctionNode::new(
            "main".to_string(),
            "main".to_string(),
            Location::new("main.rs".to_string(), 1, 0),
        );

        let mut helper_func = FunctionNode::new(
            "helper".to_string(),
            "utils::helper".to_string(),
            Location::new("utils.rs".to_string(), 1, 0),
        );
        helper_func.add_diagnostic(Diagnostic {
            location: Location::new("utils.rs".to_string(), 5, 0),
            severity: DiagnosticSeverity::WARNING,
            message: "Unused variable".to_string(),
            code: Some("W001".to_string()),
        });

        let mut error_func = FunctionNode::new(
            "error_prone".to_string(),
            "error_prone".to_string(),
            Location::new("error.rs".to_string(), 1, 0),
        );
        error_func.add_diagnostic(Diagnostic {
            location: Location::new("error.rs".to_string(), 3, 0),
            severity: DiagnosticSeverity::ERROR,
            message: "Type error".to_string(),
            code: Some("E001".to_string()),
        });

        let leaf_func = FunctionNode::new(
            "leaf".to_string(),
            "leaf".to_string(),
            Location::new("leaf.rs".to_string(), 1, 0),
        );

        let id_main = graph.add_function(main_func);
        let id_helper = graph.add_function(helper_func);
        let id_error = graph.add_function(error_func);
        let id_leaf = graph.add_function(leaf_func);

        // Create call relationships
        graph.add_call(
            id_main,
            id_helper,
            Location::new("main.rs".to_string(), 5, 4),
        );
        graph.add_call(
            id_main,
            id_error,
            Location::new("main.rs".to_string(), 6, 4),
        );
        graph.add_call(
            id_helper,
            id_leaf,
            Location::new("utils.rs".to_string(), 10, 4),
        );

        graph
    }

    #[test]
    fn test_search_functions() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let results = query_engine.search_functions("main");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "main");
    }

    #[test]
    fn test_get_function_info() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let main_func = graph.find_function_by_name("main").unwrap();
        let info = query_engine.get_function_info(&main_func.id).unwrap();

        assert_eq!(info.function.name, "main");
        assert_eq!(info.caller_count, 0); // main is entry point
        assert_eq!(info.callee_count, 2); // calls helper and error_prone
        assert!(!info.has_diagnostics);
    }

    #[test]
    fn test_analyze_impact() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let helper_func = graph.find_function_by_name("helper").unwrap();
        let impact = query_engine.analyze_impact(&helper_func.id, 5);

        assert_eq!(impact.directly_affected_count, 1); // main calls helper
        assert!(impact.total_affected_count >= 1);
    }

    #[test]
    fn test_analyze_dependencies() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let main_func = graph.find_function_by_name("main").unwrap();
        let deps = query_engine.analyze_dependencies(&main_func.id, 5);

        assert_eq!(deps.direct_dependency_count, 2); // calls helper and error_prone
        assert_eq!(deps.total_dependency_count, 4); // main, helper, error_prone, leaf
    }

    #[test]
    fn test_find_problem_areas() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let problems = query_engine.find_problem_areas();

        assert_eq!(problems.functions_with_errors, 1);
        assert_eq!(problems.functions_with_warnings, 1);
        assert_eq!(problems.entry_point_count, 1);
        // In our test graph: main -> helper -> leaf, main -> error_prone
        // So leaf functions are: leaf and error_prone (2 functions)
        assert_eq!(problems.leaf_function_count, 2);
    }

    #[test]
    fn test_get_graph_stats() {
        let graph = create_complex_test_graph();
        let query_engine = GraphQueryEngine::new(&graph);

        let stats = query_engine.get_graph_stats();

        assert_eq!(stats.total_functions, 4);
        assert_eq!(stats.total_call_relationships, 3);
        assert_eq!(stats.entry_point_count, 1);
        // In our test graph: main -> helper -> leaf, main -> error_prone
        // So leaf functions are: leaf and error_prone (2 functions)
        assert_eq!(stats.leaf_function_count, 2);
        assert_eq!(stats.functions_with_diagnostics, 2); // helper and error_prone have diagnostics
        assert!(stats.average_calls_per_function > 0.0);
    }
}
