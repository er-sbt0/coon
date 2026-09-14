# coon

Interactive call graph explorer for C/C++ projects, powered by `clangd`.

Analyzes a project via LSP and renders an interactive view for navigating function call hierarchies — incoming and outgoing calls, cross-file references, and reachability.

Three frontends, one shared application core:

| Frontend | Run it with | Notes |
|---|---|---|
| **TUI** | `coon` | Ratatui in the terminal |
| **Web** | `coon --web <PORT>` | Browser client over a WebSocket |
| **Desktop** | `coon-desktop` | Native window (Tauri), same client as web |

## Requirements

- Rust (2021 edition)
- `clangd` in `PATH` (for LSP mode), or `COON_CLANGD` set to the clangd binary to use
- A `compile_commands.json` in the project root (or coon will fall back to directory walking)

For the desktop frontend on Linux, Tauri's system dependencies:

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

# Web frontend on http://localhost:8080
cargo run -- /path/to/your/project --web 8080

# Desktop app
cargo run -p desktop -- /path/to/your/project
```

Every frontend accepts the same optional project path; omit it for demo data.

## Build

```bash
cargo build --release
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
