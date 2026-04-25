# Web Frontend Support Plan

## Current State

The `core` crate is fully implemented:
- `CoreApp` — owns `CallGraph`, `LspBridge`, `CoreWorkspaceManager`; exposes `dispatch(Command) → Vec<AppEvent>` and `tick() → Vec<AppEvent>`
- `Command` and `AppEvent` — clean input/output contracts
- `LspBridge` — frontend-agnostic, wired via MPSC channels from `runner.rs`

`model` types (`CallGraph`, `FunctionNode`, `SymbolId`, `Location`, etc.) already have `#[derive(Serialize, Deserialize)]`.

**Gaps that block the web layer:**
- `CallDirection` (in `model`) has no serde derives
- `AppEvent` and `Command` (in `core`) have no serde derives; `core` has no `serde` dependency
- No server-side layout step to attach `(x, y)` positions to nodes before sending to the browser
- No `crates/web` crate
- No `--web` flag in `main.rs`

---

## Target Architecture

```
Browser (Cytoscape.js + WebSocket client)
        ↕  JSON over WebSocket
crates/web  (Axum + per-connection WebSession)
        ↕  CoreApp::dispatch / CoreApp::tick
crates/core (CoreApp, Command, AppEvent, LspBridge)
        ↕  MPSC channels (same wiring as TUI)
lsp loader task  (unchanged)
```

Each WebSocket connection owns its own `WebSession { core: CoreApp }`.  
This matches the TUI model — one session per client, no shared mutable state.

---

## Step 1 — Add serde to `model::CallDirection`

**File:** `crates/model/src/direction.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection { Outgoing, Incoming }
```

Add `use serde::{Deserialize, Serialize};` at the top. `model` already depends on serde.

---

## Step 2 — Add serde to `AppEvent` and `Command` in `core`

**File:** `crates/core/Cargo.toml` — add:
```toml
serde = { workspace = true, features = ["derive"] }
serde_json.workspace = true
```

**File:** `crates/core/src/events.rs`
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppEvent { … }
```

**File:** `crates/core/src/command.rs`
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command { … }
```

`LspLoadPhase` (used inside `AppEvent::LspProgress`) already derives serde in `model`.

---

## Step 3 — Server-side layout helper

The browser needs `(x, y)` positions for each node so it can render the graph without reimplementing the `grid` crate.

**New file:** `crates/core/src/layout.rs`

```rust
use std::collections::HashMap;
use model::{CallGraph, SymbolId};
use model::CallDirection;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePos { pub x: f32, pub y: f32 }

/// Compute layout positions for the subgraph rooted at `root`, using the
/// grid crate's tree layout algorithm.  Returns an empty map if `root` is None.
pub fn compute_layout(
    graph: &CallGraph,
    root: Option<SymbolId>,
    direction: CallDirection,
) -> HashMap<SymbolId, NodePos> {
    // Use grid::TreeLayout (or the adapter already used by TUI's GraphViewState).
    // Exact API: grid::layout(nodes, edges, settings) → Vec<(SymbolId, f32, f32)>
    todo!("wire grid crate")
}
```

Add `grid.workspace = true` to `crates/core/Cargo.toml`.

Re-export from `crates/core/src/lib.rs`:
```rust
pub mod layout;
pub use layout::{compute_layout, NodePos};
```

---

## Step 4 — Define the WebSocket protocol

**New file:** `crates/web/src/protocol.rs`

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use model::{CallGraph, SymbolId};
use core::{AppEvent, Command, CoreWorkspace};
use core::layout::NodePos;

/// Messages the server sends to the browser.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Full state on connect (or after a Refresh).
    Snapshot {
        graph: CallGraph,
        layout: HashMap<SymbolId, NodePos>,
        workspaces: Vec<WorkspaceInfo>,
        selected: Option<SymbolId>,
    },
    /// Incremental update after a command or tick.
    Event { event: AppEvent },
    /// New layout positions after graph structure changed.
    Layout { layout: HashMap<SymbolId, NodePos> },
}

/// Messages the browser sends to the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Command { command: Command },
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: usize,
    pub name: String,
    pub direction: model::CallDirection,
    pub selected: Option<SymbolId>,
}
```

**Key design rules:**
- `Snapshot` is sent once on connect and re-sent after `Command::Refresh`.
- `Layout` is sent whenever `AppEvent::LayoutDirty` appears in tick/dispatch results.
- All other `AppEvent`s are sent as `Event` messages in real time.
- `LayoutDirty` itself is NOT forwarded as an `Event` — it is consumed to trigger a `Layout` message instead.

---

## Step 5 — Create `crates/web`

### `crates/web/Cargo.toml`

```toml
[package]
name = "web"
version = "0.1.0"
edition = "2021"

[dependencies]
core.workspace = true
model.workspace = true
lsp.workspace = true
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
axum = { version = "0.7", features = ["ws"] }
tower-http = { version = "0.5", features = ["fs"] }
log.workspace = true
anyhow.workspace = true
```

Add `web = { path = "crates/web" }` to `[workspace.dependencies]` and `web.workspace = true` to `crates/coon/Cargo.toml`.

### `crates/web/src/session.rs`

```rust
use core::{AppEvent, Command, CoreApp};
use core::layout::compute_layout;
use model::CallGraph;
use axum::extract::ws::{Message, WebSocket};
use serde_json;
use crate::protocol::{ClientMsg, ServerMsg, WorkspaceInfo};

pub struct WebSession {
    pub core: CoreApp,
}

impl WebSession {
    pub fn new(call_graph: CallGraph) -> Self {
        Self { core: CoreApp::new(call_graph) }
    }

    pub fn new_with_lsp(call_graph: CallGraph, lsp: core::LspBridge) -> Self {
        Self { core: CoreApp::new_with_lsp(call_graph, lsp) }
    }

    pub fn snapshot_msg(&self) -> ServerMsg {
        let ws = self.core.workspaces.current();
        let root = ws.and_then(|w| w.root_symbol);
        let direction = ws.map(|w| w.direction).unwrap_or_default();
        let layout = compute_layout(&self.core.call_graph, root, direction);
        ServerMsg::Snapshot {
            graph: self.core.call_graph.clone(),
            layout,
            workspaces: self.workspace_infos(),
            selected: self.core.selected_function,
        }
    }

    pub fn layout_msg(&self) -> ServerMsg {
        let ws = self.core.workspaces.current();
        let root = ws.and_then(|w| w.root_symbol);
        let direction = ws.map(|w| w.direction).unwrap_or_default();
        ServerMsg::Layout {
            layout: compute_layout(&self.core.call_graph, root, direction),
        }
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> { … }
}
```

### `crates/web/src/routes.rs`

```rust
use axum::{
    extract::{ws::WebSocketUpgrade, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;
use tokio::sync::Mutex;
// State holds factory data (call_graph + optional LspBridge channels) needed
// to create a new WebSession per connection.

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .nest_service("/", tower_http::services::ServeDir::new("crates/web/static"))
        .with_state(Arc::new(state))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut session = state.new_session().await;

    // Send initial snapshot
    send_json(&mut socket, &session.snapshot_msg()).await;

    let tick_interval = tokio::time::interval(std::time::Duration::from_millis(100));
    tokio::pin!(tick_interval);

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let events = session.core.tick();
                handle_events(&mut socket, &mut session, events).await;
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ClientMsg::Command { command }) = serde_json::from_str(&text) {
                            let events = session.core.dispatch(command);
                            handle_events(&mut socket, &mut session, events).await;
                        }
                    }
                    _ => break,
                }
            }
        }
    }
}

async fn handle_events(socket: &mut WebSocket, session: &mut WebSession, events: Vec<AppEvent>) {
    let mut layout_dirty = false;
    for event in &events {
        match event {
            AppEvent::LayoutDirty => { layout_dirty = true; }
            _ => { send_json(socket, &ServerMsg::Event { event: event.clone() }).await; }
        }
    }
    if layout_dirty {
        send_json(socket, &session.layout_msg()).await;
    }
}
```

### `crates/web/src/lib.rs`

```rust
pub mod protocol;
pub mod routes;
pub mod session;
pub use routes::{router, AppState};
```

---

## Step 6 — Static browser client

**`crates/web/static/index.html`** — minimal page, loads Cytoscape.js from CDN and `app.js`.

**`crates/web/static/app.js`** — WebSocket client:

```js
const cy = cytoscape({ container: document.getElementById('cy'), … });
const ws = new WebSocket(`ws://${location.host}/ws`);

ws.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    if (msg.type === 'snapshot') {
        rebuildGraph(msg.graph, msg.layout);
    } else if (msg.type === 'layout') {
        applyLayout(msg.layout);
    } else if (msg.type === 'event') {
        handleEvent(msg.event);
    }
};

function rebuildGraph(graph, layout) {
    cy.elements().remove();
    for (const [id, node] of Object.entries(graph.nodes)) {
        const pos = layout[id] ?? { x: 0, y: 0 };
        cy.add({ group: 'nodes', data: { id, label: node.name }, position: pos });
    }
    for (const edge of graph.edges) {
        cy.add({ group: 'edges', data: { source: edge.caller, target: edge.callee } });
    }
}

function applyLayout(layout) {
    for (const [id, pos] of Object.entries(layout)) {
        cy.$id(id).position(pos);
    }
}

// On node click → dispatch SelectFunction
cy.on('tap', 'node', (e) => {
    ws.send(JSON.stringify({ type: 'command', command: { SelectFunction: e.target.id() } }));
});
```

---

## Step 7 — Wire `--web` flag into `main.rs`

**`crates/coon/src/main.rs`** — add to `Args`:

```rust
/// Start the web server instead of the TUI (e.g. --web 8080)
#[arg(long, value_name = "PORT")]
web: Option<u16>,
```

Dispatch in `main`:

```rust
match (args.web, args.project_path) {
    (Some(port), None)        => runner::run_web(port).await?,
    (Some(port), Some(path))  => runner::run_web_with_lsp(&path, port).await?,
    (None, None)              => runner::run_with_demo_data(settings).await?,
    (None, Some(path))        => runner::run_with_lsp(&path, settings).await?,
}
```

**`crates/coon/src/runner.rs`** — add:

```rust
pub async fn run_web(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let call_graph = crate::demo::create_demo_call_graph();
    let state = web::AppState::new(call_graph);
    let app = web::router(state);
    let addr = format!("0.0.0.0:{port}");
    info!("Web server listening on http://{addr}");
    axum::serve(tokio::net::TcpListener::bind(&addr).await?, app).await?;
    Ok(())
}

pub async fn run_web_with_lsp(
    project_path: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    // Same channel setup as run_with_lsp, but passes channels to AppState
    // so each new WebSocket connection creates a fresh WebSession wired to the same loader.
    todo!()
}
```

---

## Migration Checklist (in order)

| # | Task | Files touched |
|---|------|---------------|
| 1 | `CallDirection`: add serde derives | `crates/model/src/direction.rs` |
| 2 | `AppEvent`, `Command`: add serde derives; add `serde`/`serde_json` to `core` | `crates/core/src/events.rs`, `command.rs`, `Cargo.toml` |
| 3 | Implement `compute_layout` using `grid` crate; add `grid` dep to `core` | `crates/core/src/layout.rs` |
| 4 | Create `crates/web`: `Cargo.toml`, `protocol.rs`, `session.rs`, `routes.rs`, `lib.rs` | new crate |
| 5 | Add static client: `index.html`, `app.js` (Cytoscape.js) | `crates/web/static/` |
| 6 | Add `web` to workspace and `coon` deps; add `--web PORT` flag; add `run_web*` | `Cargo.toml`, `crates/coon/` |
| 7 | Test: `cargo run -- --web 8080` with demo data, open browser | manual |
| 8 | Test: `cargo run -- /path/to/project --web 8080` with real LSP | manual |

---

## Key Decisions and Rationale

**Per-connection `CoreApp`** (not shared): mirrors the TUI model, avoids locks.  
A shared model could be added later with `Arc<Mutex<CoreApp>>` once the basic flow works.

**Layout server-side**: avoids shipping the grid algorithm to WASM for v1.  
The `grid` crate is already a workspace member, so `core` can depend on it.

**`LayoutDirty` consumed, not forwarded**: the browser doesn't know what "dirty" means.  
The server recomputes positions and sends a `Layout` message instead — same data, typed correctly for the receiver.

**`serde_json` tag format `snake_case`**: `{ "type": "snapshot", … }` is idiomatic for JS consumers and aligns with what Cytoscape.js expects for dynamic element properties.

**No WASM for v1**: the browser renders whatever positions the server computes.  
If the layout must exactly match the TUI, the grid crate can be compiled to WASM later as an optional enhancement.
