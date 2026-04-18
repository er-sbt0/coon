# coon

Interactive call graph explorer for C/C++ projects, powered by `clangd`.

Analyzes a project via LSP and renders an interactive TUI for navigating function call hierarchies — incoming and outgoing calls, cross-file references, and reachability.

## Requirements

- Rust (2021 edition)
- `clangd` in `PATH` (for LSP mode)
- A `compile_commands.json` in the project root (or coon will fall back to directory walking)

## Usage

```bash
# Demo mode — no project needed
cargo run

# Analyze a real C/C++ project
cargo run -- /path/to/your/project
```

## Build

```bash
cargo build --release
```

## Testing

```bash
cargo test                      # all tests
cargo test -p lsp_integration   # LSP tests (requires clangd)
```

See [`lsp_integration/TESTING.md`](lsp_integration/TESTING.md) for writing integration tests.
