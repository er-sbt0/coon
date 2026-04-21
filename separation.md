# Multi-Frontend Separation: Extracting a `core` Crate

## Goal

Introduce a new `crates/core` crate that owns all frontend-agnostic application logic. The `tui` crate wraps `core` and adds rendering/input concerns on top. A future `web` crate does the same with an Axum/WebSocket backend. Neither frontend duplicates LSP wiring, workspace management, or graph-mutation logic.

---

## Resulting Crate Layout

```
crates/
  model/   (unchanged) — CallGraph, FunctionNode, SymbolId, LspUiMessage …
  lsp/     (unchanged) — LspClient, LspService, LspRequest, LspResponse
  logic/   (unchanged) — BFS, path-finding, GraphQueryEngine
  grid/    (unchanged) — DAG layout algorithm
  core/    (NEW)       — CoreApp, Command, AppEvent, LspBridge, CoreWorkspaceManager
  tui/     (shrinks)   — TuiApp wraps CoreApp, adds GraphViewState, key mapping, rendering
  web/     (NEW)       — Axum server, WebSocket sessions, JSON serialization
  coon/    (thins out) — main.rs wires the chosen frontend to the LSP loader
```

Dependency edges (no cycles introduced):
```
model ← lsp
model ← logic
model ← grid
model ← core ← lsp, logic
core  ← tui
core  ← web
core  ← coon
```

---

## The `core` Crate

### 1. `CoreWorkspace` and `CoreWorkspaceManager`

`GraphWorkspace` currently fuses domain state with TUI view state (`GraphViewState`). Split it.

**What moves to `core`:**

```rust
// crates/core/src/workspace.rs

pub struct CoreWorkspace {
    pub id: usize,
    pub name: String,
    pub root_symbol: Option<SymbolId>,
    pub direction: CallDirection,      // domain concept, not rendering
    pub created_at: Instant,
    pub last_accessed: Instant,
}

pub struct CoreWorkspaceManager {
    pub workspaces: Vec<CoreWorkspace>,
    pub current_index: usize,
    pub next_id: usize,
}
```

`CoreWorkspaceManager` exposes the same CRUD methods as the current `WorkspaceManager`
(`create`, `create_with_function`, `close`, `switch_to`, `next`, `previous`, `rename`)
but **returns `AppEvent` variants instead of `StatusMessage`** (see §3 below).

The `LayoutSettings` and `GraphViewState` fields are gone — the TUI owns those separately.

---

### 2. `LspBridge` (moved from `tui` to `core`)

`LspBridge` is already well-isolated. The only change needed is replacing every
`Option<StatusMessage>` return type with `Option<AppEvent>`. The logic is identical;
only the return type changes.

```rust
// crates/core/src/lsp_bridge.rs

pub fn send_call_hierarchy(&mut self, …) -> Option<AppEvent>;
pub fn send_references(&mut self, …)     -> Option<AppEvent>;
pub fn send_workspace_symbols(&mut self) -> Option<AppEvent>;
pub fn drain_responses(&mut self)        -> (Vec<LspResponse>, Vec<AppEvent>);
// …
```

`LoadingState` and `PendingRequest` move with it unchanged.

---

### 3. `AppEvent` — the output contract

Replace `StatusMessage` as the bridge between `core` and frontends. `core` emits typed
events; each frontend translates them into its own display representation.

```rust
// crates/core/src/events.rs

#[derive(Debug, Clone)]
pub enum AppEvent {
    // Graph data changes
    GraphSymbolsUpdated { count: usize },
    OutgoingCallsLoaded { symbol_id: SymbolId },
    IncomingCallsLoaded { symbol_id: SymbolId },
    ReferencesFound    { symbol_id: SymbolId, count: usize },
    NoReferencesFound  { symbol_id: SymbolId },
    FunctionHasNoCallees,
    LayoutDirty,

    // LSP lifecycle
    LspProgress(LspLoadPhase),
    LspRequestTimedOut,
    LoadingCalls { direction: String },

    // Workspace lifecycle
    WorkspaceCreated  { id: usize, name: String },
    WorkspaceClosed,
    WorkspaceSwitched { id: usize, name: String },
    WorkspaceRenamed  { name: String },
    CannotCloseLastWorkspace,

    // Errors
    InvalidFilePath,
    NoLspChannel,
    LspError(String),
}
```

The TUI maps `AppEvent` to the existing `StatusMessage` variants in a
`fn app_event_to_status(event: &AppEvent, graph: &CallGraph) -> Option<StatusMessage>` helper.
The web frontend serialises `AppEvent` directly to JSON for the browser.

---

### 4. `Command` — the input contract

`Action` in `tui` contains both domain commands (expand, find references, workspace ops)
and TUI-specific input events (viewport pan, search cursor movement). `Command` is the
domain-only subset.

```rust
// crates/core/src/command.rs

#[derive(Debug, Clone)]
pub enum Command {
    // Graph interaction
    SelectFunction(SymbolId),
    ExpandOrCollapse,
    FindReferences,
    Refresh,
    ToggleCallDirection,

    // Workspace management
    NewWorkspace,
    CloseWorkspace(usize),
    NextWorkspace,
    PreviousWorkspace,
    SwitchWorkspace(usize),
    RenameWorkspace(usize, String),
    StartGraphFromFunction(SymbolId),

    // Search (query text is resolved by the frontend before dispatch)
    SearchSelectFunction(SymbolId),
}
```

The TUI maps `Action` → `Option<Command>` before calling `CoreApp::dispatch`. Viewport
pan actions (`MoveUp`, `MoveLeft`, etc.) and search cursor events have no `Command`
equivalent; the TUI handles them locally without touching `core`.

---

### 5. `CoreApp`

The central struct exposed by the crate.

```rust
// crates/core/src/app.rs

pub struct CoreApp {
    pub call_graph:        CallGraph,
    pub selected_function: Option<SymbolId>,
    pub lsp:               LspBridge,
    pub workspaces:        CoreWorkspaceManager,
}

impl CoreApp {
    pub fn new(call_graph: CallGraph) -> Self;
    pub fn new_with_lsp(call_graph: CallGraph, lsp: LspBridge) -> Self;

    /// Dispatch a domain command. Returns events describing what changed.
    pub fn dispatch(&mut self, cmd: Command) -> Vec<AppEvent>;

    /// Poll LSP channels and loader messages. Must be called every tick.
    /// Returns events for anything that arrived since the last tick.
    pub fn tick(&mut self) -> Vec<AppEvent>;
}
```

`dispatch` contains the logic currently scattered across `App::handle_action`,
`App::handle_lsp_response`, and the `app/lsp.rs` helpers. `tick` replaces the
per-frame polling calls currently done inline in the TUI event loop.

`CoreApp` has **no knowledge of rendering, viewport, or key bindings**.

---

## Changes to the `tui` Crate

### `TuiApp` — wraps `CoreApp`, owns view state

```rust
// crates/tui/src/app/mod.rs

pub struct TuiApp {
    pub core: CoreApp,

    // Per-workspace view state, keyed by workspace id
    pub view_states: HashMap<usize, GraphViewState>,

    // TUI chrome
    pub should_quit:       bool,
    pub show_help:         bool,
    pub show_search_bar:   bool,
    pub search_bar_state:  SearchBarState,
    pub last_viewport_size: (f32, f32),
    pub status_message:    StatusMessage,
}
```

**View state lifecycle**: whenever `core.dispatch(Command::NewWorkspace)` emits a
`WorkspaceCreated { id, .. }` event, `TuiApp` inserts a fresh `GraphViewState` into
`view_states` for that `id`. On `WorkspaceClosed`, it removes the entry. Current view
state is `view_states[core.workspaces.current().id]`.

### `Action` stays in `tui`

`Action` keeps all its current variants. The TUI event loop converts keystrokes → `Action`
as before. A new method `TuiApp::handle_action(action: Action) -> Result<()>` replaces the
current `App::handle_action`:

- Viewport pan / zoom / reset → mutate `view_states[current_id]` directly, set `status_message`.
- Search cursor / input → mutate `search_bar_state`, set `status_message`.
- Everything else → build a `Command`, call `core.dispatch(cmd)`, then call
  `self.apply_events(events)` which translates `AppEvent` → `StatusMessage` and updates
  `view_states` (e.g. marks layout dirty on `LayoutDirty`).

### `StatusMessage` stays in `tui`

It is a display concern. Remove TUI-irrelevant variants (none currently qualify; all map 1:1
from `AppEvent`). Add a translation function:

```rust
// crates/tui/src/app/events.rs

fn apply_events(app: &mut TuiApp, events: Vec<AppEvent>) {
    for event in events {
        match &event {
            AppEvent::LayoutDirty => {
                if let Some(ws_id) = app.core.workspaces.current().map(|w| w.id) {
                    if let Some(vs) = app.view_states.get_mut(&ws_id) {
                        vs.mark_layout_dirty();
                    }
                }
            }
            AppEvent::WorkspaceCreated { id, .. } => {
                app.view_states.insert(*id, GraphViewState::new(&LayoutSettings::default()));
            }
            AppEvent::WorkspaceClosed => {
                // remove stale entry (id was captured before dispatch)
            }
            _ => {}
        }
        if let Some(msg) = app_event_to_status(&event, &app.core.call_graph) {
            app.status_message = msg;
        }
    }
}
```

### `WorkspaceManager` in `tui` is deleted

It is replaced by `CoreWorkspaceManager` (in `core`) + `view_states: HashMap<usize,
GraphViewState>` (in `TuiApp`). The CRUD methods on `CoreWorkspaceManager` are identical;
callers just need to additionally manage `view_states` in `apply_events`.

---

## The `web` Crate (sketch)

```rust
// crates/web/src/lib.rs

pub struct WebSession {
    pub core:    CoreApp,
    pub clients: Vec<tokio::sync::mpsc::UnboundedSender<WebMessage>>,
}

pub enum WebMessage {
    GraphSnapshot(serde_json::Value),   // full CallGraph serialised
    Event(AppEvent),                    // incremental update
}
```

An Axum WebSocket handler:
1. Creates a `WebSession` (same LSP loader wiring as `main.rs`).
2. Calls `core.tick()` on a timer, broadcasts any `AppEvent`s to all connected clients.
3. Receives JSON commands from the browser, deserialises them to `Command`, calls
   `core.dispatch(cmd)`, broadcasts resulting events.

The browser renders the `CallGraph` JSON however it wants (D3, Cytoscape.js, etc.) —
the grid/layout algorithm can optionally be compiled to WASM and run client-side if the
layout needs to match the terminal version exactly.

---

## Migration Steps (Incremental)

The goal is to keep the TUI working after every step; no big-bang rewrite.

### Step 1 — Create `crates/core`, add `CoreWorkspace` and `CoreWorkspaceManager`

- Add `core` to `Cargo.toml` workspace.
- Copy `GraphWorkspace` → `CoreWorkspace` (drop `graph_view_state`, `LayoutSettings`).
- Copy `WorkspaceManager` → `CoreWorkspaceManager` (return `()` or a simple enum instead
  of `StatusMessage` for now; keep `StatusMessage` translation in a shim in `tui`).
- Do **not** touch `tui` yet; this step is additive.

### Step 2 — Introduce `AppEvent` and move `LspBridge` to `core`

- Define `AppEvent` in `core`.
- Copy `LspBridge` to `core/src/lsp_bridge.rs`, changing return types from
  `Option<StatusMessage>` to `Option<AppEvent>`.
- Add `fn app_event_to_status(e: &AppEvent, …) -> Option<StatusMessage>` in `tui`.
- Keep the old `tui::LspBridge` as a thin re-export of `core::LspBridge` while the TUI
  is still compiling against the old paths.

### Step 3 — Define `Command`, introduce `CoreApp::dispatch`

- Define `Command` in `core`.
- Create `CoreApp` with a `dispatch` method that contains the logic from
  `App::handle_action` (domain branches only).
- `CoreApp::tick` polls `lsp.drain_responses()` and `lsp.poll_loader_messages()`, returns
  `Vec<AppEvent>`.

### Step 4 — Wire `TuiApp` to `CoreApp`

- Add `core: CoreApp` field to `App` (rename `App` → `TuiApp` in a follow-up).
- In `handle_action`, route domain actions through `core.dispatch(cmd)` and call
  `apply_events`.
- Keep old `App` methods alive as forwarding shims until all call sites are migrated.
- Remove shims one by one; delete dead code.

### Step 5 — Split `GraphWorkspace` view state into `view_states: HashMap<usize, GraphViewState>`

- Replace `workspaces: WorkspaceManager` (the old tui one) with `view_states`.
- `apply_events` maintains `view_states` in response to workspace events.
- Delete `tui::WorkspaceManager` and `tui::GraphWorkspace`.

### Step 6 — Add `crates/web`

- Implement the Axum skeleton and `WebSession` on top of `CoreApp`.
- Add a `--web` flag to `main.rs` (or a second binary `coon-web`) to select the frontend.

---

## Key Trouble Spots

### `WorkspaceManager` creates `GraphViewState` at workspace creation time
Currently `WorkspaceManager::create` takes `LayoutSettings` and passes it into
`GraphWorkspace::new`, so the view state is born with the workspace. After the split,
`apply_events` must create the `GraphViewState` when it sees `WorkspaceCreated`. The
`LayoutSettings` live in `TuiApp` and are passed at that point.

### `CallDirection` is currently in `tui/src/graph_adapter.rs`
It should move to `model` (or `core`) since `CoreWorkspace::direction` needs it and it
is a domain concept (incoming vs. outgoing calls), not a rendering concept.

### `LspBridge::send_call_hierarchy` reads from `CallGraph`
The method signature takes `&CallGraph` already, so it is fine to move as-is. Just
confirm that `core` can depend on `lsp` (it can — see dependency graph above).

### `SearchBarState` is TUI-only
It uses `CallGraph` for result filtering (`update_results(&self.call_graph)`). The search
logic (fuzzy match, exact match, file path) could move to `logic` eventually, but for now
keeping `SearchBarState` in `tui` and passing the graph reference is fine. `Command::SearchSelectFunction(SymbolId)` is the only signal that crosses into `core`.

### `StatusMessage::PannedUp/Down/Left/Right` and similar viewport variants
These never need to exist in `AppEvent`. They stay in `StatusMessage` and are produced
directly by `TuiApp::handle_action` without going through `core` at all. This is the
correct boundary: the core doesn't know that a viewport exists.
