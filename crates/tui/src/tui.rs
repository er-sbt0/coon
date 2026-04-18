use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::sync::Once;
use tokio::sync::mpsc;

static PANIC_HOOK: Once = Once::new();

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            original(info);
        }));
    });
}

/// Interval in milliseconds for polling terminal events between UI redraws.
const EVENT_POLL_INTERVAL_MS: u64 = 100;

use lsp::{LspRequest, LspResponse};
use model::{lsp_status::LspUiMessage, CallGraph};

use crate::app::App;
use crate::key_map;
use crate::rendering::ui;

/// Main TUI application runner
pub struct TuiApp {
    app: App,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TuiApp {
    fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        Ok(Terminal::new(backend)?)
    }

    pub fn new(call_graph: CallGraph) -> Result<Self, Box<dyn std::error::Error>> {
        install_panic_hook();
        let terminal = Self::setup_terminal()?;
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
        install_panic_hook();
        let terminal = Self::setup_terminal()?;
        let app = App::new_with_lsp_async(call_graph, lsp_rx, lsp_channels_rx);
        Ok(Self { app, terminal })
    }

    pub fn set_lsp_channels(
        &mut self,
        response_rx: mpsc::UnboundedReceiver<LspResponse>,
        request_tx: mpsc::UnboundedSender<LspRequest>,
    ) {
        self.app.lsp.set_channels(response_rx, request_tx);
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
            if event::poll(std::time::Duration::from_millis(EVENT_POLL_INTERVAL_MS))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if let Some(action) = key_map::map_key_event(key, self.app.show_search_bar)
                        {
                            self.app.handle_action(action);
                        }
                    }
                }
            }

            if self.app.should_quit {
                break;
            }
        }

        Ok(())
    }
}

impl Drop for TuiApp {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}
