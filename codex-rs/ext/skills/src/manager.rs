use std::sync::Arc;

use codex_context_fragments::WorldStateSectionContribution;
use codex_core_skills::BoundSkillCatalog;
use codex_core_skills::HostSkillsSnapshot;
use codex_core_skills::SkillsSnapshot;
use codex_core_skills::runtime::SkillSource;
use codex_core_skills::runtime::SkillSources;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_protocol::protocol::Product;

use crate::executor_runtime::ExecutorSkillCatalogCache;
use crate::executor_runtime::ExecutorSkillSource;
use crate::world_state::skills_world_state_section;

/// Immutable executable and model-visible skill state captured for one sampling step.
pub struct SkillsStepState {
    skills: Arc<SkillsSnapshot>,
    world_state: WorldStateSectionContribution,
}

impl SkillsStepState {
    /// Returns the catalog and read routes that skill consumers must use for this step.
    pub fn skills(&self) -> Arc<SkillsSnapshot> {
        Arc::clone(&self.skills)
    }

    /// Returns the model-visible section derived from the same captured catalog.
    pub fn world_state(&self) -> WorldStateSectionContribution {
        self.world_state.clone()
    }
}

/// Owns the session-lifetime caches used to build immutable skill views for model steps.
///
/// Selected environment contents are stable for the session lifetime. Environment availability
/// therefore controls whether a root participates in one step, while the catalog cache remains
/// keyed by the globally unique and reconnect-stable environment identity.
#[derive(Default)]
pub struct SkillsManager {
    executor_catalog_cache: ExecutorSkillCatalogCache,
}

impl SkillsManager {
    /// Builds the complete authority-aware skill view used by one model sampling step.
    pub async fn capture_step(
        &self,
        host: HostSkillsSnapshot,
        executor_roots: &[ResolvedSelectedCapabilityRoot],
        extra_sources: Option<&SkillSources>,
        restriction_product: Option<Product>,
        context_window: Option<i64>,
        include_instructions: bool,
    ) -> SkillsStepState {
        let mut bound_catalogs = Vec::with_capacity(executor_roots.len());
        for root in executor_roots {
            let cached = self
                .executor_catalog_cache
                .catalog_for_stable_root(root, restriction_product)
                .await;
            let source: Arc<dyn SkillSource> = Arc::new(ExecutorSkillSource::new(
                root.clone(),
                restriction_product,
                cached.identity(),
            ));
            bound_catalogs.push(BoundSkillCatalog::new(cached.catalog(), source));
        }
        let skills = Arc::new(
            SkillsSnapshot::from_sources(host, &bound_catalogs, extra_sources, context_window)
                .await,
        );
        let world_state = skills_world_state_section(skills.as_ref(), include_instructions);
        SkillsStepState {
            skills,
            world_state,
        }
    }
}
