# Portable Herdr execution boundary

The orchestration model and lifecycle policy live in `taskr-core`. Concrete
stores, process execution, container bindings, timers, credentials and MCP stay
in the host. The native TASKR controller continues to use Herdr's local server
and saved SSH machines. A Workers host can use the same Herdr client and
environment protocol with container bindings.

This repository implements portable interfaces and the container adapter over
host-supplied bindings. It does not implement or deploy a Cloudflare Worker,
Durable Object, image, or SDK binding. Wasm compilation and hermetic binding
tests are the current Cloudflare evidence.

## Interfaces and implementations

| Interface | Contract | Native implementation | Container host |
| --- | --- | --- | --- |
| `SnapshotStore` | Load and atomically save a complete orchestration snapshot; persist before publishing | TASKR SQLite store and migrations | DO SQL storage supplied by the host |
| `HerdrTransport` | Owned command vector, stdin, explicit environment/cwd, deadline, output bounds; typed stdout/stderr/exit status or delivery certainty | `NativeTransport`: process runner and endpoint companion over SSH | `ContainerTransport<B>` over `ContainerBindings` |
| `EndpointProvider` | Resolve a logical endpoint, observe readiness, prepare, renew/release per-execution leases | Resolve local session or enabled saved Herdr profile; observe the running API | Binding catalog; host restores/starts, monitors API readiness and manages leases |
| `EnvironmentCompanion` | Scoped discovery/export/prepare/verify/history requests through the command port | Native catalog supplies versioned choices and source discovery | Same bundled Python protocol, using the prepared container target |
| `taskr_core::coordination` | Pure launch sequencing, allocation outcome, layout placement, ownership, audit/cleanup, lease and resume decisions | Controller actors/timer perform effects and persist results | DO request handlers/alarms perform the same decisions |

`HerdrClient<T>` contains shared Herdr argument construction and parsing.
`NativeHerdrClient` selects `NativeTransport`. Futures on the portable ports
have no `Send` requirement; the native runner supplies `Send` futures for Tokio.
Public contract tests use `Rc<RefCell<_>>` hosts.

Native features are enabled by default. Workers consumers select:

```toml
taskr-core = { path = "../taskr/crates/taskr-core", features = ["wasm-js"] }
taskr-herdr = { path = "../taskr/crates/taskr-herdr", default-features = false }
taskr-environment = { path = "../taskr/crates/taskr-environment", default-features = false }
```

The environment crate's native feature adds its SQLite catalog, source import
cache, sync jobs, and Tokio. Its portable protocol has none of these dependencies.

## Endpoint resolution and lifetime

1. Persist an execution intent before runtime effects. Select a logical endpoint
   from the host catalog; never interpret an unknown ID as controller-local.
2. `Observe` is read-only. Neither inventory nor reconciliation starts a stopped
   server/container. Native SSH is not a provisioning or keepalive mechanism.
3. `Prepare { lease_id }` may restore and start a managed container. The binding
   must wait until Herdr's API responds. A running entrypoint is insufficient.
4. Use the returned **same target** for deployment verification, companion
   operations, allocation, startup and prompts. Companions cannot select a
   different SSH catalog or substitute the controller filesystem.
5. Renew leases while workers, unresolved intents, audit holds or inspection
   panes need the endpoint. Native TASKR's timer drives lease actions separately
   from cleanup selection. A DO host must persist its lease ledger and drive
   renewal with alarms, including after DO eviction/restart.
6. Exit the verified agent, close only its verified pane, persist the outcome,
   then release that execution's lease. Release is idempotent and retries after
   failure; it does not stop an endpoint shared with another execution.

A binding's `control` implementation owns startup, restore/checkpoint policy,
lease persistence and readiness probing. `Renew` must not restart a lost
generation implicitly. `Release` must preserve any filesystem state needed for
future resume before permitting the endpoint to disappear. Repeated release of
an absent lease should be cheap. Canceling a sync/command releases only its own
work, never another execution's lifetime.

For Cloudflare, implement control with `ctx.container` lifecycle/monitoring and
inactivity timeouts, and exec with command vectors plus stdin, user, environment,
cwd and an abort signal. Clean up abort listeners and enforce output/deadline
bounds. `ContainerEndpoint` requires an explicit user and `HOME`; its base
environment is merged with frozen launch configuration because exec does not
inherit all image/start environment variables.
[Cloudflare container API](https://developers.cloudflare.com/containers/api/durable-object-container/),
[SSH behavior](https://developers.cloudflare.com/containers/guides/ssh/).

## Delivery certainty and recovery

Readiness or inventory failure before allocation is `NotDelivered`. Rejection
of an invalid target/occupied pane is also known not to allocate. A timeout,
broken response or lost receipt after dispatch is `Unknown`; an allocation
whose placement could not be saved has already been `Delivered`.

Unknown/applied allocation errors keep the execution `allocating` and flagged
for reconciliation. Replacement launches, cleanup without a placement receipt,
and metadata prune cannot silently clear it. An empty agent inventory is not
proof that an unregistered shell was never allocated. Hosts must obtain the
original receipt or positive ownership evidence before continuing; the current
native CLI has no idempotent allocation receipt lookup, so ambiguous allocations
remain unresolved. Labels are never ownership evidence.

Agent startup follows the same certainty rule: an unknown or applied error,
including malformed JSON after dispatch, retains `starting` and its pane for
reconciliation. Only a proven non-delivery permits startup rollback.

If cancellation arrives while allocation is still in flight, TASKR records the
task cancellation immediately. The continuation records the actual receipt
before cleanup and prevents agent startup/prompt submission. It never retries
the allocation to finish cancellation.

Herdr's typed pane, tab, workspace, agent and process observations reject
incomplete inventories. Missing JSON arrays cannot be interpreted as an empty
endpoint for allocation, recovery or prune.

## Resource generations

`TaskExecution.runtime_generation` and `PlanLayout.runtime_generation` pin the
resource namespace. Runtime keys include it. Container generations include the
binding plus its incarnation, so changing a logical endpoint's binding cannot
make old IDs current. Bindings must fence the actual exec against that namespace,
including any restart between observation and dispatch.

Native generations use the server socket's filesystem identity (inode/device
and change time); remote identity is read on the remote endpoint. The remote
route is part of the namespace. Herdr's protocol generation/version is never
used as an incarnation. Every operation on a pinned target checks the current
namespace before CLI dispatch, and immutable terminal/occupant checks remain
in place. Herdr's CLI does not provide an atomic server-side generation
precondition: native checks cannot eliminate a restart between the last probe
and CLI dispatch. A stronger transport must bind dispatch to an exact server
instance, as required by `ContainerBindings::exec`.

When a different generation is positively observed, TASKR marks old resources
exited/closed, retains their IDs and reports as provenance, and releases their
leases without closing replacement IDs. This occurs during restart recovery
and the lease timer. Failed probes/missing identities are not proof of loss.

Native snapshot version 6 adds optional generations with an automatic v5 backup
and one-time upgrade. Older records keep `None`; they are not assigned the
current generation retrospectively. A layout without a generation is reusable
only while a recorded immutable terminal proves its ownership.

## Resume and filesystem durability

Resume prepares a fresh generation and fresh pane, preserves the old native
conversation ID and frozen arguments/environment/group, and sends no task prompt.
Before allocation it verifies the pinned deployment and the original native
history/home/data plus workspace. Missing history or workspace causes refusal;
a native session ID alone cannot recreate them.

Hook scripts referenced by native configuration travel in the bundle and have
integrity checks. Codex's writable `hooks.state` trust decisions are excluded
from semantic configuration checks; all other launch settings, declared hook
commands and script contents remain pinned. A copied hook can still need native
review because its command path changed. Older deployments without semantic
integrity metadata retain their strict byte checks and need explicit repair or
a fresh deployment when native trust writes have changed configuration.

The companion checks Codex active/archived JSONL history, Claude project JSONL,
Kimi context history, and OpenCode file/database history. It starts no agent to
perform verification. Unsupported layouts fail explicitly; preserve original
XDG data and database overrides in the execution environment.

Container snapshots preserve filesystem state, not running processes. A host
must restore compatible image/filesystem state, launch a new Herdr process,
issue a new generation, and reopen conversations. Cloudflare snapshot lifetime,
image compatibility and excluded mounted filesystems remain host responsibilities;
do not treat a snapshot handle as permanent history storage.
[Container snapshots](https://developers.cloudflare.com/containers/guides/snapshots/).

## Verification

```bash
cargo test --workspace
make check-core-wasm
make check-execution-wasm
python3 scripts/smoke-environments.py
python3 scripts/smoke-environments.py --execution-ports
```

The HTTP smoke modes use isolated SQLite stores, homes, CLI/SSH fixtures and
fake Herdr resources. They do not start real coding agents or use a user's store.
Container bindings and shared policy are exercised in
`taskr-herdr/tests/container_contract.rs`,
`taskr-environment/tests/portable_contract.rs`, and
`taskr-core/tests/coordination.rs`.
