use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use logic::query::GraphQueryEngine;
use model::{CallGraph, FunctionNode, SymbolId};

pub struct FunctionList<'a> {
    graph: &'a CallGraph,
    query_engine: &'a GraphQueryEngine<'a>,
    pub state: ListState,
    pub functions: Vec<&'a FunctionNode>,
    search_query: String,
}

impl<'a> FunctionList<'a> {
    pub fn new(graph: &'a CallGraph, query_engine: &'a GraphQueryEngine<'a>) -> Self {
        let functions: Vec<&FunctionNode> = graph.nodes.values().collect();
        let mut state = ListState::default();
        if !functions.is_empty() {
            state.select(Some(0));
        }

        Self {
            graph,
            query_engine,
            state,
            functions,
            search_query: String::new(),
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);

        self.render_function_list(f, chunks[0]);
        self.render_function_details(f, chunks[1]);
    }

    fn render_function_list(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .functions
            .iter()
            .map(|func| {
                let (style, indicator) = self.get_function_style(func);

                ListItem::new(Line::from(vec![
                    Span::styled(indicator, style),
                    Span::styled(&func.name, style),
                    Span::raw(" "),
                    Span::styled(
                        format!("({})", func.definition_location.file_path),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Functions ({})", self.functions.len())),
            )
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray),
            );

        f.render_stateful_widget(list, area, &mut self.state);
    }

    fn render_function_details(&self, f: &mut Frame, area: Rect) {
        let content = if let Some(selected_idx) = self.state.selected() {
            if let Some(func) = self.functions.get(selected_idx) {
                self.format_function_details(func)
            } else {
                "No function selected".to_string()
            }
        } else {
            "No function selected".to_string()
        };

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Function Details"),
            )
            .style(Style::default().fg(Color::White));

        f.render_widget(paragraph, area);
    }

    fn get_function_style(&self, _func: &FunctionNode) -> (Style, &'static str) {
        (Style::default().fg(Color::Green), "✓ ")
    }

    fn format_function_details(&self, func: &FunctionNode) -> String {
        let callers = self.graph.get_callers(&func.id);
        let callees = self.graph.get_callees(&func.id);

        format!(
            "Name: {}\n\
            Qualified Name: {}\n\
            Location: {}:{}\n\
            Callers: {}\n\
            Callees: {}\n\
            References: {}\n\n\
            Description:\n\
            This is a function in the codebase.",
            func.name,
            func.qualified_name,
            func.definition_location.file_path,
            func.definition_location.line,
            callers.len(),
            callees.len(),
            func.references.len()
        )
    }

    pub fn next(&mut self) {
        if self.functions.is_empty() {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.functions.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        if self.functions.is_empty() {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.functions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn selected_function(&self) -> Option<&SymbolId> {
        self.state
            .selected()
            .and_then(|i| self.functions.get(i))
            .map(|func| &func.id)
    }

    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
        self.update_filtered_functions();
    }

    fn update_filtered_functions(&mut self) {
        if self.search_query.is_empty() {
            self.functions = self.graph.nodes.values().collect();
        } else {
            self.functions = self.query_engine.search_functions(&self.search_query);
        }

        // Reset selection to first item
        if !self.functions.is_empty() {
            self.state.select(Some(0));
        } else {
            self.state.select(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::{FunctionNode, Location};

    fn create_test_setup() -> (CallGraph, GraphQueryEngine<'static>) {
        let mut graph = CallGraph::new();

        let func1 = FunctionNode::new(
            "test_function".to_string(),
            "mod::test_function".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        let func2 = FunctionNode::new(
            "other_function".to_string(),
            "mod::other_function".to_string(),
            Location::new("other.rs".to_string(), 1, 0),
        );

        graph.add_function(func1);
        graph.add_function(func2);

        let leaked_graph = Box::leak(Box::new(graph.clone()));
        let query_engine = logic::query::GraphQueryEngine::new(leaked_graph);

        (graph, query_engine)
    }

    #[test]
    fn test_function_list_creation() {
        let (graph, query_engine) = create_test_setup();
        let list = FunctionList::new(&graph, &query_engine);

        assert_eq!(list.functions.len(), 2);
        assert!(list.state.selected().is_some());
    }

    #[test]
    fn test_navigation() {
        let (graph, query_engine) = create_test_setup();
        let mut list = FunctionList::new(&graph, &query_engine);

        assert_eq!(list.state.selected(), Some(0));

        list.next();
        assert_eq!(list.state.selected(), Some(1));

        list.next();
        assert_eq!(list.state.selected(), Some(0)); // wraps around

        list.previous();
        assert_eq!(list.state.selected(), Some(1));
    }

    #[test]
    fn test_search_filtering() {
        let (graph, query_engine) = create_test_setup();
        let mut list = FunctionList::new(&graph, &query_engine);

        assert_eq!(list.functions.len(), 2);

        list.set_search_query("test".to_string());
        assert_eq!(list.functions.len(), 1);
        assert_eq!(list.functions[0].name, "test_function");

        list.set_search_query("".to_string());
        assert_eq!(list.functions.len(), 2);
    }
}
