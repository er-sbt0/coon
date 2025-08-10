use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub mod logging;

// Re-export for convenience
pub use lsp_types;

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

/// Enhanced reference information that includes symbol context
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub location: Location,
    pub referencing_symbol: Option<ReferencingSymbol>,
}

/// Information about the symbol that is making a reference
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferencingSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: ReferenceSymbolKind,
}

/// Types of symbols that can make references
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReferenceSymbolKind {
    Function,
    Method,
    Constructor,
    Variable,
    Field,
    Class,
    Struct,
    Module,
    Unknown,
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
    pub references: Vec<Reference>, // Enhanced references with symbol information
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

// ====== NEW LAZY CALL GRAPH IMPLEMENTATION ======

/// Workspace symbol information for deduplication
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSymbolInfo {
    pub name: String,
    pub qualified_name: String,
    pub kind: lsp_types::SymbolKind,
    pub location: Location,
    pub container_name: Option<String>,
}

/// Call reference with LSP data
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReference {
    pub target_symbol_id: SymbolId,
    pub call_locations: Vec<Location>,
    pub from_ranges: Vec<lsp_types::Range>, // LSP ranges from call hierarchy
}

/// Enhanced call graph node supporting lazy loading
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallGraphNode {
    pub symbol_id: SymbolId,
    pub function: FunctionNode,

    // Lazy loading state
    pub call_hierarchy_prepared: bool,
    pub outgoing_calls_loaded: bool,
    pub incoming_calls_loaded: bool,

    // Cached LSP data
    pub call_hierarchy_item: Option<lsp_types::CallHierarchyItem>,

    // Call relationships
    pub outgoing_calls: Vec<CallReference>,
    pub incoming_calls: Vec<CallReference>,
}

impl CallGraphNode {
    pub fn new(function: FunctionNode) -> Self {
        let symbol_id = function.id.clone();
        Self {
            symbol_id,
            function,
            call_hierarchy_prepared: false,
            outgoing_calls_loaded: false,
            incoming_calls_loaded: false,
            call_hierarchy_item: None,
            outgoing_calls: Vec::new(),
            incoming_calls: Vec::new(),
        }
    }

    pub fn from_workspace_symbol(symbol: WorkspaceSymbolInfo) -> Self {
        let function = FunctionNode::new(
            symbol.name.clone(),
            symbol.qualified_name.clone(),
            symbol.location.clone(),
        );
        Self::new(function)
    }
}

/// Lazy call graph with deduplication support
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LazyCallGraph {
    pub nodes: HashMap<SymbolId, CallGraphNode>,
    pub symbol_index: HashMap<String, SymbolId>, // qualified_name -> SymbolId
}

impl LazyCallGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            symbol_index: HashMap::new(),
        }
    }

    pub fn add_function_from_workspace_symbol(&mut self, symbol: WorkspaceSymbolInfo) -> SymbolId {
        // Check for existing symbol to avoid duplicates
        if let Some(existing_id) = self.symbol_index.get(&symbol.qualified_name) {
            return existing_id.clone();
        }

        let node = CallGraphNode::from_workspace_symbol(symbol.clone());
        let symbol_id = node.symbol_id.clone();

        self.symbol_index
            .insert(symbol.qualified_name, symbol_id.clone());
        self.nodes.insert(symbol_id.clone(), node);

        symbol_id
    }

    pub fn get_or_create_node(&mut self, qualified_name: &str) -> SymbolId {
        if let Some(existing_id) = self.symbol_index.get(qualified_name) {
            return existing_id.clone();
        }

        // Create a minimal function node
        let function = FunctionNode::new(
            qualified_name
                .split("::")
                .last()
                .unwrap_or(qualified_name)
                .to_string(),
            qualified_name.to_string(),
            Location::new("unknown".to_string(), 0, 0),
        );

        let node = CallGraphNode::new(function);
        let symbol_id = node.symbol_id.clone();

        self.symbol_index
            .insert(qualified_name.to_string(), symbol_id.clone());
        self.nodes.insert(symbol_id.clone(), node);

        symbol_id
    }

    pub fn mark_call_hierarchy_prepared(
        &mut self,
        symbol_id: &SymbolId,
        item: lsp_types::CallHierarchyItem,
    ) {
        if let Some(node) = self.nodes.get_mut(symbol_id) {
            node.call_hierarchy_prepared = true;
            node.call_hierarchy_item = Some(item);
        }
    }

    pub fn add_outgoing_calls(
        &mut self,
        symbol_id: &SymbolId,
        calls: Vec<lsp_types::CallHierarchyOutgoingCall>,
    ) {
        // First, collect all target IDs we need to create
        let mut target_ids = Vec::new();

        for call in &calls {
            let target_qualified_name = format!("{}::{}", call.to.name, call.to.uri.path());
            let target_id = self.get_or_create_node(&target_qualified_name);
            target_ids.push(target_id);
        }

        // Now update the node
        if let Some(node) = self.nodes.get_mut(symbol_id) {
            node.outgoing_calls.clear(); // Replace existing calls

            for (call, target_id) in calls.iter().zip(target_ids) {
                // Convert locations
                let call_locations = call
                    .from_ranges
                    .iter()
                    .map(|range| {
                        Location::new(
                            call.to.uri.path().to_string(),
                            (range.start.line + 1) as u32,
                            (range.start.character + 1) as u32,
                        )
                    })
                    .collect();

                let call_ref = CallReference {
                    target_symbol_id: target_id,
                    call_locations,
                    from_ranges: call.from_ranges.clone(),
                };

                node.outgoing_calls.push(call_ref);
            }

            node.outgoing_calls_loaded = true;
        }
    }

    pub fn add_incoming_calls(
        &mut self,
        symbol_id: &SymbolId,
        calls: Vec<lsp_types::CallHierarchyIncomingCall>,
    ) {
        // First, collect all source IDs we need to create
        let mut source_ids = Vec::new();

        for call in &calls {
            let source_qualified_name = format!("{}::{}", call.from.name, call.from.uri.path());
            let source_id = self.get_or_create_node(&source_qualified_name);
            source_ids.push(source_id);
        }

        // Now update the node
        if let Some(node) = self.nodes.get_mut(symbol_id) {
            node.incoming_calls.clear(); // Replace existing calls

            for (call, source_id) in calls.iter().zip(source_ids) {
                // Convert locations
                let call_locations = call
                    .from_ranges
                    .iter()
                    .map(|range| {
                        Location::new(
                            call.from.uri.path().to_string(),
                            (range.start.line + 1) as u32,
                            (range.start.character + 1) as u32,
                        )
                    })
                    .collect();

                let call_ref = CallReference {
                    target_symbol_id: source_id,
                    call_locations,
                    from_ranges: call.from_ranges.clone(),
                };

                node.incoming_calls.push(call_ref);
            }

            node.incoming_calls_loaded = true;
        }
    }

    pub fn is_node_expandable(&self, symbol_id: &SymbolId) -> (bool, bool) {
        if let Some(node) = self.nodes.get(symbol_id) {
            let outgoing_expandable = node.call_hierarchy_prepared && !node.outgoing_calls_loaded;
            let incoming_expandable = node.call_hierarchy_prepared && !node.incoming_calls_loaded;
            (outgoing_expandable, incoming_expandable)
        } else {
            (false, false)
        }
    }

    pub fn get_node(&self, symbol_id: &SymbolId) -> Option<&CallGraphNode> {
        self.nodes.get(symbol_id)
    }

    pub fn get_node_mut(&mut self, symbol_id: &SymbolId) -> Option<&mut CallGraphNode> {
        self.nodes.get_mut(symbol_id)
    }

    pub fn find_function_by_name(&self, name: &str) -> Option<&CallGraphNode> {
        self.nodes.values().find(|node| node.function.name == name)
    }
}

impl Default for LazyCallGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// Migration utilities for backward compatibility
impl LazyCallGraph {
    /// Convert to the old CallGraph format for backward compatibility
    pub fn to_call_graph(&self) -> CallGraph {
        let mut call_graph = CallGraph::new();

        // Add all functions
        for node in self.nodes.values() {
            call_graph
                .nodes
                .insert(node.symbol_id.clone(), node.function.clone());
        }

        // Add call edges from outgoing calls
        for node in self.nodes.values() {
            for call_ref in &node.outgoing_calls {
                // Add edges for each call location
                for location in &call_ref.call_locations {
                    call_graph.edges.push(CallEdge {
                        caller: node.symbol_id.clone(),
                        callee: call_ref.target_symbol_id.clone(),
                        call_location: location.clone(),
                    });
                }
            }
        }

        call_graph
    }
}

/// Backward compatibility: convert CallGraph to LazyCallGraph
impl From<CallGraph> for LazyCallGraph {
    fn from(call_graph: CallGraph) -> Self {
        let mut lazy_graph = LazyCallGraph::new();

        // Add all functions
        for (symbol_id, function) in call_graph.nodes {
            let node = CallGraphNode::new(function);
            let qualified_name = node.function.qualified_name.clone();

            lazy_graph
                .symbol_index
                .insert(qualified_name, symbol_id.clone());
            lazy_graph.nodes.insert(symbol_id, node);
        }

        // Convert edges to outgoing calls
        for edge in call_graph.edges {
            if let Some(caller_node) = lazy_graph.nodes.get_mut(&edge.caller) {
                // Check if this target already exists in outgoing calls
                if let Some(existing_call) = caller_node
                    .outgoing_calls
                    .iter_mut()
                    .find(|c| c.target_symbol_id == edge.callee)
                {
                    // Add location to existing call
                    existing_call.call_locations.push(edge.call_location);
                } else {
                    // Create new call reference
                    let call_ref = CallReference {
                        target_symbol_id: edge.callee,
                        call_locations: vec![edge.call_location],
                        from_ranges: Vec::new(), // No LSP ranges available from old format
                    };
                    caller_node.outgoing_calls.push(call_ref);
                }

                // Mark as loaded since we converted from the old format
                caller_node.outgoing_calls_loaded = true;
            }
        }

        lazy_graph
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
