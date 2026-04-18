use crate::error::LayoutError;

/// Orientation of the tree layout
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TreeOrientation {
    /// Root on left, children to the right
    LeftRight,
    /// Root on top, children below
    TopDown,
    /// Root on right, children to the left
    RightLeft,
    /// Root on bottom, children above
    BottomUp,
}

/// Configuration for tree layout computation
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutConfig {
    /// Orientation of the tree
    pub orientation: TreeOrientation,

    /// Minimum horizontal distance between sibling nodes
    pub node_separation: f32,

    /// Vertical distance between tree levels
    pub level_separation: f32,

    /// Minimum horizontal distance between separate subtrees
    pub subtree_separation: f32,

    /// Width of each node
    pub node_width: f32,

    /// Height of each node
    pub node_height: f32,

    /// Number of barycenter passes in Sugiyama crossing minimization (None = 2)
    pub crossing_minimization_passes: Option<usize>,
}

impl LayoutConfig {
    /// Create a new configuration with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tree orientation
    pub fn with_orientation(mut self, orientation: TreeOrientation) -> Self {
        self.orientation = orientation;
        self
    }

    /// Set the node dimensions
    pub fn with_node_size(mut self, width: f32, height: f32) -> Self {
        self.node_width = width;
        self.node_height = height;
        self
    }

    /// Set all spacing parameters
    pub fn with_spacing(
        mut self,
        node_separation: f32,
        level_separation: f32,
        subtree_separation: f32,
    ) -> Self {
        self.node_separation = node_separation;
        self.level_separation = level_separation;
        self.subtree_separation = subtree_separation;
        self
    }

    /// Set node separation
    pub fn with_node_separation(mut self, separation: f32) -> Self {
        self.node_separation = separation;
        self
    }

    /// Set level separation
    pub fn with_level_separation(mut self, separation: f32) -> Self {
        self.level_separation = separation;
        self
    }

    /// Set subtree separation
    pub fn with_subtree_separation(mut self, separation: f32) -> Self {
        self.subtree_separation = separation;
        self
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.node_width <= 0.0 {
            return Err(LayoutError::ConfigError(
                "Node width must be positive".into(),
            ));
        }
        if self.node_height <= 0.0 {
            return Err(LayoutError::ConfigError(
                "Node height must be positive".into(),
            ));
        }
        if self.node_separation < 0.0 {
            return Err(LayoutError::ConfigError(
                "Node separation cannot be negative".into(),
            ));
        }
        if self.level_separation < 0.0 {
            return Err(LayoutError::ConfigError(
                "Level separation cannot be negative".into(),
            ));
        }
        if self.subtree_separation < 0.0 {
            return Err(LayoutError::ConfigError(
                "Subtree separation cannot be negative".into(),
            ));
        }
        Ok(())
    }

    /// Get the node width
    pub fn node_width(&self) -> f32 {
        self.node_width
    }

    /// Get the node height
    pub fn node_height(&self) -> f32 {
        self.node_height
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            orientation: TreeOrientation::LeftRight,
            node_separation: 2.0,
            level_separation: 4.0,
            subtree_separation: 4.0,
            node_width: 20.0,
            node_height: 3.0,
            crossing_minimization_passes: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = LayoutConfig::new();
        assert_eq!(config.orientation, TreeOrientation::LeftRight);
        assert_eq!(config.node_width, 20.0);
        assert_eq!(config.node_height, 3.0);
    }

    #[test]
    fn test_builder_pattern() {
        let config = LayoutConfig::new()
            .with_orientation(TreeOrientation::TopDown)
            .with_node_size(40.0, 20.0)
            .with_spacing(10.0, 30.0, 15.0);

        assert_eq!(config.orientation, TreeOrientation::TopDown);
        assert_eq!(config.node_width, 40.0);
        assert_eq!(config.node_height, 20.0);
        assert_eq!(config.node_separation, 10.0);
        assert_eq!(config.level_separation, 30.0);
        assert_eq!(config.subtree_separation, 15.0);
    }

    #[test]
    fn test_validation_positive_dimensions() {
        let mut config = LayoutConfig::new();
        config.node_width = 0.0;
        assert!(matches!(
            config.validate(),
            Err(LayoutError::ConfigError(_))
        ));

        config.node_width = 10.0;
        config.node_height = -1.0;
        assert!(matches!(
            config.validate(),
            Err(LayoutError::ConfigError(_))
        ));
    }

    #[test]
    fn test_validation_non_negative_spacing() {
        let mut config = LayoutConfig::new();
        config.node_separation = -1.0;
        assert!(matches!(
            config.validate(),
            Err(LayoutError::ConfigError(_))
        ));

        config.node_separation = 0.0; // Zero is OK
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_all_orientations() {
        let orientations = [
            TreeOrientation::LeftRight,
            TreeOrientation::TopDown,
            TreeOrientation::RightLeft,
            TreeOrientation::BottomUp,
        ];

        for orientation in orientations {
            let config = LayoutConfig::new().with_orientation(orientation);
            assert_eq!(config.orientation, orientation);
        }
    }
}
