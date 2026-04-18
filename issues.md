Here's the consolidated ranking, re-evaluated by real-world impact:

---

### Critical — App is broken or leaks resources every run

**1. Hardcoded developer path** — mod.rs, utils.rs
Binary fails on any machine except the original developer's. Absolute `/home/eransa/...` path baked in. Use `CLANGD_PATH` env var or `which clangd`.

**2. Child process never killed** — mod.rs
No `Drop` on `LspClient`. Clangd orphaned after every run. Reader task `JoinHandle` never aborted. LSP shutdown/exit protocol never sent.

**3. Blocking I/O on async executor thread** — document.rs, compile_commands.rs
`std::fs::read_to_string` / `std::fs::read_dir` block the Tokio thread. Stalls all concurrent async tasks. Use `tokio::fs`.

**4. Unbounded channels — no backpressure** — runner.rs, loader.rs
All MPSC channels are unbounded. Large workspace symbol results can grow memory without limit → OOM.

---

### High — Correctness or scalability problems

**5. O(n²) graph construction and O(n) lookups** — graph.rs
`add_call` scans all edges for dedup; `get_callers`/`get_callees` do full linear scans. Quadratic for large codebases. Need adjacency map.

**6. `resolve_symbol_at_location` returns fake data** — parsing_impl.rs
Stub returns fabricated names like `"caller_at_main:5"`. Users see incorrect symbols. Either implement or remove.

**7. Random UUID `SymbolId` — no deduplication** — symbols.rs
Rediscovering the same function creates a new ID → duplicates in graph. Need content-addressable ID.

**8. Silent channel send failures** — throughout loader.rs
`let _ = tx.send(...)` silently discards errors. When receiver drops, sender loops indefinitely burning CPU.

**9. Non-deterministic sibling navigation** — graph_view.rs
`HashMap::keys()` iteration order changes between runs. Next/previous sibling is random.

---

### Medium — Architectural debt and maintainability

**10. God object: `App` (23 fields)** — mod.rs
Mixes graph, workspace, search, LSP channels, loading, and UI state. Untestable. Extract `LspBridge`, `WorkspaceManager`.

**11. Sync TUI loop masquerading as async** — tui.rs
`event::poll` + `event::read` block the main thread inside an `async fn`. Works only because LSP is spawned separately. Fragile.

**12. Massive code duplication** — call_hierarchy.rs, utils.rs / symbol_resolution.rs / document.rs
Triple-duplicated utility functions. Four near-identical response handlers. Bugs fixed in one copy missed in others.

**13. `WorkspaceSymbolInfo` defined twice with different fields** — types.rs vs lazy_graph.rs
Same name, different structs, different crates. Causes confusion and unnecessary conversions.

**14. Swapped arrow key semantics** — tui.rs
`Up → MoveDown`, `Down → MoveUp`. Contradicts cursor bindings elsewhere. Confusing even if intentional for panning.

---

### Low-Medium — Hygiene, testing, performance

**15. Excessive `info!`-level logging** — loader.rs, response/references.rs
Every LSP reference location logged at `info!`. Massive log files, I/O overhead. Should be `debug!`/`trace!`.

**16. `result.clone()` on every JSON parse** — parsing.rs, call_hierarchy.rs
`serde_json::from_value` consumes the value, but code clones first. Double allocation on every LSP response.

**17. Dead code behind `#[allow(dead_code)]`** — lib.rs, lazy_graph.rs (~280 lines), large parts of `logic` crate
Whole modules suppressed. `LazyCallGraph` never used. `logic` crate mostly unused by TUI.

**18. Zero unit tests for LSP service layer** — `service/worker.rs`, `service/request.rs`, all `service/response/*.rs`
The complex enhanced-references state machine with pending hovers is entirely untested.

**19. `Box::leak` in tests** — call_graph_view.rs, function_list.rs
Permanent memory leak to satisfy `'static` lifetimes. Restructure the API instead.

**20. Flaky logging tests** — logging.rs
Two tests both call `fern::Dispatch::apply()` which can only succeed once globally. Order-dependent failures.
