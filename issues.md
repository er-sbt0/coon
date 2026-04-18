Here's the consolidated ranking, re-evaluated by real-world impact:

---

### Critical — App is broken or leaks resources every run

**1. Hardcoded developer path** — mod.rs, utils.rs
Binary fails on any machine except the original developer's. Absolute `/home/eransa/...` path baked in. Use `CLANGD_PATH` env var or `which clangd`.

**3. Blocking I/O on async executor thread** — document.rs, compile_commands.rs
`std::fs::read_to_string` / `std::fs::read_dir` block the Tokio thread. Stalls all concurrent async tasks. Use `tokio::fs`.

**4. Unbounded channels — no backpressure** — runner.rs, loader.rs
All MPSC channels are unbounded. Large workspace symbol results can grow memory without limit → OOM.

### High — Correctness or scalability problems

**7. Random UUID `SymbolId` — no deduplication** — symbols.rs
Rediscovering the same function creates a new ID → duplicates in graph. Need content-addressable ID.

**8. Silent channel send failures** — throughout loader.rs
`let _ = tx.send(...)` silently discards errors. When receiver drops, sender loops indefinitely burning CPU.

### Medium — Architectural debt and maintainability

**10. God object: `App` (23 fields)** — mod.rs
Mixes graph, workspace, search, LSP channels, loading, and UI state. Untestable. Extract `LspBridge`, `WorkspaceManager`.

**11. Sync TUI loop masquerading as async** — tui.rs
`event::poll` + `event::read` block the main thread inside an `async fn`. Works only because LSP is spawned separately. Fragile.

### Low-Medium — Hygiene, testing, performance

**18. Zero unit tests for LSP service layer** — `service/worker.rs`, `service/request.rs`, all `service/response/*.rs`
The complex enhanced-references state machine with pending hovers is entirely untested.

**19. `Box::leak` in tests** — call_graph_view.rs, function_list.rs
Permanent memory leak to satisfy `'static` lifetimes. Restructure the API instead.

**20. Flaky logging tests** — logging.rs
Two tests both call `fern::Dispatch::apply()` which can only succeed once globally. Order-dependent failures.
