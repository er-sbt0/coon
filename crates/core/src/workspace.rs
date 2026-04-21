use std::time::Instant;

use model::{CallDirection, SymbolId};

use crate::events::AppEvent;

/// Domain state of a single graph workspace (tab).
///
/// Contains only data — no rendering or view state. The TUI crate stores
/// `GraphViewState` separately, keyed by workspace id.
pub struct CoreWorkspace {
    pub id: usize,
    pub name: String,
    pub root_symbol: Option<SymbolId>,
    pub direction: CallDirection,
    pub created_at: Instant,
    pub last_accessed: Instant,
}

impl CoreWorkspace {
    pub fn new(id: usize, name: String) -> Self {
        let now = Instant::now();
        Self {
            id,
            name,
            root_symbol: None,
            direction: CallDirection::Outgoing,
            created_at: now,
            last_accessed: now,
        }
    }

    pub fn new_with_root(id: usize, name: String, symbol: SymbolId) -> Self {
        let mut ws = Self::new(id, name);
        ws.root_symbol = Some(symbol);
        ws
    }

    pub fn set_root(&mut self, symbol: SymbolId) {
        self.root_symbol = Some(symbol);
        self.touch();
    }

    pub fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }
}

/// Manages the collection of core workspaces (tabs).
///
/// Returns `AppEvent` variants instead of `StatusMessage` so this struct is
/// frontend-agnostic.
pub struct CoreWorkspaceManager {
    pub workspaces: Vec<CoreWorkspace>,
    pub current_index: usize,
    pub next_id: usize,
}

impl Default for CoreWorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CoreWorkspaceManager {
    /// Create a manager pre-populated with one default workspace.
    pub fn new() -> Self {
        let default_workspace = CoreWorkspace::new(1, "Graph 1".to_string());
        Self {
            workspaces: vec![default_workspace],
            current_index: 0,
            next_id: 2,
        }
    }

    /// Create a new workspace with the given name. Returns `(id, event)`.
    pub fn create(&mut self, name: String) -> (usize, AppEvent) {
        let id = self.next_id;
        self.next_id += 1;
        let workspace = CoreWorkspace::new(id, name.clone());
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, AppEvent::WorkspaceCreated { id, name })
    }

    /// Create a new workspace rooted at `symbol`. Returns `(id, event)`.
    pub fn create_with_function(&mut self, name: String, symbol: SymbolId) -> (usize, AppEvent) {
        let id = self.next_id;
        self.next_id += 1;
        let workspace = CoreWorkspace::new_with_root(id, name.clone(), symbol);
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, AppEvent::WorkspaceCreated { id, name })
    }

    /// Close a workspace by index. Returns `Ok(event)` or `Err(event)`.
    pub fn close(&mut self, index: usize) -> Result<AppEvent, AppEvent> {
        if self.workspaces.len() <= 1 {
            return Err(AppEvent::CannotCloseLastWorkspace);
        }
        if index >= self.workspaces.len() {
            return Err(AppEvent::InvalidWorkspaceIndex);
        }

        self.workspaces.remove(index);

        if self.current_index >= self.workspaces.len() {
            self.current_index = self.workspaces.len() - 1;
        } else if self.current_index > index {
            self.current_index -= 1;
        }

        Ok(AppEvent::WorkspaceClosed)
    }

    /// Switch to a specific workspace by index. Returns an optional event.
    pub fn switch_to(&mut self, index: usize) -> Option<AppEvent> {
        if index >= self.workspaces.len() {
            return None;
        }
        self.current_index = index;
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
    }

    /// Switch to the next workspace (wrapping). Returns an optional event.
    pub fn next_workspace(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        self.current_index = (self.current_index + 1) % self.workspaces.len();
        if let Some(workspace) = self.workspaces.get_mut(self.current_index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
    }

    /// Switch to the previous workspace (wrapping). Returns an optional event.
    pub fn previous(&mut self) -> Option<AppEvent> {
        if self.workspaces.is_empty() {
            return None;
        }
        if self.current_index == 0 {
            self.current_index = self.workspaces.len() - 1;
        } else {
            self.current_index -= 1;
        }
        if let Some(workspace) = self.workspaces.get_mut(self.current_index) {
            workspace.touch();
            Some(AppEvent::WorkspaceSwitched {
                id: workspace.id,
                name: workspace.name.clone(),
            })
        } else {
            None
        }
    }

    /// Rename a workspace. Returns an optional event.
    pub fn rename(&mut self, index: usize, new_name: String) -> Option<AppEvent> {
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.name = new_name.clone();
            Some(AppEvent::WorkspaceRenamed { name: new_name })
        } else {
            None
        }
    }

    /// Get reference to the current workspace.
    pub fn current(&self) -> Option<&CoreWorkspace> {
        self.workspaces.get(self.current_index)
    }

    /// Get mutable reference to the current workspace.
    pub fn current_mut(&mut self) -> Option<&mut CoreWorkspace> {
        self.workspaces.get_mut(self.current_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_one_workspace() {
        let wm = CoreWorkspaceManager::new();
        assert_eq!(wm.workspaces.len(), 1);
        assert_eq!(wm.current_index, 0);
        assert_eq!(wm.next_id, 2);
    }

    #[test]
    fn test_create() {
        let mut wm = CoreWorkspaceManager::new();
        let (id, _) = wm.create("Test".to_string());
        assert_eq!(id, 2);
        assert_eq!(wm.workspaces.len(), 2);
        assert_eq!(wm.current_index, 1);
    }

    #[test]
    fn test_close_last_fails() {
        let mut wm = CoreWorkspaceManager::new();
        assert!(wm.close(0).is_err());
    }

    #[test]
    fn test_close_and_navigate() {
        let mut wm = CoreWorkspaceManager::new();
        wm.create("Second".to_string());
        assert!(wm.close(1).is_ok());
        assert_eq!(wm.workspaces.len(), 1);
        assert_eq!(wm.current_index, 0);
    }
}
