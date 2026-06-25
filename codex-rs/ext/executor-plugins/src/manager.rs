use std::sync::Mutex;

use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_protocol::capabilities::SelectedCapabilityRoot;

use crate::ExecutorPluginRuntime;

/// Owns stable executor-plugin metadata discovered for a session.
///
/// Selected environment IDs are globally unique and stable across reconnection, and selected
/// environment contents are stable for the session lifetime. Successful plugin projections and
/// stable non-plugin roots are therefore cached by the complete selected root. Availability only
/// controls whether a root participates in the current step; replacing its live connection handle
/// does not invalidate metadata. Transient projection errors are not cached and retry later.
#[derive(Default)]
pub struct ExecutorPluginManager {
    projections: Mutex<Vec<CachedExecutorPluginProjection>>,
}

#[derive(Clone)]
struct CachedExecutorPluginProjection {
    selected_root: SelectedCapabilityRoot,
    plugin: Option<ExecutorPluginRuntime>,
}

impl ExecutorPluginManager {
    /// Captures the ordered plugin metadata used to construct one model step's MCP runtime.
    pub async fn capture_step(
        &self,
        bindings: &[(usize, ResolvedSelectedCapabilityRoot)],
    ) -> Vec<(usize, ExecutorPluginRuntime)> {
        let mut plugins = Vec::new();
        for (selection_order, root) in bindings {
            let selected_root = root.selected_root();
            if let Some(plugin) = self.cached_projection(selected_root) {
                if let Some(plugin) = plugin {
                    plugins.push((*selection_order, plugin));
                }
                continue;
            }

            match ExecutorPluginRuntime::project(root).await {
                Ok(plugin) => {
                    let plugin = self.cache_projection(selected_root.clone(), plugin);
                    if let Some(plugin) = plugin {
                        plugins.push((*selection_order, plugin));
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        selected_root = selected_root.id,
                        error = %err,
                        "failed to project selected executor plugin runtime"
                    );
                }
            }
        }
        plugins
    }

    fn cached_projection(
        &self,
        selected_root: &SelectedCapabilityRoot,
    ) -> Option<Option<ExecutorPluginRuntime>> {
        self.projections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|cached| &cached.selected_root == selected_root)
            .map(|cached| cached.plugin.clone())
    }

    fn cache_projection(
        &self,
        selected_root: SelectedCapabilityRoot,
        plugin: Option<ExecutorPluginRuntime>,
    ) -> Option<ExecutorPluginRuntime> {
        let mut projections = self
            .projections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = projections
            .iter()
            .find(|cached| cached.selected_root == selected_root)
        {
            return cached.plugin.clone();
        }
        projections.push(CachedExecutorPluginProjection {
            selected_root,
            plugin: plugin.clone(),
        });
        plugin
    }
}
