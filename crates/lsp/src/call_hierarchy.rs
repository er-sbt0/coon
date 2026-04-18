use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;

use crate::parsing::parse_lsp_response;
use crate::{IncomingCallsResponse, OutgoingCallsResponse, PrepareCallHierarchyResponse};

/// Parse prepare call hierarchy response
pub(crate) fn parse_prepare_call_hierarchy_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<PrepareCallHierarchyResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "textDocument/prepareCallHierarchy",
        |id, items| PrepareCallHierarchyResponse {
            request_id: id,
            items,
        },
        |id| PrepareCallHierarchyResponse {
            request_id: id,
            items: Vec::new(),
        },
    )
}

/// Parse outgoing calls response
pub(crate) fn parse_outgoing_calls_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<OutgoingCallsResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "callHierarchy/outgoingCalls",
        |id, calls| OutgoingCallsResponse {
            request_id: id,
            calls,
        },
        |id| OutgoingCallsResponse {
            request_id: id,
            calls: Vec::new(),
        },
    )
}

/// Parse incoming calls response
pub(crate) fn parse_incoming_calls_response_impl(
    pending_requests: &mut HashMap<i64, String>,
    response: &Value,
) -> Result<Option<IncomingCallsResponse>> {
    parse_lsp_response(
        pending_requests,
        response,
        "callHierarchy/incomingCalls",
        |id, calls| IncomingCallsResponse {
            request_id: id,
            calls,
        },
        |id| IncomingCallsResponse {
            request_id: id,
            calls: Vec::new(),
        },
    )
}
