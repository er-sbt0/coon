---

## Critical / High

### 1. Hardcoded developer machine path
lsp/src/client/mod.rs — `LspClient::new()` checks for `/home/eransa/opt/llvm/...` and prefers it over `clangd` from `PATH`. Makes the binary non-portable for everyone else and should be removed entirely.

---

## Medium

### 9. `TraversalResult` is exported but never returned
logic/src/traversal.rs — Defined, re-exported from the crate root, but every traversal method returns `Vec<SymbolId>` directly. Dead API surface.

### 10. All `LspBridge` fields are `pub`
tui/src/app/lsp_bridge.rs — ~8 internal fields all `pub`, letting callers bypass the bridge's invariants. Should be `pub(crate)` or `pub(super)` at most.

### 14. `serde_json::to_string_pretty` in a tight polling loop
lsp/src/loader.rs — Allocates and formats full JSON on every message during init even when `debug` logging is disabled. Should be a lazy format or guarded with `log_enabled!`.

### 15. `block_in_place` for the TUI event loop
coon/src/runner.rs — Runs the synchronous Ratatui loop inside `#[tokio::main]` via `block_in_place`. Works, but ties up a Tokio thread indefinitely; cleaner to give the TUI its own OS thread.

---

## Low

**16.** `DEFAULT_VIEWPORT_SIZE = (100.0, 100.0)` — no units or explanation (tui/src/app/mod.rs).  
**18.** `logic` crate doesn't re-export `GraphQueryEngine` from its root, making it awkward to use (logic/src/lib.rs).  
**19.** Emoji in help text (`📐`, `🎮`) — inconsistent terminal width handling can misalign the layout (tui/src/rendering.rs).  
**20.** Tests access private `nodes`/`edges` fields directly instead of using the existing `node_count()`/`edge_count()` accessors (model/src/graph.rs).
