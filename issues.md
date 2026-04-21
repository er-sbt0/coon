---

## Critical / High

### 1. Hardcoded developer machine path
lsp/src/client/mod.rs — `LspClient::new()` checks for `/home/eransa/opt/llvm/...` and prefers it over `clangd` from `PATH`. Makes the binary non-portable for everyone else and should be removed entirely.

### 2. Incorrect deduplication in `search_functions`
logic/src/query.rs — `sort_by(|a, b| a.name.cmp(&b.name))` followed by `dedup_by(|a, b| a.id == b.id)` is wrong. `dedup_by` only removes *consecutive* duplicates, so after sorting by name (not id) there will be silent duplicates in results.

### 3. Fragile post-insert mutation hack for `references_enhanced`
lsp/src/client/requests.rs — `send_request` inserts a request under `"textDocument/references"`, then the caller immediately overwrites it with the fake method string `"textDocument/references_enhanced"` as a routing tag. This is a layering violation; the `RequestType` enum in the worker is the proper place for that discrimination.

### 4. `lsp_loader_task` monolith — SRP violation
lsp/src/loader.rs — ~240 lines, handling clangd spawning, initialization, file discovery, symbol loading, *and* the indefinite TUI↔LSP forwarder loop. The last concern especially belongs in its own function/struct.

### 5. `unwrap()` on process stdio handles
lsp/src/client/mod.rs — `process.stdin.take().unwrap()` etc. right after spawn. They'll be `Some`, but are inconsistent with the rest of the crate using `anyhow::Result`.

---

## Medium

### 6. Magic 500ms sleeps in the initialization path
lsp/src/loader.rs — Two unexplained `sleep(500ms)` calls where the code should wait on actual LSP acknowledgement messages instead. Slow *and* still racy.

### 7. Channel-of-channels design
coon/src/runner.rs / lsp/src/loader.rs — A channel whose payload is `(UnboundedReceiver<LspResponse>, UnboundedSender<LspRequest>)` is used for a one-time handoff. A `oneshot` channel makes the intent explicit and is much easier to reason about.

### 8. Dual parallel response type systems
lsp/src/types.rs and lsp/src/service/mod.rs — `types.rs` defines response structs with `request_id: i64`; the service defines `LspResponse` with `request_id: String`. Both coexist but the former is only a transient parsing intermediary that adds indirection without value.

### 9. `TraversalResult` is exported but never returned
logic/src/traversal.rs — Defined, re-exported from the crate root, but every traversal method returns `Vec<SymbolId>` directly. Dead API surface.

### 10. All `LspBridge` fields are `pub`
tui/src/app/lsp_bridge.rs — ~8 internal fields all `pub`, letting callers bypass the bridge's invariants. Should be `pub(crate)` or `pub(super)` at most.

### 11. `GraphQueryEngine` leaky abstraction
logic/src/query.rs — `pub traversal`, `pub filter`, `pub path_analyzer` fields let callers bypass the higher-level query interface.

### 12. `Action::RenameWorkspace` is a permanently stubbed no-op
tui/src/app/events.rs — The handler is `{}` with a `TODO` comment. A user can trigger it and get no feedback. Remove it from the enum and key map until implemented.

### 13. `WorkspaceManager::rename` emits wrong `StatusMessage` variant
tui/src/app/workspace_manager.rs — Returns `StatusMessage::SwitchedToWorkspace` on a successful rename, which implies a tab switch happened.

### 14. `serde_json::to_string_pretty` in a tight polling loop
lsp/src/loader.rs — Allocates and formats full JSON on every message during init even when `debug` logging is disabled. Should be a lazy format or guarded with `log_enabled!`.

### 15. `block_in_place` for the TUI event loop
coon/src/runner.rs — Runs the synchronous Ratatui loop inside `#[tokio::main]` via `block_in_place`. Works, but ties up a Tokio thread indefinitely; cleaner to give the TUI its own OS thread.

---

## Low

**16.** `DEFAULT_VIEWPORT_SIZE = (100.0, 100.0)` — no units or explanation (tui/src/app/mod.rs).  
**17.** `initialize_with_capabilities` is a near-clone of `initialize` used only in tests — will silently diverge (lsp/src/client/requests.rs).  
**18.** `logic` crate doesn't re-export `GraphQueryEngine` from its root, making it awkward to use (logic/src/lib.rs).  
**19.** Emoji in help text (`📐`, `🎮`) — inconsistent terminal width handling can misalign the layout (tui/src/rendering.rs).  
**20.** Tests access private `nodes`/`edges` fields directly instead of using the existing `node_count()`/`edge_count()` accessors (model/src/graph.rs).
