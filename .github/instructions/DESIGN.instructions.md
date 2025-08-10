---
applyTo: '**'
---
# Coon - Code Explorer and Call Graph Visualization

This document explains the design and architecture of the Coon project, a code analysis and visualization tool that utilizes the Language Server Protocol (LSP) to analyze codebases and present call graphs and other code insights through a terminal user interface (TUI).

## 1. Codebase Components Design

The project is structured as a Rust workspace with several modules, each responsible for a specific aspect of functionality:

### 1.1. Core Components

#### `core_data`
- Foundation for all data structures used across the application
- Defines core types like `SymbolId`, `Location`, `Reference`, and `CallGraph`
- Provides logging utilities for the entire application
- Acts as a shared library to ensure consistency between modules

#### `logic`
- Contains the business logic and algorithms for code analysis
- Implements graph traversal and path finding algorithms
- Provides filtering capabilities for call graphs
- Includes the query engine that enables searching and analyzing the call graph

#### `lsp_integration`
- Manages all interactions with Language Server Protocol (LSP) servers
- Provides abstractions for LSP requests and responses
- Implements specialized LSP features like call hierarchy analysis
- Handles document management and synchronization with LSP servers

#### `tui_ui`
- Terminal User Interface implementation using the Ratatui library
- Manages UI components like function lists, call graph views, and diagnostic panels
- Implements user interaction handling and action dispatch
- Provides visualization of call graphs and code relationships

### 1.2. Main Application (`src/`)

- Entry point and application initialization
- Command-line argument parsing
- Project setup and LSP client initialization
- Compile commands parsing for C/C++ projects
- Orchestration between LSP, logic, and UI components

## 2. LSP Integration

The Language Server Protocol (LSP) integration is central to the application's functionality, providing code intelligence features without language-specific parsing:

### 2.1. LSP Client

- Establishes and maintains connections to LSP servers
- Handles JSON-RPC communication with LSP servers
- Manages request IDs and response matching
- Provides async APIs for LSP operations

### 2.2. LSP Service

- Higher-level abstraction over the raw LSP client
- Manages complex LSP operations like symbol resolution
- Handles document lifecycle (opening, syncing, closing)
- Implements specialized queries like call hierarchy and references

### 2.3. LSP Features Used

- **Call Hierarchy**: Retrieves caller/callee relationships between functions
- **Document Symbols**: Gets the structure of functions and other symbols in a file
- **Find References**: Discovers all usages of a symbol across the codebase
- **Workspace Symbols**: Finds symbols across the entire project workspace
- **Hover**: Retrieves documentation and type information for symbols

### 2.4. Enhanced LSP Capabilities

- Symbol resolution to enrich reference information
- Caching of document symbols to improve performance
- Background processing of LSP requests to keep the UI responsive
- Filtering of non-project symbols to focus on relevant code

## 3. Terminal UI Design

The Terminal User Interface (TUI) is designed to provide an interactive and responsive experience for exploring code:

### 3.1. UI Components

- **Function List**: Displays all functions/symbols in the project
- **Call Graph View**: Visualizes caller/callee relationships
- **Diagnostic Panel**: Shows errors, warnings, and status information
- **Action Bar**: Displays available keyboard shortcuts and actions

### 3.2. UI Framework

- Built on the Ratatui library (formerly tui-rs)
- Uses Crossterm for terminal manipulation and event handling
- Implements a component-based architecture
- Supports responsive layouts that adapt to terminal size

### 3.3. Interaction Model

- Keyboard-driven navigation and actions
- Modal interface with different views (overview, detail, etc.)
- Support for filtering and searching
- Real-time updates as LSP data becomes available

### 3.4. State Management

- Centralized application state
- Clear separation between UI state and domain data
- Event-based architecture for UI updates
- Loading states to indicate background operations

## 4. Data Flow

1. **Initialization**:
   - Parse compile_commands.json or discover source files
   - Initialize LSP client and service
   - Set up communication channels

2. **Data Loading**:
   - LSP requests are sent to analyze code
   - Results are collected asynchronously
   - Call graph is built incrementally

3. **User Interaction**:
   - UI events trigger actions
   - Actions may initiate new LSP requests
   - Graph traversal algorithms provide insights

4. **Visualization**:
   - Call graph is rendered based on current view mode
   - Function details are displayed on selection
   - Diagnostic information is updated

## 5. Getting Started

1. **Running with Demo Data**:
   ```
   coon
   ```

2. **Analyzing a Project**:
   ```
   coon /path/to/project
   ```

3. **Navigation**:
   - Use arrow keys to navigate function lists and call graphs
   - Enter key to select functions and view details
   - Tab key to switch between panels
   - F1 key for help and keyboard shortcuts

## 6. Future Enhancements

- Support for more LSP features (code lens, semantic tokens)
- Advanced filtering and query capabilities
- Performance optimizations for large codebases
- Custom visualization layouts
- Export capabilities for call graphs

---

*This design document was created on August 10, 2025*
