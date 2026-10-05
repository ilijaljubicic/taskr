use std::fmt;

use crate::{orchestration::OrchestrationState, SnapshotStore};

/// A failed domain operation or persistence write. Neither publishes new state.
#[derive(Debug, PartialEq, Eq)]
pub enum MutationError<E> {
    Rejected(String),
    Persistence(E),
}

impl<E: fmt::Display> fmt::Display for MutationError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(message) => message.fmt(formatter),
            Self::Persistence(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for MutationError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            Self::Rejected(_) => None,
        }
    }
}

/// Reusable single-writer orchestration service with host-provided persistence.
///
/// The host owns synchronization and supplies timestamps. Mutations run against
/// a candidate snapshot, save it, and only then publish it for inspection. The
/// closure may compose several domain operations into one atomic state change.
/// It must not perform external effects: start/stop workers after recording the
/// corresponding durable intent, through the host's execution adapter.
pub struct Orchestrator<S: SnapshotStore> {
    store: S,
    state: OrchestrationState,
}

impl<S: SnapshotStore> Orchestrator<S> {
    pub fn open(store: S) -> Result<Self, S::Error> {
        let state = store.load()?.unwrap_or_default();
        Ok(Self { store, state })
    }

    /// Borrow the last committed state; writes go through `mutate`.
    pub fn state(&self) -> &OrchestrationState {
        &self.state
    }

    pub fn snapshot(&self) -> OrchestrationState {
        self.state.clone()
    }

    pub fn mutate<T>(
        &mut self,
        now_ms: u64,
        apply: impl FnOnce(&mut OrchestrationState, u64) -> Result<T, String>,
    ) -> Result<T, MutationError<S::Error>> {
        let mut candidate = self.state.clone();
        let result = apply(&mut candidate, now_ms).map_err(MutationError::Rejected)?;
        self.store
            .save(&candidate, now_ms)
            .map_err(MutationError::Persistence)?;
        self.state = candidate;
        Ok(result)
    }
}
