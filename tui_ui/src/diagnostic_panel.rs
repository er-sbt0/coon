use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use core_data::{CallGraph, DiagnosticSeverity};
use logic::query::GraphQueryEngine;

pub struct DiagnosticPanel<'a> {
    graph: &'a CallGraph,
    query_engine: &'a GraphQueryEngine<'a>,
}

impl<'a> DiagnosticPanel<'a> {
    pub fn new(graph: &'a CallGraph, query_engine: &'a GraphQueryEngine<'a>) -> Self {
        Self {
            graph,
            query_engine,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(10), // Statistics
                Constraint::Min(0),     // Diagnostic details
            ])
            .split(area);

        self.render_statistics(f, chunks[0]);
        self.render_diagnostic_details(f, chunks[1]);
    }

    fn render_statistics(&self, f: &mut Frame, area: Rect) {
        let stats = self.query_engine.get_graph_stats();
        let problems = self.query_engine.find_problem_areas();

        let content = format!(
            "Graph Statistics:\n\
            Total Functions: {}\n\
            Functions with Diagnostics: {}\n\
            Entry Points: {}\n\
            Leaf Functions: {}\n\n\
            Problem Summary:\n\
            Functions with Errors: {}\n\
            Functions with Warnings: {}\n\
            Highly Connected Functions: {}",
            stats.total_functions,
            stats.functions_with_diagnostics,
            stats.entry_point_count,
            stats.leaf_function_count,
            problems.functions_with_errors,
            problems.functions_with_warnings,
            problems.highly_connected_functions
        );

        let paragraph = Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title("Statistics"))
            .style(Style::default().fg(Color::White));

        f.render_widget(paragraph, area);
    }

    fn render_diagnostic_details(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        self.render_errors(f, chunks[0]);
        self.render_warnings(f, chunks[1]);
    }

    fn render_errors(&self, f: &mut Frame, area: Rect) {
        let functions_with_errors = self
            .query_engine
            .filter
            .filter_by_diagnostic_severity(DiagnosticSeverity::Error);

        let items: Vec<ListItem> = functions_with_errors
            .iter()
            .flat_map(|func| {
                func.diagnostics
                    .iter()
                    .filter(|diag| diag.severity == DiagnosticSeverity::Error)
                    .map(|diag| {
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                &func.name,
                                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(": "),
                            Span::styled(&diag.message, Style::default().fg(Color::Red)),
                        ]))
                    })
            })
            .collect();

        let item_count = items.len();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Errors ({})", item_count)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));

        f.render_widget(list, area);
    }

    fn render_warnings(&self, f: &mut Frame, area: Rect) {
        let functions_with_warnings = self
            .query_engine
            .filter
            .filter_by_diagnostic_severity(DiagnosticSeverity::Warning);

        let items: Vec<ListItem> = functions_with_warnings
            .iter()
            .flat_map(|func| {
                func.diagnostics
                    .iter()
                    .filter(|diag| diag.severity == DiagnosticSeverity::Warning)
                    .map(|diag| {
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                &func.name,
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(": "),
                            Span::styled(&diag.message, Style::default().fg(Color::Yellow)),
                        ]))
                    })
            })
            .collect();

        let item_count = items.len();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Warnings ({})", item_count)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));

        f.render_widget(list, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_data::{Diagnostic, FunctionNode, Location};

    fn create_test_setup() -> (CallGraph, GraphQueryEngine<'static>) {
        let mut graph = CallGraph::new();

        let mut func_with_error = FunctionNode::new(
            "error_func".to_string(),
            "error_func".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        func_with_error.add_diagnostic(Diagnostic {
            location: Location::new("test.rs".to_string(), 2, 0),
            severity: DiagnosticSeverity::Error,
            message: "Test error".to_string(),
            code: Some("E001".to_string()),
        });

        let mut func_with_warning = FunctionNode::new(
            "warning_func".to_string(),
            "warning_func".to_string(),
            Location::new("test.rs".to_string(), 5, 0),
        );
        func_with_warning.add_diagnostic(Diagnostic {
            location: Location::new("test.rs".to_string(), 6, 0),
            severity: DiagnosticSeverity::Warning,
            message: "Test warning".to_string(),
            code: Some("W001".to_string()),
        });

        graph.add_function(func_with_error);
        graph.add_function(func_with_warning);

        let leaked_graph = Box::leak(Box::new(graph.clone()));
        let query_engine = logic::query::GraphQueryEngine::new(leaked_graph);

        (graph, query_engine)
    }

    #[test]
    fn test_diagnostic_panel_creation() {
        let (graph, query_engine) = create_test_setup();
        let panel = DiagnosticPanel::new(&graph, &query_engine);

        // Just test that we can create the panel
        assert!(true);
    }
}
