use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use tokio::sync::mpsc;

use lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspUiMessage, CallGraph};

use crate::actions::Action;
use crate::app::App;
use crate::rendering::ui;

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

    pub fn new_with_lsp_async(
        call_graph: CallGraph,
        lsp_rx: mpsc::UnboundedReceiver<LspUiMessage>,
        lsp_channels_rx: mpsc::UnboundedReceiver<(
            mpsc::UnboundedReceiver<LspResponse>,
            mpsc::UnboundedSender<LspRequest>,
        )>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Setup terminal (same as new)
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let app = App::new_with_lsp_async(call_graph, lsp_rx, lsp_channels_rx);

        Ok(Self { app, terminal })
    }

    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.app.set_lsp_channels(response_rx, request_tx);
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            // Drain any async LSP loader messages
            self.app.poll_lsp_loader_messages();

            // Poll for LSP channels and wire them up when they arrive
            self.app.poll_lsp_channels();

            // Check for LSP responses first
            self.app.check_lsp_responses();

            // Draw UI
            self.terminal.draw(|f| ui(f, &mut self.app))?;

            // Handle input with timeout to allow checking for LSP responses
            if event::poll(std::time::Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        // Handle search bar input separately
                        if self.app.show_search_bar {
                            match key.code {
                                KeyCode::Esc => {
                                    self.app.toggle_search_bar();
                                }
                                KeyCode::Enter => {
                                    self.app.select_from_search();
                                }
                                KeyCode::Up => {
                                    self.app.search_bar_state.select_previous();
                                }
                                KeyCode::Down => {
                                    self.app.search_bar_state.select_next();
                                }
                                KeyCode::Tab => {
                                    self.app.search_bar_state.cycle_search_mode();
                                    self.app
                                        .search_bar_state
                                        .update_results(&self.app.call_graph);
                                }
                                KeyCode::Backspace => {
                                    self.app.handle_search_backspace();
                                }
                                KeyCode::Delete => {
                                    self.app.search_bar_state.delete_char_forward();
                                    self.app
                                        .search_bar_state
                                        .update_results(&self.app.call_graph);
                                }
                                KeyCode::Left => {
                                    self.app.search_bar_state.move_cursor_right();
                                }
                                KeyCode::Right => {
                                    self.app.search_bar_state.move_cursor_left();
                                }
                                KeyCode::Home => {
                                    self.app.search_bar_state.move_cursor_start();
                                }
                                KeyCode::End => {
                                    self.app.search_bar_state.move_cursor_end();
                                }
                                KeyCode::Char(c) => {
                                    self.app.handle_search_input(c);
                                }
                                _ => {}
                            }
                            continue; // Don't process other actions when search is active
                        }

                        let action = match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
                            KeyCode::Char('?') => Some(Action::Help),
                            KeyCode::Char('n')
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NewWorkspace)
                            }
                            KeyCode::Char('t')
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NewWorkspace)
                            }
                            KeyCode::Char('W') => Some(Action::CloseWorkspace),
                            KeyCode::Char('f') => {
                                self.app.toggle_search_bar();
                                None
                            }
                            KeyCode::Char(']') => Some(Action::NextWorkspace),
                            KeyCode::Char('[') => Some(Action::PreviousWorkspace),
                            KeyCode::Tab
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::NextWorkspace)
                            }
                            KeyCode::BackTab
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                Some(Action::PreviousWorkspace)
                            }
                            KeyCode::Char('1') => {
                                self.app.switch_workspace(0);
                                None
                            }
                            KeyCode::Char('2') => {
                                self.app.switch_workspace(1);
                                None
                            }
                            KeyCode::Char('3') => {
                                self.app.switch_workspace(2);
                                None
                            }
                            KeyCode::Char('4') => {
                                self.app.switch_workspace(3);
                                None
                            }
                            KeyCode::Char('5') => {
                                self.app.switch_workspace(4);
                                None
                            }
                            KeyCode::Char('6') => {
                                self.app.switch_workspace(5);
                                None
                            }
                            KeyCode::Char('7') => {
                                self.app.switch_workspace(6);
                                None
                            }
                            KeyCode::Char('8') => {
                                self.app.switch_workspace(7);
                                None
                            }
                            KeyCode::Char('9') => {
                                self.app.switch_workspace(8);
                                None
                            }
                            KeyCode::Up => Some(Action::MoveDown),
                            KeyCode::Down => Some(Action::MoveUp),
                            KeyCode::Right => Some(Action::MoveLeft),
                            KeyCode::Left => Some(Action::MoveRight),
                            KeyCode::Char('h') => Some(Action::NavigateParent),
                            KeyCode::Char('l') => Some(Action::NavigateChild),
                            KeyCode::Char('k') => Some(Action::NavigatePrevSibling),
                            KeyCode::Char('j') => Some(Action::NavigateNextSibling),
                            KeyCode::Enter => Some(Action::ExpandOrCollapse),
                            KeyCode::Char('r') => Some(Action::ResetView),
                            KeyCode::Char('F') => Some(Action::FindReferences),
                            KeyCode::Char('t') => Some(Action::ToggleCallDirection),
                            KeyCode::Char('R') => Some(Action::Refresh),
                            _ => None,
                        };

                        if let Some(action) = action {
                            self.app.handle_action(action);
                        }
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
