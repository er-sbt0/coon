//! The open-document set is bounded and disposable: clangd holds a live AST
//! worker and a resident preamble per open document, so the service evicts the
//! least recently used one past its limit and closes the lot once the initial
//! symbol sweep is in. Neither may cost the caller a request — a position-based
//! request against a document clangd no longer has must reopen it, not fail.

use lsp::{LspResponse, LspService};
use lsp_types::{Position, Url};
use serde_json::Value;
use tempfile::tempdir;
use tokio::sync::mpsc;

/// Comfortably more than the service's open-document limit, so the earliest
/// files are evicted while the preload is still running.
const FILE_COUNT: usize = 40;

const RESPONSE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(10);

/// `entry_<n>` sits on line 2 of every generated file.
const ENTRY_POSITION: Position = Position {
    line: 2,
    character: 5,
};

fn source(n: usize) -> String {
    format!("void helper_{n}(void) {{}}\n\nvoid entry_{n}(void) {{\n    helper_{n}();\n}}\n")
}

/// Waits for the response to `request_id`, discarding anything else.
async fn await_response(service: &mut LspService, request_id: &str) -> LspResponse {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, service.recv_response()).await {
            Ok(Some(response)) => {
                let id = match &response {
                    LspResponse::CallHierarchyPrepared { request_id, .. }
                    | LspResponse::OutgoingCalls { request_id, .. }
                    | LspResponse::PreloadComplete { request_id, .. }
                    | LspResponse::Error { request_id, .. } => request_id.clone(),
                    _ => continue,
                };
                if id == request_id {
                    return response;
                }
            }
            Ok(None) => panic!("LSP service channel closed while waiting for {request_id}"),
            Err(_) => panic!("timed out waiting for {request_id}"),
        }
    }
}

/// Resolves `entry_<n>`'s callees through the service, which must open the
/// document itself if clangd no longer has it.
async fn outgoing_call_names(service: &mut LspService, uri: &Url) -> Vec<String> {
    let prepare_id = uuid_like("prepare");
    service
        .request_prepare_call_hierarchy(prepare_id.clone(), uri.clone(), ENTRY_POSITION)
        .await
        .expect("prepare call hierarchy could not be sent");

    let items = match await_response(service, &prepare_id).await {
        LspResponse::CallHierarchyPrepared { items, .. } => items,
        other => panic!("expected a prepared call hierarchy for {uri}, got {other:?}"),
    };
    assert!(
        !items.is_empty(),
        "clangd resolved no call hierarchy item at {uri} — the document was not reopened"
    );

    let calls_id = uuid_like("calls");
    service
        .request_outgoing_calls(calls_id.clone(), items[0].clone())
        .await
        .expect("outgoing calls could not be sent");

    match await_response(service, &calls_id).await {
        LspResponse::OutgoingCalls { calls, .. } => {
            calls.into_iter().map(|call| call.to.name).collect()
        }
        other => panic!("expected outgoing calls for {uri}, got {other:?}"),
    }
}

fn uuid_like(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

#[tokio::test]
async fn a_closed_document_is_reopened_on_demand() {
    let temp_dir = tempdir().expect("Failed to create temporary directory");
    let project_root = temp_dir.path();

    let mut uris = Vec::with_capacity(FILE_COUNT);
    let mut entries = Vec::with_capacity(FILE_COUNT);
    for n in 0..FILE_COUNT {
        let file_path = project_root.join(format!("file_{n}.c"));
        std::fs::write(&file_path, source(n)).expect("Failed to write C file");
        let path = file_path.to_str().unwrap().replace('\\', "/");
        entries.push(format!(
            r#"  {{ "directory": "{}", "command": "gcc -c {path} -o file_{n}.o", "file": "{path}" }}"#,
            project_root.to_str().unwrap().replace('\\', "/")
        ));
        uris.push(Url::from_file_path(&file_path).unwrap());
    }
    std::fs::write(
        project_root.join("compile_commands.json"),
        format!("[\n{}\n]", entries.join(",\n")),
    )
    .expect("Failed to write compile_commands.json");

    let (tx, mut rx) = mpsc::channel::<Value>(100);
    let mut client = ::lsp::LspClient::new(tx).await.unwrap();

    let init_id = client
        .initialize(Url::from_file_path(project_root).unwrap())
        .await
        .unwrap();
    while let Some(msg) = rx.recv().await {
        if msg.get("id").and_then(|i| i.as_i64()) == Some(init_id) {
            break;
        }
    }
    client.send_initialized().await.unwrap();

    let mut service = LspService::new(client, rx).await.unwrap();

    // Opening more documents than the limit is what evicts the earliest ones.
    let preload_id = service.preload_documents(uris.clone()).await.unwrap();
    match await_response(&mut service, &preload_id).await {
        LspResponse::PreloadComplete { loaded_count, .. } => {
            assert_eq!(loaded_count, FILE_COUNT, "every document should have opened")
        }
        other => panic!("expected PreloadComplete, got {other:?}"),
    }

    // And this is the loader's post-sweep close, which drops the rest.
    service.close_documents().await.unwrap();

    // The first file was evicted by the limit, the last by the close. Both must
    // still answer, which they can only do if the service reopened them.
    for n in [0usize, FILE_COUNT - 1] {
        let names = outgoing_call_names(&mut service, &uris[n]).await;
        assert_eq!(
            names,
            vec![format!("helper_{n}")],
            "file_{n}.c did not resolve its callee after being closed"
        );
    }

    service.shutdown().await.unwrap();
}
