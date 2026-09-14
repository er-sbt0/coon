use ::core::RequestOrigin;
use model::SymbolId;

use super::event_translate::apply_event_to_status;
use super::App;

impl App {
    /// Request call hierarchy for a function.
    ///
    /// What is left of this module. The rest was a second copy of the response
    /// handling in `CoreApp`, plus pass-throughs to `core.lsp` that everything
    /// reached directly anyway.
    pub fn request_call_hierarchy(&mut self, function_id: &SymbolId) {
        if let Some(ev) = self.core.lsp.send_call_hierarchy(
            &self.core.call_graph,
            function_id,
            RequestOrigin::User,
        ) {
            apply_event_to_status(&mut self.status_message, &ev);
        }
    }
}
