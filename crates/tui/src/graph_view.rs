use crate::graph_adapter::{CallDirection, CallGraphAdapter};
use grid::{Dag, LayoutConfig, LayoutEngine, LayoutResult, Position, Viewport};
use model::{CallGraph, FunctionNode, SymbolId};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, StatefulWidget, Widget},
};

/// State for the graph view
pub struct GraphViewState {
    pub adapter: CallGraphAdapter,
    pub engine: LayoutEngine,
    pub dag: Option<Dag<SymbolId>>,
    pub layout: Option<LayoutResult>,
    pub root_symbol: Option<SymbolId>,
    pub selected_node: Option<SymbolId>,
    pub viewport: Viewport,
    pub direction: CallDirection,
    pub max_depth: Option<usize>,
    layout_dirty: bool,
    tree_version: u64,
}

impl GraphViewState {
    pub fn new() -> Self {
        let config = LayoutConfig::new()
            .with_node_size(24.0, 3.0) // 24-char fixed width, 3-row height
            .with_spacing(5.0, 28.0, 1.0); // node_separation (height+gap), level_separation (width+gap), subtree_separation

        Self {
            adapter: CallGraphAdapter::new(),
            engine: LayoutEngine::with_config(config).unwrap(),
            dag: None,
            layout: None,
            root_symbol: None,
            selected_node: None,
            viewport: Viewport::new(),
            direction: CallDirection::Incoming,
            max_depth: Some(5),
            layout_dirty: true,
            tree_version: 0,
        }
    }

    /// Set the root symbol
    pub fn set_root(&mut self, symbol: SymbolId) {
        if self.root_symbol.as_ref() != Some(&symbol) {
            self.root_symbol = Some(symbol);
            self.selected_node = Some(symbol);
            self.layout_dirty = true;
        }
    }

    /// Toggle between incoming and outgoing call direction
    pub fn toggle_direction(&mut self) {
        self.direction = match self.direction {
            CallDirection::Incoming => CallDirection::Outgoing,
            CallDirection::Outgoing => CallDirection::Incoming,
        };
        self.layout_dirty = true;
    }

    /// Set the call direction
    pub fn set_direction(&mut self, direction: CallDirection) {
        if self.direction != direction {
            self.direction = direction;
            self.layout_dirty = true;
        }
    }

    /// Force layout to be recomputed on next update
    pub fn mark_layout_dirty(&mut self) {
        self.layout_dirty = true;
    }

    /// Rebuild the layout
    pub fn update_layout(
        &mut self,
        graph: &CallGraph,
        viewport_size: (f32, f32),
    ) -> Result<(), String> {
        // Early return if layout is clean
        if !self.layout_dirty {
            return Ok(());
        }

        let root = match &self.root_symbol {
            Some(r) => r,
            None => return Err("No root symbol set".to_string()),
        };

        // Check if this is the first layout (dag doesn't exist yet)
        let is_first_layout = self.dag.is_none();

        // Build DAG structure
        let dag = self
            .adapter
            .build_dag(graph, root, self.direction, self.max_depth)
            .map_err(|e| e.to_string())?;

        // Track dag version for future change detection
        self.tree_version = dag.version();

        // Compute layout using Sugiyama algorithm
        let layout = self.engine.compute_dag(&dag).map_err(|e| e.to_string())?;

        // On first layout, center the viewport vertically (put root in middle of screen on y-axis)
        if is_first_layout {
            if let Some(root_pos) = layout.position(0) {
                let inner_height = viewport_size.1 - 2.0;
                let screen_middle = inner_height / 2.0;
                let offset_y = root_pos.y - screen_middle;
                self.viewport = Viewport::with_offset(Position::new(0.0, offset_y));
            }
        }

        self.dag = Some(dag);
        self.layout = Some(layout);
        self.layout_dirty = false;

        Ok(())
    }

    /// Recenter the viewport on the root node
    pub fn recenter_viewport(&mut self, viewport_size: (f32, f32)) {
        if let Some(layout) = &self.layout {
            if let Some(root_pos) = layout.position(0) {
                let center_x = (viewport_size.0 / 2.0) - root_pos.x;
                let center_y = (viewport_size.1 / 2.0) - root_pos.y;
                self.viewport = Viewport::with_offset(Position::new(-center_x, -center_y));
            }
        }
    }

    /// Navigate to the parent of the currently selected node
    pub fn navigate_to_parent(&mut self) -> bool {
        if let Some(selected) = &self.selected_node {
            if let Some(dag) = &self.dag {
                if let Some(&selected_idx) = self.adapter.symbol_to_node.get(selected) {
                    if let Ok(node) = dag.node(selected_idx) {
                        if let Some(&parent_idx) = node.predecessors.first() {
                            if let Some(parent_symbol) =
                                self.adapter.node_to_symbol.get(&parent_idx)
                            {
                                self.selected_node = Some(*parent_symbol);
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Navigate to the first child of the currently selected node
    pub fn navigate_to_child(&mut self) -> bool {
        if let Some(selected) = &self.selected_node {
            if let Some(dag) = &self.dag {
                if let Some(&selected_idx) = self.adapter.symbol_to_node.get(selected) {
                    if let Ok(node) = dag.node(selected_idx) {
                        if !node.successors.is_empty() {
                            let middle_idx = (node.successors.len() - 1) / 2;
                            if let Some(&child_dag_idx) = node.successors.get(middle_idx) {
                                if let Some(child_symbol) =
                                    self.adapter.node_to_symbol.get(&child_dag_idx)
                                {
                                    self.selected_node = Some(*child_symbol);
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Get siblings of the currently selected node (nodes sharing the same first predecessor)
    fn get_siblings(&self) -> Option<Vec<usize>> {
        if let Some(selected) = &self.selected_node {
            if let Some(dag) = &self.dag {
                if let Some(&selected_idx) = self.adapter.symbol_to_node.get(selected) {
                    if let Ok(node) = dag.node(selected_idx) {
                        if let Some(&parent_idx) = node.predecessors.first() {
                            if let Ok(parent) = dag.node(parent_idx) {
                                return Some(parent.successors.clone());
                            }
                        }
                        // No predecessor: this is a root node
                        return Some(vec![selected_idx]);
                    }
                }
            }
        }
        None
    }

    /// Navigate to the next sibling (j key)
    pub fn navigate_next_sibling(&mut self) -> bool {
        if let Some(siblings) = self.get_siblings() {
            if let Some(selected) = &self.selected_node {
                if let Some(&selected_idx) = self.adapter.symbol_to_node.get(selected) {
                    if let Some(current_pos) = siblings.iter().position(|&idx| idx == selected_idx)
                    {
                        let next_pos = (current_pos + 1) % siblings.len();
                        let next_idx = siblings[next_pos];
                        if let Some(next_symbol) = self.adapter.node_to_symbol.get(&next_idx) {
                            self.selected_node = Some(*next_symbol);
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Navigate to the previous sibling (k key)
    pub fn navigate_prev_sibling(&mut self) -> bool {
        if let Some(siblings) = self.get_siblings() {
            if let Some(selected) = &self.selected_node {
                if let Some(&selected_idx) = self.adapter.symbol_to_node.get(selected) {
                    if let Some(current_pos) = siblings.iter().position(|&idx| idx == selected_idx)
                    {
                        let prev_pos = if current_pos == 0 {
                            siblings.len() - 1
                        } else {
                            current_pos - 1
                        };
                        let prev_idx = siblings[prev_pos];
                        if let Some(prev_symbol) = self.adapter.node_to_symbol.get(&prev_idx) {
                            self.selected_node = Some(*prev_symbol);
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

impl Default for GraphViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// The graph view widget
pub struct GraphView<'a> {
    graph: &'a CallGraph,
    show_help: bool,
}

impl<'a> GraphView<'a> {
    pub fn new(graph: &'a CallGraph) -> Self {
        Self {
            graph,
            show_help: false,
        }
    }

    pub fn show_help(mut self, show: bool) -> Self {
        self.show_help = show;
        self
    }

    fn render_node(
        &self,
        buf: &mut Buffer,
        func: &FunctionNode,
        pos: (i32, i32),
        area: Rect,
        is_selected: bool,
        node_size: (u16, u16),
    ) {
        let (node_width, node_height) = node_size;
        let screen_x = pos.0 as i16;
        let screen_y = pos.1 as i16;

        // Check if node is visible
        if screen_x < 0
            || screen_y < 0
            || screen_x >= area.width as i16
            || screen_y >= area.height as i16
        {
            return;
        }

        // Simple styling: cyan for selected, yellow for others
        let (border_style, text_style) = if is_selected {
            (
                Style::default().fg(Color::Cyan),
                Style::default().fg(Color::Cyan),
            )
        } else {
            (
                Style::default().fg(Color::Yellow),
                Style::default().fg(Color::Yellow),
            )
        };

        // Truncate label to fit fixed node width (leave 2 chars for borders)
        let max_label = (node_width.saturating_sub(2)) as usize;
        let label: std::borrow::Cow<str> = if func.name.len() > max_label && max_label >= 1 {
            std::borrow::Cow::Owned(format!("{}…", &func.name[..max_label.saturating_sub(1)]))
        } else {
            std::borrow::Cow::Borrowed(&func.name)
        };

        // Calculate node area
        let node_area = Rect {
            x: area.x + screen_x as u16,
            y: area.y + screen_y as u16,
            width: node_width.min(area.width.saturating_sub(screen_x as u16)),
            height: node_height
                .min(area.height.saturating_sub(screen_y as u16))
                .max(1),
        };

        // Widget-based rendering
        let block = Block::bordered().border_style(border_style);
        let paragraph = Paragraph::new(label.as_ref())
            .block(block)
            .style(text_style);
        paragraph.render(node_area, buf);
    }

    fn render_help(&self, buf: &mut Buffer, area: Rect) {
        let help_text = vec![
            Line::from("Graph View Controls:"),
            Line::from(""),
            Line::from("  ←↓↑→  Pan view"),
            Line::from("  j/k   Next/Prev node"),
            Line::from("  d     Toggle direction"),
            Line::from("  r     Reset view"),
            Line::from("  ?     Toggle help"),
            Line::from("  Tab   Switch view"),
        ];

        let help_area = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4).min(30),
            height: area
                .height
                .saturating_sub(4)
                .min(help_text.len() as u16 + 2),
        };

        let help_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("Help");

        let paragraph = Paragraph::new(help_text).block(help_block);
        paragraph.render(help_area, buf);
    }
}

impl<'a> StatefulWidget for GraphView<'a> {
    type State = GraphViewState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        // Render background
        let bg_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(format!(
                " Call Graph - {} (depth: {}) ",
                match state.direction {
                    CallDirection::Incoming => "Callers",
                    CallDirection::Outgoing => "Callees",
                },
                state.max_depth.unwrap_or(999)
            ));

        bg_block.render(area, buf);

        let inner_area = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // Check if layout exists (don't compute in render!)
        let Some(layout) = &state.layout else {
            let msg = "No layout available. Press 'r' to refresh.";
            buf.set_string(
                inner_area.x,
                inner_area.y,
                msg,
                Style::default().fg(Color::Yellow),
            );
            return;
        };

        let viewport_layout = layout.with_viewport(&state.viewport);

        let screen_bounds =
            grid::LayoutBounds::new(0.0, inner_area.width as f32, 0.0, inner_area.height as f32);

        // Render forward edges
        grid::render_dag_edges(
            buf,
            layout,
            &state.viewport,
            inner_area,
            Style::default().fg(Color::DarkGray),
        );

        // Render back-edges (cycles) in red
        grid::render_cross_edges(
            buf,
            layout,
            &state.viewport,
            inner_area,
            Style::default().fg(Color::Red),
        );

        // Render only visible nodes (performance optimization)
        for (node_id, screen_pos) in viewport_layout.iter_visible(screen_bounds) {
            if let Some(symbol_id) = state.adapter.get_symbol(node_id) {
                if let Some(func) = self.graph.get_function(symbol_id) {
                    let is_selected = state.selected_node.as_ref() == Some(symbol_id);
                    // Fixed node dimensions matching LayoutConfig
                    let node_width = state.engine.config().node_width as u16;
                    let node_height = state.engine.config().node_height as u16;

                    self.render_node(
                        buf,
                        func,
                        (screen_pos.x as i32, screen_pos.y as i32),
                        inner_area,
                        is_selected,
                        (node_width, node_height),
                    );
                }
            }
        }

        // Render status bar
        let status_y = area.y + area.height.saturating_sub(1);
        let offset = state.viewport.offset();
        let direction_str = match state.direction {
            CallDirection::Incoming => "Incoming",
            CallDirection::Outgoing => "Outgoing",
        };
        let status_text = format!(
            " Offset: ({:.0}, {:.0}) | Nodes: {} | Direction: {} ",
            offset.x,
            offset.y,
            state.adapter.node_to_symbol.len(),
            direction_str
        );
        buf.set_string(
            area.x + 1,
            status_y,
            &status_text,
            Style::default().fg(Color::DarkGray),
        );

        // Render help overlay if enabled
        if self.show_help {
            self.render_help(buf, area);
        }
    }
}
