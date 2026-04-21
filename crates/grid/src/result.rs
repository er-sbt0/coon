use crate::edge_router;
use crate::types::{EdgeConnectionPoints, EdgePath, LayoutBounds, Position};

/// Immutable result of a layout computation
#[derive(Clone, Debug)]
pub struct LayoutResult {
    positions: Vec<Position>,
    bounds: LayoutBounds,
    edges: Vec<EdgePath>,
    cross_edges: Vec<EdgePath>,
    /// Per-parent edge lines for nodes that have multiple incoming edges.
    /// These are trimmed to end at the merge column; no arrowhead is drawn on them.
    merged_edges: Vec<EdgePath>,
    /// One shared horizontal trunk per child node that has multiple incoming edges.
    /// Rendered with a single arrowhead pointing into the child node.
    merge_trunks: Vec<EdgePath>,
    version: u64,
}

impl LayoutResult {
    /// Create a new layout result
    pub(crate) fn new(positions: Vec<Position>, bounds: LayoutBounds, version: u64) -> Self {
        Self {
            positions,
            bounds,
            edges: Vec::new(),
            cross_edges: Vec::new(),
            merged_edges: Vec::new(),
            merge_trunks: Vec::new(),
            version,
        }
    }

    /// Get the position of a specific node
    pub fn position(&self, node_id: usize) -> Option<Position> {
        self.positions.get(node_id).copied()
    }

    /// Get all node positions
    pub fn positions(&self) -> &[Position] {
        &self.positions
    }

    /// Get the bounding box of the entire layout
    pub fn bounds(&self) -> LayoutBounds {
        self.bounds
    }

    /// Get the layout version (increments with each computation)
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Create an iterator over (node_id, position) pairs
    pub fn iter(&self) -> impl Iterator<Item = (usize, Position)> + '_ {
        self.positions
            .iter()
            .enumerate()
            .map(|(id, &pos)| (id, pos))
    }

    /// Create a viewport query interface
    pub fn with_viewport<'a>(&'a self, viewport: &'a Viewport) -> ViewportLayout<'a> {
        ViewportLayout {
            result: self,
            viewport,
        }
    }

    /// Calculate connection points between parent and child nodes
    pub fn connection_points(
        &self,
        parent_id: usize,
        child_id: usize,
        _node_width: f32,
        _node_height: f32,
    ) -> Option<EdgeConnectionPoints> {
        let parent_pos = self.position(parent_id)?;
        let child_pos = self.position(child_id)?;

        // Use hardcoded offsets to match grid-orig behavior:
        // - Parent exit: +2 horizontal (right edge), +1 vertical (middle of 3-high node)
        // - Child entry: 0 horizontal (left edge), +1 vertical (middle of 3-high node)
        Some(EdgeConnectionPoints {
            parent_exit: Position {
                x: parent_pos.x + 2.0,
                y: parent_pos.y + 1.0,
            },
            child_entry: Position {
                x: child_pos.x,
                y: child_pos.y + 1.0,
            },
        })
    }

    /// Iterator over all edges with connection points
    pub fn iter_edges_with_connections<'a, I, C>(
        &'a self,
        parent_children: I,
        node_width: f32,
        node_height: f32,
    ) -> impl Iterator<Item = (usize, usize, EdgeConnectionPoints)> + 'a
    where
        I: IntoIterator<Item = (usize, C)> + 'a,
        C: IntoIterator<Item = usize> + 'a,
    {
        parent_children
            .into_iter()
            .flat_map(move |(parent_id, children)| {
                children.into_iter().filter_map(move |child_id| {
                    self.connection_points(parent_id, child_id, node_width, node_height)
                        .map(|pts| (parent_id, child_id, pts))
                })
            })
    }

    /// Get all computed edge paths (tree edges only)
    pub fn edges(&self) -> &[EdgePath] {
        &self.edges
    }

    /// Get cross-edges (non-tree edges added after layout)
    pub fn cross_edges(&self) -> &[EdgePath] {
        &self.cross_edges
    }

    /// Get a specific edge path
    pub fn edge(&self, parent_id: usize, child_id: usize) -> Option<&EdgePath> {
        self.edges
            .iter()
            .find(|e| e.parent_id == parent_id && e.child_id == child_id)
    }

    /// Get per-parent edge lines for multi-incoming-edge nodes (no arrowhead drawn on them).
    pub fn merged_edges(&self) -> &[EdgePath] {
        &self.merged_edges
    }

    /// Get one shared trunk per child node that has multiple incoming edges.
    pub fn merge_trunks(&self) -> &[EdgePath] {
        &self.merge_trunks
    }

    /// Push a single pre-routed edge path (used by Sugiyama engine).
    pub(crate) fn push_edge(&mut self, path: EdgePath) {
        self.edges.push(path);
    }

    /// Push a single pre-routed cross-edge path (used by Sugiyama back-edge routing).
    pub(crate) fn push_cross_edge(&mut self, path: EdgePath) {
        self.cross_edges.push(path);
    }

    /// Push a trimmed per-parent edge line (part of a merge group; no arrowhead).
    pub(crate) fn push_merged_edge(&mut self, path: EdgePath) {
        self.merged_edges.push(path);
    }

    /// Push a shared merge trunk (one per multi-parent child; carries the arrowhead).
    pub(crate) fn push_merge_trunk(&mut self, path: EdgePath) {
        self.merge_trunks.push(path);
    }

    /// Add cross-edges (non-tree edges) between already-laid-out nodes.
    /// Each entry is (from_node_id, to_node_id). The edge is routed using the
    /// same orthogonal routing as tree edges.
    pub fn add_cross_edges(&mut self, edges: &[(usize, usize)], node_width: f32, node_height: f32) {
        self.cross_edges.clear();
        for &(from_id, to_id) in edges {
            if let Some(points) = self.connection_points(from_id, to_id, node_width, node_height) {
                let path = edge_router::compute_orthogonal_routing(
                    from_id,
                    to_id,
                    points.parent_exit,
                    points.child_entry,
                );
                self.cross_edges.push(path);
            }
        }
    }
}

/// Viewport for panning and zooming the layout
#[derive(Clone, Debug, PartialEq)]
pub struct Viewport {
    offset: Position,
    scale: f32,
}

impl Viewport {
    /// Create a new viewport at the origin with scale 1.0
    pub fn new() -> Self {
        Self {
            offset: Position::default(),
            scale: 1.0,
        }
    }

    /// Create a viewport with a specific offset
    pub fn with_offset(offset: Position) -> Self {
        Self { offset, scale: 1.0 }
    }

    /// Get the current offset
    pub fn offset(&self) -> Position {
        self.offset
    }

    /// Get the current scale
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Pan the viewport by a delta
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.offset.x += dx;
        self.offset.y += dy;
    }

    /// Set the scale (clamped between 0.1 and 10.0)
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.clamp(0.1, 10.0);
    }

    /// Center the viewport on a specific position
    pub fn center_on(&mut self, position: Position, screen_size: (f32, f32)) {
        self.offset.x = position.x - screen_size.0 / 2.0;
        self.offset.y = position.y - screen_size.1 / 2.0;
    }

    /// Transform world coordinates to screen coordinates
    pub fn world_to_screen(&self, world_pos: Position) -> Position {
        Position {
            x: (world_pos.x - self.offset.x) * self.scale,
            y: (world_pos.y - self.offset.y) * self.scale,
        }
    }

    /// Transform screen coordinates to world coordinates
    pub fn screen_to_world(&self, screen_pos: Position) -> Position {
        Position {
            x: screen_pos.x / self.scale + self.offset.x,
            y: screen_pos.y / self.scale + self.offset.y,
        }
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

/// Query interface combining layout result with viewport
pub struct ViewportLayout<'a> {
    result: &'a LayoutResult,
    viewport: &'a Viewport,
}

impl<'a> ViewportLayout<'a> {
    /// Get the screen position of a node
    pub fn screen_position(&self, node_id: usize) -> Option<Position> {
        self.result
            .position(node_id)
            .map(|pos| self.viewport.world_to_screen(pos))
    }

    /// Get IDs of all visible nodes within screen bounds
    pub fn visible_nodes(&self, screen_bounds: LayoutBounds) -> Vec<usize> {
        self.result
            .iter()
            .filter_map(|(id, world_pos)| {
                let screen_pos = self.viewport.world_to_screen(world_pos);
                if screen_bounds.contains(&screen_pos) {
                    Some(id)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Create an iterator over visible (node_id, screen_position) pairs
    pub fn iter_visible(
        &self,
        screen_bounds: LayoutBounds,
    ) -> impl Iterator<Item = (usize, Position)> + '_ {
        self.result.iter().filter_map(move |(id, world_pos)| {
            let screen_pos = self.viewport.world_to_screen(world_pos);
            if screen_bounds.contains(&screen_pos) {
                Some((id, screen_pos))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_result_creation() {
        let positions = vec![
            Position::new(0.0, 0.0),
            Position::new(10.0, 0.0),
            Position::new(20.0, 0.0),
        ];
        let bounds = LayoutBounds::new(0.0, 20.0, 0.0, 0.0);
        let result = LayoutResult::new(positions, bounds, 1);

        assert_eq!(result.version(), 1);
        assert_eq!(result.position(0), Some(Position::new(0.0, 0.0)));
        assert_eq!(result.position(1), Some(Position::new(10.0, 0.0)));
        assert_eq!(result.position(3), None);
    }

    #[test]
    fn test_viewport_pan() {
        let mut viewport = Viewport::new();
        viewport.pan(10.0, 20.0);
        assert_eq!(viewport.offset(), Position::new(10.0, 20.0));
    }

    #[test]
    fn test_viewport_scale_clamping() {
        let mut viewport = Viewport::new();
        viewport.set_scale(20.0);
        assert_eq!(viewport.scale(), 10.0); // Clamped to max

        viewport.set_scale(0.01);
        assert_eq!(viewport.scale(), 0.1); // Clamped to min
    }

    #[test]
    fn test_viewport_center_on() {
        let mut viewport = Viewport::new();
        viewport.center_on(Position::new(100.0, 100.0), (50.0, 50.0));
        assert_eq!(viewport.offset(), Position::new(75.0, 75.0));
    }

    #[test]
    fn test_world_to_screen() {
        let mut viewport = Viewport::new();
        viewport.offset = Position::new(10.0, 20.0);
        viewport.scale = 2.0;

        let world_pos = Position::new(15.0, 25.0);
        let screen_pos = viewport.world_to_screen(world_pos);
        assert_eq!(screen_pos, Position::new(10.0, 10.0));
    }

    #[test]
    fn test_screen_to_world() {
        let mut viewport = Viewport::new();
        viewport.offset = Position::new(10.0, 20.0);
        viewport.scale = 2.0;

        let screen_pos = Position::new(10.0, 10.0);
        let world_pos = viewport.screen_to_world(screen_pos);
        assert_eq!(world_pos, Position::new(15.0, 25.0));
    }

    #[test]
    fn test_visible_nodes() {
        let positions = vec![
            Position::new(0.0, 0.0),
            Position::new(100.0, 0.0),
            Position::new(200.0, 0.0),
        ];
        let bounds = LayoutBounds::new(0.0, 200.0, 0.0, 0.0);
        let result = LayoutResult::new(positions, bounds, 1);

        let viewport = Viewport::new();
        let view = result.with_viewport(&viewport);

        let screen_bounds = LayoutBounds::new(0.0, 150.0, -10.0, 10.0);
        let visible = view.visible_nodes(screen_bounds);

        assert_eq!(visible, vec![0, 1]); // Node 2 at x=200 is outside bounds
    }

    #[test]
    fn test_iter_visible() {
        let positions = vec![
            Position::new(0.0, 0.0),
            Position::new(100.0, 0.0),
            Position::new(200.0, 0.0),
        ];
        let bounds = LayoutBounds::new(0.0, 200.0, 0.0, 0.0);
        let result = LayoutResult::new(positions, bounds, 1);

        let viewport = Viewport::new();
        let view = result.with_viewport(&viewport);

        let screen_bounds = LayoutBounds::new(0.0, 150.0, -10.0, 10.0);
        let visible: Vec<_> = view.iter_visible(screen_bounds).collect();

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].0, 0);
        assert_eq!(visible[1].0, 1);
    }

    #[test]
    fn test_connection_points() {
        // Positions are top-left corners (matching old app behavior)
        let positions = vec![Position::new(0.0, 0.0), Position::new(20.0, 0.0)];
        let bounds = LayoutBounds::new(0.0, 20.0, 0.0, 3.0);
        let result = LayoutResult::new(positions, bounds, 1);

        let points = result.connection_points(0, 1, 10.0, 3.0).unwrap();
        // Parent exit: pos + (2, 1) for right-middle of 3x3 node
        assert_eq!(points.parent_exit, Position::new(2.0, 1.0));
        // Child entry: pos + (0, 1) for left-middle of 3x3 node
        assert_eq!(points.child_entry, Position::new(20.0, 1.0));
    }
}
