//! Optional rendering utilities for ratatui
//!
//! This module is only available when the "rendering" feature is enabled.

use crate::{EdgePath, EdgeSegment, EdgeSegmentType, LayoutResult, Viewport};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

/// Render back-edges (reversed cycles) from a DAG layout with a distinct style.
///
/// Back-edges are stored in `layout.cross_edges()` by the Sugiyama engine.
/// Call this after `render_dag_edges` so they paint on top of forward edges.
pub fn render_back_edges(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    for edge in layout.cross_edges() {
        render_edge_path(buf, edge, viewport, area, style);
    }
}

/// Render edges for a DAG layout (no tree structure needed), with arrowheads.
pub fn render_dag_edges(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    for edge in layout.edges() {
        render_edge_path(buf, edge, viewport, area, style);
        render_arrowhead(buf, edge, viewport, area, style);
    }
}

/// Render a `▶` or `▼` arrowhead at the tip of the last segment of an edge path.
fn render_arrowhead(
    buf: &mut Buffer,
    path: &EdgePath,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Find the last non-Corner segment to determine arrival direction
    let last_real = path
        .segments
        .iter()
        .rev()
        .find(|s| !matches!(s.segment_type, EdgeSegmentType::Corner(_)));

    if let Some(seg) = last_real {
        let tip = seg.to;
        let screen = viewport.world_to_screen(tip);
        let x = screen.x as i32;
        let y = screen.y as i32;

        if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
            let arrow = match seg.segment_type {
                EdgeSegmentType::Horizontal => {
                    if seg.to.x >= seg.from.x {
                        "▶"
                    } else {
                        "◀"
                    }
                }
                EdgeSegmentType::Vertical => {
                    if seg.to.y >= seg.from.y {
                        "▼"
                    } else {
                        "▲"
                    }
                }
                _ => return,
            };
            buf.set_string(area.x + x as u16, area.y + y as u16, arrow, style);
        }
    }
}

/// Render cross-edges (non-tree edges) with a distinct dimmed style
pub fn render_cross_edges(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    for edge in layout.cross_edges() {
        render_edge_path(buf, edge, viewport, area, style);
    }
}

/// Render a single edge path
pub fn render_edge_path(
    buf: &mut Buffer,
    path: &EdgePath,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Render segments with awareness of adjacent corners to avoid overlaps
    for (i, segment) in path.segments.iter().enumerate() {
        let has_corner_before = i > 0
            && matches!(
                path.segments[i - 1].segment_type,
                EdgeSegmentType::Corner(_)
            );
        let has_corner_after = i + 1 < path.segments.len()
            && matches!(
                path.segments[i + 1].segment_type,
                EdgeSegmentType::Corner(_)
            );

        render_edge_segment_with_context(
            buf,
            segment,
            has_corner_before,
            has_corner_after,
            viewport,
            area,
            style,
        );
    }
}

/// Render a single edge segment with context about adjacent corners
fn render_edge_segment_with_context(
    buf: &mut Buffer,
    segment: &EdgeSegment,
    has_corner_before: bool,
    has_corner_after: bool,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    use EdgeSegmentType::*;

    match segment.segment_type {
        Horizontal => render_horizontal_line_with_context(
            buf,
            segment.from,
            segment.to,
            has_corner_before,
            has_corner_after,
            viewport,
            area,
            style,
        ),
        Vertical => render_vertical_line_with_context(
            buf,
            segment.from,
            segment.to,
            has_corner_before,
            has_corner_after,
            viewport,
            area,
            style,
        ),
        Corner(corner_type) => render_corner(buf, segment.from, corner_type, viewport, area, style),
    }
}

/// Render a single edge segment
pub fn render_edge_segment(
    buf: &mut Buffer,
    segment: &EdgeSegment,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    use EdgeSegmentType::*;

    match segment.segment_type {
        Horizontal => render_horizontal_line(buf, segment.from, segment.to, viewport, area, style),
        Vertical => render_vertical_line(buf, segment.from, segment.to, viewport, area, style),
        Corner(corner_type) => render_corner(buf, segment.from, corner_type, viewport, area, style),
    }
}

fn render_horizontal_line_with_context(
    buf: &mut Buffer,
    from: crate::Position,
    to: crate::Position,
    has_corner_before: bool,
    has_corner_after: bool,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Transform world coordinates to screen coordinates
    let from_screen = viewport.world_to_screen(from);
    let to_screen = viewport.world_to_screen(to);

    let y = from_screen.y as i32;
    let mut start_x = from_screen.x.min(to_screen.x) as i32;
    let mut end_x = from_screen.x.max(to_screen.x) as i32;

    // Skip endpoints if they have adjacent corners
    if has_corner_before {
        start_x += 1;
    }
    if has_corner_after {
        end_x -= 1;
    }

    for x in start_x..=end_x {
        if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
            buf.set_string(area.x + x as u16, area.y + y as u16, "─", style);
        }
    }
}

fn render_horizontal_line(
    buf: &mut Buffer,
    from: crate::Position,
    to: crate::Position,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Transform world coordinates to screen coordinates
    let from_screen = viewport.world_to_screen(from);
    let to_screen = viewport.world_to_screen(to);

    let y = from_screen.y as i32;
    let start_x = from_screen.x.min(to_screen.x) as i32;
    let end_x = from_screen.x.max(to_screen.x) as i32;

    for x in start_x..=end_x {
        if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
            buf.set_string(area.x + x as u16, area.y + y as u16, "─", style);
        }
    }
}

fn render_vertical_line(
    buf: &mut Buffer,
    from: crate::Position,
    to: crate::Position,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Transform world coordinates to screen coordinates
    let from_screen = viewport.world_to_screen(from);
    let to_screen = viewport.world_to_screen(to);

    let x = from_screen.x as i32;
    let start_y = from_screen.y.min(to_screen.y) as i32;
    let end_y = from_screen.y.max(to_screen.y) as i32;

    // Skip endpoints (start_y+1 to end_y-1) to avoid overwriting corner characters
    // Corners are rendered separately as their own segments
    for y in (start_y + 1)..end_y {
        if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
            buf.set_string(area.x + x as u16, area.y + y as u16, "│", style);
        }
    }
}

fn render_vertical_line_with_context(
    buf: &mut Buffer,
    from: crate::Position,
    to: crate::Position,
    has_corner_before: bool,
    has_corner_after: bool,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    // Transform world coordinates to screen coordinates
    let from_screen = viewport.world_to_screen(from);
    let to_screen = viewport.world_to_screen(to);

    let x = from_screen.x as i32;
    let mut start_y = from_screen.y.min(to_screen.y) as i32;
    let mut end_y = from_screen.y.max(to_screen.y) as i32;

    // Skip endpoints if they have adjacent corners
    if has_corner_before {
        start_y += 1;
    }
    if has_corner_after {
        end_y -= 1;
    }

    for y in start_y..=end_y {
        if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
            buf.set_string(area.x + x as u16, area.y + y as u16, "│", style);
        }
    }
}

fn render_corner(
    buf: &mut Buffer,
    pos: crate::Position,
    corner_type: crate::CornerType,
    viewport: &Viewport,
    area: Rect,
    style: Style,
) {
    use crate::CornerType::*;

    // Transform world coordinates to screen coordinates
    let screen_pos = viewport.world_to_screen(pos);
    let x = screen_pos.x as i32;
    let y = screen_pos.y as i32;

    if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
        let char = match corner_type {
            TopRight => "┐",
            TopLeft => "┌",
            BottomRight => "┘",
            BottomLeft => "└",
        };
        buf.set_string(area.x + x as u16, area.y + y as u16, char, style);
    }
}
