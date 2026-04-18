use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use logic::query::GraphQueryEngine;
use model::{CallGraph, SymbolId};

pub struct CallGraphView<'a> {
    graph: &'a CallGraph,
    query_engine: &'a GraphQueryEngine<'a>,
}

impl<'a> CallGraphView<'a> {
    pub fn new(graph: &'a CallGraph, query_engine: &'a GraphQueryEngine<'a>) -> Self {
        Self {
            graph,
            query_engine,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect, selected_function: Option<&SymbolId>) {
        if let Some(selected_id) = selected_function {
            self.render_detailed_view(f, area, selected_id);
        } else {
            self.render_overview(f, area);
        }
    }

    fn render_detailed_view(&self, f: &mut Frame, area: Rect, selected_id: &SymbolId) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left panel: Callers
        self.render_callers(f, chunks[0], selected_id);

        // Right panel: Callees
        self.render_callees(f, chunks[1], selected_id);
    }

    fn render_callers(&self, f: &mut Frame, area: Rect, selected_id: &SymbolId) {
        let callers = self.graph.get_callers(selected_id);
        let items: Vec<ListItem> = callers
            .iter()
            .map(|func| {
                let style = if func.diagnostics.is_empty() {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };

                ListItem::new(Line::from(vec![
                    Span::styled(&func.name, style),
                    Span::raw(format!(" ({})", func.definition_location.file_path)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Callers ({})", callers.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));

        f.render_widget(list, area);
    }

    fn render_callees(&self, f: &mut Frame, area: Rect, selected_id: &SymbolId) {
        let callees = self.graph.get_callees(selected_id);
        let items: Vec<ListItem> = callees
            .iter()
            .map(|func| {
                let style = if func.diagnostics.is_empty() {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };

                ListItem::new(Line::from(vec![
                    Span::styled(&func.name, style),
                    Span::raw(format!(" ({})", func.definition_location.file_path)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Callees ({})", callees.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));

        f.render_widget(list, area);
    }

    fn render_overview(&self, f: &mut Frame, area: Rect) {
        let stats = self.query_engine.get_graph_stats();

        let content = format!(
            "Call Graph Overview\n\n\
            Select a function from the Functions tab to see detailed call relationships.\n\n\
            Current Statistics:\n\
            • Total Functions: {}\n\
            • Total Call Relationships: {}\n\
            • Entry Points: {}\n\
            • Leaf Functions: {}\n\
            • Average Calls per Function: {:.2}",
            stats.total_functions,
            stats.total_call_relationships,
            stats.entry_point_count,
            stats.leaf_function_count,
            stats.average_calls_per_function
        );

        let paragraph = Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title("Call Graph"))
            .style(Style::default().fg(Color::Cyan));

        f.render_widget(paragraph, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    fn create_test_setup() -> (CallGraph, GraphQueryEngine<'static>) {
        let mut graph = CallGraph::new();

        let func1 = FunctionNode::new(
            "func1".to_string(),
            "func1".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        let func2 = FunctionNode::new(
            "func2".to_string(),
            "func2".to_string(),
            Location::new("test.rs".to_string(), 5, 0),
        );

        let id1 = graph.add_function(func1);
        let id2 = graph.add_function(func2);

        graph.add_call(id1, id2, Location::new("test.rs".to_string(), 2, 4));

        let leaked_graph = Box::leak(Box::new(graph.clone()));
        let query_engine = logic::query::GraphQueryEngine::new(leaked_graph);

        (graph, query_engine)
    }

    #[test]
    fn test_call_graph_view_creation() {
        let (graph, query_engine) = create_test_setup();
        let view = CallGraphView::new(&graph, &query_engine);

        // Just test that we can create the view
        assert!(true);
    }
}
