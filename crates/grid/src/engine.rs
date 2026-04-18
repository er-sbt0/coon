use crate::config::LayoutConfig;
use crate::dag::Dag;
use crate::error::LayoutError;
use crate::result::LayoutResult;
use crate::sugiyama;

/// Layout engine for computing DAG positions using the Sugiyama algorithm.
pub struct LayoutEngine {
    config: LayoutConfig,
    version: u64,
}

impl LayoutEngine {
    /// Create a new layout engine with default configuration
    pub fn new() -> Self {
        Self {
            config: LayoutConfig::new(),
            version: 0,
        }
    }

    /// Create a layout engine with specific configuration
    pub fn with_config(config: LayoutConfig) -> Result<Self, LayoutError> {
        config.validate()?;
        Ok(Self { config, version: 0 })
    }

    /// Compute layout for a DAG using the Sugiyama layered-graph algorithm.
    pub fn compute_dag<T>(&mut self, dag: &Dag<T>) -> Result<LayoutResult, LayoutError> {
        let result = sugiyama::compute_sugiyama(dag, &self.config)?;
        self.version += 1;
        Ok(result)
    }

    /// Update the configuration
    pub fn set_config(&mut self, config: LayoutConfig) -> Result<(), LayoutError> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    /// Get the current configuration
    pub fn config(&self) -> &LayoutConfig {
        &self.config
    }
}

impl Default for LayoutEngine {
    fn default() -> Self {
        Self::new()
    }
}
