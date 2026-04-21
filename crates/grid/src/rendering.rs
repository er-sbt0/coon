//! Optional rendering utilities for ratatui
//!
//! This module is only available when the "rendering" feature is enabled.

use crate::{EdgePath, EdgeSegment, EdgeSegmentType, LayoutResult, Viewport};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::collections::HashSet;

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
///
/// When `reverse_arrows` is `true` the arrowhead is placed at the **from** end of
/// each edge instead of the **to** end.  Use this in "incoming callers" views
/// where DAG edges flow caller-outward but the logical arrow should point back
/// toward the callee (selected node).
///
/// Also renders per-parent lines for nodes with multiple incoming edges (without
/// arrowheads — those are emitted by `render_merge_trunks`).
pub fn render_dag_edges(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
    reverse_arrows: bool,
) {
    // Draw all edge lines first.
    for edge in layout.edges() {
        render_edge_path(buf, edge, viewport, area, style);
    }
    // Merged per-parent lines — draw wires only, no arrowheads.
    for edge in layout.merged_edges() {
        render_edge_path(buf, edge, viewport, area, style);
    }

    // Draw arrowheads deduplicated by logical target node:
    //   • normal mode   → deduplicate by child_id  (multiple parents → same child)
    //   • reverse mode  → deduplicate by parent_id (multiple children → same parent,
    //                     arrows point BACK into the parent/root node)
    // This ensures exactly one arrowhead per target node regardless of how many
    // wires converge on it.
    let mut seen: HashSet<usize> = HashSet::new();
    for edge in layout.edges() {
        let key = if reverse_arrows {
            edge.parent_id
        } else {
            edge.child_id
        };
        if seen.insert(key) {
            render_arrowhead(buf, edge, viewport, area, style, reverse_arrows);
        }
    }

    // In reverse mode, merged_edges represent paths from each parent toward a
    // shared child.  The arrowhead should appear at the parent (FROM) end of
    // each wire — one per distinct parent — so draw them here instead of
    // relying on the merge trunk (which points the wrong way in reverse mode).
    if reverse_arrows {
        for edge in layout.merged_edges() {
            if seen.insert(edge.parent_id) {
                render_arrowhead(buf, edge, viewport, area, style, reverse_arrows);
            }
        }
    }
}

/// Render the single shared arrowhead trunk for each node that has multiple
/// incoming edges.  Call this after `render_dag_edges` so trunks paint on top.
pub fn render_merge_trunks(
    buf: &mut Buffer,
    layout: &LayoutResult,
    viewport: &Viewport,
    area: Rect,
    style: Style,
    reverse_arrows: bool,
) {
    for trunk in layout.merge_trunks() {
        render_edge_path(buf, trunk, viewport, area, style);
        // In reverse mode the arrowheads are drawn on the individual
        // merged_edges (one per parent) inside render_dag_edges, so the
        // shared trunk does not need its own arrowhead.
        if !reverse_arrows {
            render_arrowhead(buf, trunk, viewport, area, style, reverse_arrows);
        }
    }
}

/// Render a `▶` or `▼` arrowhead near the destination (or source when
/// `reverse_arrows` is `true`) of an edge path.
fn render_arrowhead(
    buf: &mut Buffer,
    path: &EdgePath,
    viewport: &Viewport,
    area: Rect,
    style: Style,
    reverse_arrows: bool,
) {
    // In normal mode: arrowhead at the TO end of the last real segment.
    // In reverse mode: arrowhead at the FROM end of the first real segment,
    // pointing back toward the source node.
    let (seg, tip_pos, going_right, going_down) = if !reverse_arrows {
        let last = match path
            .segments
            .iter()
            .rev()
            .find(|s| !matches!(s.segment_type, EdgeSegmentType::Corner(_)))
        {
            Some(s) => s,
            None => return,
        };
        (
            last,
            last.to,
            last.to.x >= last.from.x,
            last.to.y >= last.from.y,
        )
    } else {
        let first = match path
            .segments
            .iter()
            .find(|s| !matches!(s.segment_type, EdgeSegmentType::Corner(_)))
        {
            Some(s) => s,
            None => return,
        };
        // Use the first segment's type; tip is the FROM end; direction reflects edge travel
        (
            first,
            first.from,
            first.to.x >= first.from.x,
            first.to.y >= first.from.y,
        )
    };

    let screen = viewport.world_to_screen(tip_pos);
    let sx = screen.x as i32;
    let sy = screen.y as i32;

    let (arrow, x, y) = match seg.segment_type {
        EdgeSegmentType::Horizontal => {
            if !reverse_arrows {
                if going_right {
                    ("▶", sx - 1, sy) // arrives from left → just left of right node's border
                } else {
                    ("◀", sx + 1, sy) // arrives from right → just right of left node's border
                }
            } else {
                // Reversed: sit just outside the FROM node pointing back inward
                if going_right {
                    ("◀", sx + 1, sy) // edge goes right; reversed ◀ clears the right border of FROM node
                } else {
                    ("▶", sx - 1, sy) // edge goes left; reversed ▶ clears the left border of FROM node
                }
            }
        }
        EdgeSegmentType::Vertical => {
            if !reverse_arrows {
                if going_down {
                    ("▼", sx, sy - 1)
                } else {
                    ("▲", sx, sy + 1)
                }
            } else {
                if going_down {
                    ("▲", sx, sy + 1)
                } else {
                    ("▼", sx, sy - 1)
                }
            }
        }
        _ => return,
    };

    if x >= 0 && x < area.width as i32 && y >= 0 && y < area.height as i32 {
        buf.set_string(area.x + x as u16, area.y + y as u16, arrow, style);
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
