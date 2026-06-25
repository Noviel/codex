use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ContextualUserFragment;

/// What is known about a section's previously model-visible state.
pub enum PreviousSectionState<'a, T> {
    /// No persisted snapshot or matching fragment exists in retained history.
    Absent,
    /// Retained history contains the section, but its typed snapshot is unavailable.
    Unknown,
    /// The exact persisted snapshot is available.
    Known(&'a T),
}

/// A typed portion of the state visible to the model.
///
/// Implementations own how their current state is rendered relative to an
/// earlier snapshot of the same section. `ID` is persisted in rollouts and
/// must remain stable. `Snapshot` should contain only the comparison data
/// needed to decide what the model must be told next, and must not serialize
/// to null because merge-patch nulls represent deletion. Sections migrated
/// from older context can recognize their previous fragments through
/// `matches_legacy_fragment`.
pub trait WorldStateSection: Send + Sync + 'static {
    /// Stable rollout identity for this section.
    const ID: &'static str;
    /// Persisted comparison state for this section.
    type Snapshot: DeserializeOwned + Serialize;

    /// Captures the current comparison state.
    fn snapshot(&self) -> Self::Snapshot;

    /// Recognizes model-visible context written before structured snapshots existed.
    fn matches_legacy_fragment(_role: &str, _text: &str) -> bool {
        false
    }

    /// Renders the model update required to advance from `previous` to the current state.
    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>>;
}

trait ErasedWorldStateSection: Send + Sync {
    fn snapshot(&self) -> Option<Value>;

    fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool;

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>>;
}

impl<S: WorldStateSection> ErasedWorldStateSection for S {
    fn snapshot(&self) -> Option<Value> {
        let mut snapshot = match serde_json::to_value(WorldStateSection::snapshot(self)) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                tracing::error!(
                    section_id = S::ID,
                    %err,
                    "failed to serialize world-state section snapshot"
                );
                return None;
            }
        };
        remove_null_object_fields(&mut snapshot);
        if snapshot.is_null() {
            tracing::error!(
                section_id = S::ID,
                "world-state section snapshot cannot be null"
            );
            return None;
        }
        Some(snapshot)
    }

    fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool {
        S::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let typed_snapshot;
        let previous = match previous {
            PreviousSectionState::Known(previous) => {
                match serde_json::from_value::<S::Snapshot>(previous.clone()) {
                    Ok(previous) => {
                        typed_snapshot = previous;
                        PreviousSectionState::Known(&typed_snapshot)
                    }
                    Err(err) => {
                        tracing::warn!(
                            section_id = S::ID,
                            %err,
                            "failed to restore world-state section snapshot"
                        );
                        PreviousSectionState::Unknown
                    }
                }
            }
            PreviousSectionState::Absent => PreviousSectionState::Absent,
            PreviousSectionState::Unknown => PreviousSectionState::Unknown,
        };
        WorldStateSection::render_diff(self, previous)
    }
}

/// Type-erased world-state section supplied by a subsystem or extension.
#[derive(Clone)]
pub struct WorldStateSectionContribution {
    id: &'static str,
    section: Arc<dyn ErasedWorldStateSection>,
}

impl fmt::Debug for WorldStateSectionContribution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorldStateSectionContribution")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl WorldStateSectionContribution {
    /// Erases one typed section while retaining its stable identity.
    pub fn new<S: WorldStateSection>(section: S) -> Self {
        Self {
            id: S::ID,
            section: Arc::new(section),
        }
    }

    /// Returns the stable rollout identity of this section.
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// Returns the serialized comparison snapshot for persistence and diffing.
    pub fn snapshot(&self) -> Option<Value> {
        self.section.snapshot()
    }

    /// Returns whether a retained legacy fragment represents this section.
    pub fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool {
        self.section.matches_legacy_fragment(role, text)
    }

    /// Renders the update from a previously model-visible serialized snapshot.
    pub fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Value>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        self.section.render_diff(previous)
    }
}

fn remove_null_object_fields(value: &mut Value) {
    // RFC 7386 reserves object-valued nulls for deletion, but arrays are replaced whole.
    match value {
        Value::Object(values) => {
            values.retain(|_, value| !value.is_null());
            values.values_mut().for_each(remove_null_object_fields);
        }
        Value::Array(_) => {}
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}
