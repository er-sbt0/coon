# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Scope — where work goes

Read this before editing. The details are in **Architecture** below; this is the rule.

| Path | Status |
|---|---|
| `crates/desktop` | The frontend. Work here. |
| `crates/web/static` | The shared JavaScript client — the desktop app ships it. Work here. |
| `crates/core`, `model`, `lsp`, `logic`, `grid` | The shared core. Work here. |
| `crates/tui`, `crates/coon`, `crates/web/src` | **Deprecated. Do not do new work in these.** |

Deprecated means kept working, not frozen out. In those three paths:

- **Do** fix them when a change to `core` or `protocol` breaks their build, and
  fix a test they catch. A break there is a change to fix, not a reason to
  delete the crate. They are still workspace members and still tested.
- **Do not** port a desktop feature across, refactor, clean up, or improve them
  in passing. A feature belongs in the desktop app and the shared client and is
  not expected to reach the other two.

If a task asks for work in a deprecated path, check whether it actually wants
the desktop app, and say so rather than building it twice. Each of those
directories carries its own `CLAUDE.md` repeating this where you will land.

## Commands

```bash
# Build
cargo build
cargo build --release

# Run — `cargo run` is the desktop app; `-p desktop` is the same thing spelled out
cargo run                              # Desktop app, demo mode (no clangd needed)
cargo run -- /path/to/project          # Desktop app, LSP mode (real C/C++ project via clangd)

# Deprecated frontends — `coon` is outside default-members, so name it
cargo run -p coon                              # TUI, demo mode
cargo run -p coon -- /path/to/project          # TUI, LSP mode
cargo run -p coon -- --web 8080                # Web frontend, demo mode
cargo run -p coon -- /path/to/project --web 8080   # Web frontend, LSP mode

# Test
cargo test                       # All tests
cargo test -p lsp                # LSP crate only (requires clangd in PATH)
cargo test -p logic              # Graph logic only
cargo test <test_name>           # Single test by name pattern
```

LSP integration tests require `clangd` to be installed and in `PATH`. See `lsp/TESTING.md` for details on writing integration tests.

`LspClient::new` runs `clangd` from `PATH`; set `COON_CLANGD` to the full path of a
particular binary to use that one instead. If `COON_CLANGD` is set but names nothing
executable, startup fails rather than falling back to `PATH` — picking a different
clangd than the one asked for is how an index ends up quietly wrong. Either way the
choice, and where it came from, is logged at info on startup.

Logs are written to `logs/` directory.

## Architecture

COON is a Rust workspace that visualizes C/C++ call graphs, using `clangd` as the analysis backend. It ships **three frontends** over one shared application core — the desktop app, and the TUI and web server it replaced.

### Crates

- **`model`** — Shared types: `CallGraph`, `FunctionNode`, `CallEdge`, `SymbolId`, LSP progress/status enums.
- **`lsp`** — All clangd communication: `LspClient` (raw protocol), `LspService` (higher-level async service with request/response handling, document caching, symbol resolution).
- **`logic`** — Pure graph algorithms (BFS traversal, path-finding, reachability) operating on `CallGraph`.
- **`core`** — Frontend-agnostic application core. `CoreApp` (owns `CallGraph`, `LspBridge`, `CoreWorkspaceManager`) exposes `dispatch(Command) -> Vec<AppEvent>` and `tick() -> Vec<AppEvent>`. Also holds `layout::compute_layout` (node positions), `protocol` (the shared wire format), `boot::spawn_lsp_bridge`, `demo`, and `logging`.
- **`grid`** — Tree/DAG layout algorithm (a workspace crate, used by `core::layout` and the TUI).
- **`desktop`** — The `coon-desktop` binary: Tauri v2 shell hosting the client in `crates/web/static`. **The frontend**, and the binary a bare `cargo run` launches.
- **`coon`** — *Deprecated.* The `coon` binary: CLI parsing, and the TUI + web runners.
- **`tui`** — *Deprecated.* Ratatui TUI: `TuiApp` (main state/event loop), `GraphView`/`GraphViewState`, search bar, workspace management.
- **`web`** — *Deprecated server, live client.* Axum server: WebSocket at `/ws`, client assets embedded via `rust-embed`. The Rust is what is deprecated; `static/` is the shared client and the desktop app's `frontendDist`.

### Frontends

| Frontend | Entry point | Transport to `CoreApp` | Status |
|---|---|---|---|
| Desktop | `coon-desktop` (default) | JSON over Tauri IPC | Where frontend work goes |
| Web | `coon --web <PORT>` | The same JSON over a WebSocket | Deprecated |
| TUI | `coon` | Direct calls — no serialisation | Deprecated |

**The desktop app is the default; the TUI and the web server are deprecated.** Deprecated means kept working, not frozen out: both still build, still run, are still covered by the workspace's tests, and both still have to compile after a change to `core` or `protocol` — a change that breaks them is a change to fix, not a reason to delete them. What stops is new work: a feature belongs in the desktop app and the shared client, and is not expected to reach the other two. Neither is `#[deprecated]` as an attribute, because `coon` is their only caller and is deprecated itself, so the warnings would land nowhere that does not already say so. The notice a user sees is `main::warn_deprecated`, printed to stderr before the TUI takes the terminal.

Two consequences in the manifests, both deliberate: `crates/desktop` is in `default-members` and `crates/coon` is not — `cargo run` picks a binary from the default set and refuses if there are two — which is also why a bare `cargo build`/`cargo test` now compiles the Tauri tree it used to skip. And `crates/web/static` is not deprecated with the crate around it: it is the client the desktop app ships, so client work goes there exactly as before.

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

`Ctrl+Shift+P` opens the command palette, `Ctrl+P` the symbol finder. **New user-facing actions belong in `buildCommands()` in `app.js`, not as another toolbar button** — that's how the commands `core` exposes stay reachable without the chrome growing a control each time. The toolbar's four controls are the exception that defines the rule: depth, direction, orientation, and the map's zoom and method are *settings whose current value has to be visible*, not actions. A palette entry can invoke a setting but cannot show you what it is, and all five of these are things a reader of the canvas needs to know to read it. Everything else goes in the palette, and each of these is in the palette too.

### Readability at scale

A whole call graph is unreadable by default: rooting at `main` in the demo reaches 56 nodes. Four mechanisms keep it legible, and they are the first things to reach for before adding more visual weight:

| Mechanism | Where |
|---|---|
| **Depth limit** (`Command::SetGraphDepth`, per workspace, default 3) | `core::layout` stops the BFS |
| **Collapse** (`Command::ToggleCollapsed`, per workspace) | `core::layout` stops descending; client drops the node's outgoing edges |
| **Expand** (`Command::ExpandFunction`, per workspace) | `core::layout` walks one level *past* the limit at those nodes |
| **Hover focus** — fade everything non-adjacent | `app.js`, transient |
| **Map zoom** (`Command::SetMapLevel`, per workspace) — the map's own control, since it has no depth to limit | `logic::cluster` builds the rungs; `core::layout` draws one |

Nodes hiding children are dashed and labelled `+N`. Auto-fit is clamped at both ends (`MAX_FIT_ZOOM` / `MIN_FIT_ZOOM`): a small graph must not render at cartoon size, and a large one must not shrink past readable — below the floor it holds the zoom and anchors on the focused node so the user pans.

Design rules worth keeping:

- **Sans for chrome, mono for code.** Function names and namespaces get `--font-mono`; every label, tab and button gets `--font-ui`. Reaching for monospace everywhere is what made this read as a terminal.
- **One accent.** State is carried by accent strength and a left/top accent bar, not by a different hue per state.
- **Nodes sit on a fixed grid.** `core::compute_layout` lays out in the same pixels the canvas draws in and returns node **centres**, so the client uses positions verbatim. Graph nodes use a fixed `NODE_W` with truncated labels; label-sized nodes overlap their neighbours as soon as a C++ symbol name gets long. Keep `NODE_W`/`NODE_H` in `app.js` in step with `NODE_WIDTH`/`NODE_HEIGHT` in `crates/core/src/layout.rs`.
- **Orientation is a transpose, not a `grid` setting.** `grid::TreeOrientation` exists but the Sugiyama pass ignores it: levels always advance along x. `core::layout` gets top-down by swapping the node size going in and the coordinates coming out, so both orientations share one code path. The *spacing* is not transposed with it: a node is 150×32, so top-down advances levels along the short axis and spreads siblings along the long one, and reusing the left-to-right gutters gives a flat stripe. Top-down has its own pair (`*_TOPDOWN`) — generous between levels, tighter between siblings. Per workspace, via `Command::SetGraphOrientation`. The toolbar shows it as a segmented control — both options visible, the live one lit — because a single pill whose label is the current state reads as a command that does the opposite of what it says.
- **Levels stretch to fill the canvas.** `grid` packs each axis as tightly as its separations allow, which leaves a wide sibling fan as a band with the levels almost touching and half the screen empty. `layout::balance_aspect` scales the level axis — uniformly, so it is just a bigger pitch and nothing can collide — until the bounding box reaches `TARGET_ASPECT`, capped at `MAX_LEVEL_STRETCH` because past a point spreading levels only means panning further to follow one edge.
- **`grid` spacing is pitch, not gutter.** `level_separation` and `node_separation` are full centre-to-centre distances, not gaps added to the node's size. `core::layout` derives them as `size + gutter` explicitly, because the crate's terminal defaults (`node_width: 20`, `level_separation: 4`) make the distinction easy to miss — and getting it wrong overlaps every node.
- **A click points; it does not act.** Tapping a node sends `Command::FocusFunction`, which moves `selected_function` and emits nothing at all — the client lights the border and moves the breadcrumb itself, with no rebuild. Every event routes to a snapshot and every snapshot tears the canvas down and rebuilds it, which is far too much for saying "this one". Loading a node's calls is the hover `+` button, and hiding them is a double-click. A node's border carries exactly two states, selected and not: `--accent` at 2px against `--node-border` at 1px, with no fill either way — the ground stays the common surface. Exactly one node is selected at a time. The root wears nothing of its own; it used to take a ring at a lighter weight, which made the border a three-way signal to be measured rather than read, and the breadcrumb says where the tree is rooted anyway. For the same reason the root→selection path is drawn on the edges alone, never by lighting the nodes along it. (Dashed is a separate axis, not a third state: it means a node is hiding children.)
- **Expanding appends; it never re-roots.** Asking for one node's calls adds that node to the workspace's `expanded` set, and the layout BFS — still rooted where it was — walks one level further there. It used to move the BFS origin onto the expanded node instead, which redrew the whole graph around it and dropped every sibling branch. For the same reason the client's auto-fit key is the *root*, not the focused node: appending must not move the viewport the user positioned.
- **This is a DAG, not a tree.** Collapsing a node does not necessarily remove its children from the canvas: another caller may still reach them. Collapse therefore also drops the edges *leaving* the collapsed node, client-side, so the action is always visible.
- Graph styles are read from the CSS custom properties at startup, so the canvas and the chrome stay on one palette.

The desktop window is **undecorated** (`decorations: false`); `#titlebar` is the titlebar and carries the drag region, workspace tabs and window controls. To go back to native decorations, flip that flag in `tauri.conf.json` and the `is-desktop` window controls will hide themselves.

### Data Flow

Every frontend's `main.rs` — `crates/desktop/src/main.rs`, and `crates/coon/src/main.rs` for the deprecated two — has the same two paths, chosen by whether a project path was given:

1. **Demo mode** (`cargo run`): builds a synthetic `CallGraph` and hands it straight to the frontend.
2. **LSP mode** (`cargo run -- <path>`): spins up a background `lsp_loader_task` that connects to clangd, opens every source file in the compilation database, and builds a `CallGraph` via workspace symbol queries. Those documents do not stay open — see **Open documents are a cache, not the project** below. The frontend starts immediately while loading proceeds asynchronously. A Tokio MPSC bridge forwards `LspRequest` messages to `LspService` and returns `LspResponse`/`LspUiMessage` results back. `core::boot::spawn_lsp_bridge` performs this wiring for every frontend.

The web and desktop backends both poll `CoreApp::tick()` every 100ms and translate the resulting events through `core::protocol::EventRouter`, which decides what to forward verbatim and when to push a fresh snapshot.

### Key Design Patterns

- **Async message passing**: Tokio MPSC channels connect the LSP loader, the LSP service worker, and the TUI — they never share mutable state directly.
- **`CallGraphAdapter`**: Wraps `CallGraph` to abstract call direction (incoming vs. outgoing) so graph traversal and rendering are direction-agnostic.
- **Lazy call hierarchy**: Initial load fetches workspace symbols; per-function call hierarchy is fetched on demand when the user selects a function.
- **Open documents are a cache, not the project.** clangd's memory is dominated by what it has *open*, not by the project's size: each open document costs a live AST worker and a resident preamble index. The preload exists only to make clangd load `compile_commands.json` and start indexing, so `LspService` keeps at most `OPEN_DOCUMENT_LIMIT` documents open (LRU, `service::document`) and the loader closes the rest once the initial symbol sweep lands. Everything after that is served either from clangd's index (`workspace/symbol`) or from a document `ensure_document_opened` reopens on demand — which is why the limit must stay comfortably above `core`'s `WARMUP_IN_FLIGHT`: evicting a document with a request still outstanding answers it with "trying to get AST for non-added document". On jemalloc (217 TUs) this is the difference between ~3.2 GB and ~1.7 GB; what is left is the background indexer, which no amount of closing touches.
- **The discovery query waits for the index.** `workspace/symbol` is answered from clangd's background index exactly as it stands the instant the request arrives — clangd never blocks on the indexer — so a query sent early describes a *sample* of the project. It is capped as well: `--limit-results` defaults to 100, and clangd applies that before `handle_workspace_symbols_response` filters the reply down to functions in project files, so on jemalloc the same 100 results came back as anywhere between 15 and 57 functions, and since those are the only seeds the warmup ever expands, the graph came out a different size on every run (481 to 585 nodes over the same unchanged tree). Both halves have to be fixed: clangd is spawned with `--limit-results=0`, and `loader::await_background_index` holds the query until the indexer reports itself done — which took ~2,700 functions and ~3,300 nodes where it used to take 15. That report exists only because `initialize` sets `window.workDoneProgress`; without it clangd marks its background-index progress `Unsupported` and sends no `$/progress` at all, which is what the two `sleep(500ms)`s in the loader were standing in for. Three things about the wait are not optional: `window/workDoneProgress/create` must be answered or clangd reports nothing; an `end` is believed only after `INDEX_SETTLE` of quiet, because the queue drains between batches of the preload's 217 `didOpen`s and an `end` mid-preload is a lull, not the finish; and every wait in the loader has to notice an `end` itself, because they all drain the one response channel and a progress message read by one is gone from the next. When `INDEX_WAIT_TIMEOUT` expires first the query goes out against a partial index anyway — a partial graph beats a blank one while nothing is interactive — and `run_tui_lsp_bridge` repeats it once indexing finishes, which lands as an append because `CoreApp` ingests workspace symbols whether or not it issued the request.

- **Edge warmup**: workspace symbols carry no edges, so a freshly loaded project is a heap of isolated nodes — and `logic::cluster` drops isolated symbols, so the clustering comes out empty and the cluster map the app opens on has nothing to draw. Once the load reports `Completed`, `CoreApp::pump_edge_warmup` walks the loaded symbols through the same prepare/follow-up path a manual expand uses, `WARMUP_IN_FLIGHT` at a time. The queue is fed, not seeded: `Completed` is not the instant symbols stop arriving — the loader's discovery reply can land after it times out, and Ctrl+R fetches more whenever the user asks — so every symbol from a *query* enqueues itself through `CoreApp::ingest_symbol`. It is not cascaded into the callees a reply discovers: following those would walk the whole program, which is why a loaded project holds far more functions than the warmup ever claims. While it runs, `CoreApp::loading` still reports the graph as arriving — `Completed` says the *symbols* have stopped coming, not the edges, and clustering a heap of isolated nodes produces nothing to draw. It also reports itself as `AppEvent::ResolvingCalls { done, total }`, because `LspProgress(Completed)` is not the end of the wait and a frontend that showed only the phase announced "ready" over a graph still growing edges.
- **A background fetch is not a redraw.** Every request carries a `RequestOrigin`, and only `User` answers emit `AppEvent::LayoutDirty` — which is what every out-of-process frontend turns into a full snapshot. The warmup used to answer with one per reply: on jemalloc that was 767 whole-graph snapshots in 110 seconds of loading, of which exactly one changed the screen, because the map the app opens on has no root and draws no call edges at all. The client answers a snapshot by tearing the canvas down and re-rendering a sidebar holding every symbol in the project, so this was the lag. Nothing is lost by the silence: the graph still grows, and the map is not computed until it stops. The origin has to survive the prepare → follow-up hop, since the second half is the one that carries the edges. `EventRouter` holds a second line of defence, rate-limiting snapshots to `LOADING_SNAPSHOT_INTERVAL` while `ResolvingCalls` says the warmup is running — holding one back, never dropping it, and releasing it the moment the warmup drains.
- **The snapshot ships what the client reads, and borrows it.** `core::protocol::WireGraph` is not `CallGraph`: the client never touches a node's `references` or `diagnostics`, nor an edge's `call_location`, and those cost more than everything it does read put together — 4.63 MB of a 5.97 MB jemalloc snapshot was call sites dropped on the floor. Edges go out one per caller/callee pair rather than one per call site, because that is what the client was already reducing them to on arrival. Same graph, 2.05 MB instead of 5.97 MB. It borrows the app rather than owning a copy, which is why `snapshot_of` returns a `ServerMsg<'_>` and both backends serialise it while still holding the lock instead of handing the message around. Field names match what `app.js` already reads; shortening them would save a little more and cost the client a rewrite.
- **Loading is reported in the status bar, and only there.** It has two stages — clangd handing over symbols (`LspProgress`), then their edges (`ResolvingCalls`) — which the client folds into one `load` object in `app.js`. That object exists to keep the one status line honest: the transient events used to fight over it, and a warmup reply would set it to "Ready" while the project was still indexing. A stage carrying a total reads as a fraction, one without as a running count, since clangd streams workspace symbols without ever saying how many are coming. The TUI shows the same two stages in its own status bar, reading `CoreApp::warmup_progress()` directly.
- **The cluster map is modularity, on a resolution ladder.** `logic::cluster` optimises modularity with the Louvain method: sweep the functions in symbol-id order, move each into the neighbouring box that gains the most, repeat until nothing moves, collapse every box into one node, go again. Each collapse is a rung, so the levels are a **zoom** — every level partitions the same functions, and the client shows exactly one at a time (`Command::SetMapLevel`, per workspace) rather than opening boxes. It replaced heavy-edge matching, which grouped by what merges cheaply: 328 of a real project's 370 calls weighed one, so "heaviest" was a tie decided by symbol id, and `log_write`, `mem_free` and `strcmp` grew a box of every caller they had — 26 of the 33 boxes the map opened on spanned more than one file. Under the null model a call to something everything calls is expected and does not pay for a merge, and a sweep can undo a grouping a previous sweep made, where a pair once matched was matched for good. The rungs are resolutions: one pass per entry of `RESOLUTION_LADDER` (8, 4, 2, 1), finest first, then passes at 1 until the optimiser is done, then — only while the top is wider than `TARGET_TOP_CLUSTERS` — halvings below 1. A pass that merges nothing adds no rung. The quotient of a rung is still **not** acyclic: a box can sit on both ends of a one-way path, and `LayoutEdge::mutual` draws such a pair once with a head at each end.
- **Where the ladder stops and where the map opens are different questions.** `Ladder::opening` — carried through as `Clustering::opening_level` and the default in `CoreApp::map_level_for` — is the last rung built at plain modularity, the grouping the objective stands behind. Rungs past it exist only when that one was wider than the target: they merge modules that did not want to merge, to fit a count, and are where a user zooms out *to*. `cargo run -p core --example rungs` prints the ladder with density and mutual-pair counts, and given a cluster-map DOT export it rebuilds that project's graph and prints its — which is how the method was judged without a clangd run. What that judging found, and is still open: from calls alone no objective recovers the source tree on a layered C project. The by-file grouping of the 188-function example has modularity 0.32 where Louvain finds 0.58, because a module there is a layer whose functions share callers and callees rather than a dense clique — so the fine rungs track files well and the top rung groups by lifecycle phase (everything `_init`, everything `_free`). Neighbourhood-similarity weighting, directed modularity and stripping leaves were all measured and none closed the gap; the lever left is the file path, which the graph carries and the clusterer has so far deliberately not been shown.
- **The last rung folds its leftovers.** A function no box gains by taking in comes through as a box holding one function beside boxes holding thirty; on a real project that was `fputc`, alone, next to clusters of 22 and 29. `fold_singletons` merges each of those into the neighbour it shares the heaviest edge with, and is composed into the last round rather than added as a rung, because it is a repair to the top of the ladder and not a step of the zoom. It is the one place the objective is overruled.

- **A clustering is computed once, when the graph stops moving.** Coarsening is not incremental: a run over a graph that gained a thousand functions is a different ladder, not the same one extended. So `CoreApp::sync_clusterings` starts nothing while `loading()` is true — an LSP phase short of `Completed`, or an edge warmup still running — and after that computes what is owed, one job at a time on a worker thread. Demo mode is the exception and is clustered in `with_bridge`, because a graph that arrives already populated is the whole of it and no loader will ever say so. The cache is one table keyed by `ClusterTarget` (method, scope, direction) rather than a whole-program clustering plus a table of scoped ones, which is what makes one staleness rule and one job queue cover both; a target's staleness is cleared at *spawn*, so a batch landing mid-run is owed again rather than swallowed. The whole-program map for each method in use is never evicted, because it is what a new tab opens on and what widening a scope goes back to.
- **The method is a value, not a constant.** `logic::ClusterMethod` has one variant today and is named anyway: every method makes a different claim about a program's shape, and which claim is on screen is the user's to pick. It travels on the workspace beside depth and orientation, in `Command::SetClusterMethod`, and in the snapshot's `cluster_methods` — which the client renders into its picker rather than carrying its own copy of the list, so a method added in `logic` is reachable without touching the client.
- **The hint line yields; a result holds it.** `#info` has many writers — hovering a node writes the symbol under the pointer, leaving it writes an empty string, a regrouping writes that the map moved — and during the warmup that is constant. Right for a hint, wrong for a result: an export names the file it just wrote, and on jemalloc that line was gone before it could be read, overwritten by the next `ClusteringUpdated` or by the mouse crossing the canvas, which made a working export look like a key that did nothing. So `setInfo` is a hint and yields to anything held, `holdInfo` is a result and outranks them for `INFO_HOLD_MS`. Same reason the `load` object exists for the status bar: one line, several writers, and only one of them saying something worth waiting for.
- **An export is a view, in Graphviz DOT.** `core::export` renders what is on screen — this workspace's depth limit, collapsed and expanded nodes, direction and orientation already applied — and nothing else; exporting the whole loaded graph would hand back ~3,300 nodes nobody has ever looked at, which is what the mechanisms in `layout` exist to prevent. DOT rather than SVG or PNG because it is already the interchange format of C/C++ call graphs (doxygen, egypt, gprof2dot, callgrind all emit it), `dot -T…` turns it into every raster and vector format afterwards, `networkx` and `gvpr` read it *back* as a graph, and — the reason that decided it — `subgraph cluster_…` is a native DOT concept that the cluster map maps onto with no translation at all. Being text is what lets one implementation serve all three frontends: the TUI cannot render a PNG and the web client would need a rendering library to. It carries no colour, because the canvas reads its palette from the CSS custom properties and a second copy in Rust would drift. `CoreApp` builds the bytes and stops there — `AppEvent::GraphExported` hands them to the frontend, because where a file belongs is the one part of this that differs per pipe, and a web server writing to its own disk on behalf of a tab on another machine is the wrong answer in the only case with a network in it. TUI: `e`, into the working directory. Web: `Ctrl+E`, a download — the only place a page can put a file. Desktop: `Ctrl+E`, and `save_export` **asks**, opening a native save dialog on the project directory; choosing for the user sent three exports to whatever XDG calls downloads, which on a `XDG_DOWNLOAD_DIR="$HOME/"` machine is the home directory. The dialog is opened from Rust, not from `transport.js`: a plugin's JS bindings come as an npm package and reaching for one would cost this client the property that it is a plain script with no build step. `CoonHost.saveFile` resolves to the path, or to `null` for a cancelled dialog — not a failure, and not reported as one. Both halves live in `transport.js`, with the window controls, for the same reason they do.
- **`grid` crate**: provides the tree layout algorithm used by `GraphView` and by `core::layout::compute_layout`.
- **One protocol, many pipes**: `core::protocol` is the single definition of what a frontend sees. Adding a fourth frontend should mean writing a transport, not a protocol.

### Gotcha: the crate named `core`

The workspace crate `core` shadows Rust's built-in `core`. Crates that use macros expanding to `::core::…` paths (Tauri does) must alias it:

```toml
coon_core = { package = "core", path = "../core" }
```

`crates/desktop` does this. Others refer to it as `::core::`.
