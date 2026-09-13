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
        self.compute_dag_grouped(dag, &[])
    }

    /// Compute layout for a DAG whose nodes carry group membership.
    ///
    /// `groups[i]` is the group of node `i`; `None` — and any index past the end
    /// of the slice — means the node belongs to no group. Group ids are opaque:
    /// they need not be dense, ordered or meaningful, only equal for nodes that
    /// belong together.
    ///
    /// Within a layer the members of one group are kept contiguous, by ordering
    /// on the group's barycenter before the node's own. An ungrouped node is
    /// ordered as a group of one, so it can still come to rest between two
    /// groups. Edges that span several layers are routed through dummy nodes,
    /// and a dummy inherits the group only when both endpoints of its edge share
    /// one — a wire routed through a group does not break the group's run, a
    /// wire merely passing by would.
    ///
    /// Passing an empty slice (or all `None`) reproduces
    /// [`compute_dag`](Self::compute_dag) exactly.
    ///
    /// Contiguity constrains the barycenter heuristic rather than assisting it:
    /// where a group's members genuinely belong on opposite sides of a layer,
    /// holding them together can cost edge crossings.
    pub fn compute_dag_grouped<T>(
        &mut self,
        dag: &Dag<T>,
        groups: &[Option<u32>],
    ) -> Result<LayoutResult, LayoutError> {
        let result = sugiyama::compute_sugiyama_grouped(dag, &self.config, groups)?;
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
