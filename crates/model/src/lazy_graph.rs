use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::graph::*;
use crate::symbols::*;

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

        // Add call edges from incoming calls
        for node in self.nodes.values() {
            for call_ref in &node.incoming_calls {
                // Add edges for each call location
                for location in &call_ref.call_locations {
                    call_graph.edges.push(CallEdge {
                        caller: call_ref.target_symbol_id.clone(),
                        callee: node.symbol_id.clone(),
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
