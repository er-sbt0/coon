// Updated call_hierarchy.rs with import for parent module
use anyhow::Result;
use log;
use lsp_types as lsp;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::{IncomingCallsResponse, OutgoingCallsResponse, PrepareCallHierarchyResponse};

/// Helper function to parse prepare call hierarchy response
pub(crate) fn parse_prepare_call_hierarchy_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<PrepareCallHierarchyResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "textDocument/prepareCallHierarchy" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!(
                        "LSP error for textDocument/prepareCallHierarchy: {:?}",
                        error
                    );
                    return Ok(Some(PrepareCallHierarchyResponse {
                        request_id: id,
                        items: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(PrepareCallHierarchyResponse {
                            request_id: id,
                            items: Vec::new(),
                        }));
                    }

                    // Try to parse the result as an array of CallHierarchyItem objects
                    match Vec::<lsp::CallHierarchyItem>::deserialize(result) {
                        Ok(items) => {
                            return Ok(Some(PrepareCallHierarchyResponse {
                                request_id: id,
                                items,
                            }));
                        }
                        Err(parse_error) => {
                            let error_msg = format!(
                                "Failed to parse prepareCallHierarchy result: {}. Raw result: {:?}",
                                parse_error, result
                            );
                            log::error!("{}", error_msg);
                            return Err(anyhow::anyhow!(error_msg));
                        }
                    }
                }

                // If we get here, there was no result but also no error
                log::warn!("No result in prepareCallHierarchy response");
                return Ok(Some(PrepareCallHierarchyResponse {
                    request_id: id,
                    items: Vec::new(),
                }));
            }
        }
    }

    // This response doesn't match any pending prepareCallHierarchy request
    Ok(None)
}

/// Helper function to parse outgoing calls response
pub(crate) fn parse_outgoing_calls_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<OutgoingCallsResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "callHierarchy/outgoingCalls" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for callHierarchy/outgoingCalls: {:?}", error);
                    return Ok(Some(OutgoingCallsResponse {
                        request_id: id,
                        calls: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(OutgoingCallsResponse {
                            request_id: id,
                            calls: Vec::new(),
                        }));
                    }

                    // Try to parse the result as an array of CallHierarchyOutgoingCall objects
                    match Vec::<lsp::CallHierarchyOutgoingCall>::deserialize(result) {
                        Ok(calls) => {
                            return Ok(Some(OutgoingCallsResponse {
                                request_id: id,
                                calls,
                            }));
                        }
                        Err(parse_error) => {
                            let error_msg = format!(
                                "Failed to parse outgoingCalls result: {}. Raw result: {:?}",
                                parse_error, result
                            );
                            log::error!("{}", error_msg);
                            return Err(anyhow::anyhow!(error_msg));
                        }
                    }
                }

                // If we get here, there was no result but also no error
                log::warn!("No result in outgoingCalls response");
                return Ok(Some(OutgoingCallsResponse {
                    request_id: id,
                    calls: Vec::new(),
                }));
            }
        }
    }

    // This response doesn't match any pending outgoingCalls request
    Ok(None)
}

/// Helper function to parse incoming calls response
pub(crate) fn parse_incoming_calls_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<IncomingCallsResponse>> {
    if let Some(id) = response.get("id").and_then(|v| v.as_i64()) {
        if let Some(method) = pending_requests.remove(&id) {
            if method == "callHierarchy/incomingCalls" {
                // Check if this is an error response
                if let Some(error) = response.get("error") {
                    log::warn!("LSP error for callHierarchy/incomingCalls: {:?}", error);
                    return Ok(Some(IncomingCallsResponse {
                        request_id: id,
                        calls: Vec::new(),
                    }));
                }

                if let Some(result) = response.get("result") {
                    if result.is_null() {
                        return Ok(Some(IncomingCallsResponse {
                            request_id: id,
                            calls: Vec::new(),
                        }));
                    }

                    // Try to parse the result as an array of CallHierarchyIncomingCall objects
                    match Vec::<lsp::CallHierarchyIncomingCall>::deserialize(result) {
                        Ok(calls) => {
                            return Ok(Some(IncomingCallsResponse {
                                request_id: id,
                                calls,
                            }));
                        }
                        Err(parse_error) => {
                            let error_msg = format!(
                                "Failed to parse incomingCalls result: {}. Raw result: {:?}",
                                parse_error, result
                            );
                            log::error!("{}", error_msg);
                            return Err(anyhow::anyhow!(error_msg));
                        }
                    }
                }

                // If we get here, there was no result but also no error
                log::warn!("No result in incomingCalls response");
                return Ok(Some(IncomingCallsResponse {
                    request_id: id,
                    calls: Vec::new(),
                }));
            }
        }
    }

    // This response doesn't match any pending incomingCalls request
    Ok(None)
}
