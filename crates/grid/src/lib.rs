// New API modules
mod config;
pub mod dag;
mod edge_router;
mod engine;
mod error;
mod result;
mod sugiyama;
mod types;

#[cfg(feature = "rendering")]
pub mod rendering;

// Public API
pub use config::{LayoutConfig, TreeOrientation};
pub use dag::{Dag, DagNode};
pub use engine::LayoutEngine;
pub use error::LayoutError;
pub use result::{LayoutResult, Viewport, ViewportLayout};
pub use types::{
    CornerType, EdgeConnectionPoints, EdgePath, EdgeSegment, EdgeSegmentType, LayoutBounds,
    Position,
};

// Re-export rendering functions when feature is enabled
#[cfg(feature = "rendering")]
pub use rendering::{render_back_edges, render_cross_edges, render_dag_edges};
