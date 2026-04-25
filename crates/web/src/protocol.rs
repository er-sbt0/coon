use std::collections::HashMap;

use model::{CallDirection, CallGraph, SymbolId};
use serde::{Deserialize, Serialize};

use ::core::layout::NodePos;
use ::core::{AppEvent, Command};

/// Messages the server sends to the browser.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Snapshot {
        graph: CallGraph,
        layout: HashMap<SymbolId, NodePos>,
        workspaces: Vec<WorkspaceInfo>,
        selected: Option<SymbolId>,
    },
    Event {
        event: AppEvent,
    },
    Layout {
        layout: HashMap<SymbolId, NodePos>,
    },
}

/// Messages the browser sends to the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Command { command: Command },
}

#[derive(Debug, Serialize)]
pub struct WorkspaceInfo {
    pub id: usize,
    pub name: String,
    pub direction: CallDirection,
    pub selected: Option<SymbolId>,
}
