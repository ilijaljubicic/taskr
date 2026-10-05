//! Storage contract implemented by the embedding host.

use crate::orchestration::OrchestrationState;

/// Persist one complete orchestration snapshot.
///
/// Implementations own serialization, schema versions, migrations, connections
/// and durability. `save` must publish atomically: returning an error means the
/// previous durable snapshot is unchanged. A host must resolve an uncertain
/// commit outcome before returning, or stop mutations and recover from storage.
///
/// One [`crate::Orchestrator`] is the exclusive writer for its stored snapshot.
/// Hosts serialize access to that writer; this interface is not a cross-process
/// compare-and-swap protocol. A multiwriter host needs its own store transaction
/// or locking mechanism. Database calls and remote persistence stay outside core.
///
/// Calls are synchronous, fitting native SQLite and Durable Object SQL storage.
/// No threading or runtime bounds are imposed on the store or its error type.
/// Async-only backends need a separate host integration; do not block a Wasm
/// event loop to adapt them to this interface.
pub trait SnapshotStore {
    type Error;

    /// `None` denotes a new store; a load error must never become an empty state.
    fn load(&self) -> Result<Option<OrchestrationState>, Self::Error>;

    /// `now_ms` is supplied by the host, with no clock access inside core.
    fn save(&self, state: &OrchestrationState, now_ms: u64) -> Result<(), Self::Error>;
}

impl<S: SnapshotStore + ?Sized> SnapshotStore for Box<S> {
    type Error = S::Error;

    fn load(&self) -> Result<Option<OrchestrationState>, Self::Error> {
        (**self).load()
    }

    fn save(&self, state: &OrchestrationState, now_ms: u64) -> Result<(), Self::Error> {
        (**self).save(state, now_ms)
    }
}
