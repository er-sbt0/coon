use crate::service::document;
use crate::service::worker::{EnhancedRequestInfo, LspWorkerState, RequestType};
use crate::service::LspResponse;
use serde_json::Value;
use tokio::sync::mpsc;

pub(super) async fn handle_references_response(
    message: Value,
    request_id: String,
    _state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if super::check_and_send_lsp_error(&message, &request_id, "references", response_tx).await {
        return;
    }

    let locations = parse_references_response_content(&message);
    log::debug!(
        "LSP References Response: found {} references for request {}",
        locations.len(),
        request_id
    );
    for (i, loc) in locations.iter().enumerate() {
        log::trace!(
            "  Reference {}: {}:{}:{}",
            i,
            loc.file_path,
            loc.line,
            loc.column
        );
    }
    let _ = response_tx
        .send(LspResponse::References {
            request_id,
            locations,
        })
        .await;
}

pub(super) async fn handle_references_with_symbols_response(
    message: Value,
    request_id: String,
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    if super::check_and_send_lsp_error(&message, &request_id, "enhanced references", response_tx)
        .await
    {
        return;
    }

    let enhanced_references =
        parse_enhanced_references_response(&message, &request_id, state, response_tx).await;

    if !enhanced_references.is_empty() {
        log::debug!(
            "LSP Enhanced References Response: found {} references for request {}",
            enhanced_references.len(),
            request_id
        );
        for (i, ref_info) in enhanced_references.iter().enumerate() {
            if let Some(symbol) = &ref_info.referencing_symbol {
                log::trace!(
                    "  Enhanced Reference {}: {}:{}:{} (from {}::{})",
                    i,
                    ref_info.location.file_path,
                    ref_info.location.line,
                    ref_info.location.column,
                    symbol.qualified_name,
                    symbol.name
                );
            } else {
                log::trace!(
                    "  Enhanced Reference {}: {}:{}:{} (no symbol info)",
                    i,
                    ref_info.location.file_path,
                    ref_info.location.line,
                    ref_info.location.column
                );
            }
        }
        let _ = response_tx
            .send(LspResponse::ReferencesWithSymbols {
                request_id,
                references: enhanced_references,
            })
            .await;
    }
}

pub(super) async fn handle_document_symbols_for_enhanced_references(
    base_request_id: &str,
    document_symbols: &[lsp_types::DocumentSymbol],
    state: &mut LspWorkerState,
    response_tx: &mpsc::Sender<LspResponse>,
) {
    log::debug!(
        "Processing document symbols for enhanced references request: {}",
        base_request_id
    );

    if let Some(pending_request) = state.pending_enhanced_requests.get(base_request_id) {
        let mut enhanced_references = Vec::new();

        for location in &pending_request.locations {
            let position = lsp_types::Position {
                line: location.line.saturating_sub(1),
                character: location.column.saturating_sub(1),
            };

            if let Some(containing_symbol) =
                document::find_containing_symbol(document_symbols, &position)
            {
                enhanced_references.push(model::Reference {
                    location: location.clone(),
                    referencing_symbol: Some(model::ReferencingSymbol {
                        name: containing_symbol.name.clone(),
                        qualified_name: document::get_qualified_symbol_name(containing_symbol),
                        kind: document::convert_lsp_symbol_kind(containing_symbol.kind),
                    }),
                });
                log::debug!(
                    "Found containing symbol '{}' for reference at {}:{}:{}",
                    containing_symbol.name,
                    location.file_path,
                    location.line,
                    location.column
                );
            } else {
                enhanced_references.push(model::Reference {
                    location: location.clone(),
                    referencing_symbol: None,
                });
                log::debug!(
                    "No containing symbol found for reference at {}:{}:{}",
                    location.file_path,
                    location.line,
                    location.column
                );
            }
        }

        let response = LspResponse::ReferencesWithSymbols {
            request_id: base_request_id.to_string(),
            references: enhanced_references,
        };

        if let Err(e) = response_tx.send(response).await {
            log::error!("Failed to send enhanced references response: {:?}", e);
        } else {
            log::debug!(
                "Sent enhanced references response for request {}",
                base_request_id
            );
        }

        state.pending_enhanced_requests.remove(base_request_id);
    } else {
        log::warn!(
            "No pending request found for enhanced references: {}",
            base_request_id
        );
    }
}

pub(crate) fn is_references_response(response: &Value) -> bool {
    if let Some(result) = response.get("result") {
        if result.is_null() {
            return true;
        }
        // Check structural markers instead of cloning + deserializing:
        // a references response is an array of objects each with "uri" and "range" keys.
        if let Some(arr) = result.as_array() {
            return arr.is_empty()
                || arr
                    .first()
                    .and_then(|item| item.as_object())
                    .is_some_and(|obj| obj.contains_key("uri") && obj.contains_key("range"));
        }
    }
    false
}

pub(super) fn parse_references_response_content(response: &Value) -> Vec<model::Location> {
    if let Some(result) = response.get("result") {
        if result.is_null() {
            log::debug!("References response has null result, returning empty vec");
            return Vec::new();
        }
        match serde_json::from_value::<Vec<lsp_types::Location>>(result.clone()) {
            Ok(lsp_locations) => {
                log::debug!("Successfully parsed {} LSP locations", lsp_locations.len());
                lsp_locations
                    .iter()
                    .map(crate::convert_lsp_location)
                    .collect()
            }
            Err(e) => {
                log::error!("Failed to parse references response: {}", e);
                Vec::new()
            }
        }
    } else {
        log::debug!("References response has no result field");
        Vec::new()
    }
}

pub(crate) fn was_enhanced_references_request(message: &Value, state: &LspWorkerState) -> bool {
    if let Some(id) = message.get("id").and_then(|i| i.as_i64()) {
        let is_enhanced = state.enhanced_lsp_requests.contains(&id);
        log::debug!(
            "Checking if request {} is enhanced: {} (tracked enhanced requests: {:?})",
            id,
            is_enhanced,
            state.enhanced_lsp_requests
        );
        return is_enhanced;
    }
    log::debug!("No request ID found in message for enhanced check");
    false
}

async fn parse_enhanced_references_response(
    message: &Value,
    service_request_id: &str,
    state: &mut LspWorkerState,
    _response_tx: &mpsc::Sender<LspResponse>,
) -> Vec<model::Reference> {
    let locations = parse_references_response_content(message);
    let _lsp_request_id = message.get("id").and_then(|i| i.as_i64());

    log::debug!(
        "Enhancing {} reference locations with symbol information using hover requests",
        locations.len()
    );

    if locations.is_empty() {
        return Vec::new();
    }

    log::debug!("Using service request ID: {}", service_request_id);

    state.pending_enhanced_requests.insert(
        service_request_id.to_string(),
        EnhancedRequestInfo {
            locations: locations.clone(),
        },
    );

    let mut files_to_analyze: std::collections::HashSet<String> = std::collections::HashSet::new();
    for location in locations.iter() {
        files_to_analyze.insert(location.file_path.clone());
    }

    log::debug!(
        "Need to analyze {} unique files for symbol information",
        files_to_analyze.len()
    );

    for file_path in files_to_analyze {
        if let Ok(document_uri) = lsp_types::Url::from_file_path(&file_path) {
            let text_document = lsp_types::TextDocumentIdentifier { uri: document_uri };
            match state.client.document_symbol(text_document).await {
                Ok(lsp_request_id) => {
                    state.track_request(
                        lsp_request_id,
                        service_request_id.to_string(),
                        RequestType::DocumentSymbolsForEnhancedRefs {
                            base_request_id: service_request_id.to_string(),
                        },
                    );
                    state.enhanced_lsp_requests.insert(lsp_request_id);
                    log::debug!(
                        "Sent document symbol request for {} (lsp_request_id: {})",
                        file_path,
                        lsp_request_id
                    );
                }
                Err(e) => {
                    log::error!(
                        "Failed to send document symbol request for {}: {:?}",
                        file_path,
                        e
                    );
                }
            }
        } else {
            log::error!("Failed to convert file path to URI: {}", file_path);
        }
    }

    Vec::new()
}
