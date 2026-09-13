# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build
cargo build --release

# Run
cargo run                             # TUI, demo mode (no clangd needed)
cargo run -- /path/to/project          # TUI, LSP mode (real C/C++ project via clangd)
cargo run -- --web 8080                # Web frontend, demo mode
cargo run -- /path/to/project --web 8080   # Web frontend, LSP mode

cargo run -p desktop                   # Desktop app, demo mode
cargo run -p desktop -- /path/to/project   # Desktop app, LSP mode

# Test
cargo test                       # All tests
cargo test -p lsp                # LSP crate only (requires clangd in PATH)
cargo test -p logic              # Graph logic only
cargo test <test_name>           # Single test by name pattern
```

LSP integration tests require `clangd` to be installed and in `PATH`. See `lsp/TESTING.md` for details on writing integration tests.

Logs are written to `logs/` directory.

## Architecture

COON is a Rust workspace that visualizes C/C++ call graphs, using `clangd` as the analysis backend. It ships **three frontends** over one shared application core.

### Crates

- **`model`** — Shared types: `CallGraph`, `FunctionNode`, `CallEdge`, `SymbolId`, LSP progress/status enums.
- **`lsp`** — All clangd communication: `LspClient` (raw protocol), `LspService` (higher-level async service with request/response handling, document caching, symbol resolution).
- **`logic`** — Pure graph algorithms (BFS traversal, path-finding, reachability) operating on `CallGraph`.
- **`core`** — Frontend-agnostic application core. `CoreApp` (owns `CallGraph`, `LspBridge`, `CoreWorkspaceManager`) exposes `dispatch(Command) -> Vec<AppEvent>` and `tick() -> Vec<AppEvent>`. Also holds `layout::compute_layout` (node positions), `protocol` (the shared wire format), `boot::spawn_lsp_bridge`, `demo`, and `logging`.
- **`grid`** — Tree/DAG layout algorithm (a workspace crate, used by `core::layout` and the TUI).
- **`coon`** — The `coon` binary: CLI parsing, and the TUI + web runners.
- **`tui`** — Ratatui TUI: `TuiApp` (main state/event loop), `GraphView`/`GraphViewState`, search bar, workspace management.
- **`web`** — Axum server: WebSocket at `/ws`, client assets embedded via `rust-embed`.
- **`desktop`** — The `coon-desktop` binary: Tauri v2 shell hosting the same client as `web`.

### Frontends

| Frontend | Entry point | Transport to `CoreApp` |
|---|---|---|
| TUI | `coon` (default) | Direct calls — no serialisation |
| Web | `coon --web <PORT>` | JSON over WebSocket |
| Desktop | `coon-desktop` | The same JSON over Tauri IPC |

The web and desktop frontends **share one JavaScript client** (`crates/web/static`). They differ only in `transport.js`, which feature-detects `window.__TAURI__` and picks either a WebSocket or Tauri `invoke`/`listen`. Because both backends emit identical `core::protocol::ServerMsg` shapes, `app.js` needs no knowledge of which one it is talking to. Keep it that way: put shared behaviour in `app.js`, and anything pipe-specific (including window management, via `CoonHost`) in `transport.js`.

Client assets are vendored, not CDN-loaded (`static/vendor/cytoscape.min.js`), because the desktop app must work offline.

### Client files

| File | Holds |
|---|---|
| `theme.css` | Design tokens only — colours, type scale, 4px spacing grid, shell metrics. Light and dark. |
| `app.css` | Component styles. Reads tokens; defines no raw colours. |
| `transport.js` | The pipe (WebSocket vs Tauri IPC) and `CoonHost` window controls. The only file that touches `__TAURI__`. |
| `palette.js` | The command palette / fuzzy picker. Generic: callers hand it items and get a pick callback. Knows nothing about commands or symbols. |
| `app.js` | Everything else: tabs, graph, sidebar, breadcrumb, status bar, commands, keybindings. |

`Ctrl+Shift+P` opens the command palette, `Ctrl+P` the symbol finder. **New user-facing actions belong in `buildCommands()` in `app.js`, not as another toolbar button** — that's how the commands `core` exposes stay reachable without the chrome growing a control each time.

### Readability at scale

A whole call graph is unreadable by default: rooting at `main` in the demo reaches 56 nodes. Three mechanisms keep it legible, and they are the first things to reach for before adding more visual weight:

| Mechanism | Where |
|---|---|
| **Depth limit** (`Command::SetGraphDepth`, per workspace, default 3) | `core::layout` stops the BFS |
| **Collapse** (`Command::ToggleCollapsed`, per workspace) | `core::layout` stops descending; client drops the node's outgoing edges |
| **Hover focus** — fade everything non-adjacent | `app.js`, transient |

Nodes hiding children are dashed and labelled `+N`. Auto-fit is clamped at both ends (`MAX_FIT_ZOOM` / `MIN_FIT_ZOOM`): a small graph must not render at cartoon size, and a large one must not shrink past readable — below the floor it holds the zoom and anchors on the focused node so the user pans.

Design rules worth keeping:

- **Sans for chrome, mono for code.** Function names and namespaces get `--font-mono`; every label, tab and button gets `--font-ui`. Reaching for monospace everywhere is what made this read as a terminal.
- **One accent.** State is carried by accent strength and a left/top accent bar, not by a different hue per state.
- **Nodes sit on a fixed grid.** `core::compute_layout` lays out in the same pixels the canvas draws in and returns node **centres**, so the client uses positions verbatim. Graph nodes use a fixed `NODE_W` with truncated labels; label-sized nodes overlap their neighbours as soon as a C++ symbol name gets long. Keep `NODE_W`/`NODE_H` in `app.js` in step with `NODE_WIDTH`/`NODE_HEIGHT` in `crates/core/src/layout.rs`.
- **Orientation is a transpose, not a `grid` setting.** `grid::TreeOrientation` exists but the Sugiyama pass ignores it: levels always advance along x. `core::layout` gets top-down by swapping the separations going in and the coordinates coming out, so both orientations share one code path. Per workspace, via `Command::ToggleGraphOrientation`.
- **`grid` spacing is pitch, not gutter.** `level_separation` and `node_separation` are full centre-to-centre distances, not gaps added to the node's size. `core::layout` derives them as `size + gutter` explicitly, because the crate's terminal defaults (`node_width: 20`, `level_separation: 4`) make the distinction easy to miss — and getting it wrong overlaps every node.
- **This is a DAG, not a tree.** Collapsing a node does not necessarily remove its children from the canvas: another caller may still reach them. Collapse therefore also drops the edges *leaving* the collapsed node, client-side, so the action is always visible.
- Graph styles are read from the CSS custom properties at startup, so the canvas and the chrome stay on one palette.

The desktop window is **undecorated** (`decorations: false`); `#titlebar` is the titlebar and carries the drag region, workspace tabs and window controls. To go back to native decorations, flip that flag in `tauri.conf.json` and the `is-desktop` window controls will hide themselves.

### Data Flow

`main.rs` has two paths:

1. **Demo mode** (`cargo run`): builds a synthetic `CallGraph` and passes it directly to `run_tui`.
2. **LSP mode** (`cargo run -- <path>`): spins up a background `lsp_loader_task` that connects to clangd, opens all source files, and builds a `CallGraph` via workspace symbol queries. The frontend starts immediately while loading proceeds asynchronously. A Tokio MPSC bridge forwards `LspRequest` messages to `LspService` and returns `LspResponse`/`LspUiMessage` results back. `core::boot::spawn_lsp_bridge` performs this wiring for every frontend.

The web and desktop backends both poll `CoreApp::tick()` every 100ms and translate the resulting events through `core::protocol::EventRouter`, which decides what to forward verbatim and when to push a fresh snapshot.

### Key Design Patterns

- **Async message passing**: Tokio MPSC channels connect the LSP loader, the LSP service worker, and the TUI — they never share mutable state directly.
- **`CallGraphAdapter`**: Wraps `CallGraph` to abstract call direction (incoming vs. outgoing) so graph traversal and rendering are direction-agnostic.
- **Lazy call hierarchy**: Initial load fetches workspace symbols; per-function call hierarchy is fetched on demand when the user selects a function.
- **`grid` crate**: provides the tree layout algorithm used by `GraphView` and by `core::layout::compute_layout`.
- **One protocol, many pipes**: `core::protocol` is the single definition of what a frontend sees. Adding a fourth frontend should mean writing a transport, not a protocol.

### Gotcha: the crate named `core`

The workspace crate `core` shadows Rust's built-in `core`. Crates that use macros expanding to `::core::…` paths (Tauri does) must alias it:

```toml
coon_core = { package = "core", path = "../core" }
```

`crates/desktop` does this. Others refer to it as `::core::`.
