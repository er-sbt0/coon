use crate::error::LayoutError;

/// An arena-allocated node in a DAG containing user data and adjacency lists
#[derive(Clone, Debug, PartialEq)]
pub struct DagNode<T> {
    pub data: T,
    pub successors: Vec<usize>,
    pub predecessors: Vec<usize>,
}

impl<T> DagNode<T> {
    fn new(data: T) -> Self {
        Self {
            data,
            successors: Vec::new(),
            predecessors: Vec::new(),
        }
    }
}

/// Directed acyclic graph using an arena-Vec representation.
///
/// Cycles are permitted at the data level — cycle detection and removal are
/// the responsibility of the Sugiyama layout engine (Phase 2).
#[derive(Clone, Debug, PartialEq)]
pub struct Dag<T> {
    nodes: Vec<DagNode<T>>,
    version: u64,
}

impl<T> Dag<T> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            version: 0,
        }
    }

    /// Add a node and return its index.
    pub fn add_node(&mut self, data: T) -> usize {
        let id = self.nodes.len();
        self.nodes.push(DagNode::new(data));
        self.version += 1;
        id
    }

    /// Add a directed edge from `from` to `to`, recording both adjacency lists.
    pub fn add_edge(&mut self, from: usize, to: usize) -> Result<(), LayoutError> {
        if from >= self.nodes.len() {
            return Err(LayoutError::InvalidNode(from));
        }
        if to >= self.nodes.len() {
            return Err(LayoutError::InvalidNode(to));
        }
        self.nodes[from].successors.push(to);
        self.nodes[to].predecessors.push(from);
        self.version += 1;
        Ok(())
    }

    pub fn node(&self, id: usize) -> Result<&DagNode<T>, LayoutError> {
        self.nodes.get(id).ok_or(LayoutError::InvalidNode(id))
    }

    pub fn node_mut(&mut self, id: usize) -> Result<&mut DagNode<T>, LayoutError> {
        self.nodes.get_mut(id).ok_or(LayoutError::InvalidNode(id))
    }

    pub fn nodes(&self) -> &[DagNode<T>] {
        &self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Iterate over all edges as `(from, to)` pairs.
    pub fn edges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .flat_map(|(from, node)| node.successors.iter().map(move |&to| (from, to)))
    }

    /// Return indices of all nodes with no predecessors (source nodes / roots).
    pub fn roots(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.predecessors.is_empty())
            .map(|(i, _)| i)
            .collect()
    }
}

impl<T> Default for Dag<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_nodes_and_edges() {
        let mut dag: Dag<&str> = Dag::new();
        let a = dag.add_node("a");
        let b = dag.add_node("b");
        let c = dag.add_node("c");
        dag.add_edge(a, b).unwrap();
        dag.add_edge(a, c).unwrap();

        assert_eq!(dag.len(), 3);
        assert_eq!(dag.node(a).unwrap().successors, vec![b, c]);
        assert_eq!(dag.node(b).unwrap().predecessors, vec![a]);
        assert_eq!(dag.node(c).unwrap().predecessors, vec![a]);
    }

    #[test]
    fn roots_returns_nodes_without_predecessors() {
        let mut dag: Dag<u32> = Dag::new();
        let r = dag.add_node(0);
        let a = dag.add_node(1);
        let b = dag.add_node(2);
        dag.add_edge(r, a).unwrap();
        dag.add_edge(r, b).unwrap();

        assert_eq!(dag.roots(), vec![r]);
    }

    #[test]
    fn edges_iterator() {
        let mut dag: Dag<u32> = Dag::new();
        let a = dag.add_node(0);
        let b = dag.add_node(1);
        let c = dag.add_node(2);
        dag.add_edge(a, b).unwrap();
        dag.add_edge(b, c).unwrap();

        let edges: Vec<_> = dag.edges().collect();
        assert_eq!(edges, vec![(a, b), (b, c)]);
    }

    #[test]
    fn invalid_edge_returns_error() {
        let mut dag: Dag<u32> = Dag::new();
        let a = dag.add_node(0);
        assert!(matches!(
            dag.add_edge(a, 99),
            Err(LayoutError::InvalidNode(99))
        ));
    }

    #[test]
    fn version_increments_on_mutation() {
        let mut dag: Dag<u32> = Dag::new();
        let v0 = dag.version();
        let a = dag.add_node(0);
        let v1 = dag.version();
        let b = dag.add_node(1);
        let v2 = dag.version();
        dag.add_edge(a, b).unwrap();
        let v3 = dag.version();

        assert!(v1 > v0);
        assert!(v2 > v1);
        assert!(v3 > v2);
    }

    #[test]
    fn diamond_dag_no_duplicate_nodes() {
        // root → a → c, root → b → c
        let mut dag: Dag<&str> = Dag::new();
        let root = dag.add_node("root");
        let a = dag.add_node("a");
        let b = dag.add_node("b");
        let c = dag.add_node("c");
        dag.add_edge(root, a).unwrap();
        dag.add_edge(root, b).unwrap();
        dag.add_edge(a, c).unwrap();
        dag.add_edge(b, c).unwrap();

        assert_eq!(dag.len(), 4);
        assert_eq!(dag.node(c).unwrap().predecessors, vec![a, b]);
    }

    #[test]
    fn cycle_allowed_in_dag_structure() {
        // Cycles are handled by the Sugiyama engine; Dag itself permits them
        let mut dag: Dag<u32> = Dag::new();
        let a = dag.add_node(0);
        let b = dag.add_node(1);
        dag.add_edge(a, b).unwrap();
        dag.add_edge(b, a).unwrap(); // back-edge

        assert_eq!(dag.len(), 2);
        let edges: Vec<_> = dag.edges().collect();
        assert_eq!(edges.len(), 2);
    }
}
