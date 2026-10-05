# taskr-core

Storage-agnostic orchestration library for native TASKR and WebAssembly hosts,
including Cloudflare Workers with Durable Objects.

The core contains the project → plan → task model, edges, dependency/status
rules, gates/outcomes/blockers, execution records and `Orchestrator<S>`, a
transactional state service. Its normal dependencies are Serde and UUID.
It contains no SQLite, filesystem access, Tokio, MCP transport, Reqvire model,
ScopeTrail runtime or execution adapter implementation.

| Boundary | Core | Embedding host |
| --- | --- | --- |
| State | Domain records, state operations and validation | Tool routing, scheduling and external observations |
| Persistence | `SnapshotStore` interface and persist-before-publish service | Concrete store, atomic writes, serialization, schema/migrations and connection settings |
| Execution | Records of intent, observations and outcomes | Adapter implementation, launch configuration, worker lifecycle, recovery and cleanup |
| Runtime | Caller-supplied timestamps and exclusive mutable service access | Clocks, synchronization, actors, transport, authentication and shutdown |

TASKR provides `SqliteOrchestrationStore` in
[`taskr-controller/src/store.rs`](../taskr-controller/src/store.rs), and holds
`Orchestrator<SqliteOrchestrationStore>` behind its own mutex. Herdr interaction
uses the separate `taskr-herdr` adapter and host lifecycle code. Native environment
discovery/deployment uses `taskr-environment`; it stays outside the core too.

## Embed the library

Another host can depend on this crate without linking the TASKR controller:

```toml
[dependencies]
taskr-core = { path = "/path/to/taskr/crates/taskr-core" }
```

Implement `SnapshotStore` in the host. The synchronous interface has an associated
error type, `load() -> Result<Option<OrchestrationState>, Error>` and
`save(&OrchestrationState, now_ms) -> Result<(), Error>`. A boxed store can be
selected at runtime. Only an absent snapshot creates a new state; a load error
is returned unchanged.

Construct `Orchestrator::open(store)`, then perform mutations through:

```rust,ignore
let mut orchestration = taskr_core::Orchestrator::open(host_store)?;
let project = orchestration.mutate(host_now_ms, |state, now| {
    state.create_project(taskr_core::orchestration::CreateProject {
        title: "Implement capability".into(),
        description: "Tasks may exist without a Reqvire system model".into(),
        ..Default::default()
    }, now)
})?;
```

`state()` borrows committed state; `snapshot()` returns a detached clone.
`mutate` runs domain operations on a candidate snapshot, saves it once, and then
publishes it. Domain rejection or save failure leaves the committed in-memory
state unchanged. Multiple domain operations in a closure form one state commit.
Errors distinguish `MutationError::Rejected` from `MutationError::Persistence`.

Stores must make each save atomic, and leave the previous durable snapshot intact
on error. Uncertain commit outcomes need backend reconciliation or a stopped writer.
The host must serialize access to one service per snapshot; this interface does
not provide distributed multiwriter coordination. A remote store integration
must account for this synchronous interface in its host runtime.

Mutation closures must have no external effects. The host first persists intent,
then invokes its execution adapter, and records observations in later mutations.
Adapter failures and unreachable endpoints must not erase potentially live
execution records. Core state operations do not start workers or close terminals.

The public consumer tests in [`tests/embedding.rs`](tests/embedding.rs) demonstrate
a host-defined memory store, composed operations, rollback, retry, reopening and
dynamic store selection without an executor or database.

## Runtime portability

Native TASKR and Workers/Durable Object compatibility are requirements of the
core. The same synchronous domain/service API works on both. Store and error
types have no `Send` or `Sync` bound; the consumer tests use `Rc<RefCell<_>>` to
keep that single-threaded embedding contract checked. The core never reads the
clock, starts threads, opens files, spawns processes or initiates network calls.

For a JavaScript/Wasm host, enable UUID's JavaScript randomness integration:

```toml
[dependencies]
taskr-core = { path = "/path/to/taskr/crates/taskr-core", features = ["wasm-js"] }
```

The feature supplies UUID randomness; it does not introduce a Cloudflare SDK,
storage implementation or execution adapter. Cloudflare's Rust tooling builds
for `wasm32-unknown-unknown`. Its Rust `SqlStorage::exec` API is synchronous,
which allows a host-defined `SnapshotStore` to use Durable Object SQL storage.
[Workers Rust support](https://developers.cloudflare.com/workers/languages/rust/),
[Rust SQL storage API](https://docs.rs/worker/latest/worker/struct.SqlStorage.html)

The planned Cloudflare host boundary is:

| Responsibility | TASKR host today | Cloudflare host implementation |
| --- | --- | --- |
| Snapshot persistence | `SqliteOrchestrationStore` | Host-defined `SnapshotStore` over Durable Object SQL storage, publishing each complete snapshot atomically |
| Ownership and time | Mutex per service, host clock | One Durable Object per orchestration workspace, host clock and serialized mutations |
| Scheduling and recovery | Controller actors and restart sweeps | Durable Object alarms and recovery from persisted intent/observations |
| Agent execution | Native Herdr CLI adapter | Portable Herdr client with host-supplied container bindings |
| MCP and credentials | Native controller HTTP/authentication | Worker routing and host authentication/bindings |

The Durable Object owns its snapshot across eviction and restart. Complete a
state mutation before awaiting a network operation, release mutable service
access, then recheck the current task/execution identity before recording the
reply. Requests can interleave around awaits; alarms and retried operations
must reconcile durable intent rather than rely on an in-memory background job.
[Durable Object event and recovery rules](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/)

The Cloudflare host and SDK bindings are subsequent implementations. Portable Herdr/container and environment companion ports are implemented outside core; see [execution contracts](../../docs/execution-ports.md).
The repository provides the native TASKR host and checks the core plus public API
consumer tests for the Workers Wasm target; it does not deploy a Durable Object.
Async-only stores require a separate integration rather than blocking the
Workers event loop behind the synchronous store contract.

```bash
rustup target add wasm32-unknown-unknown
make check-core-wasm
make check-execution-wasm
cargo test -p taskr-core
cargo test --workspace
```

CI keeps the Wasm check separate from native workspace tests, so accidental
native dependencies or runtime bounds in the core break the portability check.

## Taskr cutover

Rust consumers use `taskr_core`. The native host packages are
`taskr-controller`, `taskr-herdr` and `taskr-environment`, with a `taskr`
executable. MCP tool names and snapshot serialization remain stable.
Reqvire Agent integration is a subsequent host change.

`Passed` remains an intermediate domain status. The native controller treats
`Passed`, `Delivered`, `Canceled`, and `Failed` as worker exit points, holding
successful workers while linked audits are unfinished. Herdr interaction and
pane closure and other effects belong to the host. Shared pure decisions for launch,
layout selection, audit/cleanup, ownership and resume are in `coordination`; changing core task state alone does not perform terminal
operations. Replaced attempts retain their native conversation references,
launch settings, reports, and placement provenance in `retained_executions`
until metadata pruning.
