mod agents_md;
mod environment;

use crate::context::ContextualUserFragment;
pub(crate) use codex_context_fragments::PreviousSectionState;
pub(crate) use codex_context_fragments::WorldStateSection;
use codex_context_fragments::WorldStateSectionContribution;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

pub(crate) use agents_md::AgentsMdState;
pub(crate) use environment::EnvironmentsState;

/// Live model-visible state, keyed by the same stable section IDs used in rollouts.
#[derive(Default)]
pub(crate) struct WorldState {
    sections: IndexMap<&'static str, WorldStateSectionContribution>,
}

/// Compact comparison state for each model-visible world-state section.
#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct WorldStateSnapshot {
    sections: BTreeMap<String, Value>,
}

impl WorldStateSnapshot {
    pub(crate) fn into_value(self) -> Value {
        Value::Object(self.sections.into_iter().collect())
    }

    /// Returns the RFC 7386 merge patch that advances `previous` to `self`.
    pub(crate) fn merge_patch_from(&self, previous: &Self) -> Option<Value> {
        let previous = Value::Object(previous.sections.clone().into_iter().collect());
        let current = Value::Object(self.sections.clone().into_iter().collect());
        create_merge_patch(&previous, &current)
    }

    pub(crate) fn apply_merge_patch(&mut self, patch: &Value) -> serde_json::Result<()> {
        let mut current = self.clone().into_value();
        apply_merge_patch_value(&mut current, patch);
        *self = serde_json::from_value(current)?;
        Ok(())
    }
}

impl fmt::Debug for WorldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorldState")
            .field("section_count", &self.sections.len())
            .finish()
    }
}

impl WorldState {
    pub(crate) fn add_section<S: WorldStateSection>(&mut self, section: S) {
        self.add_contribution(WorldStateSectionContribution::new(section));
    }

    pub(crate) fn add_contribution(&mut self, contribution: WorldStateSectionContribution) {
        let id = contribution.id();
        assert!(
            !self.sections.contains_key(id),
            "duplicate world-state section ID: {id}"
        );
        self.sections.insert(id, contribution);
    }

    pub(crate) fn snapshot(&self) -> WorldStateSnapshot {
        WorldStateSnapshot {
            sections: self
                .sections
                .iter()
                .filter_map(|(id, section)| {
                    section
                        .snapshot()
                        .map(|snapshot| ((*id).to_string(), snapshot))
                })
                .collect(),
        }
    }

    /// Renders every section as new, without any known previous state.
    pub(crate) fn render_full(&self) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_with(|_, _| PreviousSectionState::Absent)
    }

    /// Renders each section against the exact persisted snapshot when available.
    pub(crate) fn render_diff(
        &self,
        previous: &WorldStateSnapshot,
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_with(|id, _| match previous.sections.get(id) {
            Some(previous) => PreviousSectionState::Known(previous),
            None => PreviousSectionState::Absent,
        })
    }

    /// Falls back to retained model history when no exact persisted snapshot is available.
    pub(crate) fn render_history_diff(
        &self,
        previous: Option<&WorldStateSnapshot>,
        items: &[ResponseItem],
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        self.render_with(|id, section| {
            if let Some(previous) = previous.and_then(|previous| previous.sections.get(id)) {
                PreviousSectionState::Known(previous)
            } else if has_legacy_fragment(items, section) {
                PreviousSectionState::Unknown
            } else {
                PreviousSectionState::Absent
            }
        })
    }

    fn render_with<'a>(
        &self,
        mut previous: impl FnMut(
            &str,
            &WorldStateSectionContribution,
        ) -> PreviousSectionState<'a, Value>,
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        self.sections
            .iter()
            .filter_map(|(id, section)| section.render_diff(previous(id, section)))
            .collect()
    }
}

fn has_legacy_fragment(items: &[ResponseItem], section: &WorldStateSectionContribution) -> bool {
    items.iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message { role, content, .. }
                if content.iter().any(|content| {
                    matches!(
                        content,
                        ContentItem::InputText { text }
                            if section.matches_legacy_fragment(role, text)
                    )
                })
        )
    })
}

fn create_merge_patch(previous: &Value, current: &Value) -> Option<Value> {
    if previous == current {
        return None;
    }

    let Value::Object(current) = current else {
        return Some(current.clone());
    };
    let previous = previous.as_object();
    let mut patch = Map::new();

    if let Some(previous) = previous {
        for key in previous.keys() {
            if !current.contains_key(key) {
                patch.insert(key.clone(), Value::Null);
            }
        }
    }

    for (key, current_value) in current {
        let Some(previous_value) = previous.and_then(|previous| previous.get(key)) else {
            patch.insert(key.clone(), current_value.clone());
            continue;
        };
        if let Some(value_patch) = create_merge_patch(previous_value, current_value) {
            patch.insert(key.clone(), value_patch);
        }
    }

    Some(Value::Object(patch))
}

fn apply_merge_patch_value(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        target.clone_from(patch);
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                apply_merge_patch_value(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

#[cfg(test)]
#[path = "world_state_tests.rs"]
mod tests;
