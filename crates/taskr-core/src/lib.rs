//! Storage-agnostic task orchestration for embeddable hosts.
//!
//! This crate owns projects, plans, tasks, gates and execution records. Hosts
//! supply a [`SnapshotStore`] and serialize mutations through [`Orchestrator`].
//! Persistence format, migrations, execution adapters, scheduling, clocks, MCP
//! transports and resource cleanup belong to the host. There are no SQLite,
//! filesystem, Tokio, Herdr, Reqvire or ScopeTrail dependencies here.
//!
//! Native hosts and single-threaded Wasm hosts use the same API; stores need not
//! implement `Send` or `Sync`. JavaScript/Wasm hosts enable the `wasm-js` feature
//! for UUID randomness. Durable Object storage and alarms belong to that host.

pub mod coordination;
pub mod orchestration;
mod service;
pub mod store;

pub use service::{MutationError, Orchestrator};
pub use store::SnapshotStore;
