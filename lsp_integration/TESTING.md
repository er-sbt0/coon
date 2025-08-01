# Testing the `lsp_integration` Crate

This guide outlines the strategy for writing and running tests for the `lsp_integration` crate. The goal is to ensure our Language Server Protocol (LSP) client communicates correctly with `clangd` and accurately translates LSP responses into our internal `core_data` types.

## Testing Philosophy

Instead of mocking the LSP server, we test against a **real `clangd` instance**. This provides high-fidelity testing and ensures our client works with the actual tool it's designed to integrate with.

Each test follows these steps:
1.  **Isolate**: Create a temporary directory to act as a mock project root.
2.  **Scaffold**: Populate the directory with a simple C source file (`.c`) and a corresponding `compile_commands.json`.
3.  **Execute**: Launch `clangd` as a child process, with its working directory set to the temporary project root.
4.  **Communicate**: Instantiate our `LspClient` and perform the LSP communication flow (initialize, open file, request call hierarchy, etc.).
5.  **Assert**: Verify that the data returned by the client matches the expected call graph from the mock C code.
6.  **Cleanup**: The temporary directory is automatically removed on test completion.

## Prerequisites

Ensure `clangd` is installed and accessible in your system's `PATH`. The tests will fail if they cannot find the `clangd` executable.

## How to Write a New Test

Here is a step-by-step guide to creating a new integration test.

### 1. Test Setup

Use the `tempfile` crate to create a temporary directory. This ensures each test runs in a clean, isolated environment.

```rust
use tempfile::tempdir;
let temp_dir = tempdir().expect("Failed to create temporary directory");
let project_root = temp_dir.path();
```

### 2. Create Mock Source Code

Write a simple C file containing the function calls you want to test.

```rust
let c_code = r#"
void callee1() {}
void callee2() {}

void entry_point() {
    callee1();
    callee2();
}
"#;
let file_path = project_root.join("test.c");
std::fs::write(&file_path, c_code).expect("Failed to write C file");
```

### 3. Create `compile_commands.json`

`clangd` requires a `compile_commands.json` file to understand how to analyze the source files. Create one that points to your mock C file.

```rust
let compile_commands = format!(
    r#"[
  {{
    "directory": "{}",
    "command": "gcc -c {} -o test.o",
    "file": "{}"
  }}
]"#,
    project_root.to_str().unwrap().replace("\\", "/"), // JSON-friendly path
    file_path.to_str().unwrap(),
    file_path.to_str().unwrap()
);
std::fs::write(project_root.join("compile_commands.json"), compile_commands)
    .expect("Failed to write compile_commands.json");
```

### 4. Launch `clangd` and the `LspClient`

You'll need a helper function to spawn the `clangd` process and create an `LspClient` instance to communicate with it.

*(This functionality should be part of the `lsp_integration` crate's test helpers).*

### 5. Perform LSP Communication

Use the client to initialize the server, open the document, and request the call hierarchy.

```rust
// Assume 'client' is an initialized LspClient instance

// 1. Open the document
client.text_document_did_open("file:///path/to/your/test.c").await?;

// 2. Prepare the call hierarchy at the entry_point function
let position = Position { line: 4, character: 5 }; // Position of "entry_point"
let hierarchy_items = client.prepare_call_hierarchy(file_uri, position).await?;

// 3. Get outgoing calls from the entry_point
let calls = client.get_outgoing_calls(hierarchy_items[0].clone()).await?;
```

### 6. Assert the Results

Check that the returned data is correct.

```rust
assert_eq!(calls.len(), 2);
assert!(calls.iter().any(|item| item.to.name == "callee1"));
assert!(calls.iter().any(|item| item.to.name == "callee2"));
```

### Full Example

Here is a complete example of what a test function could look like. It's recommended to place this inside the `lsp_integration` crate's tests module.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use tempfile::tempdir;
    use tokio::runtime::Runtime;
    use lsp_types::{Position, Url};
    use crate::client::LspClient; // Assuming you have a client module

    // A helper to set up the test environment and client would be ideal
    
    #[test]
    fn test_outgoing_calls_from_entry_point() {
        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            // 1. Setup
            let temp_dir = tempdir().unwrap();
            let project_root = temp_dir.path();
            
            let file_path = project_root.join("test.c");
            let file_uri = Url::from_file_path(&file_path).unwrap();

            let c_code = r#"
void callee1() {}
void callee2() {}

void entry_point() {
    callee1();
    callee2();
}
"#;
            std::fs::write(&file_path, c_code).unwrap();

            let compile_commands = format!(
                r#"[{{ "directory": "{}", "command": "gcc -c {} -o test.o", "file": "{}" }}]"#,
                project_root.to_str().unwrap().replace("\\", "/"),
                file_path.to_str().unwrap(),
                file_path.to_str().unwrap()
            );
            std::fs::write(project_root.join("compile_commands.json"), compile_commands).unwrap();

            // 2. Launch clangd and client
            // Note: This part is simplified. You need a robust way to manage the process and streams.
            let mut process = Command::new("clangd")
                .current_dir(project_root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("Failed to start clangd");

            let stdin = process.stdin.take().unwrap();
            let stdout = process.stdout.take().unwrap();
            
            let mut client = LspClient::new(stdin, stdout);
            
            // 3. Communicate
            client.initialize(project_root.to_str().unwrap()).await.unwrap();
            client.text_document_did_open(file_uri.as_str()).await.unwrap();

            let position = Position { line: 4, character: 5 }; // Position of "entry_point"
            let hierarchy_items = client.prepare_call_hierarchy(file_uri.clone(), position).await.unwrap();
            
            assert!(!hierarchy_items.is_empty(), "Prepare call hierarchy should return at least one item");

            let calls = client.get_outgoing_calls(hierarchy_items[0].clone()).await.unwrap();

            // 4. Assert
            assert_eq!(calls.len(), 2, "Should find two outgoing calls");
            assert!(
                calls.iter().any(|call| call.to.name == "callee1"),
                "callee1 should be in the list of outgoing calls"
            );
            assert!(
                calls.iter().any(|call| call.to.name == "callee2"),
                "callee2 should be in the list of outgoing calls"
            );

            // 5. Shutdown
            client.shutdown().await.unwrap();
            process.wait().unwrap();
        });
    }
}
```
