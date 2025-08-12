lomes0: we tried adding support for a 2d graph ui, though the result wasn't great.

review the patch, and provide suggestions and alternatives for a 2d ui graph approach/implementation.

what framework should we use? how to have smooth lines? how to have a robuest ui?

GitHub Copilot: I'll review the patch to understand the attempted 2D graph UI implementation and provide suggestions for a better approach. Let me continue reading the patch to see more of the implementation: Let me continue reading to see more of the implementation and examine what was actually attempted: Let me look at what was actually implemented in the source files: Let me search for actual canvas-related files from the patch: Let me look at the actual implementation that was added in the patch: Let me look at the specific implementation files that were added: Now let me examine the current lib.rs to see what's already there: Let me continue reading the lib.rs to understand the current structure: Now let me examine what's in the actions.rs to see what was implemented: Based on my review of the patch and current codebase, I can see that the 2D graph implementation was attempted but wasn't fully integrated. Let me provide a comprehensive analysis and recommendations:

## Review of the 2D Graph UI Patch

### What Was Attempted

The patch shows an ambitious attempt to add 2D graph visualization with:

1. **Canvas-based rendering** using Ratatui's Canvas widget
2. **Multiple layout algorithms** (Force-directed, Hierarchical, Grid)
3. **Comprehensive data structures** for visual nodes, edges, and viewport management
4. **Navigation controls** (pan, zoom, center)
5. **Text overlay system** to display node labels outside the canvas

### Issues with the Attempted Approach

1. **Text Rendering Complexity**: The Canvas widget in Ratatui has very limited text capabilities
2. **Braille Character Limitations**: Lines appear dotted due to discrete point plotting
3. **Complex Architecture**: Over-engineered with many abstractions that add complexity
4. **Performance Concerns**: Force-directed algorithms can be computationally expensive

## Recommendations for a Better 2D Graph UI

### 1. Framework Choice

**Option A: Stay with Ratatui but Simplify**
- Use a hybrid approach: Canvas for basic shapes, regular widgets for text
- Focus on one good layout algorithm instead of multiple complex ones
- Use Ratatui's `List` widgets positioned absolutely for node labels

**Option B: Consider egui**
- Much better 2D graphics capabilities
- Native support for smooth lines, text rendering, and mouse interactions
- Better suited for complex graph visualizations
- Example integration:

```rust
// Cargo.toml
[dependencies]
egui = "0.28"
eframe = "0.28"

// src/graph_ui.rs
use egui::*;

pub struct GraphApp {
    call_graph: CallGraph,
    nodes: HashMap<SymbolId, Pos2>,
    // ... other fields
}

impl eframe::App for GraphApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        CentralPanel::default().show(ctx, |ui| {
            // Custom painter for smooth lines
            let painter = ui.painter();
            
            // Draw edges with smooth lines
            for edge in &self.call_graph.edges {
                if let (Some(from_pos), Some(to_pos)) = (
                    self.nodes.get(&edge.caller),
                    self.nodes.get(&edge.callee)
                ) {
                    painter.line_segment([*from_pos, *to_pos], Stroke::new(1.0, Color32::WHITE));
                }
            }
            
            // Draw nodes with text
            for (symbol_id, pos) in &self.nodes {
                if let Some(function) = self.call_graph.nodes.get(symbol_id) {
                    // Draw node rectangle
                    let rect = Rect::from_center_size(*pos, Vec2::new(100.0, 30.0));
                    ui.painter().rect_filled(rect, 5.0, Color32::BLUE);
                    
                    // Draw text on top
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        &function.name,
                        FontId::default(),
                        Color32::WHITE,
                    );
                }
            }
        });
    }
}
```

**Option C: Web-based with Tauri**
- Use web technologies (D3.js, Cytoscape.js) for graph visualization
- Embed in a Rust desktop app using Tauri
- Best of both worlds: Rust backend + mature web graph libraries

### 2. Simplified Ratatui Approach (Recommended for staying in current stack)

If you want to stick with Ratatui, here's a much simpler approach:

```rust
// tui_ui/src/graph_2d.rs
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Canvas, Clear, Paragraph},
    Frame,
};

pub struct SimpleGraph2D {
    nodes: HashMap<SymbolId, (f64, f64)>, // Simple x,y positions
    viewport: (f64, f64, f64), // center_x, center_y, zoom
}

impl SimpleGraph2D {
    pub fn render(&self, f: &mut Frame, area: Rect, call_graph: &CallGraph) {
        // Split area: 80% for canvas, 20% for labels
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(80), Constraint::Percentage(20)])
            .split(area);

        // Render canvas with simple rectangles and lines
        self.render_canvas(f, chunks[0], call_graph);
        
        // Render labels in sidebar
        self.render_labels(f, chunks[1], call_graph);
    }

    fn render_canvas(&self, f: &mut Frame, area: Rect, call_graph: &CallGraph) {
        let canvas = Canvas::default()
            .block(Block::default().borders(Borders::ALL).title("Call Graph"))
            .paint(|ctx| {
                // Draw simple rectangles for nodes
                for (symbol_id, (x, y)) in &self.nodes {
                    ctx.draw(&ratatui::widgets::canvas::Rectangle {
                        x: x - 5.0,
                        y: y - 2.0,
                        width: 10.0,
                        height: 4.0,
                        color: Color::Blue,
                    });
                }
                
                // Draw lines for edges (accept that they're dotted)
                for edge in &call_graph.edges {
                    if let (Some((x1, y1)), Some((x2, y2))) = (
                        self.nodes.get(&edge.caller),
                        self.nodes.get(&edge.callee)
                    ) {
                        ctx.draw(&ratatui::widgets::canvas::Line {
                            x1: *x1, y1: *y1, x2: *x2, y2: *y2,
                            color: Color::White,
                        });
                    }
                }
            })
            .x_bounds([-50.0, 50.0])
            .y_bounds([-30.0, 30.0]);

        f.render_widget(canvas, area);
    }

    fn render_labels(&self, f: &mut Frame, area: Rect, call_graph: &CallGraph) {
        // Show node labels in a side panel
        let items: Vec<_> = self.nodes.iter()
            .filter_map(|(symbol_id, _)| {
                call_graph.nodes.get(symbol_id).map(|func| func.name.clone())
            })
            .collect();

        let list = ratatui::widgets::List::new(
            items.into_iter().map(ratatui::widgets::ListItem::new)
        )
        .block(Block::default().borders(Borders::ALL).title("Functions"));

        f.render_widget(list, area);
    }
}
```

### 3. For Smooth Lines in Ratatui

If you must have smooth lines in Ratatui:

```rust
// Custom line renderer using dense points
fn draw_smooth_line(ctx: &mut Context, x1: f64, y1: f64, x2: f64, y2: f64, color: Color) {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let distance = (dx * dx + dy * dy).sqrt();
    let steps = (distance * 4.0) as usize; // High density

    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let x = x1 + t * dx;
        let y = y1 + t * dy;
        
        ctx.draw(&ratatui::widgets::canvas::Points {
            coords: &[(x, y)],
            color,
        });
    }
}
```

### 4. Robust UI Architecture

Focus on these principles:

1. **Start Simple**: Basic rectangular nodes + simple lines
2. **Incremental Enhancement**: Add features one at a time
3. **Clear Separation**: Separate layout calculation from rendering
4. **Performance First**: Profile before optimizing
5. **User Testing**: Get feedback early and often

### 5. Recommended Implementation Plan

**Phase 1: Basic 2D View (1-2 weeks)**
```rust
// Add to App struct
pub struct App {
    // ... existing fields
    pub view_mode: ViewMode,
    pub graph_2d: Option<SimpleGraph2D>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ViewMode {
    Tree,
    Graph2D,
}
```

**Phase 2: Simple Layout (1 week)**
- Implement basic circular or grid layout
- Focus on making it functional, not pretty

**Phase 3: Interaction (1 week)**
- Add pan/zoom controls
- Node selection in 2D space

**Phase 4: Polish (ongoing)**
- Better layouts
- Visual improvements
- Performance optimization

### 6. Alternative: Hybrid Approach

Consider keeping the current tree view as primary and adding a simple "minimap" style 2D view:

```rust
// Small 2D overview in a corner
fn render_mini_graph(&self, f: &mut Frame, area: Rect) {
    // Very simple dot-and-line representation
    // Just for spatial awareness, not primary navigation
}
```

## Conclusion

The attempted 2D implementation was overly complex. I recommend:

1. **Short term**: Implement a simple 2D view using the simplified Ratatui approach above
2. **Medium term**: Consider migrating to egui for better 2D graphics if the simple approach proves limiting
3. **Focus**: Make one layout algorithm work really well rather than implementing many poorly

The current tree view is actually quite good for code navigation. A 2D view should complement it, not replace it entirely.