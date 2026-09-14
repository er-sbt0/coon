use std::sync::Arc;

use tokio::sync::Mutex;

use ::core::protocol::snapshot_of;
use ::core::CoreApp;

use crate::protocol::ServerMsg;

pub struct WebSession {
    pub core: Arc<Mutex<CoreApp>>,
}

impl WebSession {
    pub fn new(core: Arc<Mutex<CoreApp>>) -> Self {
        Self { core }
    }

    pub async fn snapshot_msg(&self) -> ServerMsg {
        snapshot_of(&*self.core.lock().await)
    }
}
