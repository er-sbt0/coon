use lsp_types::{self as lsp, Position, Url};
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_outgoing_calls_from_entry_point() {
    let temp_dir = tempdir().expect("Failed to create temporary directory");
    let project_root = temp_dir.path();

    // Create a simple C file with known function calls
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

    // Create compile_commands.json for clangd
    let compile_commands = format!(
        r#"[
  {{
    "directory": "{}",
    "command": "gcc -c {} -o test.o",
    "file": "{}"
  }}
]"#,
        project_root.to_str().unwrap().replace('\\', "/"),
        file_path.to_str().unwrap().replace('\\', "/"),
        file_path.to_str().unwrap().replace('\\', "/")
    );
    std::fs::write(project_root.join("compile_commands.json"), compile_commands)
        .expect("Failed to write compile_commands.json");

    // Create LSP client and channels
    let (tx, mut rx) = mpsc::channel::<Value>(100);
    let mut client = ::lsp::LspClient::new(tx).await.unwrap();

    let file_uri = Url::from_file_path(&file_path).unwrap();
    let root_uri = Url::from_file_path(project_root).unwrap();

    // Initialize the LSP server
    let init_id = client.initialize(root_uri).await.unwrap();

    // Wait for initialization response
    while let Some(msg) = rx.recv().await {
        if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
            break;
        }
    }

    client
        .send_notification("initialized", serde_json::json!({}))
        .await
        .unwrap();

    // Open the document
    let open_params = lsp_types::DidOpenTextDocumentParams {
        text_document: lsp_types::TextDocumentItem::new(
            file_uri.clone(),
            "c".to_string(),
            1,
            c_code.to_string(),
        ),
    };
    client
        .send_notification(
            "textDocument/didOpen",
            serde_json::to_value(open_params).unwrap(),
        )
        .await
        .unwrap();

    // Give clangd time to process the file
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Prepare call hierarchy for the "entry_point" function (line 4)
    let position = Position {
        line: 4,
        character: 5,
    }; // Position of "entry_point"
    let prep_id = client
        .prepare_call_hierarchy(file_uri.clone(), position)
        .await
        .unwrap();

    // Wait for prepare call hierarchy response
    let mut prep_response = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if msg.get("id").and_then(|i| i.as_i64()) == Some(prep_id) {
                    prep_response = Some(msg);
                    break;
                }
            }
            _ = &mut timeout => {
                panic!("Timeout waiting for prepare call hierarchy response");
            }
        }
    }

    // Parse the prepare call hierarchy response
    let prep_result = client
        .parse_prepare_call_hierarchy_response(&prep_response.unwrap())
        .unwrap();
    assert!(
        prep_result.is_some(),
        "Should have received a valid prepare call hierarchy response"
    );

    let hierarchy_items = prep_result.unwrap().items;
    assert!(
        !hierarchy_items.is_empty(),
        "Should have at least one call hierarchy item"
    );

    // The first item should be our entry_point function
    let entry_point_item = &hierarchy_items[0];
    assert_eq!(
        entry_point_item.name, "entry_point",
        "First item should be entry_point function"
    );

    // Get outgoing calls from entry_point
    let outgoing_id = client
        .get_outgoing_calls(entry_point_item.clone())
        .await
        .unwrap();

    // Wait for outgoing calls response
    let mut outgoing_response = None;
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                if msg.get("id").and_then(|i| i.as_i64()) == Some(outgoing_id) {
                    outgoing_response = Some(msg);
                    break;
                }
            }
            _ = &mut timeout => {
                panic!("Timeout waiting for outgoing calls response");
            }
        }
    }

    // Parse the outgoing calls response
    let outgoing_result = client
        .parse_outgoing_calls_response(&outgoing_response.unwrap())
        .unwrap();
    assert!(
        outgoing_result.is_some(),
        "Should have received a valid outgoing calls response"
    );

    let calls = outgoing_result.unwrap().calls;
    assert_eq!(calls.len(), 2, "Should find two outgoing calls");

    // Check that we have calls to both callee1 and callee2
    let call_names: Vec<String> = calls.iter().map(|call| call.to.name.clone()).collect();
    assert!(
        call_names.contains(&"callee1".to_string()),
        "Should find call to callee1"
    );
    assert!(
        call_names.contains(&"callee2".to_string()),
        "Should find call to callee2"
    );

    // Shutdown the LSP server
    let _ = client.send_request("shutdown", Value::Null).await.unwrap();
    let _ = client.send_notification("exit", Value::Null).await;
}
