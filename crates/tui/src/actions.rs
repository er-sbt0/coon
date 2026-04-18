/// Actions that can be performed in the TUI
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    ExpandOrCollapse,
    FindReferences,
    Refresh,
    Quit,
    Help,
    ToggleCallDirection,
    ResetView,

    // Graph navigation (hjkl)
    NavigateParent,
    NavigateChild,
    NavigateNextSibling,
    NavigatePrevSibling,

    // Workspace management
    NewWorkspace,
    CloseWorkspace,
    NextWorkspace,
    PreviousWorkspace,
    RenameWorkspace,
}
