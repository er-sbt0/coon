use core_data::{CallGraph, FunctionNode, SymbolId};
use grid::{LayoutError, Tree};
use std::collections::{HashMap, HashSet, VecDeque};

/// Direction for building call hierarchy tree
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    /// Show who this function calls (callees)
    Outgoing,
    /// Show who calls this function (callers)
    Incoming,
}

/// Adapter between CallGraph and Grid's TreeStructure
#[derive(Debug)]
pub struct CallGraphAdapter {
    /// Maps from SymbolId to TreeStructure node index
    pub symbol_to_node: HashMap<SymbolId, usize>,
    /// Maps from TreeStructure node index to SymbolId
    pub node_to_symbol: HashMap<usize, SymbolId>,
    /// Current direction of the tree
    current_direction: CallDirection,
}

impl CallGraphAdapter {
    pub fn new() -> Self {
        Self {
            symbol_to_node: HashMap::new(),
            node_to_symbol: HashMap::new(),
            current_direction: CallDirection::Incoming,
        }
    }

    /// Build a tree structure from the call graph starting at root
    pub fn build_tree(
        &mut self,
        graph: &CallGraph,
        root: &SymbolId,
        direction: CallDirection,
        max_depth: Option<usize>,
    ) -> Result<Tree<SymbolId>, LayoutError> {
        // Clear previous mappings
        self.symbol_to_node.clear();
        self.node_to_symbol.clear();
        self.current_direction = direction;

        // Get root function to validate it exists
        let _root_func = graph.get_function(root).ok_or_else(|| {
            LayoutError::InvalidTree(format!("Root function not found: {:?}", root))
        })?;

        // Create tree structure with SymbolId as data
        let mut tree = Tree::new(root.clone());

        // Add root to mappings (note: only first occurrence is in symbol_to_node map)
        self.symbol_to_node.insert(root.clone(), 0);
        self.node_to_symbol.insert(0, root.clone());

        // Build tree using BFS - track path to detect direct cycles only
        let mut queue = VecDeque::new();

        // Queue contains: (symbol_id, parent_idx, depth, path_from_root)
        let mut initial_path = HashSet::new();
        initial_path.insert(root.clone());
        queue.push_back((root.clone(), 0, 0, initial_path));

        while let Some((symbol, parent_idx, depth, path)) = queue.pop_front() {
            // Check depth limit
            if let Some(max) = max_depth {
                if depth >= max {
                    continue;
                }
            }

            // Get children based on direction
            let children = self.get_children(graph, &symbol, direction);

            for child_func in children {
                // Only prevent direct cycles in current path (not all previously seen nodes)
                if path.contains(&child_func.id) {
                    continue;
                }

                // Add child to tree
                let child_idx = tree.add_child(parent_idx, child_func.id.clone())?;

                // Update node_to_symbol mapping (all nodes)
                self.node_to_symbol.insert(child_idx, child_func.id.clone());

                // Only update symbol_to_node for first occurrence (for backward compatibility)
                self.symbol_to_node
                    .entry(child_func.id.clone())
                    .or_insert(child_idx);

                // Create new path including this child
                let mut new_path = path.clone();
                new_path.insert(child_func.id.clone());

                // Enqueue with new path
                queue.push_back((child_func.id.clone(), child_idx, depth + 1, new_path));
            }
        }

        Ok(tree)
    }

    /// Get children of a node based on direction
    fn get_children<'a>(
        &self,
        graph: &'a CallGraph,
        symbol: &SymbolId,
        direction: CallDirection,
    ) -> Vec<&'a FunctionNode> {
        match direction {
            CallDirection::Outgoing => graph.get_callees(symbol),
            CallDirection::Incoming => graph.get_callers(symbol),
        }
    }

    /// Get the SymbolId for a tree node index
    pub fn get_symbol(&self, node_idx: usize) -> Option<&SymbolId> {
        self.node_to_symbol.get(&node_idx)
    }

    /// Get the tree node index for a SymbolId
    pub fn get_node_index(&self, symbol: &SymbolId) -> Option<usize> {
        self.symbol_to_node.get(symbol).copied()
    }

    /// Get the current direction
    pub fn direction(&self) -> CallDirection {
        self.current_direction
    }
}

impl Default for CallGraphAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_data::Location;

    fn create_test_graph() -> CallGraph {
        let mut graph = CallGraph::new();

        // Create functions: main -> foo -> bar
        //                        -> baz
        let main_func = FunctionNode::new(
            "main".to_string(),
            "main".to_string(),
            Location::new("test.c".to_string(), 1, 0),
        );
        let main_id = main_func.id.clone();

        let foo_func = FunctionNode::new(
            "foo".to_string(),
            "foo".to_string(),
            Location::new("test.c".to_string(), 5, 0),
        );
        let foo_id = foo_func.id.clone();

        let bar_func = FunctionNode::new(
            "bar".to_string(),
            "bar".to_string(),
            Location::new("test.c".to_string(), 10, 0),
        );
        let bar_id = bar_func.id.clone();

        let baz_func = FunctionNode::new(
            "baz".to_string(),
            "baz".to_string(),
            Location::new("test.c".to_string(), 15, 0),
        );
        let baz_id = baz_func.id.clone();

        graph.add_function(main_func);
        graph.add_function(foo_func);
        graph.add_function(bar_func);
        graph.add_function(baz_func);

        // Add edges
        graph.add_call(
            main_id.clone(),
            foo_id.clone(),
            Location::new("test.c".to_string(), 2, 0),
        );
        graph.add_call(
            main_id.clone(),
            baz_id.clone(),
            Location::new("test.c".to_string(), 3, 0),
        );
        graph.add_call(
            foo_id.clone(),
            bar_id.clone(),
            Location::new("test.c".to_string(), 6, 0),
        );

        graph
    }

    #[test]
    fn test_build_tree_outgoing() {
        let graph = create_test_graph();
        let mut adapter = CallGraphAdapter::new();

        // Get main function
        let main_id = graph
            .nodes
            .values()
            .find(|f| f.name == "main")
            .unwrap()
            .id
            .clone();

        let tree = adapter
            .build_tree(&graph, &main_id, CallDirection::Outgoing, None)
            .unwrap();

        // Root should be main
        assert_eq!(tree.nodes()[0].data, main_id);
        assert_eq!(tree.nodes()[0].children.len(), 2); // foo and baz

        // Check mappings
        assert_eq!(adapter.get_node_index(&main_id), Some(0));
        assert_eq!(adapter.get_symbol(0), Some(&main_id));
    }

    #[test]
    fn test_build_tree_with_depth_limit() {
        let graph = create_test_graph();
        let mut adapter = CallGraphAdapter::new();

        let main_id = graph
            .nodes
            .values()
            .find(|f| f.name == "main")
            .unwrap()
            .id
            .clone();

        let tree = adapter
            .build_tree(&graph, &main_id, CallDirection::Outgoing, Some(1))
            .unwrap();

        // Should have main and its direct children only
        assert!(tree.nodes().len() <= 3); // main, foo, baz (bar should not be included)
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = CallGraph::new();

        // Create a cycle: a -> b -> a
        let a_func = FunctionNode::new(
            "a".to_string(),
            "a".to_string(),
            Location::new("test.c".to_string(), 1, 0),
        );
        let a_id = a_func.id.clone();

        let b_func = FunctionNode::new(
            "b".to_string(),
            "b".to_string(),
            Location::new("test.c".to_string(), 5, 0),
        );
        let b_id = b_func.id.clone();

        graph.add_function(a_func);
        graph.add_function(b_func);

        graph.add_call(
            a_id.clone(),
            b_id.clone(),
            Location::new("test.c".to_string(), 2, 0),
        );
        graph.add_call(
            b_id.clone(),
            a_id.clone(),
            Location::new("test.c".to_string(), 6, 0),
        );

        let mut adapter = CallGraphAdapter::new();
        let tree = adapter
            .build_tree(&graph, &a_id, CallDirection::Outgoing, None)
            .unwrap();

        // Should not infinite loop, should only have a and b once
        assert_eq!(tree.nodes().len(), 2);
    }
}
