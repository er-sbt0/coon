### 1. Hardcoded developer-specific path (bug/security)

mod.rs hardcodes `/home/eransa/opt/llvm/llvm-20.1.8-build/bin/clangd`. This will silently use a stale binary on your machine and fail on anyone else's. Should use an env var (`CLANGD_PATH`) or config file, falling back to `"clangd"` on PATH.

---

### 2. Massive boilerplate in the TUI-LSP forwarder (~100 lines of near-identical match arms)

In loader.rs, every `LspRequest` variant follows the exact same pattern: call service method → on error, log + send `LspResponse::Error`. This should be a macro or a helper function like:
```rust
async fn forward<F, Fut>(service: &mut LspService, request_id: String, ...) { ... }
```
The same pattern repeats in request.rs across all 8 handler functions.

---

### 3. `SymbolId` should derive `Copy`

`SymbolId` wraps `Uuid` (16 bytes, `Copy`), yet it only derives `Clone`. This forces hundreds of unnecessary `.clone()` calls across the entire codebase — graph.rs, traversal.rs, graph_adapter.rs, etc. Adding `Copy` would eliminate the majority of them and make the code cleaner.

---

### 4. O(n) linear scan for function deduplication

In mod.rs:
```rust
if !self.functions.contains(&id) {
    self.functions.push(id);
}
```
`self.functions` is a `Vec<SymbolId>`. `.contains()` is $O(n)$ per call, making the total cost $O(n^2)$ as symbols arrive. Use a `HashSet<SymbolId>` alongside the vec, or replace the vec with an `IndexSet`.

---

### 5. Inconsistent error handling: `Box<dyn Error>` vs `anyhow::Result`

The codebase mixes `Box<dyn std::error::Error>` (in loader.rs, runner.rs, tui.rs) with `anyhow::Result` (in parsing, client, service). Pick one consistently — `anyhow` is already a dependency and more ergonomic.

---

### 6. Expensive edge deduplication key clones a `String` per check

In graph.rs:
```rust
let key = (
    caller.clone(), callee.clone(),
    call_location.file_path.clone(), // heap allocation
    call_location.line, call_location.column,
);
```
Every `add_call` clones the file path string into the `edge_set`. For large graphs this wastes memory. Consider hashing the location into a compact key or interning file paths.

---

### 10. No LSP shutdown on application exit

In runner.rs, the `lsp_loader_task` is spawned with `tokio::spawn` but never joined. When the TUI exits, the Tokio runtime is dropped, aborting the task without sending LSP `shutdown`/`exit`. The `Drop` impl on `LspClient` tries `start_kill()` but the child process might not receive it reliably.

---

### 12. Glob import `use crate::symbols::*`

In graph.rs, this obscures type origins. Use explicit imports (`use crate::symbols::{SymbolId, Location, ...}`).

---

### 13. `parse_document_symbol_response_impl` and `parse_hover_response_impl` don't use the generic helper

parsing.rs manually replicates the exact id-extraction → method-match → remove-pending → parse-result pattern that `parse_lsp_response` already implements generically. These should delegate to the generic helper.

---

### 14. `is_references_response` clones the entire JSON result for a type-probe

In references.rs:
```rust
serde_json::from_value::<Vec<lsp_types::Location>>(result.clone())
```
Clones potentially large JSON just to check if it's a references response. This is only used in the legacy detection path, but still wasteful. Check for structural markers (array of objects with `uri`/`range` keys) instead.

---

### 15. `DocumentSymbols` response round-trips through `WorkspaceSymbolInfo` lossy

In symbols.rs, the handler parses into `WorkspaceSymbolInfo` (losing range data) then converts back to `DocumentSymbol` with `Range::default()`. The original `DocumentSymbol` data from the LSP should be forwarded directly.

---

### 16. Excessive info-level logging of full responses

update.rs logs `{:?}` of the entire `LspResponse` at `log::info!` for every response, and parsing_impl.rs logs the full JSON of every references response. This generates enormous log files. Use `log::debug!` or `log::trace!` for payload details.

---

### 18. `get_function_mut` can invalidate `name_index`

graph.rs hands out `&mut FunctionNode`, but if the caller changes `node.name`, the `name_index` becomes stale. Either make the index lazy/rebuild-on-access or don't expose `&mut FunctionNode` and provide specific mutation methods instead.

---

### 19. Magic numbers for pan distances and viewport defaults

Hard-coded constants like `3.0` / `5.0` for pan delta (events.rs), `(100.0, 100.0)` for default viewport (mod.rs), and `200` for max symbols (loader.rs) should be named constants.
