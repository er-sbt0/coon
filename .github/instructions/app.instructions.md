applyTo: "**"
---

# Architecture Overview

- This Rust project is a Cargo workspace with these crates: `core_data`, `lsp_integration`, `logic`, `tui_ui`, and the binary crate `app_cli`.
- **core_data**: Implements graph and symbol data types (`CallGraph`, `FunctionNode`, `SymbolId`, `Location`, `Diagnostic`). It is well-structured and tested, with no external dependencies except for serialization.
- **lsp_integration**: Handles communication with clangd via JSON-RPC, converting LSP responses into `core_data` types. Conversion functions and basic response types are implemented.
- **logic**: Intended for graph traversal, filtering, and decision-making using only `core_data` types. Currently, this crate is a stub and needs implementation.
- **tui_ui**: Intended for UI rendering with Ratatui, handling layouts, user input, and model-view updates. This crate is a stub and needs implementation.
- **app_cli**: The main binary crate, responsible for orchestrating LSP, logic, and TUI UI. The main loop is not yet implemented.


## Code Organization & Crate Responsibilities

- **core_data**: Data types and state only. No references to LSP or UI code.
- **lsp_integration**: Parse clangd JSON, handle errors, and map to `core_data` structures. Unit-testable with mock LSP messages.
- **logic**: Implement graph operations, filtering, and queries. No UI or LSP code.
- **tui_ui**: UI rendering and event loop. Interacts only with logic and core_data.
- **app_cli**: Orchestrates all components, runs the event loop, and wires LSP, logic, and UI.


## Testing Best Practices

- **core_data**: Unit tests for graph invariants and data transformations (already present).
- **lsp_integration**: Mock clangd JSON-RPC responses, test conversion to core_data.
- **logic**: Add unit tests for graph filtering and traversal logic as you implement.
- **tui_ui**: Use smoke tests or snapshot-based tests for UI screens.
- **app_cli**: Prefer lightweight integration tests with fake LSP inputs and UI state transitions.


## Development Guidelines

- Keep data, logic, LSP, and UI code strictly separated.
- Follow MVC: **Model** = `core_data` + logic state, **View** = `tui_ui`, **Controller** = `app_cli`.
- Do not import UI code into logic or data crates.
- Use dependency inversion: `app_cli` supplies LSP and UI layers to logic and data.


## Naming and Conventions

- Call-graph types: `CallGraph`, `FunctionNode`, `SymbolId`, `Location`, `Diagnostic`.
- Filters and queries: `logic::filters`, `logic::path`, `logic::query`.
- UI widgets in `tui_ui`: separate modules per screen (e.g. `call_graph_view.rs`, `diagnostic_panel.rs`).
- Main loop in `app_cli/src/main.rs`.


## Next Steps

1. **logic**: Implement graph traversal, filtering, and query functions using `core_data` types.
2. **tui_ui**: Implement basic UI screens and event handling using Ratatui. Start with a call graph view and diagnostics panel.
3. **app_cli**: Implement the main event loop, wiring together LSP, logic, and UI. Add basic CLI argument parsing if needed.
4. **lsp_integration**: Expand LSP request/response handling and add more conversion functions as needed.
5. **Testing**: Add and expand tests in all crates as features are implemented.


## Notes

- clangd is used for LSP.
- LSP documentation: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/

---

This revision reflects your current project state and provides clear next steps for development.