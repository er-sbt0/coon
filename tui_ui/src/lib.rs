use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Tabs},
    Frame, Terminal,
};
use std::io;

use core_data::{CallGraph, FunctionNode, SymbolId};
use logic::query::GraphQueryEngine;

pub mod call_graph_view;
pub mod diagnostic_panel;
pub mod function_list;

pub use call_graph_view::CallGraphView;
pub use diagnostic_panel::DiagnosticPanel;
pub use function_list::FunctionList;

/// Main application state
pub struct App {
    pub call_graph: CallGraph,
    pub query_engine: GraphQueryEngine<'static>,
    pub current_tab: usize,
    pub selected_function: Option<SymbolId>,
    pub search_query: String,
    pub should_quit: bool,
    pub status_message: String,
}

impl App {
    pub fn new(call_graph: CallGraph) -> Self {
        // We need to create a static reference for the query engine
        // In a real application, this would be handled differently
        let leaked_graph = Box::leak(Box::new(call_graph.clone()));
        let query_engine = GraphQueryEngine::new(leaked_graph);

        Self {
            call_graph,
            query_engine,
            current_tab: 0,
            selected_function: None,
            search_query: String::new(),
            should_quit: false,
            status_message: "Ready".to_string(),
        }
    }

    pub fn select_function(&mut self, id: SymbolId) {
        self.selected_function = Some(id);
        self.status_message = "Function selected".to_string();
    }

    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
    }

    pub fn next_tab(&mut self) {
        self.current_tab = (self.current_tab + 1) % 3;
    }

    pub fn previous_tab(&mut self) {
        if self.current_tab == 0 {
            self.current_tab = 2;
        } else {
            self.current_tab -= 1;
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }
}

/// Main TUI application runner
pub struct TuiApp {
    app: App,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TuiApp {
    pub fn new(call_graph: CallGraph) -> Result<Self, Box<dyn std::error::Error>> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let app = App::new(call_graph);

        Ok(Self { app, terminal })
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // Create a reference to app for the UI drawing
            let app_ref = &self.app;
            self.terminal.draw(|f| ui(f, app_ref))?;

            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            self.app.quit();
                        }
                        KeyCode::Tab => {
                            self.app.next_tab();
                        }
                        KeyCode::BackTab => {
                            self.app.previous_tab();
                        }
                        KeyCode::Char('1') => {
                            self.app.current_tab = 0;
                        }
                        KeyCode::Char('2') => {
                            self.app.current_tab = 1;
                        }
                        KeyCode::Char('3') => {
                            self.app.current_tab = 2;
                        }
                        _ => {}
                    }
                }
            }

            if self.app.should_quit {
                break;
            }
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;

        Ok(())
    }
}

/// UI rendering function (separate from TuiApp to avoid borrowing issues)
fn ui(f: &mut Frame, app: &App) {
    let size = f.area();

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tab bar
            Constraint::Min(0),    // Main content
            Constraint::Length(3), // Status bar
        ])
        .split(size);

    // Render tab bar
    let tab_titles = vec!["Functions", "Call Graph", "Diagnostics"];
    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("Navigation"))
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(app.current_tab);
    f.render_widget(tabs, chunks[0]);

    // Render main content based on selected tab
    match app.current_tab {
        0 => render_function_list(f, chunks[1], app),
        1 => render_call_graph(f, chunks[1], app),
        2 => render_diagnostics(f, chunks[1], app),
        _ => {}
    }

    // Render status bar
    let status_paragraph = Paragraph::new(format!(
        "Status: {} | Selected: {} | Tab: {} | Press 'q' to quit",
        app.status_message,
        app.selected_function
            .as_ref()
            .and_then(|id| app.call_graph.get_function(id))
            .map(|f| f.name.as_str())
            .unwrap_or("None"),
        match app.current_tab {
            0 => "Functions",
            1 => "Call Graph",
            2 => "Diagnostics",
            _ => "Unknown",
        }
    ))
    .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(status_paragraph, chunks[2]);
}

fn render_function_list(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let functions: Vec<&FunctionNode> = app.call_graph.nodes.values().collect();
    let items: Vec<ListItem> = functions
        .iter()
        .map(|func| {
            let style = if func.diagnostics.is_empty() {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Red)
            };

            ListItem::new(Line::from(vec![
                Span::styled(func.name.clone(), style),
                Span::raw(format!(" ({})", func.qualified_name)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Functions"))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    f.render_widget(list, area);
}

fn render_call_graph(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let content = if let Some(selected_id) = &app.selected_function {
        if let Some(function) = app.call_graph.get_function(selected_id) {
            let callers = app.call_graph.get_callers(selected_id);
            let callees = app.call_graph.get_callees(selected_id);

            format!(
                "Selected Function: {}\n\nCallers ({}):\n{}\n\nCallees ({}):\n{}",
                function.name,
                callers.len(),
                callers
                    .iter()
                    .map(|f| format!("  - {}", f.name))
                    .collect::<Vec<_>>()
                    .join("\n"),
                callees.len(),
                callees
                    .iter()
                    .map(|f| format!("  - {}", f.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        } else {
            "Selected function not found".to_string()
        }
    } else {
        "No function selected.\nSelect a function from the Functions tab to see its call graph."
            .to_string()
    };

    let paragraph =
        Paragraph::new(content).block(Block::default().borders(Borders::ALL).title("Call Graph"));
    f.render_widget(paragraph, area);
}

fn render_diagnostics(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let stats = app.query_engine.get_graph_stats();
    let problems = app.query_engine.find_problem_areas();

    let content = format!(
        "Graph Statistics:\n\
        Total Functions: {}\n\
        Total Call Relationships: {}\n\
        Entry Points: {}\n\
        Leaf Functions: {}\n\
        Functions with Diagnostics: {}\n\
        Average Calls per Function: {:.2}\n\n\
        Problem Areas:\n\
        Functions with Errors: {}\n\
        Functions with Warnings: {}\n\
        Highly Connected Functions: {}",
        stats.total_functions,
        stats.total_call_relationships,
        stats.entry_point_count,
        stats.leaf_function_count,
        stats.functions_with_diagnostics,
        stats.average_calls_per_function,
        problems.functions_with_errors,
        problems.functions_with_warnings,
        problems.highly_connected_functions
    );

    let paragraph = Paragraph::new(content).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Diagnostics & Statistics"),
    );
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_data::{FunctionNode, Location};

    fn create_test_app() -> App {
        let mut graph = CallGraph::new();
        let func = FunctionNode::new(
            "test_func".to_string(),
            "test::test_func".to_string(),
            Location::new("test.rs".to_string(), 1, 0),
        );
        graph.add_function(func);
        App::new(graph)
    }

    #[test]
    fn test_app_creation() {
        let app = create_test_app();
        assert_eq!(app.current_tab, 0);
        assert!(app.selected_function.is_none());
        assert_eq!(app.search_query, "");
        assert!(!app.should_quit);
    }

    #[test]
    fn test_tab_navigation() {
        let mut app = create_test_app();

        app.next_tab();
        assert_eq!(app.current_tab, 1);

        app.next_tab();
        assert_eq!(app.current_tab, 2);

        app.next_tab();
        assert_eq!(app.current_tab, 0);

        app.previous_tab();
        assert_eq!(app.current_tab, 2);
    }

    #[test]
    fn test_function_selection() {
        let mut app = create_test_app();
        let func_id = app.call_graph.nodes.keys().next().unwrap().clone();

        app.select_function(func_id.clone());
        assert_eq!(app.selected_function, Some(func_id));
        assert_eq!(app.status_message, "Function selected");
    }
}
