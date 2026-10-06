---
name: taskr-developer
description: Use when changing taskr source code, tests, controller/runtime behavior, MCP tools, orchestration state, Herdr delegation, launch profiles, packaging, docs, or smoke tests in the taskr repository.
---

# taskr Developer

## Skill Definition

Use this skill when implementing or reviewing taskr code. taskr is an MCP
orchestration controller over a SQLite store; Herdr, an external
terminal/agent engine, owns terminals and coding-CLI execution.

Primary use cases:

- Change taskr source code, tests, CLI behavior, MCP tools, prompt templates, or
  orchestration state.
- Change Herdr delegation behavior: launches, executions, waits, agent
  interaction.
- Change native environment discovery/sync, launch choices, or project home constraints.
- Update user-facing docs, skills, recipes, or smoke tests
  for changed taskr behavior.

## Catalog

Code areas:

- `crates/taskr-core`: storage-agnostic orchestration model and
  `Orchestrator<S>` mutation service. The host implements `SnapshotStore`.
  `coordination` contains pure launch, layout, ownership, audit/cleanup, lease
  and resume decisions; hosts supply observations, time, persistence and effects.
  Store/adapter implementations, SQL, filesystems, Tokio and MCP stay outside core.
- `crates/taskr-controller`: MCP server, tool schemas/handlers, HTTP middleware,
  orchestration actors, prompt templates, SQLite store and migrations.
- `crates/taskr-environment`: portable Python companion protocol;
  `native` adds the SQLite catalog, source discovery/imports, deployments and
  asynchronous admin sync jobs.
- `crates/taskr-herdr`: generic `HerdrClient<T>`, typed observations and command/
  endpoint lifecycle ports. `NativeTransport` runs processes/SSH;
  `ContainerTransport<B>` uses host-supplied container bindings. Portable futures
  do not require `Send`; native features are enabled by default.
- `crates/taskr-controller/src/store_paths.rs`: controller-local database and
  path helpers. General TASKR file-transfer tools remain deferred.
- `crates/taskr-controller/src/endpoint_migration.rs` and `store.rs`: one-time
  conversion of legacy references; resolved records store Herdr IDs directly,
  without a launch-time alias map.
- `crates/taskr-controller/src/execution_cleanup.rs`: verified coding-agent
  exit followed by verified pane closure, shared by explicit stop, final task
  transitions, and retry/restart sweeps. Save native IDs and reports before exit;
  preserve audit holds and reject late startup updates after cancel.
- `crates/taskr-controller/src/plan_layout.rs`: persisted plan/endpoint spaces,
  Work/Validation/Review/Quality tabs, and fresh panes with two-agent overflow.
- `crates/taskr-controller/src/execution_resume.rs`: reopen saved native IDs
  with frozen launch settings, without task prompts or status changes.
- `src/main.rs`: root CLI subcommands (controller, project commands,
  prune).
- `crates/taskr-controller/src/prompts`: compile-time prompt templates used by
  MCP tools.
- `Plan.instructions` in `taskr-core` stores optional plan-wide
  Markdown instructions rendered into all `coding_task_send` prompt templates.
  Keep tool schemas, prompt rendering tests, README, and operator recipes
  aligned when changing this behavior.

Primary commands:

- `rg`: inspect existing structs, helpers, tests, and docs before editing.
- `cargo fmt --check`, `cargo fmt`: check or apply Rust formatting.
- `git diff --check`: catch whitespace problems.
- `cargo test -p taskr-controller <filter>`: focused controller tests.
- `cargo test -p taskr-core <filter>`: focused orchestration/core
  tests.
- `cargo test -p taskr-herdr <filter>`: focused Herdr client tests.
- `cargo test --workspace`: full workspace test suite.
- `make check-core-wasm`: compile core and consumer tests for
  `wasm32-unknown-unknown` with `wasm-js`; install the Rust target first.
- `make check-execution-wasm`: compile Herdr/environment ports and consumers
  without native features for `wasm32-unknown-unknown`.
- `cargo run -- controller ...`: isolated controller smoke tests.
- `make run-local LOCAL_ARGS="..."` or
  `make run-controller CONTROLLER_ARGS="..."`: pass controller flags through
  the target's argument variable rather than as standalone `make` options.

User-facing surfaces to keep aligned:

- MCP tool schemas and handlers.
- CLI flags and subcommands.
- README and bundled Codex and Claude skills.
- Keep README focused on capabilities and the project/plan/task model. Put
  installation, CLI, environment, MCP, and development details in the matching
  guides under `docs/` (`installation.md`, `cli.md`, `environments.md`, `mcp.md`,
  and `development.md`). Update links when moving a contract.
- `.codex/skills/taskr-*` and `.claude/skills/taskr-*` skills and
  `references/mcp-recipes.md`; keep matching copies synchronized.
- Installed taskr skills in default and project-specific Codex/Claude homes
  when updating installations; preserve unrelated skills and configuration.
- Prompt templates under `crates/taskr-controller/src/prompts`.

## Bootstrap

1. Run `git status --short` and preserve unrelated user changes.
2. Use `rg` to inspect the current implementation, tests, docs, and prompt
   templates before designing changes.
3. Identify the affected boundary: controller core, controller MCP/runtime,
   Herdr client, orchestration contracts, CLI, docs, or skills.
4. Pick the smallest change that preserves the current module boundary.
5. Choose focused tests before editing so validation matches the behavioral
   risk.

## Development Rules

- Read the current code before designing. taskr has moved quickly; do not assume
  old README text or prior conversation state matches the implementation.
- Prefer canonical behavior only. Do not add legacy aliases, compatibility
  fallbacks, or parallel old/new paths unless the user explicitly asks.
  Deprecated node/session/profile surface names are rejected by schema
  (`deny_unknown_fields`); do not reintroduce them.
- Keep runtime-neutral contracts out of controller-runtime code. Shared task,
  project, execution, and DTO logic belongs in the core crate; Herdr
  process details belong in the `taskr-herdr` client and controller runtime
  glue.
- Reusable state mutations go through `taskr_core::Orchestrator` and the host's
  `SnapshotStore` implementation; the host serializes access and supplies time.
  Persist candidate state before publishing it. Keep adapter effects outside
  mutation closures. SQLite schema/migrations and all Herdr execution remain
  host concerns. Consult `crates/taskr-core/README.md` before changing this seam.
- Keep core compatible with native TASKR and Cloudflare Workers/Durable Objects.
  Avoid runtime/threading bounds, including `Send`/`Sync` requirements on stores.
  The `wasm-js` feature selects UUID randomness; SDKs, SQL storage, alarms and
  network execution adapters belong to hosts. Check Wasm consumers when changing
  core APIs or dependencies; a successful build is not a deployed DO proof.
- Keep portable execution contracts compatible with non-`Send` hosts. The
  container adapter supplies shared transport behavior; a Cloudflare host still
  supplies SDK bindings, storage, alarms, readiness and checkpoint policy.
  Consult `docs/execution-ports.md` when changing this boundary.
- Persist allocation intent before dispatch. Unknown delivery or a lost receipt
  stays unresolved; neither an empty inventory nor restart permits a replacement
  allocation. Cleanup/prune must retain unresolved intents. Default launch facts
  treat a recovered startup intent as dispatched until evidence proves otherwise.
- Observe endpoints without starting them. Prepare/renew/release per-execution
  leases separately from cleanup selection, including audit and inspection holds.
  Repeated release is idempotent and must not stop another execution's endpoint.
- Pin execution/layout runtime generations and immutable terminal ownership.
  A positively observed replacement closes old metadata without touching new
  resources; unavailable probes are not proof of loss. Native CLI probes have a
  dispatch race; container bindings must fence the actual exec to the instance.
- Treat endpoint workspace paths as endpoint-owned strings. Do not
  canonicalize them against the controller host filesystem.
- Preserve actor boundaries. Long-running wait jobs run outside quick tool
  paths; do not block inspection tools behind wait jobs or launches.
- Be careful with execution visibility. Normal execution discovery is
  project-scoped; raw endpoint/agent discovery belongs in explicit admin/debug
  tools (`--enable-admin-tools`).
- Keep task orchestration simple in v1. Prefer strings for descriptive role,
  kind, and skill metadata unless the value controls runtime authority.
- When adding MCP tools, update schema, handler, tests, `docs/mcp.md`, affected
  guides, and relevant Codex and Claude skills/recipes in the same change.
  Update the README when the capabilities or work model change.
- Choose the layout group from the initial launch template, not role/kind
  strings or later prompts. Persist actual layout IDs; labels never confer
  ownership. Serialize allocation and last-pane closure per plan/endpoint.
- Preserve closed conversations and attempt reports in execution history.
  Native resume uses the saved ID, home/environment, cwd, arguments, and group
  in a fresh pane. Inspection resumes survive final-task cleanup and restart;
  explicit stop closes them without changing the task's status. Never replay
  a task prompt automatically on resume.
- Resume creates a new TASKR execution ID linked by `resumed_from`, while the
  native session ID stays the same. Keep active inspections discoverable even
  on finished tasks. Capture attempt reports before cleanup and preserve them
  across replacement; current task results may subsequently differ.
- Keep terminal viewing, focus, and navigation in Herdr. TASKR's MCP launch/resume
  tools coordinate durable execution lifecycle; they do not open a terminal
  UI client. Keep execution operations in MCP rather than adding CLI wrappers.
- Launch choices come from prepared endpoint deployments in `taskr.db`, not a
  startup JSON file. Keep sync admin-only and asynchronous; launches and resume
  must verify their pinned deployment before pane allocation. Configuration and
  skills are versioned, credentials require an explicit policy, and bundles
  never enter SQLite or launch arguments. Herdr owns screen/input handling.
- Environment verification and companion commands use the same prepared target
  and transport as Herdr. Native remote sync uses the provider-resolved saved SSH
  target, with JSON on stdin; do not introduce a second host catalog or local
  fallback. Resume must verify pinned deployment, native history and workspace
  before allocation. Config bundles do not preserve native conversation history;
  a managed host must checkpoint/restore it separately. Python 3.11+ and native
  CLIs are endpoint prerequisites; report unsupported dependencies explicitly.
- Test the companion with isolated native homes and fake CLI/SSH fixtures.
  `cargo test -p taskr-environment` also runs the Python bundle contract suite.
  Preserve old deployments and native history; ordinary prune does not delete them.
- Bundle/check native hook script dependencies, including commands in
  `hooks.json`. Codex writes reviewed hook trust under `hooks.state`; semantic
  integrity allows that runtime metadata while checking all other settings,
  hook declarations and script contents. Native trust review is distinct from
  prepared-environment readiness.
- Use the companion's shared dependency inventory for native config graphs,
  provider helpers, MCP/notify commands and Claude API helpers. Resolve file
  references against their declaring config or explicit command cwd, rebase
  them into the deployment, and reject cycles or ambiguous relative commands.
  Opaque commands and non-cat credential helpers require home-local
  `taskr-dependencies.json` declarations; never execute them during setup.
- Credential files obey explicit copy/endpoint policy, including declared helper
  data. Keep credential bytes out of discovery/SQLite/argv and include allowed
  transfers in deployment digests. Endpoint helper/env providers must not demand
  an unrelated native login file. Preserve old homes on rotation.
- Compute prerequisite metadata from effective native profiles. Only ready
  profiles publish choices; report missing variable names without their values,
  recheck the selected profile before launch/resume, and refresh readiness when
  endpoint variables change. Metadata defaults keep retained deployments readable.
- Extract Claude registry MCP configuration only. Preserve project scope using
  explicit mappings; exclude trust/sign-in/history and permit native metadata
  writes while pinning MCP definitions. Document manifest contracts in
  `docs/environment-dependencies.md` and test imports without ambient fallback.
- Discovery supports installed homes or `source_path` collection folders/ZIPs.
  Preserve source provenance across restart; imports cannot read dependencies or
  user skills outside their collection. Keep ZIP extraction bounded, private and
  atomic. Document manifest/path rules in `docs/environment-imports.md`.

## Implementation Checklist

1. Add or update focused tests for the behavior, not only parsing.
2. Update docs and skills when the user-facing MCP surface, CLI flags,
   execution semantics, launch profiles, or orchestration behavior changes.
3. Run focused tests first, then workspace tests.
4. Smoke test real MCP behavior when the change touches tool schemas, tool
   handlers, executions, Herdr launch/interaction paths, or persistent
   orchestration state.

## Test Commands

Use narrow tests while iterating:

```bash
cargo test -p taskr-controller <test-name-or-filter>
cargo test -p taskr-core <test-name-or-filter>
cargo test -p taskr-herdr <test-name-or-filter>
```

Before finishing, run:

```bash
cargo fmt --check
git diff --check
cargo test --workspace
```

For core APIs/dependencies, also run `make check-core-wasm`. For execution or
environment ports, run `make check-execution-wasm` and their portable contract tests.

If formatting fails, run `cargo fmt`, then rerun `cargo fmt --check`.

## MCP Smoke Tests

Do a real MCP smoke test for changes involving:

- tool schemas or arguments;
- execution listing, launching, adopting, stopping, reading, sending, waiting,
  or cleanup;
- task/project orchestration state;
- Herdr delegation behavior;
- launch profile resolution or agent homes;
- auth or token flag behavior.

Use the user's running controller when explicitly requested, with a disposable
project/plan and only test-owned executions. Preserve its store, configuration,
Herdr server, and unrelated panes. Close the test workers and archive the test
project; leave the user's controller running.

Otherwise use a fresh isolated store and port. Discover and explicitly sync a
test environment through admin MCP before attempting a launch:

```bash
taskr_smoke_store=$(mktemp -d /tmp/taskr-smoke.XXXXXX)
env -u TASKR_MCP_TOKEN \
  cargo run -- controller \
  --enable-admin-tools \
  --allow-remote-without-mcp-token \
  --port 3197 \
  --store-path "$taskr_smoke_store"
```

Then call `http://127.0.0.1:3197/mcp` with JSON-RPC. Include:

- `admin_environment_discover`, `admin_environment_sync`, sync status/cancel,
  and endpoint-scoped `list_launch_profiles` when testing environment setup.
  Use fixture credentials; never clone real login files implicitly.
- `project_create`, `plan_create`, and `task_create` when testing
  project/plan/task-scoped behavior.
- `start_coding_session` with a valid `task_id`, `endpoint_id`,
  `launch_profile_id`, and `workspace_path` when testing launches;
  `execution_adopt` with a valid task for an existing running agent.
- Missing/invalid-task launches must fail without contacting Herdr. `exec`
  never creates executions.
- Deprecated argument names (`node`, `session`, `coder_profile`) are rejected
  as unknown fields.
- The changed tool with both positive and negative cases.
- Cleanup of any launched executions (`execution_stop`) and the temporary
  store.

`python3 scripts/smoke-environments.py --execution-ports` runs the isolated HTTP
proof for pinned launch/resume, generation replacement/restart and missing
history refusal with fake Herdr/CLI resources. It starts no real coding agents.

For project-scoped execution listing, verify:

- `list_executions` without `project_id` fails.
- `list_executions(project_id)` accepts either project UUID id or globally
  unique slug and returns only durable executions attached to tasks in that
  project.
- `include_completed=true` surfaces stopped/exited executions.
- An active inspection resume appears even when its task is finished; previous
  attempts are available through `task_get.execution_history`.

For layout and resume changes, verify plan/endpoint spaces, template-derived
groups, bounded tab overflow, and preservation of manually added panes. Verify
native IDs/names and reports are saved before closure; audit holds/release;
resume without a prompt/status change; restart survival; explicit inspection
stop; and a fresh repeat preserving the original report and conversation.

For prompt paths, verify:

- complex text containing quotes, apostrophes, backticks, and fenced code is
  submitted correctly;
- `coding_send` returns quickly and long work is tracked with
  `wait_start(kind="coding_ready")`, `wait_status`, and `coding_read`;
- `check_state` reports lifecycle and occupant facts and never implies task
  acceptance.

For an isolated smoke controller, stop it and remove only its temporary store
afterwards. Documentation-only changes need skill/reference and whitespace
validation; do not launch agents merely to recheck unchanged runtime behavior.

## Final Report

Report:

- source files changed;
- docs/skills updated;
- focused tests run;
- workspace test result;
- smoke test result or why smoke was not applicable.
