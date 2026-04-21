use crate::types::{CornerType, EdgePath, EdgeSegment, EdgeSegmentType, Position};

/// Compute orthogonal edge routing (horizontal-vertical-horizontal).
/// `mid_x` controls where the vertical bar is placed; pass `None` to use the
/// midpoint between `from.x` and `to.x`.
pub fn compute_orthogonal_routing(
    parent_id: usize,
    child_id: usize,
    from: Position,
    to: Position,
) -> EdgePath {
    compute_orthogonal_routing_with_mid(parent_id, child_id, from, to, (from.x + to.x) / 2.0)
}

/// Like [`compute_orthogonal_routing`] but with an explicit `mid_x` for the
/// vertical bar.  Callers that route many edges between the same layer pair
/// should pass distinct `mid_x` values to avoid all bars stacking on the same
/// column.
pub fn compute_orthogonal_routing_with_mid(
    parent_id: usize,
    child_id: usize,
    from: Position,
    to: Position,
    mid_x: f32,
) -> EdgePath {
    let mut segments = Vec::new();

    // Special case: if Y values are the same, just draw a straight horizontal line
    if (to.y - from.y).abs() <= 0.1 {
        segments.push(EdgeSegment::new(from, to, EdgeSegmentType::Horizontal));
    } else {
        // Use caller-supplied mid_x (may differ per edge to avoid overlap)

        // Segment 1: Horizontal from parent to midpoint
        if (mid_x - from.x).abs() > 0.1 {
            segments.push(EdgeSegment::new(
                from,
                Position {
                    x: mid_x,
                    y: from.y,
                },
                EdgeSegmentType::Horizontal,
            ));
        }

        // Segment 2: Vertical at midpoint (with corners)
        // Add corner at start of vertical segment
        let corner_start = if to.y > from.y {
            CornerType::TopRight
        } else {
            CornerType::BottomRight
        };

        segments.push(EdgeSegment::new(
            Position {
                x: mid_x,
                y: from.y,
            },
            Position {
                x: mid_x,
                y: from.y,
            },
            EdgeSegmentType::Corner(corner_start),
        ));

        // Vertical line
        segments.push(EdgeSegment::new(
            Position {
                x: mid_x,
                y: from.y,
            },
            Position { x: mid_x, y: to.y },
            EdgeSegmentType::Vertical,
        ));

        // Add corner at end of vertical segment
        let corner_end = if to.y > from.y {
            CornerType::BottomLeft
        } else {
            CornerType::TopLeft
        };

        segments.push(EdgeSegment::new(
            Position { x: mid_x, y: to.y },
            Position { x: mid_x, y: to.y },
            EdgeSegmentType::Corner(corner_end),
        ));

        // Segment 3: Horizontal from midpoint to child
        if (to.x - mid_x).abs() > 0.1 {
            segments.push(EdgeSegment::new(
                Position { x: mid_x, y: to.y },
                to,
                EdgeSegmentType::Horizontal,
            ));
        }
    }

    EdgePath {
        parent_id,
        child_id,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orthogonal_routing_horizontal_only() {
        let from = Position::new(0.0, 5.0);
        let to = Position::new(20.0, 5.0);
        let path = compute_orthogonal_routing(0, 1, from, to);

        // Should only have horizontal segment (same Y)
        assert_eq!(path.segments.len(), 1);
        assert!(matches!(
            path.segments[0].segment_type,
            EdgeSegmentType::Horizontal
        ));
    }

    #[test]
    fn test_orthogonal_routing_with_vertical() {
        let from = Position::new(0.0, 0.0);
        let to = Position::new(20.0, 10.0);
        let path = compute_orthogonal_routing(0, 1, from, to);

        // Should have: H, corner, V, corner, H
        assert!(path.segments.len() >= 3);
    }
}
