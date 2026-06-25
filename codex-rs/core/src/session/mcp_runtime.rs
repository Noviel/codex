use std::sync::Arc;

use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::McpRuntimeSnapshot;

/// Live base and selected-plugin MCP runtimes retained between model steps.
///
/// A selected runtime is reusable only for the same ordered selected roots and the same
/// process-local environment handles. The executor-plugin extension separately retains stable
/// plugin metadata across connection-handle replacement.
///
/// Within a live session, the selected runtime is invalidated in exactly two ways:
///
/// 1. [`Self::replace_base_and_invalidate_selected`] installs a new base MCP runtime.
/// 2. [`Self::replace_selected_runtime`] stores a newly projected runtime. This happens when the
///    active bindings or effective runtime configuration change.
///
/// In-flight [`McpRuntimeSnapshot`] values retain their manager until their model step finishes.
#[derive(Default)]
pub(crate) struct SelectedMcpRuntimeCache {
    base_runtime: Option<Arc<McpRuntimeSnapshot>>,
    runtime: Option<CachedSelectedRuntime>,
}

struct CachedSelectedRuntime {
    bindings: Vec<(usize, ResolvedSelectedCapabilityRoot)>,
    runtime: Arc<McpRuntimeSnapshot>,
}

impl SelectedMcpRuntimeCache {
    pub(crate) fn replace_base_and_invalidate_selected(
        &mut self,
        runtime: Arc<McpRuntimeSnapshot>,
    ) {
        self.base_runtime = Some(runtime);
        self.runtime = None;
    }

    pub(crate) fn base_runtime(&self) -> Arc<McpRuntimeSnapshot> {
        self.base_runtime
            .as_ref()
            .map(Arc::clone)
            .expect("base MCP runtime must be installed before capturing a step")
    }

    pub(crate) fn runtime_for_bindings(
        &self,
        bindings: &[(usize, ResolvedSelectedCapabilityRoot)],
    ) -> Option<Arc<McpRuntimeSnapshot>> {
        self.runtime
            .as_ref()
            .filter(|cached| same_bindings(&cached.bindings, bindings))
            .map(|cached| Arc::clone(&cached.runtime))
    }

    pub(crate) fn replace_selected_runtime(
        &mut self,
        bindings: Vec<(usize, ResolvedSelectedCapabilityRoot)>,
        runtime: Arc<McpRuntimeSnapshot>,
    ) {
        self.runtime = Some(CachedSelectedRuntime { bindings, runtime });
    }
}

fn same_bindings(
    left: &[(usize, ResolvedSelectedCapabilityRoot)],
    right: &[(usize, ResolvedSelectedCapabilityRoot)],
) -> bool {
    // Order is part of the key because later selected roots can be renamed when MCP server names
    // collide. Arc identity is only a live-connection key: stable plugin metadata is cached above
    // by selected root and survives connection-handle replacement.
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|((left_order, left), (right_order, right))| {
                left_order == right_order && same_binding(left, right)
            })
}

fn same_binding(
    left: &ResolvedSelectedCapabilityRoot,
    right: &ResolvedSelectedCapabilityRoot,
) -> bool {
    left.selected_root() == right.selected_root()
        && Arc::ptr_eq(left.environment(), right.environment())
}
