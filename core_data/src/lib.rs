use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub mod logging;

/// Unique identifier for symbols in the call graph
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub Uuid);

impl SymbolId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SymbolId {
    fn default() -> Self {
        Self::new()
    }
}

/// Location information for symbols
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub length: Option<u32>,
}

impl Location {
    pub fn new(file_path: String, line: u32, column: u32) -> Self {
        Self {
            file_path,
            line,
            column,
            length: None,
        }
    }

    pub fn with_length(mut self, length: u32) -> Self {
        self.length = Some(length);
        self
    }
}

/// Diagnostic information
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub location: Location,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// A node in the call graph representing a function or method
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionNode {
    pub id: SymbolId,
    pub name: String,
    pub qualified_name: String,
    pub definition_location: Location,
    pub references: Vec<Location>,
    pub diagnostics: Vec<Diagnostic>,
}

impl FunctionNode {
    pub fn new(name: String, qualified_name: String, definition_location: Location) -> Self {
        Self {
            id: SymbolId::new(),
            name,
            qualified_name,
            definition_location,
            references: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn add_reference(&mut self, location: Location) {
        self.references.push(location);
    }

    pub fn add_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

/// Edge in the call graph representing a function call relationship
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: SymbolId,
    pub callee: SymbolId,
    pub call_location: Location,
}

/// The main call graph structure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallGraph {
    pub nodes: HashMap<SymbolId, FunctionNode>,
    pub edges: Vec<CallEdge>,
}

impl CallGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
        }
    }

    pub fn add_function(&mut self, function: FunctionNode) -> SymbolId {
        let id = function.id.clone();
        self.nodes.insert(id.clone(), function);
        id
    }

    pub fn add_call(&mut self, caller: SymbolId, callee: SymbolId, call_location: Location) {
        self.edges.push(CallEdge {
            caller,
            callee,
            call_location,
        });
    }

    pub fn get_function(&self, id: &SymbolId) -> Option<&FunctionNode> {
        self.nodes.get(id)
    }

    pub fn get_function_mut(&mut self, id: &SymbolId) -> Option<&mut FunctionNode> {
        self.nodes.get_mut(id)
    }

    pub fn find_function_by_name(&self, name: &str) -> Option<&FunctionNode> {
        self.nodes.values().find(|f| f.name == name)
    }

    pub fn get_callers(&self, callee_id: &SymbolId) -> Vec<&FunctionNode> {
        self.edges
            .iter()
            .filter(|edge| &edge.callee == callee_id)
            .filter_map(|edge| self.nodes.get(&edge.caller))
            .collect()
    }

    pub fn get_callees(&self, caller_id: &SymbolId) -> Vec<&FunctionNode> {
        self.edges
            .iter()
            .filter(|edge| &edge.caller == caller_id)
            .filter_map(|edge| self.nodes.get(&edge.callee))
            .collect()
    }
}

impl Default for CallGraph {
    fn default() -> Self {
        Self::new()
    }
}

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
