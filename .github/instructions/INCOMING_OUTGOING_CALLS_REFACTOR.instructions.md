---
applyTo: '**'
---
# Incoming/Outgoing Call Graph Toggle Refactor Plan

## Current State Analysis

The current system exclusively shows **outgoing calls** in the lazy graph tree view. Here's how it works:

### Current Call Flow (Outgoing Only)
1. User expands a node in the tree view (`handle_expand_node()`)
2. System calls `load_callees_for_node()` which uses `call_graph.get_callees()`
3. This triggers `request_call_hierarchy()` → LSP PrepareCallHierarchy → LSP GetOutgoingCalls
4. Response is processed via `update_function_outgoing_calls()` which populates `node.outgoing_calls`
5. Tree view shows outgoing calls as child nodes

### Existing Infrastructure 
✅ **Already Implemented:**
- LSP client methods: `get_incoming_calls()` and `get_outgoing_calls()`
- Data structures: `CallGraphNode` has both `outgoing_calls` and `incoming_calls` fields
- LSP response handling: `LspResponse::IncomingCalls` and `update_function_incoming_calls()`
- LSP request types: `LspRequest::GetIncomingCalls`

❌ **Missing:**
- UI toggle mechanism to switch between incoming/outgoing views
- Logic to choose which type of calls to load when expanding nodes
- Visual indication of current mode
- Keyboard binding for toggle

## Refactor Plan

### 1. Add Call Direction State

**Location:** `tui_ui/src/lib.rs` - App struct

Add a new field to track the current call direction:

```rust
pub struct App {
    // ... existing fields ...
    pub call_direction: CallDirection,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallDirection {
    Outgoing,  // Show functions that THIS function calls (current behavior)
    Incoming,  // Show functions that call THIS function
}
```

### 2. Add Toggle Action

**Location:** `tui_ui/src/actions.rs`

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // ... existing actions ...
    ToggleCallDirection,
}
```

### 3. Update Keyboard Handling

**Location:** `tui_ui/src/lib.rs` - input handling

Add 't' key to toggle call direction:

```rust
KeyCode::Char('t') => Some(Action::ToggleCallDirection),
```

### 4. Implement Toggle Handler

**Location:** `tui_ui/src/lib.rs`

```rust
fn handle_toggle_call_direction(&mut self) {
    self.call_direction = match self.call_direction {
        CallDirection::Outgoing => CallDirection::Incoming,
        CallDirection::Incoming => CallDirection::Outgoing,
    };
    
    // Clear tree view and force reload with new direction
    self.tree_view_state = TreeViewState::new();
    if let Some(root_id) = &self.selected_function.clone() {
        self.start_call_graph_with_function(root_id.clone());
    }
    
    let direction_str = match self.call_direction {
        CallDirection::Outgoing => "outgoing",
        CallDirection::Incoming => "incoming",
    };
    self.status_message = format!("Switched to {} calls view", direction_str);
}
```

### 5. Update Node Loading Logic

**Location:** `tui_ui/src/lib.rs`

Refactor `load_callees_for_node()` to handle both directions:

```rust
fn load_callees_for_node(&mut self, symbol_id: SymbolId) {
    match self.call_direction {
        CallDirection::Outgoing => self.load_outgoing_calls_for_node(symbol_id),
        CallDirection::Incoming => self.load_incoming_calls_for_node(symbol_id),
    }
}

fn load_outgoing_calls_for_node(&mut self, symbol_id: SymbolId) {
    // Existing logic - get callees
    let callees = self.call_graph.get_callees(&symbol_id);
    let callee_ids: Vec<SymbolId> = callees.iter().map(|f| f.id.clone()).collect();
    
    if let Some(node_index) = self.tree_view_state.find_node_index(&symbol_id) {
        self.tree_view_state.insert_children(node_index, callee_ids);
    }
}

fn load_incoming_calls_for_node(&mut self, symbol_id: SymbolId) {
    // New logic - get callers
    let callers = self.call_graph.get_callers(&symbol_id);
    let caller_ids: Vec<SymbolId> = callers.iter().map(|f| f.id.clone()).collect();
    
    if let Some(node_index) = self.tree_view_state.find_node_index(&symbol_id) {
        self.tree_view_state.insert_children(node_index, caller_ids);
    }
}
```

### 6. Update CallGraph to Support Callers

**Location:** `core_data/src/lib.rs`

Add a `get_callers()` method to the CallGraph:

```rust
impl CallGraph {
    pub fn get_callers(&self, symbol_id: &SymbolId) -> Vec<&FunctionNode> {
        self.edges
            .iter()
            .filter(|edge| &edge.callee == symbol_id)
            .filter_map(|edge| self.nodes.get(&edge.caller))
            .collect()
    }
}
```

### 7. Update LSP Request Logic

**Location:** `tui_ui/src/lib.rs` - LSP response handling

Modify the call hierarchy response handler to request the correct call type:

```rust
// In handle_lsp_response() for CallHierarchy response
match self.call_direction {
    CallDirection::Outgoing => {
        // Existing logic - request outgoing calls
        let request = LspRequest::GetOutgoingCalls { ... };
    }
    CallDirection::Incoming => {
        // New logic - request incoming calls
        let request = LspRequest::GetIncomingCalls { ... };
    }
}
```

### 8. Update Main Request Handler

**Location:** `src/main.rs`

Ensure the main LSP request loop handles incoming calls:

```rust
LspRequest::GetIncomingCalls { request_id, call_hierarchy_item } => {
    if let Err(e) = lsp_service.request_incoming_calls(request_id.clone(), call_hierarchy_item).await {
        log::error!("Failed to send incoming calls request: {}", e);
        // ... error handling
    }
}
```

### 9. Update LSP Service

**Location:** `lsp_integration/src/service.rs`

Add `request_incoming_calls()` method:

```rust
impl LspService {
    pub async fn request_incoming_calls(
        &mut self,
        request_id: String,
        call_hierarchy_item: lsp_types::CallHierarchyItem,
    ) -> Result<()> {
        let request = LspRequest::GetIncomingCalls {
            request_id,
            call_hierarchy_item,
        };
        self.request_tx.send(request).await.map_err(|e| anyhow::anyhow!("Failed to send incoming calls request: {}", e))
    }
}
```

### 10. Update UI Labels and Help

**Location:** `tui_ui/src/lib.rs` - UI rendering

Update the tree view title to show current direction:

```rust
let direction_str = match app.call_direction {
    CallDirection::Outgoing => "Outgoing Calls",
    CallDirection::Incoming => "Incoming Calls", 
};
let title = format!("Call Graph - {} ({} nodes) - Use ↑↓/kj, →l/←h, t to toggle", 
                   direction_str, app.tree_view_state.nodes.len());
```

Add to help text:

```rust
Line::from("  t         - Toggle between incoming/outgoing calls"),
```

### 11. Update Loading States

**Location:** `tui_ui/src/lib.rs`

Modify loading state handling to be direction-aware:

```rust
fn update_loading_state(&mut self, symbol_id: &SymbolId, state: LoadingState) {
    match self.call_direction {
        CallDirection::Outgoing => {
            // Existing logic for outgoing calls
            self.loading_states.insert(symbol_id.clone(), state);
        }
        CallDirection::Incoming => {
            // New logic for incoming calls - could use separate state map
            // or extend LoadingState to be direction-aware
            self.loading_states.insert(symbol_id.clone(), state);
        }
    }
}
```

## Implementation Order

1. **Phase 1: Core State** - Add CallDirection enum and field to App
2. **Phase 2: UI Controls** - Add toggle action and keyboard binding
3. **Phase 3: Toggle Handler** - Implement direction switching logic
4. **Phase 4: Loading Logic** - Update node loading to be direction-aware
5. **Phase 5: LSP Integration** - Ensure incoming calls requests work properly
6. **Phase 6: UI Polish** - Update labels, help text, and visual indicators

## Benefits

- **User Experience**: Users can explore both "who calls this function" and "what does this function call"
- **Code Analysis**: Enables both top-down (outgoing) and bottom-up (incoming) code exploration
- **Backward Compatibility**: Default to outgoing calls (current behavior)
- **Incremental**: Can be implemented step by step without breaking existing functionality

## Technical Notes

- The LSP infrastructure for incoming calls is already complete
- The core data structures already support both directions
- Main work is in the UI layer and request orchestration
- 't' key is available and intuitive for "toggle"
- Tree expansion logic is already well-abstracted and easy to modify

## Testing Strategy

1. Test toggle between directions maintains tree state appropriately
2. Verify incoming calls are correctly loaded and displayed
3. Ensure keyboard shortcuts work in all tabs
4. Test with various function types (entry points, leaf functions, etc.)
5. Verify status messages and help text are updated correctly
