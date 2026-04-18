use model::SymbolId;

use super::lsp_bridge::LoadingState;
use super::App;

impl App {
    // LSP integration methods — delegate to self.lsp (LspBridge)

    /// Request call hierarchy for a function
    pub fn request_call_hierarchy(&mut self, function_id: &SymbolId) {
        if let Some(msg) = self.lsp.send_call_hierarchy(&self.call_graph, function_id) {
            self.status_message = msg;
        }
    }

    /// Request references for a function
    pub fn request_references(&mut self, function_id: &SymbolId) {
        if let Some(msg) = self.lsp.send_references(&self.call_graph, function_id) {
            self.status_message = msg;
        }
    }

    /// Request fresh workspace symbols from LSP server
    pub fn request_workspace_symbols(&mut self) {
        if let Some(msg) = self.lsp.send_workspace_symbols() {
            self.status_message = msg;
        }
    }

    /// Check for LSP responses and update state
    pub fn check_lsp_responses(&mut self) {
        let (responses, timeout_messages) = self.lsp.drain_responses();

        // Apply timeout status messages
        for msg in timeout_messages {
            self.status_message = msg;
        }

        // Process each response
        for response in responses {
            self.handle_lsp_response(response);
        }
    }

    /// Check if a function is already loaded
    pub fn is_function_loaded(&self, symbol_id: &SymbolId) -> bool {
        self.lsp.is_function_loaded(symbol_id)
    }

    /// Check if a function is currently loading
    pub fn is_function_loading(&self, symbol_id: &SymbolId) -> bool {
        self.lsp.is_function_loading(symbol_id)
    }

    /// Get the loading state of a function
    pub fn get_loading_state(&self, symbol_id: &SymbolId) -> &LoadingState {
        self.lsp.get_loading_state(symbol_id)
    }

    /// Update loading state for a symbol and corresponding UI elements
    pub(super) fn update_loading_state(&mut self, symbol_id: &SymbolId, state: LoadingState) {
        self.lsp.set_loading_state(symbol_id.clone(), state.clone());

        match state {
            LoadingState::Loading => {
                self.status_message = "Loading data for function...".to_string();
            }
            LoadingState::Loaded => {
                self.status_message = String::new();
            }
            LoadingState::Failed(error) => {
                self.status_message = format!("Error: {}", error);
            }
            LoadingState::NotLoaded => {}
        }
    }
}
