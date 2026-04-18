### 7. **`CallGraphAdapter.build_tree` clones `HashSet<SymbolId>` on every BFS step**

graph_adapter.rs: Each node enqueued gets `let mut new_path = path.clone()`. For graphs with branching factor *b* and depth *d*, this creates *O(b^d)* cloned hash sets, each growing in size. This is a potential performance bomb on large call graphs.

**Fix:** Use a global `visited` set with backtracking, or use `Rc<HashSet>` with persistent data structures.

---

### 10. **Inconsistent error signaling in `LspBridge`**

`send_call_hierarchy`, `send_references`, and `send_workspace_symbols` in lsp_bridge.rs all return `Option<String>` where `None` sometimes means "success" (for `send_call_hierarchy`) and sometimes means "no channel available / function not found" (for `send_references`). The caller has no reliable way to distinguish success from silent failure.

**Fix:** Return `Result<Option<String>, String>` or a dedicated enum.

---

