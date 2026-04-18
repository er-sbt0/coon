use model::{CallGraph, SymbolId};
use std::collections::{HashMap, HashSet, VecDeque};

/// Result of a graph traversal operation
#[derive(Debug, Clone)]
pub struct TraversalResult {
    pub visited_nodes: Vec<SymbolId>,
    pub path: Vec<SymbolId>,
    pub depth: usize,
}

/// Graph traversal operations
#[derive(Debug)]
pub struct GraphTraversal<'a> {
    graph: &'a CallGraph,
}

impl<'a> GraphTraversal<'a> {
    pub fn new(graph: &'a CallGraph) -> Self {
        Self { graph }
    }

    /// Generic BFS traversal that can work with different neighbor functions
    fn bfs_traverse<F>(&self, start_id: &SymbolId, get_neighbors: F) -> Vec<SymbolId>
    where
        F: Fn(&SymbolId) -> Vec<SymbolId>,
    {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();

        queue.push_back(*start_id);
        visited.insert(*start_id);

        while let Some(current_id) = queue.pop_front() {
            result.push(current_id);

            for neighbor_id in get_neighbors(&current_id) {
                if !visited.contains(&neighbor_id) {
                    visited.insert(neighbor_id);
                    queue.push_back(neighbor_id);
                }
            }
        }

        result
    }

    /// Find all functions reachable from a starting function (BFS)
    pub fn find_reachable_from(&self, start_id: &SymbolId) -> Vec<SymbolId> {
        self.bfs_traverse(start_id, |id| {
            self.graph
                .get_callees(id)
                .iter()
                .map(|callee| callee.id)
                .collect()
        })
    }

    /// Find all functions that can reach a target function (reverse BFS)
    pub fn find_can_reach(&self, target_id: &SymbolId) -> Vec<SymbolId> {
        self.bfs_traverse(target_id, |id| {
            self.graph
                .get_callers(id)
                .iter()
                .map(|caller| caller.id)
                .collect()
        })
    }

    /// Find the shortest path between two functions
    pub fn find_path(&self, from: &SymbolId, to: &SymbolId) -> Option<Vec<SymbolId>> {
        if from == to {
            return Some(vec![*from]);
        }

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut parent = HashMap::new();

        queue.push_back(*from);
        visited.insert(*from);

        while let Some(current_id) = queue.pop_front() {
            let callees = self.graph.get_callees(&current_id);
            for callee in callees {
                if !visited.contains(&callee.id) {
                    visited.insert(callee.id);
                    parent.insert(callee.id, current_id);
                    queue.push_back(callee.id);

                    if &callee.id == to {
                        // Reconstruct path
                        let mut path = Vec::new();
                        let mut current = *to;

                        while let Some(prev) = parent.get(&current) {
                            path.push(current);
                            current = *prev;
                        }
                        path.push(*from);
                        path.reverse();
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    /// Get all functions at a specific depth from the starting function
    pub fn get_functions_at_depth(
        &self,
        start_id: &SymbolId,
        target_depth: usize,
    ) -> Vec<SymbolId> {
        if target_depth == 0 {
            return vec![*start_id];
        }

        let mut current_level = vec![*start_id];
        let mut visited = HashSet::new();
        visited.insert(*start_id);

        for _ in 0..target_depth {
            let mut next_level = Vec::new();

            for id in current_level {
                let callees = self.graph.get_callees(&id);
                for callee in callees {
                    if !visited.contains(&callee.id) {
                        visited.insert(callee.id);
                        next_level.push(callee.id);
                    }
                }
            }

            current_level = next_level;
            if current_level.is_empty() {
                break;
            }
        }

        current_level
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    fn create_test_graph() -> CallGraph {
        let mut graph = CallGraph::new();

        // Create test functions
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

        // Create call relationships: A -> B -> C
        graph.add_call(id_a, id_b, Location::new("test.rs".to_string(), 2, 4));
        graph.add_call(id_b, id_c, Location::new("test.rs".to_string(), 6, 4));

        graph
    }

    #[test]
    fn test_find_reachable_from() {
        let graph = create_test_graph();
        let traversal = GraphTraversal::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let reachable = traversal.find_reachable_from(&func_a.id);

        assert_eq!(reachable.len(), 3); // A, B, C
    }

    #[test]
    fn test_find_can_reach() {
        let graph = create_test_graph();
        let traversal = GraphTraversal::new(&graph);

        let func_c = graph.find_function_by_name("func_c").unwrap();
        let can_reach = traversal.find_can_reach(&func_c.id);

        assert_eq!(can_reach.len(), 3); // C, B, A (reverse order)
    }

    #[test]
    fn test_find_path() {
        let graph = create_test_graph();
        let traversal = GraphTraversal::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();
        let func_c = graph.find_function_by_name("func_c").unwrap();

        let path = traversal.find_path(&func_a.id, &func_c.id).unwrap();
        assert_eq!(path.len(), 3); // A -> B -> C
    }

    #[test]
    fn test_get_functions_at_depth() {
        let graph = create_test_graph();
        let traversal = GraphTraversal::new(&graph);

        let func_a = graph.find_function_by_name("func_a").unwrap();

        let depth_0 = traversal.get_functions_at_depth(&func_a.id, 0);
        assert_eq!(depth_0.len(), 1);

        let depth_1 = traversal.get_functions_at_depth(&func_a.id, 1);
        assert_eq!(depth_1.len(), 1); // func_b

        let depth_2 = traversal.get_functions_at_depth(&func_a.id, 2);
        assert_eq!(depth_2.len(), 1); // func_c
    }
}
