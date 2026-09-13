pub mod app;
pub mod boot;
pub mod command;
pub mod demo;
pub mod events;
pub mod layout;
pub mod logging;
pub mod lsp_bridge;
pub mod protocol;
pub mod workspace;

pub use app::CoreApp;
pub use boot::spawn_lsp_bridge;
pub use demo::create_demo_call_graph;
pub use command::Command;
pub use events::AppEvent;
pub use layout::{compute_layout, NodePos};
pub use lsp_bridge::{LoadingState, LspBridge, PendingRequest};
pub use protocol::{
    layout_of, snapshot_of, ClientMsg, EventRouter, Routed, ServerMsg, WorkspaceInfo,
};
pub use workspace::{CoreWorkspace, CoreWorkspaceManager};
