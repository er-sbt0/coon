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

    /// Enhance references with symbol information
    pub async fn enhance_references(
        &mut self,
        references: Vec<model::Location>,
    ) -> Result<Vec<model::Reference>> {
        let mut enhanced_references = Vec::new();

        for location in references {
            // Try to extract a meaningful symbol name from the file context
            let referencing_symbol = self
                .resolve_symbol_at_location(&location)
                .await
                .unwrap_or_else(|| {
                    // Fallback to a descriptive name showing location
                    let filename = std::path::Path::new(&location.file_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");

                    model::ReferencingSymbol {
                        name: format!("{}:{}", filename, location.line),
                        qualified_name: format!(
                            "{}::{}:{}",
                            filename, location.line, location.column
                        ),
                        kind: model::ReferenceSymbolKind::Function,
                    }
                });

            enhanced_references.push(model::Reference {
                location,
                referencing_symbol: Some(referencing_symbol),
            });
        }

        Ok(enhanced_references)
    }

    /// Attempt to resolve the symbol at a given location
    async fn resolve_symbol_at_location(
        &mut self,
        location: &model::Location,
    ) -> Option<model::ReferencingSymbol> {
        // For now, create a basic symbol name
        // In a full implementation, this would:
        // 1. Request document symbols for the file
        // 2. Find the symbol containing the location
        // 3. Extract the actual symbol name

        let filename = std::path::Path::new(&location.file_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        // Create a more meaningful name format
        Some(model::ReferencingSymbol {
            name: format!("caller_at_{}:{}", filename, location.line),
            qualified_name: format!(
                "{}::caller_at_{}:{}",
                filename, location.line, location.column
            ),
            kind: model::ReferenceSymbolKind::Function,
        })
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
