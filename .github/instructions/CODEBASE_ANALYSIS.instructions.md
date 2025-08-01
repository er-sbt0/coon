---
applyTo: '**'
---
# Coon Codebase Analysis

## Project Overview

Coon is a code exploration tool designed for C/C++ projects that leverages the Language Server Protocol (LSP) to analyze codebases and visualize call graphs. The application provides a terminal-based user interface (TUI) for navigating and understanding code relationships. Written in Rust, the project is organized as a workspace with several interconnected crates.

## Project Structure

The project is organized into these main components:

1. **Main Application (`src/`)**
   - Entry point and orchestration logic
   - Manages initialization of LSP, UI, and data structures

2. **Core Data (`core_data/`)**
   - Fundamental data structures for the application
   - Defines the call graph, function nodes, and diagnostic information
   - Handles logging setup and configuration

3. **LSP Integration (`lsp_integration/`)**
   - Communication with Language Server Protocol (LSP) server (clangd)
   - Request/response handling for code intelligence
   - Call hierarchy implementation

4. **Logic (`logic/`)**
   - Analysis algorithms and graph traversal
   - Path analysis and function filtering
   - Query interface for code exploration

5. **TUI UI (`tui_ui/`)**
   - Terminal-based user interface using Ratatui
   - Views for call graphs, function lists, and diagnostics
   - Action handling and state management

## Core Data Structures

### `CallGraph`
The central data structure representing function relationships:
- `nodes`: Map of `SymbolId` to `FunctionNode` representing all functions
- `edges`: Collection of `CallEdge` objects representing caller-callee relationships
- Methods for adding/retrieving functions and call relationships

### `FunctionNode`
Represents a function or method in the code:
- Unique identifier (`SymbolId`)
- Name and qualified name
- Definition location
- References (locations where the function is called)
- Associated diagnostics (errors/warnings)

### `CallEdge`
Represents a caller-callee relationship:
- References to caller and callee functions
- Location of the call site

### `Location`
Code location information:
- File path
- Line and column numbers
- Optional length for selections

## LSP Request Flow and Logic

### Initialization Process
1. The application starts by initializing the LSP client (`LspClient`)
2. Sends an `initialize` request with the root project URI
3. Waits for initialization response
4. Sends an `initialized` notification
5. Begins discovery of source files from compile_commands.json or directory walking

### Symbol and Call Graph Discovery
1. **Initial Loading**:
   - Workspace symbols are requested via `workspace_symbol` request
   - Symbols are parsed and converted to `FunctionNode` objects
   - Initial call graph is constructed with basic function information

2. **Lazy Loading Strategy**:
   - Only basic information is loaded at startup for quick response time
   - Detailed information is loaded on-demand as the user explores

3. **Call Hierarchy Exploration**:
   - When a function is selected:
     - `textDocument/prepareCallHierarchy` request prepares the function for analysis
     - `callHierarchy/outgoingCalls` requests discover function calls made by the selected function
     - Call graph is updated with new relationships
     - UI reflects the expanded information

4. **Reference Discovery**:
   - `textDocument/references` requests find all usage locations
   - References are added to the corresponding function nodes
   - Used to build a comprehensive view of how functions are used

5. **Document Analysis**:
   - `textDocument/documentSymbol` requests analyze specific files
   - File-specific information is extracted and integrated into the call graph

## Query and Analysis Capabilities

### Graph Traversal
- Finding all functions reachable from a given function
- Finding all functions that can reach a given function
- Measuring code distances (call steps) between functions

### Filtering
- By function name
- By file path or pattern
- By presence of errors/warnings

### Path Analysis
- Finding shortest paths between functions
- Analyzing call chains and dependency relationships

## UI Components and Interaction

### Main UI Components
1. **Function List**:
   - Displays all functions in the codebase
   - Supports filtering and searching
   - Selection triggers detailed information display

2. **Call Graph View**:
   - Visualizes caller/callee relationships
   - Split view showing functions that call the selected function and functions called by it
   - Supports navigation through the call hierarchy

3. **Diagnostic Panel**:
   - Displays errors and warnings associated with functions
   - Categorizes issues by severity
   - Links diagnostics to source locations

4. **Tree View**:
   - Hierarchical representation of call relationships
   - Expandable nodes with lazy-loading
   - Visual indicators for loading state and errors

### Action System
- Navigation through keyboard shortcuts
- Expand/collapse functionality for tree nodes
- Tab switching between different views
- Search and filtering capabilities

### Loading and State Management
- Visual feedback for ongoing LSP requests
- Tracking of pending requests
- Error handling and display
- Lazy loading to ensure responsiveness even with large codebases

## Performance Considerations

1. **Lazy Loading**:
   - Only load detailed information when requested
   - Initial startup focuses on basic structure
   - Background loading of additional details

2. **Request Management**:
   - Throttling of LSP requests to avoid overwhelming the server
   - Prioritization of user-focused requests
   - Caching of results to avoid redundant queries

3. **Memory Efficiency**:
   - Strategic storage of call graph information
   - Reuse of common data structures
   - Clear separation of concerns between components

## Current State and Usage

The project provides a terminal-based interface for exploring C/C++ codebases with these main features:
- Call graph visualization and navigation
- Function relationship analysis
- Error and warning identification
- Interactive code exploration

Usage involves:
1. Starting the application with a project path
2. Navigating the initial function list
3. Selecting functions to explore their relationships
4. Using keyboard shortcuts to navigate the call hierarchy
5. Searching for specific functions or patterns

The tool is particularly useful for:
- Understanding unfamiliar codebases
- Analyzing function dependencies
- Identifying problematic areas with diagnostics
- Planning refactoring efforts
