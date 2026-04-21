/// Actions that can be performed in the TUI.
///
/// Every key press is mapped to an `Action` (or ignored) before reaching the
/// application logic — including search-bar input and workspace switching.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // -- viewport / panning --
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,

    // -- tree interaction --
    ExpandOrCollapse,
    FindReferences,
    Refresh,
    Quit,
    Help,
    ToggleCallDirection,
    ResetView,

    // -- graph navigation (hjkl) --
    NavigateParent,
    NavigateChild,
    NavigateNextSibling,
    NavigatePrevSibling,
    /// Cycle the active parent of the currently selected node (for DAG nodes
    /// that have more than one parent).  Does not move the selection.
    CycleParent,

    // -- workspace management --
    NewWorkspace,
    CloseWorkspace,
    NextWorkspace,
    PreviousWorkspace,
    RenameWorkspace,
    /// Switch to workspace at the given 0-based index.
    SwitchWorkspace(usize),

    // -- search bar --
    /// Open or close the search bar.
    ToggleSearch,
    /// Confirm the currently highlighted search result.
    SearchConfirm,
    /// Move the result highlight up.
    SearchPrevResult,
    /// Move the result highlight down.
    SearchNextResult,
    /// Cycle through search modes (Fuzzy / Exact / FilePath).
    SearchCycleMode,
    /// Backspace in the search query.
    SearchBackspace,
    /// Delete-forward in the search query.
    SearchDeleteForward,
    /// Move the query cursor left.
    SearchCursorLeft,
    /// Move the query cursor right.
    SearchCursorRight,
    /// Move the query cursor to the start.
    SearchCursorHome,
    /// Move the query cursor to the end.
    SearchCursorEnd,
    /// Insert a character into the search query.
    SearchInput(char),
}
