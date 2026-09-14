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

A whole call graph is unreadable by default: rooting at `main` in the demo reaches 56 nodes. Four mechanisms keep it legible, and they are the first things to reach for before adding more visual weight:

| Mechanism | Where |
|---|---|
| **Depth limit** (`Command::SetGraphDepth`, per workspace, default 3) | `core::layout` stops the BFS |
| **Collapse** (`Command::ToggleCollapsed`, per workspace) | `core::layout` stops descending; client drops the node's outgoing edges |
| **Expand** (`Command::ExpandFunction`, per workspace) | `core::layout` walks one level *past* the limit at those nodes |
| **Hover focus** — fade everything non-adjacent | `app.js`, transient |

Nodes hiding children are dashed and labelled `+N`. Auto-fit is clamped at both ends (`MAX_FIT_ZOOM` / `MIN_FIT_ZOOM`): a small graph must not render at cartoon size, and a large one must not shrink past readable — below the floor it holds the zoom and anchors on the focused node so the user pans.

Design rules worth keeping:

- **Sans for chrome, mono for code.** Function names and namespaces get `--font-mono`; every label, tab and button gets `--font-ui`. Reaching for monospace everywhere is what made this read as a terminal.
- **One accent.** State is carried by accent strength and a left/top accent bar, not by a different hue per state.
- **Nodes sit on a fixed grid.** `core::compute_layout` lays out in the same pixels the canvas draws in and returns node **centres**, so the client uses positions verbatim. Graph nodes use a fixed `NODE_W` with truncated labels; label-sized nodes overlap their neighbours as soon as a C++ symbol name gets long. Keep `NODE_W`/`NODE_H` in `app.js` in step with `NODE_WIDTH`/`NODE_HEIGHT` in `crates/core/src/layout.rs`.
- **Orientation is a transpose, not a `grid` setting.** `grid::TreeOrientation` exists but the Sugiyama pass ignores it: levels always advance along x. `core::layout` gets top-down by swapping the node size going in and the coordinates coming out, so both orientations share one code path. The *spacing* is not transposed with it: a node is 150×32, so top-down advances levels along the short axis and spreads siblings along the long one, and reusing the left-to-right gutters gives a flat stripe. Top-down has its own pair (`*_TOPDOWN`) — generous between levels, tighter between siblings. Per workspace, via `Command::SetGraphOrientation`. The toolbar shows it as a segmented control — both options visible, the live one lit — because a single pill whose label is the current state reads as a command that does the opposite of what it says.
- **Levels stretch to fill the canvas.** `grid` packs each axis as tightly as its separations allow, which leaves a wide sibling fan as a band with the levels almost touching and half the screen empty. `layout::balance_aspect` scales the level axis — uniformly, so it is just a bigger pitch and nothing can collide — until the bounding box reaches `TARGET_ASPECT`, capped at `MAX_LEVEL_STRETCH` because past a point spreading levels only means panning further to follow one edge.
- **`grid` spacing is pitch, not gutter.** `level_separation` and `node_separation` are full centre-to-centre distances, not gaps added to the node's size. `core::layout` derives them as `size + gutter` explicitly, because the crate's terminal defaults (`node_width: 20`, `level_separation: 4`) make the distinction easy to miss — and getting it wrong overlaps every node.
- **A click points; it does not act.** Tapping a node sends `Command::FocusFunction`, which moves `selected_function` and emits nothing at all — the client lights the border and moves the breadcrumb itself, with no rebuild. Every event routes to a snapshot and every snapshot tears the canvas down and rebuilds it, which is far too much for saying "this one". Loading a node's calls is the hover `+` button, and hiding them is a double-click. Exactly one node is selected at a time, and it wears no fill at all: the ground stays the common surface and the selection is carried by a bolder ring — `--accent-hover` at 3px against the root's `--accent` at 2px. A second ring of the *same* weight would read as a second selection, which is why the root→selection path is drawn on its edges alone.
- **Expanding appends; it never re-roots.** Asking for one node's calls adds that node to the workspace's `expanded` set, and the layout BFS — still rooted where it was — walks one level further there. It used to move the BFS origin onto the expanded node instead, which redrew the whole graph around it and dropped every sibling branch. For the same reason the client's auto-fit key is the *root*, not the focused node: appending must not move the viewport the user positioned.
- **This is a DAG, not a tree.** Collapsing a node does not necessarily remove its children from the canvas: another caller may still reach them. Collapse therefore also drops the edges *leaving* the collapsed node, client-side, so the action is always visible.
- Graph styles are read from the CSS custom properties at startup, so the canvas and the chrome stay on one palette.

The desktop window is **undecorated** (`decorations: false`); `#titlebar` is the titlebar and carries the drag region, workspace tabs and window controls. To go back to native decorations, flip that flag in `tauri.conf.json` and the `is-desktop` window controls will hide themselves.

### Data Flow

`main.rs` has two paths:

1. **Demo mode** (`cargo run`): builds a synthetic `CallGraph` and passes it directly to `run_tui`.
2. **LSP mode** (`cargo run -- <path>`): spins up a background `lsp_loader_task` that connects to clangd, opens every source file in the compilation database, and builds a `CallGraph` via workspace symbol queries. Those documents do not stay open — see **Open documents are a cache, not the project** below. The frontend starts immediately while loading proceeds asynchronously. A Tokio MPSC bridge forwards `LspRequest` messages to `LspService` and returns `LspResponse`/`LspUiMessage` results back. `core::boot::spawn_lsp_bridge` performs this wiring for every frontend.

The web and desktop backends both poll `CoreApp::tick()` every 100ms and translate the resulting events through `core::protocol::EventRouter`, which decides what to forward verbatim and when to push a fresh snapshot.

### Key Design Patterns

- **Async message passing**: Tokio MPSC channels connect the LSP loader, the LSP service worker, and the TUI — they never share mutable state directly.
- **`CallGraphAdapter`**: Wraps `CallGraph` to abstract call direction (incoming vs. outgoing) so graph traversal and rendering are direction-agnostic.
- **Lazy call hierarchy**: Initial load fetches workspace symbols; per-function call hierarchy is fetched on demand when the user selects a function.
- **Open documents are a cache, not the project.** clangd's memory is dominated by what it has *open*, not by the project's size: each open document costs a live AST worker and a resident preamble index. The preload exists only to make clangd load `compile_commands.json` and start indexing, so `LspService` keeps at most `OPEN_DOCUMENT_LIMIT` documents open (LRU, `service::document`) and the loader closes the rest once the initial symbol sweep lands. Everything after that is served either from clangd's index (`workspace/symbol`) or from a document `ensure_document_opened` reopens on demand — which is why the limit must stay comfortably above `core`'s `WARMUP_IN_FLIGHT`: evicting a document with a request still outstanding answers it with "trying to get AST for non-added document". On jemalloc (217 TUs) this is the difference between ~3.2 GB and ~1.7 GB; what is left is the background indexer, which no amount of closing touches.
- **The discovery query waits for the index.** `workspace/symbol` is answered from clangd's background index exactly as it stands the instant the request arrives — clangd never blocks on the indexer — so a query sent early describes a *sample* of the project. It is capped as well: `--limit-results` defaults to 100, and clangd applies that before `handle_workspace_symbols_response` filters the reply down to functions in project files, so on jemalloc the same 100 results came back as anywhere between 15 and 57 functions, and since those are the only seeds the warmup ever expands, the graph came out a different size on every run (481 to 585 nodes over the same unchanged tree). Both halves have to be fixed: clangd is spawned with `--limit-results=0`, and `loader::await_background_index` holds the query until the indexer reports itself done — which took ~2,700 functions and ~3,300 nodes where it used to take 15. That report exists only because `initialize` sets `window.workDoneProgress`; without it clangd marks its background-index progress `Unsupported` and sends no `$/progress` at all, which is what the two `sleep(500ms)`s in the loader were standing in for. Three things about the wait are not optional: `window/workDoneProgress/create` must be answered or clangd reports nothing; an `end` is believed only after `INDEX_SETTLE` of quiet, because the queue drains between batches of the preload's 217 `didOpen`s and an `end` mid-preload is a lull, not the finish; and every wait in the loader has to notice an `end` itself, because they all drain the one response channel and a progress message read by one is gone from the next. When `INDEX_WAIT_TIMEOUT` expires first the query goes out against a partial index anyway — a partial graph beats a blank one while nothing is interactive — and `run_tui_lsp_bridge` repeats it once indexing finishes, which lands as an append because `CoreApp` ingests workspace symbols whether or not it issued the request.

- **Edge warmup**: workspace symbols carry no edges, so a freshly loaded project is a heap of isolated nodes — and `logic::cluster` drops isolated symbols, so the clustering comes out empty and the cluster map the app opens on has nothing to draw. Once the load reports `Completed`, `CoreApp::pump_edge_warmup` walks the loaded symbols through the same prepare/follow-up path a manual expand uses, `WARMUP_IN_FLIGHT` at a time. The queue is fed, not seeded: `Completed` is not the instant symbols stop arriving — the loader's discovery reply can land after it times out, and Ctrl+R fetches more whenever the user asks — so every symbol from a *query* enqueues itself through `CoreApp::ingest_symbol`. It is not cascaded into the callees a reply discovers: following those would walk the whole program, which is why a loaded project holds far more functions than the warmup ever claims. While it runs, `maybe_recluster` drops back to the settling timer — `Completed` otherwise means "nothing more is coming", which during warmup would be a Louvain pass every tick. It also reports itself as `AppEvent::ResolvingCalls { done, total }`, because `LspProgress(Completed)` is not the end of the wait and a frontend that showed only the phase announced "ready" over a graph still growing edges.
- **A background fetch is not a redraw.** Every request carries a `RequestOrigin`, and only `User` answers emit `AppEvent::LayoutDirty` — which is what every out-of-process frontend turns into a full snapshot. The warmup used to answer with one per reply: on jemalloc that was 767 whole-graph snapshots in 110 seconds of loading, of which exactly one changed the screen, because the map the app opens on has no root and draws no call edges at all. The client answers a snapshot by tearing the canvas down and re-rendering a sidebar holding every symbol in the project, so this was the lag. Nothing is lost by the silence: the graph still grows, and the map still catches up through the regrouping `maybe_recluster`'s settling timer starts. The origin has to survive the prepare → follow-up hop, since the second half is the one that carries the edges. `EventRouter` holds a second line of defence, rate-limiting snapshots to `LOADING_SNAPSHOT_INTERVAL` while `ResolvingCalls` says the warmup is running — holding one back, never dropping it, and releasing it the moment the warmup drains.
- **The snapshot ships what the client reads, and borrows it.** `core::protocol::WireGraph` is not `CallGraph`: the client never touches a node's `references` or `diagnostics`, nor an edge's `call_location`, and those cost more than everything it does read put together — 4.63 MB of a 5.97 MB jemalloc snapshot was call sites dropped on the floor. Edges go out one per caller/callee pair rather than one per call site, because that is what the client was already reducing them to on arrival. Same graph, 2.05 MB instead of 5.97 MB. It borrows the app rather than owning a copy, which is why `snapshot_of` returns a `ServerMsg<'_>` and both backends serialise it while still holding the lock instead of handing the message around. Field names match what `app.js` already reads; shortening them would save a little more and cost the client a rewrite.
- **Loading is reported in the status bar, and only there.** It has two stages — clangd handing over symbols (`LspProgress`), then their edges (`ResolvingCalls`) — which the client folds into one `load` object in `app.js`. That object exists to keep the one status line honest: the transient events used to fight over it, and a warmup reply would set it to "Ready" while the project was still indexing. A stage carrying a total reads as a fraction, one without as a running count, since clangd streams workspace symbols without ever saying how many are coming. The TUI shows the same two stages in its own status bar, reading `CoreApp::warmup_progress()` directly.
- **The cluster map runs one way.** Louvain maximises modularity, which is blind to direction — `build_base_graph` hands it an *undirected* copy of the call graph — so a community is free to straddle a one-way chain. Group `a` and `c` but not the `b` between them and the map draws both `A -> B` and `B -> A`, which reads as "these call each other" over code where nothing of the sort happens: on jemalloc's `test_malloc_conf_2` neighbourhood that was 39 mutual pairs, three quarters of the edges, against a call graph that is a DAG apart from one 281-function component. `logic::cluster::make_convex` fixes the partition rather than the drawing, because a partition's quotient is acyclic **iff** every community is *convex* — everything on a path between two members is a member. Two consequences follow and neither is a choice: mutually recursive functions share a cluster (a component split across two makes them call each other by definition), and the hierarchy above the finest level is built by `agglomerate_acyclically` merging under the constraint, not by Louvain's own aggregation — which grouped that same undirected copy, so repairing its levels means cutting groupings formed across the cuts just made. The guarantee costs width: 19 top-level clusters became 32.

- **`grid` crate**: provides the tree layout algorithm used by `GraphView` and by `core::layout::compute_layout`.
- **One protocol, many pipes**: `core::protocol` is the single definition of what a frontend sees. Adding a fourth frontend should mean writing a transport, not a protocol.

### Gotcha: the crate named `core`

The workspace crate `core` shadows Rust's built-in `core`. Crates that use macros expanding to `::core::…` paths (Tauri does) must alias it:

```toml
coon_core = { package = "core", path = "../core" }
```

`crates/desktop` does this. Others refer to it as `::core::`.
