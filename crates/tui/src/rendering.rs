use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame,
};

use model::lsp_status::LspLoadPhase;

use crate::app::App;
use crate::graph_view::GraphView;
use crate::search_bar::SearchBar;

/// UI rendering function (separate from TuiApp to avoid borrowing issues)
pub fn ui(f: &mut Frame, app: &mut App) {
    let size = f.area();

    // Show help overlay if requested
    if app.show_help {
        render_help_overlay(f, size);
        return;
    }

    // Show search bar if active
    if app.show_search_bar {
        render_search_bar_overlay(f, size, app);
        return;
    }

    // Show workspace manager if requested
    if app.show_workspace_manager {
        render_workspace_manager_modal(f, size, app);
        return;
    }

    // Create main layout with conditional LSP status bar above graph view.
    // `Completed` is not the end of the wait: clangd has handed over the
    // symbols, but their edges are still being fetched, so the bar stays up
    // until the warmup drains too.
    let show_lsp_status = !matches!(app.core.lsp.status, LspLoadPhase::Completed)
        || app.core.warmup_progress().is_some();

    let chunks = if show_lsp_status {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Workspace tabs
                Constraint::Length(1), // LSP Status bar
                Constraint::Length(1), // Blank line separator
                Constraint::Min(0),    // Main content (graph view)
            ])
            .split(size)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Workspace tabs
                Constraint::Min(0),    // Main content (graph view)
            ])
            .split(size)
    };

    // Render workspace tabs
    render_workspace_tabs(f, chunks[0], app);

    if show_lsp_status {
        // Render LSP loading status bar above graph view
        render_lsp_status_bar(f, chunks[1], app);
        // chunks[2] is blank separator
        // Render graph view (current workspace)
        render_graph_view(f, chunks[3], app);
    } else {
        // Render graph view (current workspace)
        render_graph_view(f, chunks[1], app);
    }
}

fn render_lsp_status_bar(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // The warmup outlives the load phase, so it wins the line while it runs:
    // saying "ready" over a graph that is still growing edges is the one
    // reading the user cannot act on.
    if let Some((done, total)) = app.core.warmup_progress() {
        let status_text = format!("LSP: resolving calls… {done}/{total}");
        let paragraph = Paragraph::new(Line::from(status_text)).block(
            Block::default()
                .borders(Borders::NONE)
                .style(Style::default().fg(Color::Gray)),
        );
        f.render_widget(paragraph, area);
        return;
    }

    let status_text = match &app.core.lsp.status {
        LspLoadPhase::NotStarted => "LSP: not started".to_string(),
        LspLoadPhase::SpawningServer => "LSP: starting clangd…".to_string(),
        LspLoadPhase::Initializing => "LSP: initializing…".to_string(),
        LspLoadPhase::Initialized => "LSP: initialized".to_string(),
        LspLoadPhase::DiscoveringFiles => "LSP: discovering project files…".to_string(),
        LspLoadPhase::PreloadingDocuments { done, total } => {
            format!("LSP: preloading documents… {}/{}", done, total)
        }
        LspLoadPhase::IndexingProject { done, total } => {
            if *total > 0 {
                format!("LSP: indexing project… {}/{}", done, total)
            } else {
                "LSP: indexing project…".to_string()
            }
        }
        LspLoadPhase::LoadingWorkspaceSymbols { loaded } => {
            format!("LSP: loading workspace symbols… {}", loaded)
        }
        LspLoadPhase::Completed => "LSP: ready".to_string(),
        LspLoadPhase::Failed(err) => format!("LSP: failed ({})", err),
    };

    let paragraph = Paragraph::new(Line::from(status_text)).block(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().fg(Color::Gray)),
    );

    f.render_widget(paragraph, area);
}

fn render_help_overlay(f: &mut Frame, area: ratatui::layout::Rect) {
    let help_text = vec![
        Line::from("📐 lsp-callgraph-tui - Help"),
        Line::from(""),
        Line::from("🎮 Key Bindings:"),
        Line::from(""),
        Line::from("Workspace Management:"),
        Line::from("  CtrlN/T   - Create new workspace"),
        Line::from("  W         - Close current workspace"),
        Line::from("  CtrlTab/] - Next workspace"),
        Line::from("  [/Ctrl⇧Tab - Previous workspace"),
        Line::from("  f         - Search symbols (create workspace"),
        Line::from("  1-9       - Jump to workspace 1-9"),
        Line::from(""),
        Line::from("Graph View Navigation:"),
        Line::from("  ↑↓←→/hjkl - Pan the view"),
        Line::from("  r         - Reset view"),
        Line::from("  t         - Toggle call direction"),
        Line::from("  Enter     - Select next node"),
        Line::from(""),
        Line::from("General:"),
        Line::from("  F         - Find references"),
        Line::from("  R         - Refresh from LSP"),
        Line::from("  ?         - Show/hide this help"),
        Line::from("  q / Esc   - Quit application"),
        Line::from(""),
        Line::from("🌲 Graph View Behavior:"),
        Line::from("  • Each workspace shows an independent call graph"),
        Line::from("  • Create multiple workspaces to compare graphs"),
        Line::from("  • Panning is per-workspace"),
        Line::from(""),
        Line::from("💡 Key Changes:"),
        Line::from("  • 'W' (Shift+w) closes the current workspace"),
        Line::from("  • 'q' or 'Esc' exits the entire application"),
        Line::from("  • 'f' opens search/symbol finder"),
        Line::from(""),
        Line::from("Press ? again to close this help"),
    ];

    // Create a centered popup
    let popup_area = ratatui::layout::Rect {
        x: area.width / 8,
        y: area.height / 8,
        width: area.width * 3 / 4,
        height: area.height * 3 / 4,
    };

    // Clear the background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    let help_paragraph = Paragraph::new(help_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Help - Key Bindings")
                .style(Style::default().fg(Color::Yellow)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(help_paragraph, popup_area);
}

fn render_search_bar_overlay(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    // Create centered overlay
    let popup_width = area.width.min(100);
    let popup_height = area.height.min(30);
    let popup_area = ratatui::layout::Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Dim background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    // Render search bar
    let search_bar = SearchBar::new();
    search_bar.render(popup_area, f.buffer_mut(), &mut app.search_bar_state);
}

fn render_workspace_tabs(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let mut tab_titles = Vec::new();

    for (i, workspace) in app.core.workspaces.workspaces.iter().enumerate() {
        let mut name = workspace.name.clone();

        // Truncate long names
        if name.len() > 15 {
            name = format!("{}...", &name[..12]);
        }

        // Add active indicator
        if i == app.core.workspaces.current_index {
            name = format!("[{}*]", name);
        } else {
            name = format!("[{}]", name);
        }

        tab_titles.push(name);
    }

    let tabs = Tabs::new(tab_titles)
        .block(Block::default().borders(Borders::ALL).title("Workspaces"))
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .select(app.core.workspaces.current_index);
    f.render_widget(tabs, area);
}

fn render_graph_view(f: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    // Get current workspace id
    let workspace_id = match app.core.workspaces.current() {
        Some(w) => w.id,
        None => {
            let empty_text = vec![
                Line::from("No workspace available"),
                Line::from(""),
                Line::from("Press CtrlN to create a new workspace"),
            ];
            let paragraph = Paragraph::new(empty_text)
                .block(Block::default().borders(Borders::ALL).title("Graph View"))
                .style(Style::default().fg(Color::Gray));
            f.render_widget(paragraph, area);
            return;
        }
    };

    let viewport_size = (area.width as f32, area.height as f32);
    app.last_viewport_size = viewport_size;

    if app.view_states.contains_key(&workspace_id) {
        // Update layout if dirty (borrow call_graph separately to satisfy borrow checker)
        let update_result = {
            let call_graph = &app.core.call_graph;
            let vs = app.view_states.get_mut(&workspace_id).unwrap();
            vs.update_layout(call_graph, viewport_size)
        };

        if let Err(e) = update_result {
            let error_text = vec![
                Line::from("Layout Error"),
                Line::from(""),
                Line::from(format!("Failed to compute layout: {}", e)),
            ];
            let paragraph = Paragraph::new(error_text)
                .block(Block::default().borders(Borders::ALL).title("Graph View"))
                .style(Style::default().fg(Color::Red));
            f.render_widget(paragraph, area);
            return;
        }

        let graph_view = GraphView::new(&app.core.call_graph).show_help(app.show_help);
        let vs = app.view_states.get_mut(&workspace_id).unwrap();
        f.render_stateful_widget(graph_view, area, vs);
    } else {
        let empty_text = vec![
            Line::from("No workspace available"),
            Line::from(""),
            Line::from("Press CtrlN to create a new workspace"),
        ];
        let paragraph = Paragraph::new(empty_text)
            .block(Block::default().borders(Borders::ALL).title("Graph View"))
            .style(Style::default().fg(Color::Gray));
        f.render_widget(paragraph, area);
    }
}

fn render_workspace_manager_modal(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    // Create a centered popup
    let popup_width = area.width.min(80);
    let popup_height = area.height.min(30);
    let popup_area = ratatui::layout::Rect {
        x: (area.width - popup_width) / 2,
        y: (area.height - popup_height) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Clear the background
    let clear_block = Block::default()
        .style(Style::default().bg(Color::Black))
        .borders(Borders::NONE);
    f.render_widget(clear_block, area);

    let mut text = vec![
        Line::from("Workspace Manager"),
        Line::from(""),
        Line::from(format!(
            "Total Workspaces: {}",
            app.core.workspaces.workspaces.len()
        )),
        Line::from(""),
    ];

    for (i, workspace) in app.core.workspaces.workspaces.iter().enumerate() {
        let marker = if i == app.core.workspaces.current_index {
            "→"
        } else {
            " "
        };

        let root_info = workspace
            .root_symbol
            .as_ref()
            .and_then(|id| app.core.call_graph.get_function(id))
            .map(|f| f.name.as_str())
            .unwrap_or("(empty)");

        text.push(Line::from(format!(
            "{} {}. {} - Root: {}",
            marker,
            i + 1,
            workspace.name,
            root_info
        )));
    }

    text.push(Line::from(""));
    text.push(Line::from("Use 1-9 to switch, Esc to close"));

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Workspaces")
                .style(Style::default().fg(Color::Green)),
        )
        .style(Style::default().fg(Color::White).bg(Color::Black));

    f.render_widget(paragraph, popup_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::CallGraph;
    use ratatui::{backend::TestBackend, Terminal};

    /// Render the whole UI and return the screen as text.
    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).expect("test backend");
        terminal.draw(|f| ui(f, app)).expect("draws");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(60)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_status_bar_stays_up_while_the_edges_are_still_arriving() {
        // `Completed` only means clangd has stopped handing over symbols. Their
        // call edges are fetched afterwards, and on a large project that is the
        // part the user actually waits through — so the bar must not vanish the
        // moment the phase flips.
        let mut app = App::new(CallGraph::new());
        app.core.lsp.status = LspLoadPhase::Completed;

        assert!(
            !screen(&mut app).contains("LSP:"),
            "a finished load with no warmup running shows no status bar"
        );

        // Mid-warmup: seeded, and part of the queue still owed.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.core.lsp.request_tx = Some(tx);
        for i in 0..5 {
            app.core.call_graph.add_function(model::FunctionNode::new(
                format!("f{i}"),
                format!("f{i}"),
                model::Location::new("/src/t.c".to_string(), i + 1, 1),
            ));
        }
        app.core.tick();

        let rendered = screen(&mut app);
        assert!(
            rendered.contains("resolving calls"),
            "the warmup owns the line while it runs, got:\n{rendered}"
        );
        assert!(
            !rendered.contains("LSP: ready"),
            "and must not claim the wait is over"
        );
    }
}
