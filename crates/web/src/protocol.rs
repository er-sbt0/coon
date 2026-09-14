//! The wire protocol now lives in `core::protocol`, shared with the desktop
//! frontend. Re-exported here so existing `web::protocol::…` paths keep working.

pub use ::core::protocol::{
    snapshot_of, workspace_infos, ClientMsg, EventRouter, Routed, ServerMsg, WorkspaceInfo,
};
