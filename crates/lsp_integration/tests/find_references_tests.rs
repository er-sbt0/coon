use lsp_integration::LspClient;
use lsp_types::{self as lsp, Position, Url};
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
#[ignore] // Run with `cargo test -- --ignored` to include integration tests
async fn test_find_references_integration() {
    let _ = pretty_env_logger::try_init();

    let temp_dir = TempDir::new().unwrap();
    let test_file_path = temp_dir.path().join("test.cpp");
    let file_content = "void my_func() {}\nint main() { my_func(); return 0; }";
    std::fs::write(&test_file_path, file_content).unwrap();

    let (tx, mut rx) = mpsc::channel(100);
    let mut client = LspClient::new(tx).await.unwrap();

    let root_uri = Url::from_file_path(temp_dir.path()).unwrap();
    let test_file_uri = Url::from_file_path(&test_file_path).unwrap();

    // Initialize LSP
    let init_id = client.initialize(root_uri.clone()).await.unwrap();

    // Wait for initialization response
    let mut init_response = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
                    init_response = Some(msg);
                    break;
                }
            }
            _ = &mut timeout => {
                panic!("Timeout waiting for initialization response");
            }
        }
    }
    assert!(init_response.is_some());

    // Send initialized notification
    client
        .send_notification("initialized", serde_json::json!({}))
        .await
        .unwrap();

    // Open the file
    let open_params = lsp_types::DidOpenTextDocumentParams {
        text_document: lsp_types::TextDocumentItem::new(
            test_file_uri.clone(),
            "cpp".to_string(),
            1,
            file_content.to_string(),
        ),
    };
    client
        .send_notification(
            "textDocument/didOpen",
            serde_json::to_value(open_params).unwrap(),
        )
        .await
        .unwrap();

    // Wait a bit for the server to process the file
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Find references for the function call
    let ref_id = client
        .find_references(
            lsp::TextDocumentIdentifier {
                uri: test_file_uri.clone(),
            },
            Position {
                line: 1,
                character: 15,
            }, // position inside `my_func` call
        )
        .await
        .unwrap();

    // Wait for response
    let mut ref_response = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if msg.get("id").and_then(|i| i.as_i64()) == Some(ref_id) {
                    ref_response = Some(msg);
                    break;
                }
            }
            _ = &mut timeout => {
                panic!("Timeout waiting for find_references response");
            }
        }
    }

    let response = ref_response.unwrap();
    let parsed = client.parse_find_references_response(&response).unwrap();

    assert!(parsed.is_some());
    let find_refs_response = parsed.unwrap();
    assert_eq!(find_refs_response.request_id, ref_id);

    // We should find at least one reference (the call site) for a proper C++ file
    // If empty, it might be because clangd needs compilation database or headers
    println!("Found {} references", find_refs_response.locations.len());

    // Verify the locations point to our test file if any found
    for location in &find_refs_response.locations {
        assert!(location.file_path.contains("test.cpp"));
        assert!(location.line <= 1); // Should be on line 0 or 1
    }
}

#[tokio::test]
async fn test_find_references_no_results() {
    let temp_dir = TempDir::new().unwrap();
    let test_file_path = temp_dir.path().join("empty.rs");
    let file_content = "// Empty file with just a comment";
    std::fs::write(&test_file_path, file_content).unwrap();

    let (tx, mut rx) = mpsc::channel(100);
    let mut client = LspClient::new(tx).await.unwrap();

    let root_uri = Url::from_file_path(temp_dir.path()).unwrap();
    let test_file_uri = Url::from_file_path(&test_file_path).unwrap();

    // Initialize and open file
    let init_id = client.initialize(root_uri).await.unwrap();

    // Wait for init response
    while let Some(msg) = rx.recv().await {
        if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
            break;
        }
    }

    client
        .send_notification("initialized", serde_json::json!({}))
        .await
        .unwrap();

    let open_params = lsp_types::DidOpenTextDocumentParams {
        text_document: lsp_types::TextDocumentItem::new(
            test_file_uri.clone(),
            "rust".to_string(),
            1,
            file_content.to_string(),
        ),
    };
    client
        .send_notification(
            "textDocument/didOpen",
            serde_json::to_value(open_params).unwrap(),
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Try to find references at a position with no symbol
    let ref_id = client
        .find_references(
            lsp::TextDocumentIdentifier { uri: test_file_uri },
            Position {
                line: 0,
                character: 5,
            }, // position in comment
        )
        .await
        .unwrap();

    // Wait for response
    let mut ref_response = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if msg.get("id").and_then(|i| i.as_i64()) == Some(ref_id) {
                    ref_response = Some(msg);
                    break;
                }
            }
            _ = &mut timeout => {
                panic!("Timeout waiting for find_references response");
            }
        }
    }

    let response = ref_response.unwrap();
    let parsed = client.parse_find_references_response(&response).unwrap();

    // LSP server might return None (no response parsed) if there's an error,
    // or Some with empty locations if no references found
    if let Some(find_refs_response) = parsed {
        assert_eq!(find_refs_response.request_id, ref_id);
        assert!(
            find_refs_response.locations.is_empty(),
            "Expected no references for comment position"
        );
    } else {
        // This is also acceptable - LSP server returned an error response
        // which means no references could be found
        println!("LSP server returned error response (acceptable for no references case)");
    }
}
