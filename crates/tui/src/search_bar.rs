use model::{CallGraph, SymbolId};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, Paragraph, Widget},
};

/// Default assumed number of visible search results for scroll adjustment.
const DEFAULT_VISIBLE_RESULTS: usize = 20;

/// Search mode for filtering symbols
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Fuzzy search on function names
    Fuzzy,
    /// Exact substring match on function names
    Exact,
    /// Search by file path
    FilePath,
}

impl SearchMode {
    pub fn as_str(&self) -> &str {
        match self {
            SearchMode::Fuzzy => "[Fuzzy]",
            SearchMode::Exact => "[Exact]",
            SearchMode::FilePath => "[Path]",
        }
    }
}

/// A search result entry
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub symbol_id: SymbolId,
    pub name: String,
    pub file_path: String,
    pub line: u32,
    pub match_score: f32, // For ranking fuzzy matches
}

/// State for the search bar
pub struct SearchBarState {
    pub query: String,
    pub cursor_position: usize,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub search_mode: SearchMode,
    pub is_active: bool,
    pub filtered_results: Vec<SearchResult>,
    pub max_results: usize,
}

impl SearchBarState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            cursor_position: 0,
            selected_index: 0,
            scroll_offset: 0,
            search_mode: SearchMode::Fuzzy,
            is_active: false,
            filtered_results: Vec::new(),
            max_results: 50,
        }
    }

    /// Activate the search bar and clear previous state
    pub fn activate(&mut self) {
        self.is_active = true;
        self.query.clear();
        self.cursor_position = 0;
        self.selected_index = 0;
        self.scroll_offset = 0;
        self.filtered_results.clear();
    }

    /// Deactivate the search bar
    pub fn deactivate(&mut self) {
        self.is_active = false;
    }

    /// Add a character at the cursor position
    pub fn insert_char(&mut self, c: char) {
        self.query.insert(self.cursor_position, c);
        self.cursor_position += 1;
    }

    /// Delete character before cursor (backspace)
    pub fn delete_char(&mut self) {
        if self.cursor_position > 0 {
            self.query.remove(self.cursor_position - 1);
            self.cursor_position -= 1;
        }
    }

    /// Delete character at cursor (delete)
    pub fn delete_char_forward(&mut self) {
        if self.cursor_position < self.query.len() {
            self.query.remove(self.cursor_position);
        }
    }

    /// Move cursor left
    pub fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }

    /// Move cursor right
    pub fn move_cursor_right(&mut self) {
        if self.cursor_position < self.query.len() {
            self.cursor_position += 1;
        }
    }

    /// Move cursor to start
    pub fn move_cursor_start(&mut self) {
        self.cursor_position = 0;
    }

    /// Move cursor to end
    pub fn move_cursor_end(&mut self) {
        self.cursor_position = self.query.len();
    }

    /// Select next result
    pub fn select_next(&mut self) {
        if !self.filtered_results.is_empty() {
            self.selected_index = (self.selected_index + 1).min(self.filtered_results.len() - 1);
            self.adjust_scroll(DEFAULT_VISIBLE_RESULTS);
        }
    }

    /// Select previous result
    pub fn select_previous(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
            self.adjust_scroll(DEFAULT_VISIBLE_RESULTS);
        }
    }

    /// Get the currently selected result
    pub fn get_selected(&self) -> Option<&SearchResult> {
        self.filtered_results.get(self.selected_index)
    }

    /// Cycle through search modes
    pub fn cycle_search_mode(&mut self) {
        self.search_mode = match self.search_mode {
            SearchMode::Fuzzy => SearchMode::Exact,
            SearchMode::Exact => SearchMode::FilePath,
            SearchMode::FilePath => SearchMode::Fuzzy,
        };
    }

    fn adjust_scroll(&mut self, visible_height: usize) {
        // Keep selected item visible
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + visible_height {
            self.scroll_offset = self.selected_index - visible_height + 1;
        }
    }

    /// Update filtered results based on current query
    pub fn update_results(&mut self, graph: &CallGraph) {
        if self.query.is_empty() {
            // Show all functions (limited by max_results)
            self.filtered_results = graph
                .nodes
                .values()
                .take(self.max_results)
                .map(|func| SearchResult {
                    symbol_id: func.id.clone(),
                    name: func.name.clone(),
                    file_path: func.definition_location.file_path.clone(),
                    line: func.definition_location.line,
                    match_score: 1.0,
                })
                .collect();
        } else {
            self.filtered_results = self.search_symbols(graph);
        }

        // Reset selection if out of bounds
        if self.selected_index >= self.filtered_results.len() && !self.filtered_results.is_empty() {
            self.selected_index = 0;
        }
        self.scroll_offset = 0;
    }

    fn search_symbols(&self, graph: &CallGraph) -> Vec<SearchResult> {
        let query_lower = self.query.to_lowercase();

        let mut results: Vec<SearchResult> = graph
            .nodes
            .values()
            .filter_map(|func| {
                let score = match self.search_mode {
                    SearchMode::Fuzzy => self.fuzzy_match(&func.name, &query_lower),
                    SearchMode::Exact => {
                        if func.name.to_lowercase().contains(&query_lower) {
                            Some(1.0)
                        } else {
                            None
                        }
                    }
                    SearchMode::FilePath => {
                        if func
                            .definition_location
                            .file_path
                            .to_lowercase()
                            .contains(&query_lower)
                        {
                            Some(1.0)
                        } else {
                            None
                        }
                    }
                };

                score.map(|score| SearchResult {
                    symbol_id: func.id.clone(),
                    name: func.name.clone(),
                    file_path: func.definition_location.file_path.clone(),
                    line: func.definition_location.line,
                    match_score: score,
                })
            })
            .collect();

        // Sort by score (descending)
        results.sort_by(|a, b| b.match_score.partial_cmp(&a.match_score).unwrap());

        // Limit results
        results.truncate(self.max_results);

        results
    }

    fn fuzzy_match(&self, text: &str, pattern: &str) -> Option<f32> {
        // Simple fuzzy matching algorithm
        let text_lower = text.to_lowercase();
        let mut pattern_chars = pattern.chars().peekable();
        let mut text_chars = text_lower.chars().enumerate();
        let mut last_match_idx = 0;
        let mut matched_chars = 0;
        let mut first_match_idx = None;

        while let Some(pattern_char) = pattern_chars.next() {
            let mut found = false;
            while let Some((idx, text_char)) = text_chars.next() {
                if text_char == pattern_char {
                    matched_chars += 1;
                    last_match_idx = idx;
                    if first_match_idx.is_none() {
                        first_match_idx = Some(idx);
                    }
                    found = true;
                    break;
                }
            }
            if !found {
                return None;
            }
        }

        // Calculate score based on:
        // - Number of matched characters
        // - Proximity of matches (penalize gaps)
        // - Position of first match (prefer early matches)
        let match_ratio = matched_chars as f32 / pattern.len() as f32;
        let first_match_idx = first_match_idx.unwrap_or(0);
        let position_score = 1.0 / (first_match_idx as f32 + 1.0).log2().max(1.0);
        let gap_penalty = 1.0 / ((last_match_idx - first_match_idx) as f32 + 1.0);

        let score = match_ratio * position_score * gap_penalty;

        Some(score)
    }
}

impl Default for SearchBarState {
    fn default() -> Self {
        Self::new()
    }
}

/// Widget for rendering the search bar
pub struct SearchBar;

impl SearchBar {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, state: &SearchBarState) {
        use ratatui::layout::{Constraint, Direction, Layout};

        // Split area into input, results, and status
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Input bar
                Constraint::Min(5),    // Results list
                Constraint::Length(1), // Status line
            ])
            .split(area);

        // Render input bar
        self.render_input_bar(chunks[0], buf, state);

        // Render results list
        self.render_results_list(chunks[1], buf, state);

        // Render status line
        self.render_status_line(chunks[2], buf, state);
    }

    fn render_input_bar(&self, area: Rect, buf: &mut Buffer, state: &SearchBarState) {
        let mode_str = state.search_mode.as_str();

        // Show cursor in the input
        let mut display_query = state.query.clone();
        if state.cursor_position <= display_query.len() {
            display_query.insert(state.cursor_position, '█');
        }

        let title = format!("Symbol Search {}", mode_str);

        let input_widget = Paragraph::new(display_query)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .style(Style::default().fg(Color::Cyan)),
            )
            .style(Style::default().fg(Color::White));

        input_widget.render(area, buf);
    }

    fn render_results_list(&self, area: Rect, buf: &mut Buffer, state: &SearchBarState) {
        let visible_height = area.height.saturating_sub(2) as usize; // Account for borders
        let start_idx = state.scroll_offset;
        let end_idx = (start_idx + visible_height).min(state.filtered_results.len());

        let items: Vec<ListItem> = state.filtered_results[start_idx..end_idx]
            .iter()
            .enumerate()
            .map(|(i, result)| {
                let actual_idx = start_idx + i;
                let marker = if actual_idx == state.selected_index {
                    "→ "
                } else {
                    "  "
                };

                // Extract filename from path
                let filename = result
                    .file_path
                    .split('/')
                    .last()
                    .unwrap_or(&result.file_path);

                let content = format!("{}{:<40} {}:{}", marker, result.name, filename, result.line);

                let style = if actual_idx == state.selected_index {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };

                ListItem::new(content).style(style)
            })
            .collect();

        let results_widget =
            List::new(items).block(Block::default().borders(Borders::ALL).title("Results"));

        results_widget.render(area, buf);
    }

    fn render_status_line(&self, area: Rect, buf: &mut Buffer, state: &SearchBarState) {
        let status_text = format!(
            "Found {} matches | ↑↓:navigate Enter:select Tab:mode Esc:cancel",
            state.filtered_results.len()
        );

        let status_widget = Paragraph::new(status_text).style(Style::default().fg(Color::Gray));

        status_widget.render(area, buf);
    }
}
