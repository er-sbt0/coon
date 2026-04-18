### 1. **God Struct — `App` owns everything and leaks internals via `pub` fields**

mod.rs exposes *all* fields as `pub`: `call_graph`, `functions`, `search_bar_state`, `lsp`, `workspaces`, `show_search_bar`, etc. This eliminates encapsulation — any code can directly mutate `App` state from anywhere, bypassing the methods. The rendering code, the event handlers, and the search bar all reach directly into `App`'s guts (e.g., `self.app.search_bar_state.update_results(&self.app.call_graph)`). This creates tight coupling and makes it impossible to enforce invariants.

**Fix:** Make fields private, expose narrow accessor methods, and pass only the data each subsystem needs.

---

### 2. **Massive input-handling match block inlined in the event loop**

tui.rs contains a ~150-line monolithic `match` on every key code — search-bar input, workspace switching (`1` through `9` individually listed), navigation, modals. The search bar's key handling is completely duplicated from the `Action` pattern used for the rest. This violates the Single Responsibility Principle and makes the input layer fragile.

**Fix:** Route *all* keys through the `Action` enum or a stateful input handler — including search-bar input. Extract key-to-action mapping into a separate module/function.

---

### 3. **Swapped arrow-key semantics — silent correctness bug**

tui.rs:
```rust
KeyCode::Up => Some(Action::MoveDown),
KeyCode::Down => Some(Action::MoveUp),
KeyCode::Right => Some(Action::MoveLeft),
KeyCode::Left => Some(Action::MoveRight),
```
And in the search bar at tui.rs:
```rust
KeyCode::Left => { self.app.search_bar_state.move_cursor_right(); }
KeyCode::Right => { self.app.search_bar_state.move_cursor_left(); }
```
All four arrow keys are mapped to the *opposite* action. If intentional (inverted viewport panning), this is extremely confusing and undocumented. In the search bar case it's almost certainly a bug.

---

### 4. **Dead code: `TreeViewState`, `TreeNode`, `SwitchTab`, `ExpandNode`, `CollapseNode`**

Several `Action` variants and the entire `TreeViewState` / `TreeNode` system in actions.rs are vestigial — their handlers are no-ops:
- events.rs: `Action::SwitchTab => {} // Removed - tabs no longer exist`
- events.rs: `handle_expand_node` and `handle_collapse_node` are empty.
- `TreeViewState` is still mutated in lsp.rs and events.rs despite the tree view being removed.
- `CallGraphView` and `FunctionList` in call_graph_view.rs and function_list.rs appear completely unused by the rendering pipeline.

This dead code is confusing and adds maintenance burden.

---

### 5. **Duplicated outgoing/incoming call processing logic**

update.rs — `update_function_outgoing_calls` and `update_function_incoming_calls` are near-identical: both iterate calls, build a `Location`, construct a qualified name, call `add_function`, then iterate `from_ranges` to `add_call`. The only difference is which field is `from` vs `to` and the edge direction. This is a textbook DRY violation.

**Fix:** Extract a shared helper parameterized on direction.

---

### 6. **Duplicated navigation handler boilerplate**

events.rs — `handle_navigate_parent`, `handle_navigate_child`, `handle_navigate_next_sibling`, and `handle_navigate_prev_sibling` all follow the identical pattern: get workspace by index, call a method on `graph_view_state`, look up function name, set `status_message`. Only the navigation method name and status text differ.

**Fix:** Extract a generic `navigate_and_report(op, success_prefix, fail_msg)` helper.

---

### 7. **`CallGraphAdapter.build_tree` clones `HashSet<SymbolId>` on every BFS step**

graph_adapter.rs: Each node enqueued gets `let mut new_path = path.clone()`. For graphs with branching factor *b* and depth *d*, this creates *O(b^d)* cloned hash sets, each growing in size. This is a potential performance bomb on large call graphs.

**Fix:** Use a global `visited` set with backtracking, or use `Rc<HashSet>` with persistent data structures.

---

### 8. **`select_next_sibling` / `select_prev_sibling` cycle through ALL nodes, not actual siblings**

graph_view.rs: The comment says "sibling" navigation, but the implementation sorts `symbol_to_node` by node index and cycles through *every* node in the tree. Contrast with `navigate_next_sibling` (line 275) which correctly uses `get_siblings()`. This is inconsistent and misleading.

---

### 9. **Test leaks memory intentionally**

call_graph_view.rs:
```rust
let leaked_graph = Box::leak(Box::new(graph.clone()));
let query_engine = logic::query::GraphQueryEngine::new(leaked_graph);
```
`Box::leak` in tests avoids lifetime issues but leaks memory. This is a workaround for a design issue where `GraphQueryEngine` requires `'static` or overly restrictive lifetimes.

---

### 10. **Inconsistent error signaling in `LspBridge`**

`send_call_hierarchy`, `send_references`, and `send_workspace_symbols` in lsp_bridge.rs all return `Option<String>` where `None` sometimes means "success" (for `send_call_hierarchy`) and sometimes means "no channel available / function not found" (for `send_references`). The caller has no reliable way to distinguish success from silent failure.

**Fix:** Return `Result<Option<String>, String>` or a dedicated enum.

---

### 11. **Magic numbers scattered throughout**

- `20` for visible results: search_bar.rs — `self.adjust_scroll(20); // Assume 20 visible results for now`
- `100.0, 100.0` default viewport: mod.rs
- `30` seconds timeout: lsp_bridge.rs
- `100ms` poll interval: tui.rs

These should be named constants.

---

### 12. **No terminal cleanup on panic (resource leak)**

tui.rs: `run()` enables raw mode and enters the alternate screen, but cleanup only happens on the happy path. If any `?` propagates an error or the thread panics, the terminal is left in raw mode, rendering the user's shell unusable.

**Fix:** Use a drop guard (RAII) pattern — wrap terminal setup/teardown in a struct whose `Drop` impl restores the terminal, or install a panic hook.
