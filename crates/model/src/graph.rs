use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::symbols::*;

/// A node in the call graph representing a function or method
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionNode {
    pub id: SymbolId,
    pub name: String,
    pub qualified_name: String,
    pub definition_location: Location,
    pub references: Vec<Reference>, // Enhanced references with symbol information
    pub diagnostics: Vec<Diagnostic>,
}

impl FunctionNode {
    pub fn new(name: String, qualified_name: String, definition_location: Location) -> Self {
        let id = SymbolId::from_content(
            &qualified_name,
            &definition_location.file_path,
            definition_location.line,
        );
        Self {
            id,
            name,
            qualified_name,
            definition_location,
            references: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Add a simple reference (backward compatibility)
    pub fn add_reference(&mut self, location: Location) {
        self.references.push(Reference {
            location,
            referencing_symbol: None,
        });
    }

    /// Add an enhanced reference with symbol information
    pub fn add_reference_with_symbol(
        &mut self,
        location: Location,
        symbol: Option<ReferencingSymbol>,
    ) {
        self.references.push(Reference {
            location,
            referencing_symbol: symbol,
        });
    }

    /// Get the names of functions that reference this function
    pub fn get_referencing_function_names(&self) -> Vec<&str> {
        self.references
            .iter()
            .filter_map(|r| r.referencing_symbol.as_ref())
            .filter(|s| {
                matches!(
                    s.kind,
                    ReferenceSymbolKind::Function
                        | ReferenceSymbolKind::Method
                        | ReferenceSymbolKind::Constructor
                )
            })
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Get all referencing symbols of a specific kind
    pub fn get_referencing_symbols_by_kind(
        &self,
        kind: ReferenceSymbolKind,
    ) -> Vec<&ReferencingSymbol> {
        self.references
            .iter()
            .filter_map(|r| r.referencing_symbol.as_ref())
            .filter(|s| s.kind == kind)
            .collect()
    }

    /// Get reference locations only (for backward compatibility)
    pub fn get_reference_locations(&self) -> Vec<&Location> {
        self.references.iter().map(|r| &r.location).collect()
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

/// Serde-only intermediate for deserializing CallGraph (holds only persisted fields).
#[derive(Deserialize)]
struct CallGraphData {
    nodes: HashMap<SymbolId, FunctionNode>,
    edges: Vec<CallEdge>,
}

/// The main call graph structure
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(from = "CallGraphData")]
pub struct CallGraph {
    pub nodes: HashMap<SymbolId, FunctionNode>,
    pub edges: Vec<CallEdge>,
    #[serde(skip)]
    edge_set: HashSet<(SymbolId, SymbolId, String, u32, u32)>,
    #[serde(skip)]
    callers_map: HashMap<SymbolId, Vec<SymbolId>>,
    #[serde(skip)]
    callees_map: HashMap<SymbolId, Vec<SymbolId>>,
}

impl<'de> serde::Deserialize<'de> for CallGraph {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        CallGraphData::deserialize(d).map(CallGraph::from)
    }
}

impl From<CallGraphData> for CallGraph {
    fn from(data: CallGraphData) -> Self {
        let mut g = CallGraph {
            nodes: data.nodes,
            edges: data.edges,
            edge_set: HashSet::new(),
            callers_map: HashMap::new(),
            callees_map: HashMap::new(),
        };
        g.rebuild_indexes();
        g
    }
}

impl CallGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: Vec::new(),
            edge_set: HashSet::new(),
            callers_map: HashMap::new(),
            callees_map: HashMap::new(),
        }
    }

    pub fn add_function(&mut self, function: FunctionNode) -> SymbolId {
        let id = function.id.clone();
        self.nodes.entry(id.clone()).or_insert(function);
        id
    }

    pub fn add_call(&mut self, caller: SymbolId, callee: SymbolId, call_location: Location) {
        let key = (
            caller.clone(),
            callee.clone(),
            call_location.file_path.clone(),
            call_location.line,
            call_location.column,
        );
        if self.edge_set.insert(key) {
            self.callees_map
                .entry(caller.clone())
                .or_default()
                .push(callee.clone());
            self.callers_map
                .entry(callee.clone())
                .or_default()
                .push(caller.clone());
            self.edges.push(CallEdge {
                caller,
                callee,
                call_location,
            });
        }
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
        self.callers_map
            .get(callee_id)
            .map(|ids| ids.iter().filter_map(|id| self.nodes.get(id)).collect())
            .unwrap_or_default()
    }

    pub fn get_callees(&self, caller_id: &SymbolId) -> Vec<&FunctionNode> {
        self.callees_map
            .get(caller_id)
            .map(|ids| ids.iter().filter_map(|id| self.nodes.get(id)).collect())
            .unwrap_or_default()
    }

    fn rebuild_indexes(&mut self) {
        self.edge_set.clear();
        self.callers_map.clear();
        self.callees_map.clear();
        for edge in &self.edges {
            let key = (
                edge.caller.clone(),
                edge.callee.clone(),
                edge.call_location.file_path.clone(),
                edge.call_location.line,
                edge.call_location.column,
            );
            self.edge_set.insert(key);
            self.callees_map
                .entry(edge.caller.clone())
                .or_default()
                .push(edge.callee.clone());
            self.callers_map
                .entry(edge.callee.clone())
                .or_default()
                .push(edge.caller.clone());
        }
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
    fn test_serde_round_trip_rebuilds_indexes() {
        let mut graph = CallGraph::new();
        let f1 = FunctionNode::new(
            "a".to_string(),
            "a".to_string(),
            Location::new("x.rs".to_string(), 1, 0),
        );
        let f2 = FunctionNode::new(
            "b".to_string(),
            "b".to_string(),
            Location::new("x.rs".to_string(), 5, 0),
        );
        let id1 = graph.add_function(f1);
        let id2 = graph.add_function(f2);
        let loc = Location::new("x.rs".to_string(), 2, 4);
        graph.add_call(id1.clone(), id2.clone(), loc.clone());

        let json = serde_json::to_string(&graph).unwrap();
        let restored: CallGraph = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.get_callees(&id1).len(), 1);
        assert_eq!(restored.get_callers(&id2).len(), 1);

        // deduplication must still work after round-trip
        let mut restored = restored;
        restored.add_call(id1.clone(), id2.clone(), loc);
        assert_eq!(restored.edges.len(), 1, "duplicate edge was not deduplicated");
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
