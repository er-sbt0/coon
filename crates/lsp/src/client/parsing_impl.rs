use anyhow::Result;
use serde_json::Value;

use crate::types::*;

use super::LspClient;

impl LspClient {
    /// Parse find_references response and convert to our data structures
    pub fn parse_find_references_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<FindReferencesResponse>> {
        log::info!(
            "parse_find_references_response called with response: {}",
            response
        );
        let result = crate::parsing::parse_find_references_response_impl(
            &mut self.pending_requests,
            response,
        );
        log::info!(
            "parse_find_references_response returning: {:?}",
            result
                .as_ref()
                .map(|r| r.as_ref().map(|resp| resp.locations.len()))
        );
        result
    }

    /// Parse workspace symbol response and convert to our data structures
    pub fn parse_workspace_symbol_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        crate::parsing::parse_workspace_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse document symbol response and convert to our data structures
    pub fn parse_document_symbol_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<DocumentSymbolResponse>> {
        crate::parsing::parse_document_symbol_response_impl(&mut self.pending_requests, response)
    }

    /// Parse hover response
    pub fn parse_hover_response(&mut self, response: &Value) -> Result<Option<HoverResponse>> {
        crate::parsing::parse_hover_response_impl(&mut self.pending_requests, response)
    }

    /// Parse prepare call hierarchy response
    pub fn parse_prepare_call_hierarchy_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<PrepareCallHierarchyResponse>> {
        crate::call_hierarchy::parse_prepare_call_hierarchy_response_impl(
            &mut self.pending_requests,
            response,
        )
    }

    /// Parse outgoing calls response
    pub fn parse_outgoing_calls_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<OutgoingCallsResponse>> {
        crate::call_hierarchy::parse_outgoing_calls_response_impl(
            &mut self.pending_requests,
            response,
        )
    }

    /// Parse incoming calls response
    pub fn parse_incoming_calls_response(
        &mut self,
        response: &Value,
    ) -> Result<Option<IncomingCallsResponse>> {
        crate::call_hierarchy::parse_incoming_calls_response_impl(
            &mut self.pending_requests,
            response,
        )
    }
}
