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
        // Read the direction here, at send time, and let the request carry it.
        // The core workspace is the authority: `t` dispatches
        // `Command::ToggleCallDirection`, so that is where the toggle lands.
        let direction = self.core.current_direction();
        if let Some(ev) = self
            .core
            .lsp
            .send_call_hierarchy(
                &self.core.call_graph,
                function_id,
                RequestOrigin::User,
                direction,
            )
            .event()
        {
            apply_event_to_status(&mut self.status_message, &ev);
        }
    }
}
