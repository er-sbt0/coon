use std::sync::Arc;

use tokio::sync::Mutex;

use ::core::CoreApp;

use crate::protocol::{ServerMsg, WorkspaceInfo};

pub struct WebSession {
    pub core: Arc<Mutex<CoreApp>>,
}

impl WebSession {
    pub fn new(core: Arc<Mutex<CoreApp>>) -> Self {
        Self { core }
    }

    pub async fn snapshot_msg(&self) -> ServerMsg {
        let core = self.core.lock().await;
        let ws = core.workspaces.current();
        let root = ws.and_then(|w| w.root_symbol);
        let direction = ws
            .map(|w| w.direction)
            .unwrap_or(model::CallDirection::Outgoing);
        // Use layout_focus (set by ExpandFunction) as the BFS origin when present,
        // so the layout window shifts to the expanded node rather than staying
        // anchored to the workspace root.
        let layout_root = core.layout_focus.or(root);
        let layout = ::core::compute_layout(&core.call_graph, layout_root, direction);

        log::info!(
            "[web] snapshot: nodes={} edges={} layout_nodes={} root={:?} layout_focus={:?} direction={:?} selected={:?}",
            core.call_graph.node_count(),
            core.call_graph.edge_count(),
            layout.len(),
            root,
            core.layout_focus,
            direction,
            core.selected_function,
        );

        ServerMsg::Snapshot {
            graph: core.call_graph.clone(),
            layout,
            workspaces: workspace_infos(&core),
            selected: core.selected_function,
        }
    }

    pub async fn layout_msg(&self) -> ServerMsg {
        let core = self.core.lock().await;
        let ws = core.workspaces.current();
        let root = ws.and_then(|w| w.root_symbol);
        let direction = ws
            .map(|w| w.direction)
            .unwrap_or(model::CallDirection::Outgoing);
        ServerMsg::Layout {
            layout: ::core::compute_layout(&core.call_graph, root, direction),
        }
    }
}

fn workspace_infos(core: &CoreApp) -> Vec<WorkspaceInfo> {
    core.workspaces
        .workspaces
        .iter()
        .map(|ws| WorkspaceInfo {
            id: ws.id,
            name: ws.name.clone(),
            direction: ws.direction,
            selected: ws.root_symbol,
        })
        .collect()
}
