use model::SymbolId;

use crate::graph_workspace::GraphWorkspace;

/// Manages the collection of graph workspaces (tabs).
///
/// Extracted from `App` to isolate workspace CRUD and navigation logic,
/// making it independently testable without any LSP or graph dependencies.
pub struct WorkspaceManager {
    pub workspaces: Vec<GraphWorkspace>,
    pub current_index: usize,
    pub next_id: usize,
    pub show_manager: bool,
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceManager {
    /// Create a manager pre-populated with one default workspace.
    pub fn new() -> Self {
        let default_workspace = GraphWorkspace::new(1, "Graph 1".to_string());
        Self {
            workspaces: vec![default_workspace],
            current_index: 0,
            next_id: 2,
            show_manager: false,
        }
    }

    /// Create a new workspace with the given name. Returns `(id, status_message)`.
    pub fn create(&mut self, name: String) -> (usize, String) {
        let id = self.next_id;
        self.next_id += 1;

        let workspace = GraphWorkspace::new(id, name);
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, format!("Created workspace #{}", id))
    }

    /// Create a new workspace rooted at `symbol`. Returns `(id, status_message)`.
    pub fn create_with_function(&mut self, name: String, symbol: SymbolId) -> (usize, String) {
        let id = self.next_id;
        self.next_id += 1;

        let workspace = GraphWorkspace::new_with_root(id, name, symbol);
        self.workspaces.push(workspace);
        self.current_index = self.workspaces.len() - 1;
        (id, format!("Created workspace #{} with function", id))
    }

    /// Close a workspace by index (cannot close the last one).
    /// Returns `Ok(status_message)` or `Err(status_message)`.
    pub fn close(&mut self, index: usize) -> Result<String, String> {
        if self.workspaces.len() <= 1 {
            return Err("Cannot close the last workspace".to_string());
        }
        if index >= self.workspaces.len() {
            return Err("Invalid workspace index".to_string());
        }

        self.workspaces.remove(index);

        if self.current_index >= self.workspaces.len() {
            self.current_index = self.workspaces.len() - 1;
        } else if self.current_index > index {
            self.current_index -= 1;
        }

        Ok("Workspace closed".to_string())
    }

    /// Switch to a specific workspace. Returns an optional status message.
    pub fn switch_to(&mut self, index: usize) -> Option<String> {
        if index >= self.workspaces.len() {
            return None;
        }
        self.current_index = index;
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.touch();
            Some(format!("Switched to workspace: {}", workspace.name))
        } else {
            None
        }
    }

    /// Switch to the next workspace (wrapping). Returns a status message.
    pub fn next_workspace(&mut self) -> Option<String> {
        if self.workspaces.is_empty() {
            return None;
        }
        self.current_index = (self.current_index + 1) % self.workspaces.len();
        if let Some(workspace) = self.workspaces.get_mut(self.current_index) {
            workspace.touch();
            Some(format!("Switched to workspace: {}", workspace.name))
        } else {
            None
        }
    }

    /// Switch to the previous workspace (wrapping). Returns a status message.
    pub fn previous(&mut self) -> Option<String> {
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
            Some(format!("Switched to workspace: {}", workspace.name))
        } else {
            None
        }
    }

    /// Rename a workspace. Returns a status message.
    pub fn rename(&mut self, index: usize, new_name: String) -> Option<String> {
        if let Some(workspace) = self.workspaces.get_mut(index) {
            workspace.name = new_name.clone();
            Some(format!("Renamed workspace to: {}", new_name))
        } else {
            None
        }
    }

    /// Get current workspace reference.
    pub fn current(&self) -> Option<&GraphWorkspace> {
        self.workspaces.get(self.current_index)
    }

    /// Get current workspace mutable reference.
    pub fn current_mut(&mut self) -> Option<&mut GraphWorkspace> {
        self.workspaces.get_mut(self.current_index)
    }

    /// Toggle workspace manager modal visibility.
    pub fn toggle_manager(&mut self) {
        self.show_manager = !self.show_manager;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_one_workspace() {
        let wm = WorkspaceManager::new();
        assert_eq!(wm.workspaces.len(), 1);
        assert_eq!(wm.current_index, 0);
        assert_eq!(wm.next_id, 2);
    }

    #[test]
    fn test_create() {
        let mut wm = WorkspaceManager::new();
        let (id, _msg) = wm.create("Test".to_string());
        assert_eq!(id, 2);
        assert_eq!(wm.workspaces.len(), 2);
        assert_eq!(wm.current_index, 1);
        assert_eq!(wm.workspaces[1].name, "Test");
    }

    #[test]
    fn test_close_last_fails() {
        let mut wm = WorkspaceManager::new();
        assert!(wm.close(0).is_err());
        assert_eq!(wm.workspaces.len(), 1);
    }

    #[test]
    fn test_close_second() {
        let mut wm = WorkspaceManager::new();
        wm.create("Second".to_string());
        assert_eq!(wm.workspaces.len(), 2);
        assert!(wm.close(1).is_ok());
        assert_eq!(wm.workspaces.len(), 1);
    }

    #[test]
    fn test_next_previous() {
        let mut wm = WorkspaceManager::new();
        wm.create("W2".to_string());

        // Currently at index 1 (just created)
        wm.next_workspace();
        assert_eq!(wm.current_index, 0); // Wraps

        wm.previous();
        assert_eq!(wm.current_index, 1); // Wraps back

        wm.switch_to(0);
        assert_eq!(wm.current_index, 0);
    }

    #[test]
    fn test_rename() {
        let mut wm = WorkspaceManager::new();
        let msg = wm.rename(0, "Renamed".to_string());
        assert!(msg.is_some());
        assert_eq!(wm.workspaces[0].name, "Renamed");
    }
}
