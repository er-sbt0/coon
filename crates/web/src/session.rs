use std::sync::Arc;

use tokio::sync::Mutex;

use ::core::protocol::snapshot_of;
use ::core::CoreApp;

pub struct WebSession {
    pub core: Arc<Mutex<CoreApp>>,
}

impl WebSession {
    pub fn new(core: Arc<Mutex<CoreApp>>) -> Self {
        Self { core }
    }

    /// The current state, already serialised.
    ///
    /// Serialised here, under the lock, rather than handed back as a message:
    /// a `ServerMsg` borrows the app it describes, so it cannot outlive the
    /// guard — which is the point. The alternative was copying the whole call
    /// graph out first so the guard could be dropped, which is the copy this
    /// was written to avoid.
    pub async fn snapshot_json(&self) -> Option<String> {
        let core = self.core.lock().await;
        match serde_json::to_string(&snapshot_of(&core)) {
            Ok(text) => Some(text),
            Err(e) => {
                log::error!("[web] failed to serialise snapshot: {}", e);
                None
            }
        }
    }
}
