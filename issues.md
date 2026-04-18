### 1. **God Struct — `App` owns everything and leaks internals via `pub` fields**

mod.rs exposes *all* fields as `pub`: `call_graph`, `functions`, `search_bar_state`, `lsp`, `workspaces`, `show_search_bar`, etc. This eliminates encapsulation — any code can directly mutate `App` state from anywhere, bypassing the methods. The rendering code, the event handlers, and the search bar all reach directly into `App`'s guts (e.g., `self.app.search_bar_state.update_results(&self.app.call_graph)`). This creates tight coupling and makes it impossible to enforce invariants.

**Fix:** Make fields private, expose narrow accessor methods, and pass only the data each subsystem needs.

---

### 2. **Massive input-handling match block inlined in the event loop**

tui.rs contains a ~150-line monolithic `match` on every key code — search-bar input, workspace switching (`1` through `9` individually listed), navigation, modals. The search bar's key handling is completely duplicated from the `Action` pattern used for the rest. This violates the Single Responsibility Principle and makes the input layer fragile.

**Fix:** Route *all* keys through the `Action` enum or a stateful input handler — including search-bar input. Extract key-to-action mapping into a separate module/function.

---

### 7. **`CallGraphAdapter.build_tree` clones `HashSet<SymbolId>` on every BFS step**

graph_adapter.rs: Each node enqueued gets `let mut new_path = path.clone()`. For graphs with branching factor *b* and depth *d*, this creates *O(b^d)* cloned hash sets, each growing in size. This is a potential performance bomb on large call graphs.

**Fix:** Use a global `visited` set with backtracking, or use `Rc<HashSet>` with persistent data structures.

---

### 10. **Inconsistent error signaling in `LspBridge`**

`send_call_hierarchy`, `send_references`, and `send_workspace_symbols` in lsp_bridge.rs all return `Option<String>` where `None` sometimes means "success" (for `send_call_hierarchy`) and sometimes means "no channel available / function not found" (for `send_references`). The caller has no reliable way to distinguish success from silent failure.

**Fix:** Return `Result<Option<String>, String>` or a dedicated enum.

---

### 12. **No terminal cleanup on panic (resource leak)**

tui.rs: `run()` enables raw mode and enters the alternate screen, but cleanup only happens on the happy path. If any `?` propagates an error or the thread panics, the terminal is left in raw mode, rendering the user's shell unusable.

**Fix:** Use a drop guard (RAII) pattern — wrap terminal setup/teardown in a struct whose `Drop` impl restores the terminal, or install a panic hook.
