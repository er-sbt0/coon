# coon

Interactive call graph explorer for C/C++ projects, powered by `clangd`.

Analyzes a project via LSP and renders an interactive view for navigating function call hierarchies — incoming and outgoing calls, cross-file references, and reachability.

The **desktop app** is the frontend. Two older ones ship alongside it and still
work, but they are deprecated and no longer developed:

| Frontend | Run it with | Status |
|---|---|---|
| **Desktop** | `coon-desktop` | Native window (Tauri) — the one to use |
| Web | `coon --web <PORT>` | Deprecated: browser client over a WebSocket |
| TUI | `coon` | Deprecated: Ratatui in the terminal |

All three sit on one application core, and the desktop app and the web client
are the same JavaScript — only the transport differs. Deprecated means kept
working, not frozen out: they still build, still run and are still tested. New
features are not expected to reach them.

## Requirements

- Rust (2021 edition)
- `clangd` in `PATH` (for LSP mode), or `COON_CLANGD` set to the clangd binary to use
- A `compile_commands.json` in the project root (or coon will fall back to directory walking)

On Linux the desktop app needs Tauri's system dependencies:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
                 librsvg2-dev build-essential curl file
```

## Usage

```bash
# Demo mode — no project needed
cargo run

# Analyze a real C/C++ project
cargo run -- /path/to/your/project
```

`cargo run` launches the desktop app; `cargo run -p desktop` is the same thing
spelled out. Every frontend accepts the same optional project path; omit it for
demo data.

The deprecated frontends live in the `coon` binary, which is outside the
workspace's default members — name it explicitly:

```bash
# TUI (deprecated)
cargo run -p coon -- /path/to/your/project

# Web frontend on http://localhost:8080 (deprecated)
cargo run -p coon -- /path/to/your/project --web 8080
```

Each prints a deprecation notice and then starts normally.

## Exporting a graph

Any view — a rooted call tree or a cluster map — exports as [Graphviz](https://graphviz.org) DOT: `Ctrl+E` (or the command palette) in the desktop and web clients, `e` in the TUI. What you get is the graph on screen, with its depth limit, collapsed nodes and direction already applied — not the whole project.

```bash
dot -Tsvg main.dot -o main.svg     # or -Tpng, -Tpdf
```

The desktop app saves to your downloads directory, the browser downloads the file, and the TUI writes into its working directory; each says where it went. Open communities come out as nested `subgraph cluster_…` boxes, and closed ones as a single node carrying the number of functions it stands for.

## Build

```bash
cargo build --release            # desktop app and the shared crates
cargo build --release -p coon    # the deprecated TUI/web binary
```

To produce desktop installers (`.deb` / AppImage), install the Tauri CLI and bundle:

```bash
cargo install tauri-cli --version '^2'
cargo tauri build --config crates/desktop/tauri.conf.json
```

## Testing

```bash
cargo test                      # all tests
cargo test -p lsp               # LSP tests (requires clangd)
```

See [`lsp/TESTING.md`](lsp/TESTING.md) for writing integration tests.
