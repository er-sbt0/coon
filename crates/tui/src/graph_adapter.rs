use grid::{Dag, LayoutError};
use model::{CallGraph, FunctionNode, SymbolId};
use std::collections::{HashMap, VecDeque};

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

    /// Build a DAG from the call graph starting at root.
    ///
    /// When a symbol is encountered that is already in the DAG, only an edge is
    /// added — no duplicate node.  Cycles are preserved as back-edges and are
    /// handled by the Sugiyama engine.
    pub fn build_dag(
        &mut self,
        graph: &CallGraph,
        root: &SymbolId,
        direction: CallDirection,
        max_depth: Option<usize>,
    ) -> Result<Dag<SymbolId>, LayoutError> {
        self.symbol_to_node.clear();
        self.node_to_symbol.clear();
        self.current_direction = direction;

        let _root_func = graph.get_function(root).ok_or_else(|| {
            LayoutError::InvalidTree(format!("Root function not found: {:?}", root))
        })?;

        let mut dag = Dag::new();
        let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::new();

        let root_idx = dag.add_node(*root);
        self.symbol_to_node.insert(*root, root_idx);
        self.node_to_symbol.insert(root_idx, *root);
        queue.push_back((*root, 0));

        while let Some((symbol, depth)) = queue.pop_front() {
            if max_depth.map_or(false, |m| depth >= m) {
                continue;
            }
            let parent_idx = self.symbol_to_node[&symbol];

            for child_func in self.get_children(graph, &symbol, direction) {
                let child_idx = if let Some(&idx) = self.symbol_to_node.get(&child_func.id) {
                    idx
                } else {
                    let idx = dag.add_node(child_func.id);
                    self.symbol_to_node.insert(child_func.id, idx);
                    self.node_to_symbol.insert(idx, child_func.id);
                    queue.push_back((child_func.id, depth + 1));
                    idx
                };
                dag.add_edge(parent_idx, child_idx)?;
            }
        }
        Ok(dag)
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
