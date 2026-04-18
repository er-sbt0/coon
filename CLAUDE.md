# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build
cargo build --release

# Run
cargo run                        # Demo mode (no clangd needed)
cargo run -- /path/to/project   # LSP mode (analyzes a real C/C++ project via clangd)

# Test
cargo test                       # All tests
cargo test -p lsp_integration    # LSP crate only (requires clangd in PATH)
cargo test -p logic              # Graph logic only
cargo test <test_name>           # Single test by name pattern
```

LSP integration tests require `clangd` to be installed and in `PATH`. See `lsp_integration/TESTING.md` for details on writing integration tests.

Logs are written to `logs/` directory.

## Architecture

COON is a Rust workspace (4 crates) that visualizes C/C++ call graphs in a TUI, using `clangd` as the analysis backend.

### Crates

- **`model`** — Shared types: `CallGraph`, `FunctionNode`, `CallEdge`, `SymbolId`, LSP progress/status enums.
- **`lsp_integration`** — All clangd communication: `LspClient` (raw protocol), `LspService` (higher-level async service with request/response handling, document caching, symbol resolution).
- **`logic`** — Pure graph algorithms (BFS traversal, path-finding, reachability) operating on `CallGraph`.
- **`tui_ui`** — Ratatui TUI: `TuiApp` (main state/event loop), `GraphView`/`GraphViewState` (tree rendering via the external `grid` crate), search bar, workspace management.

### Data Flow

`main.rs` has two paths:

1. **Demo mode** (`cargo run`): builds a synthetic `CallGraph` and passes it directly to `run_tui`.
2. **LSP mode** (`cargo run -- <path>`): spins up a background `lsp_loader_task` that connects to clangd, opens all source files, and builds a `CallGraph` via workspace symbol queries. The TUI starts immediately while loading proceeds asynchronously. A Tokio MPSC bridge forwards `LspRequest` messages from the TUI to `LspService` and returns `LspResponse`/`LspUiMessage` results back.

### Key Design Patterns

- **Async message passing**: Tokio MPSC channels connect the LSP loader, the LSP service worker, and the TUI — they never share mutable state directly.
- **`CallGraphAdapter`**: Wraps `CallGraph` to abstract call direction (incoming vs. outgoing) so graph traversal and rendering are direction-agnostic.
- **Lazy call hierarchy**: Initial load fetches workspace symbols; per-function call hierarchy is fetched on demand when the user selects a function.
- **`grid` crate** (sibling at `../grid`): external dependency providing the tree layout algorithm used by `GraphView`.
