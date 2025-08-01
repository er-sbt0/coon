use core_data::{CallGraph, SymbolId};
use std::collections::{HashMap, HashSet, VecDeque};

/// Result of a path analysis operation
#[derive(Debug, Clone)]
pub struct PathAnalysisResult {
    pub paths: Vec<Vec<SymbolId>>,
    pub total_paths: usize,
    pub max_depth: usize,
    pub avg_depth: f64,
}

/// Advanced path analysis operations
#[derive(Debug)]
pub struct PathAnalyzer<'a> {
    graph: &'a CallGraph,
}

impl<'a> PathAnalyzer<'a> {
    pub fn new(graph: &'a CallGraph) -> Self {
        Self { graph }
    }

    /// Find all paths from source to target (up to max_depth to avoid infinite loops)
    pub fn find_all_paths(
        &self,
        from: &SymbolId,
        to: &SymbolId,
        max_depth: usize,
    ) -> Vec<Vec<SymbolId>> {
        let mut all_paths = Vec::new();
        let mut current_path = vec![from.clone()];
        let mut visited = HashSet::new();

        self.find_all_paths_recursive(
            from,
            to,
            &mut current_path,
            &mut visited,
            &mut all_paths,
            max_depth,
            0,
        );

        all_paths
    }

    fn find_all_paths_recursive(
        &self,
        current: &SymbolId,
        target: &SymbolId,
        current_path: &mut Vec<SymbolId>,
        visited: &mut HashSet<SymbolId>,
        all_paths: &mut Vec<Vec<SymbolId>>,
        max_depth: usize,
        current_depth: usize,
    ) {
        if current_depth >= max_depth {
            return;
        }

        if current == target {
            all_paths.push(current_path.clone());
            return;
        }

        visited.insert(current.clone());

        let callees = self.graph.get_callees(current);
        for callee in callees {
            if !visited.contains(&callee.id) {
                current_path.push(callee.id.clone());
                self.find_all_paths_recursive(
                    &callee.id,
                    target,
                    current_path,
                    visited,
                    all_paths,
                    max_depth,
                    current_depth + 1,
                );
                current_path.pop();
            }
        }

        visited.remove(current);
    }

    /// Find the shortest paths from source to all reachable nodes
    pub fn shortest_paths_from(&self, source: &SymbolId) -> HashMap<SymbolId, Vec<SymbolId>> {
        let mut distances = HashMap::new();
        let mut paths = HashMap::new();
        let mut queue = VecDeque::new();

        distances.insert(source.clone(), 0);
        paths.insert(source.clone(), vec![source.clone()]);
        queue.push_back(source.clone());

        while let Some(current) = queue.pop_front() {
            let current_distance = distances[&current];
            let current_path = paths[&current].clone();

            let callees = self.graph.get_callees(&current);
            for callee in callees {
                let new_distance = current_distance + 1;

                if !distances.contains_key(&callee.id) || distances[&callee.id] > new_distance {
                    distances.insert(callee.id.clone(), new_distance);

                    let mut new_path = current_path.clone();
                    new_path.push(callee.id.clone());
                    paths.insert(callee.id.clone(), new_path);

                    queue.push_back(callee.id.clone());
                }
            }
        }

        paths
    }

    /// Analyze the call depth from a starting function
    pub fn analyze_call_depth(&self, start: &SymbolId) -> HashMap<SymbolId, usize> {
        let mut depths = HashMap::new();
        let mut queue = VecDeque::new();

        depths.insert(start.clone(), 0);
        queue.push_back((start.clone(), 0));

        while let Some((current, depth)) = queue.pop_front() {
            let callees = self.graph.get_callees(&current);
            for callee in callees {
                let new_depth = depth + 1;

                if !depths.contains_key(&callee.id) || depths[&callee.id] > new_depth {
                    depths.insert(callee.id.clone(), new_depth);
                    queue.push_back((callee.id.clone(), new_depth));
                }
            }
        }

        depths
    }

    /// Find cycles in the call graph starting from a specific function
    pub fn find_cycles_from(&self, start: &SymbolId, max_depth: usize) -> Vec<Vec<SymbolId>> {
        let mut cycles = Vec::new();
        let mut current_path = vec![start.clone()];
        let mut visited_in_path = HashSet::new();

        self.find_cycles_recursive(
            start,
            &mut current_path,
            &mut visited_in_path,
            &mut cycles,
            max_depth,
            0,
        );

        cycles
    }

    fn find_cycles_recursive(
        &self,
        current: &SymbolId,
        current_path: &mut Vec<SymbolId>,
        visited_in_path: &mut HashSet<SymbolId>,
        cycles: &mut Vec<Vec<SymbolId>>,
        max_depth: usize,
        current_depth: usize,
    ) {
        if current_depth >= max_depth {
            return;
        }

        visited_in_path.insert(current.clone());

        let callees = self.graph.get_callees(current);
        for callee in callees {
            if visited_in_path.contains(&callee.id) {
                // Found a cycle
                if let Some(cycle_start) = current_path.iter().position(|id| id == &callee.id) {
                    let cycle = current_path[cycle_start..].to_vec();
                    let mut complete_cycle = cycle;
                    complete_cycle.push(callee.id.clone()); // Close the cycle
                    cycles.push(complete_cycle);
                }
            } else {
                current_path.push(callee.id.clone());
                self.find_cycles_recursive(
                    &callee.id,
                    current_path,
                    visited_in_path,
                    cycles,
                    max_depth,
                    current_depth + 1,
                );
                current_path.pop();
            }
        }

        visited_in_path.remove(current);
    }

    /// Get comprehensive path analysis from a starting function
    pub fn analyze_paths_from(&self, start: &SymbolId, _max_depth: usize) -> PathAnalysisResult {
        let shortest_paths = self.shortest_paths_from(start);
        let paths: Vec<Vec<SymbolId>> = shortest_paths.values().cloned().collect();

        let total_paths = paths.len();
        let max_depth_found = paths.iter().map(|p| p.len() - 1).max().unwrap_or(0);
        let avg_depth = if total_paths > 0 {
            paths.iter().map(|p| p.len() - 1).sum::<usize>() as f64 / total_paths as f64
        } else {
            0.0
        };

        PathAnalysisResult {
            paths,
            total_paths,
            max_depth: max_depth_found,
            avg_depth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_data::{FunctionNode, Location};

    fn create_test_graph_with_cycle() -> CallGraph {
        let mut graph = CallGraph::new();

        // Create functions: A -> B -> C -> A (cycle)
        let func_a = FunctionNode::new(
            "func_a".to_string(),
            "func_a".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        let func_b = FunctionNode::new(
            "func_b".to_string(),
            "func_b".to_string(),
            Location::new("test.rs".to_string(), 5, 0),
        );
        let func_c = FunctionNode::new(
            "func_c".to_string(),
            "func_c".to_string(),
            Location::new("test.rs".to_string(), 10, 0),
        );

        let id_a = graph.add_function(func_a);
        let id_b = graph.add_function(func_b);
        let id_c = graph.add_function(func_c);

        // Create cycle: A -> B -> C -> A
        graph.add_call(
            id_a.clone(),
            id_b.clone(),
            Location::new("test.rs".to_string(), 2, 4),
        );
        graph.add_call(
            id_b.clone(),
            id_c.clone(),
            Location::new("test.rs".to_string(), 6, 4),
        );
        graph.add_call(
            id_c.clone(),
            id_a.clone(),
            Location::new("test.rs".to_string(), 11, 4),
        );

        graph
    }

    #[test]
    fn test_shortest_paths_from() {
        let graph = create_test_graph_with_cycle();
        let analyzer = PathAnalyzer::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let paths = analyzer.shortest_paths_from(&func_a.id);

        assert_eq!(paths.len(), 3); // A, B, C are all reachable
        assert!(paths.contains_key(&func_a.id));
    }

    #[test]
    fn test_analyze_call_depth() {
        let graph = create_test_graph_with_cycle();
        let analyzer = PathAnalyzer::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let depths = analyzer.analyze_call_depth(&func_a.id);

        assert_eq!(depths[&func_a.id], 0);
        assert!(depths.len() >= 3);
    }

    #[test]
    fn test_find_cycles() {
        let graph = create_test_graph_with_cycle();
        let analyzer = PathAnalyzer::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let cycles = analyzer.find_cycles_from(&func_a.id, 5);

        assert!(!cycles.is_empty(), "Should find at least one cycle");
    }

    #[test]
    fn test_analyze_paths_from() {
        let graph = create_test_graph_with_cycle();
        let analyzer = PathAnalyzer::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let analysis = analyzer.analyze_paths_from(&func_a.id, 5);

        assert_eq!(analysis.total_paths, 3);
        assert!(analysis.max_depth >= 2);
        assert!(analysis.avg_depth > 0.0);
    }
}
