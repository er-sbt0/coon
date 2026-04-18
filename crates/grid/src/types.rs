/// Position in the layout coordinate system.
///
/// # Coordinate System
///
/// Positions represent node **CENTERS** (not top-left corners). This is important for:
/// - The Reingold-Tilford algorithm which positions nodes by their centers
/// - Edge connection calculations which need to find node borders
/// - Rendering which converts centers to top-left corners
///
/// ## Coordinate Axes
///
/// The coordinate system changes based on tree orientation:
///
/// ### LeftRight Orientation (default)
/// - Origin (0, 0) is at the root node center
/// - X-axis: Increases to the right (depth in tree, root at x=0)
/// - Y-axis: Increases downward (sibling offset)
///
/// ### TopDown Orientation
/// - Origin (0, 0) is at the root node center
/// - X-axis: Increases to the right (sibling offset)
/// - Y-axis: Increases downward (depth in tree, root at y=0)
///
/// ### RightLeft Orientation
/// - Origin (0, 0) is at the root node center
/// - X-axis: Decreases to the left (depth in tree, root at x=0)
/// - Y-axis: Increases downward (sibling offset)
///
/// ### BottomUp Orientation
/// - Origin (0, 0) is at the root node center
/// - X-axis: Increases to the right (sibling offset)
/// - Y-axis: Decreases upward (depth in tree, root at y=0)
///
/// ## Units
///
/// Positions use floating-point values that represent logical grid positions.
/// These are typically rounded to integers for rendering in terminal cells or pixels.
///
/// ## Converting Between Systems
///
/// - **Center to Top-Left:** `top_left = center - (width/2, height/2)`
/// - **Center to Border:** Use half-width/half-height offsets based on direction
/// - **Layout to Screen:** Apply viewport offset and scale transformations
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

impl Position {
    /// Create a new position
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Calculate Euclidean distance to another position
    pub fn distance_to(&self, other: &Position) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

impl Default for Position {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

/// Axis-aligned bounding box in layout space
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LayoutBounds {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
}

impl LayoutBounds {
    /// Create a new bounding box
    pub fn new(min_x: f32, max_x: f32, min_y: f32, max_y: f32) -> Self {
        Self {
            min_x,
            max_x,
            min_y,
            max_y,
        }
    }

    /// Get the width of the bounding box
    pub fn width(&self) -> f32 {
        self.max_x - self.min_x
    }

    /// Get the height of the bounding box
    pub fn height(&self) -> f32 {
        self.max_y - self.min_y
    }

    /// Get the center point of the bounding box
    pub fn center(&self) -> Position {
        Position {
            x: (self.min_x + self.max_x) / 2.0,
            y: (self.min_y + self.max_y) / 2.0,
        }
    }

    /// Check if a position is contained within the bounds
    pub fn contains(&self, pos: &Position) -> bool {
        pos.x >= self.min_x && pos.x <= self.max_x && pos.y >= self.min_y && pos.y <= self.max_y
    }
}

impl Default for LayoutBounds {
    fn default() -> Self {
        Self {
            min_x: 0.0,
            max_x: 0.0,
            min_y: 0.0,
            max_y: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_distance() {
        let p1 = Position::new(0.0, 0.0);
        let p2 = Position::new(3.0, 4.0);
        assert_eq!(p1.distance_to(&p2), 5.0);
    }

    #[test]
    fn test_bounds_dimensions() {
        let bounds = LayoutBounds::new(10.0, 50.0, 20.0, 80.0);
        assert_eq!(bounds.width(), 40.0);
        assert_eq!(bounds.height(), 60.0);
    }

    #[test]
    fn test_bounds_center() {
        let bounds = LayoutBounds::new(0.0, 100.0, 0.0, 50.0);
        let center = bounds.center();
        assert_eq!(center.x, 50.0);
        assert_eq!(center.y, 25.0);
    }

    #[test]
    fn test_bounds_contains() {
        let bounds = LayoutBounds::new(0.0, 100.0, 0.0, 50.0);
        assert!(bounds.contains(&Position::new(50.0, 25.0)));
        assert!(bounds.contains(&Position::new(0.0, 0.0)));
        assert!(bounds.contains(&Position::new(100.0, 50.0)));
        assert!(!bounds.contains(&Position::new(-1.0, 25.0)));
        assert!(!bounds.contains(&Position::new(50.0, 51.0)));
    }
}

/// Connection points for an edge between two nodes
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EdgeConnectionPoints {
    /// Exit point from parent node (typically right edge, middle)
    pub parent_exit: Position,
    /// Entry point to child node (typically left edge, middle)
    pub child_entry: Position,
}

/// Type of corner in edge routing
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CornerType {
    /// Top-right corner: ┐
    TopRight,
    /// Top-left corner: ┌
    TopLeft,
    /// Bottom-right corner: ┘
    BottomRight,
    /// Bottom-left corner: └
    BottomLeft,
}

/// Type of edge segment
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum EdgeSegmentType {
    /// Horizontal line segment
    Horizontal,
    /// Vertical line segment
    Vertical,
    /// Corner segment
    Corner(CornerType),
}

/// A single segment of an edge path
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EdgeSegment {
    /// Starting position of segment
    pub from: Position,
    /// Ending position of segment
    pub to: Position,
    /// Type of segment
    pub segment_type: EdgeSegmentType,
}

impl EdgeSegment {
    pub fn new(from: Position, to: Position, segment_type: EdgeSegmentType) -> Self {
        Self {
            from,
            to,
            segment_type,
        }
    }
}

/// Complete path for an edge
#[derive(Clone, Debug, PartialEq)]
pub struct EdgePath {
    /// Parent node ID
    pub parent_id: usize,
    /// Child node ID
    pub child_id: usize,
    /// Ordered segments making up the path
    pub segments: Vec<EdgeSegment>,
}
