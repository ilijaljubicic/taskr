# taskr — durable agent orchestration over MCP

taskr is a Rust MCP server for durable agent orchestration. It gives operators a
project -> plan -> task model for coordinating coding agents, recording
outcomes and blockers, gating work behind validations, and pruning finished
orchestration state.

taskr does not own terminals. Terminal and coding-CLI execution is delegated to
[Herdr](https://github.com/ilijaljubicic/herdr), an external terminal/agent
engine. The controller tells Herdr where and how to start an agent, observes
the result, and drives the agent through MCP tools.

The reusable orchestration library is [taskr-core](crates/taskr-core/README.md).
It owns domain state and transactional mutations through a storage interface.
TASKR supplies SQLite persistence, MCP transport and Herdr execution integration.
Other hosts can embed the core with their own stores and adapters, without a
Reqvire system model or the TASKR controller. The core supports native and
`wasm32-unknown-unknown` builds; `make check-core-wasm` checks its public APIs
for a Workers host. A Cloudflare Durable Object host provides its own
storage, alarms and execution SDK bindings; that host is not implemented yet.

The core idea is simple:

- `taskr controller` exposes the MCP HTTP endpoint and owns the durable
  orchestration store (SQLite).
- Herdr owns workspaces, tabs, panes, and agent lifecycle on each endpoint.
  Endpoints are Herdr's `local` session plus any saved machine profiles.
- Durable orchestration state groups work as projects, Markdown plan briefs,
  optional plan-wide instructions, and executable tasks with task-owned
  executions. Every agent launch is bound to a task: **no task, no execution.**
- Launch profiles select prepared, versioned native agent environments on an
  execution endpoint. They carry
  native agent arguments and non-secret configuration environment only; screen
  handling, readiness detection, and input strategy belong to Herdr.
- Agents run in Herdr-owned panes. Controller credentials stay out of worker
  environments; launch profiles carry configuration, not secrets.

The execution boundary now exposes portable command and endpoint lifecycle ports.
Native process/SSH transport and a container binding adapter share Herdr command
construction and parsing. Runtime generations fence saved placement, and
uncertain allocations remain unresolved rather than authorize duplicate launches.
See [execution contracts and Cloudflare host requirements](docs/execution-ports.md).
The container adapter is tested with bindings and compiles for Wasm; a deployed
Cloudflare host and SDK integration remain separate work.

## Project Status

taskr is in early development. The project aims to provide a secure control
plane for agent orchestration, but interfaces, configuration, and runtime
behavior may still change in breaking ways. Security guarantees cannot be made
at this stage. Review the configuration for your environment and use taskr at
your own risk.

## Prerequisites

| Dependency | Required for | Notes |
| ---------- | ------------ | ----- |
| Node.js and npm | `npx @mmux/taskr` quick start | The npm package extracts and runs the bundled native `taskr` binary for the current platform. |
| Rust and Cargo | Build, test, run | Install with rustup or your system package manager. |
| Herdr binary | All terminal/agent execution | taskr shells out to Herdr for every terminal operation (`--herdr-bin`, default `herdr` on `PATH`). Herdr owns its own terminal dependencies. Install: `curl -fsSL https://herdr.dev/install.sh \| sh`. |

Provisioning, store migration, active-worker cutover, and rollback are
documented in [docs/deployment.md](docs/deployment.md).

## Quick Start

### Run from npm

These commands apply once a Taskr npm release has been published. Until then,
build and run the renamed source with `make run-local`.

For a local loopback-only MCP server driving the local Herdr session (least
secure version; run only in trusted environments):

```bash
npx --yes @mmux/taskr controller --allow-remote-without-mcp-token
```

The MCP endpoint is:

```text
http://127.0.0.1:3000/mcp
```

Register that HTTP MCP server with codex:

```bash
codex mcp add taskr --url http://127.0.0.1:3000/mcp
```

Register it with claude code:

```bash
claude mcp add --transport http taskr http://127.0.0.1:3000/mcp
```

For authenticated local setup, start taskr with an MCP bearer token and register
the same token with each MCP client:

```bash
export TASKR_MCP_TOKEN="$(openssl rand -hex 32)"
npx --yes @mmux/taskr controller --mcp-token-env TASKR_MCP_TOKEN
```

In another shell with `TASKR_MCP_TOKEN` set, register codex:

```bash
codex mcp add taskr \
  --url http://127.0.0.1:3000/mcp \
  --bearer-token-env-var TASKR_MCP_TOKEN
```

Register claude code by adding the bearer header:

```bash
claude mcp add --transport http taskr http://127.0.0.1:3000/mcp \
  --header "Authorization: Bearer $TASKR_MCP_TOKEN"
```

The controller needs at least one launch profile before it can start agents.
Use [Launch Profiles](#launch-profiles) to discover and sync a native environment
through the admin MCP tools, then select one prepared launch choice.

### Install native binary

Taskr-named native archives will be available with the first Taskr release.
Until then, build and run the source with `make run-local`.

Install the latest released `taskr` binary:

```bash
curl -fsSL https://raw.githubusercontent.com/ilijaljubicic/taskr/main/scripts/install.sh | bash
```

Pin a specific release:

```bash
curl -fsSL https://raw.githubusercontent.com/ilijaljubicic/taskr/main/scripts/install.sh | VERSION=vX.Y.Z bash
```

Then run a local controller:

```bash
taskr controller
```

### Local development

From a repo checkout, the development command is:

```bash
make run-local
```

### Local MCP access

Default local runtime paths:

```text
store path:  ~/.taskr
MCP path:    http://127.0.0.1:3000/mcp
```

With `--store-path <path>`, taskr uses that path for durable state. Pass the
same path to store-backed CLI commands:

```bash
taskr controller --store-path /tmp/taskr-dev
taskr --store-path /tmp/taskr-dev create-project "Release hardening" --description "..." --slug release-hardening
taskr --store-path /tmp/taskr-dev list-projects
taskr --store-path /tmp/taskr-dev prune --dry-run
```

If you are calling the MCP endpoint directly, include both accepted response
types:

```bash
curl -X POST "http://<controller-host>:3000/mcp" \
  -H "Accept: application/json, text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Raw MCP clients must check for JSON-RPC `error` and tool-level `isError`
before parsing a response as a successful tool result. Tool failures are clear
but still returned in the MCP response envelope.

## CLI Entrypoints

| Command | Purpose |
| ------- | ------- |
| `taskr controller` | Runs the MCP control plane. Every terminal operation is delegated to Herdr. |
| `taskr create-project <title> --description <text>` | Creates a durable orchestration project in the local taskr store. Supports optional `--slug <slug>` and per-agent `--codex-home`, `--claude-home`, `--opencode-home`, `--kimi-home` paths. |
| `taskr delete-project <id-or-slug>` | Deletes a durable orchestration project from the local taskr store, including all contained plans, task cards, and task edges. |
| `taskr list-projects` | Lists durable orchestration projects from the local taskr store so project ids/slugs are discoverable. |
| `taskr prune` | Removes old retained worker terminals, stale execution records, and finished plans after observing Herdr endpoints. Defaults to dry-run, all categories included, and `--older-than-days 14`; pass `--execute` to apply cleanup. |

`src/main.rs` dispatches to the controller when no subcommand matches, so
`taskr --herdr-bin herdr` is equivalent to `taskr controller --herdr-bin herdr`.

Important controller flags:

| Flag | Default | Purpose |
| ---- | ------- | ------- |
| `--host` | `127.0.0.1` | Bind host for the MCP HTTP server. |
| `--port` | `3000` | Bind port. |
| `--mcp-token` | `TASKR_MCP_TOKEN` | Bearer token for MCP requests. |
| `--mcp-token-file` | none | Reads the MCP bearer token from a file. Prefer `/run/secrets` paths in containers. |
| `--mcp-token-env` | `TASKR_MCP_TOKEN` | Env var used when MCP token flags are omitted. |
| `--allow-remote-without-mcp-token` | false | Allows MCP without bearer auth and ignores `TASKR_MCP_TOKEN`; mutually exclusive with explicit MCP token flags. |
| `--store-path` | `~/.taskr` | Directory for durable state (`taskr.db`). |
| `--enable-admin-tools` | false | Enables admin-only MCP tools that create or change project boundaries. |
| `--herdr-bin` | `herdr` | Herdr executable used for every terminal operation. |
| `--herdr-session` | none | Explicit local Herdr session selection. Never affects saved-machine endpoints. |
| `--environment-python-bin` | `python3` | Python 3.11+ for local environment discovery/preparation. |
| `--environment-ssh-bin` | `ssh` | OpenSSH client for environment sync to saved Herdr machines. |
| `--max-timeout-seconds` | `120` | Maximum wait timeout accepted by wait tools. |
| `--max-request-bytes` | `2097152` | Maximum MCP HTTP request body size. |
| `--max-capture-bytes` | `2097152` | Maximum bytes returned by terminal capture tools. |

Prune flags:

| Flag | Default | Purpose |
| ---- | ------- | ------- |
| `--dry-run` | default | Preview what would be pruned without mutating state. |
| `--execute` | false | Apply the prune. |
| `--older-than-days` | `14` | Age cutoff for stale execution records and finished plans. |
| `--include-stale-execution-records` | false | Scope the run to stale durable execution records only. |
| `--include-finished-plans` | false | Scope the run to finished plans only. |
| `--herdr-bin` | `herdr` | Herdr executable used to observe live endpoints. |
| `--herdr-session` | none | Explicit local Herdr session selection. |

## Make Targets

```bash
make build
make check
make test
make lint
make release
make run-local
```

Pass entrypoint flags through the target variable:

```bash
make run-local LOCAL_ARGS="--port 3001 --enable-admin-tools"
```

Release publishing uses git tags. The release version comes from
`[workspace.package].version` in top-level `Cargo.toml`; all crates inherit it
with `version.workspace = true`. Bump that version with `make update-patch`,
`make update-minor`, or `make update-major`, merge the version change to
`main`, then run `make release-tag` from a clean `main` checkout. The
`v<version>` tag triggers GitHub Actions to build and attach platform archives
used by `scripts/install.sh`, then publish the npm package with all supported
platform archives.

### Configure npm publishing

The npm package is `@mmux/taskr`; its command is `taskr`. The `@mmux` npm scope
is independent of the GitHub repository name, `ilijaljubicic/taskr`. The old
`@mmux/mmux` package and its trusted-publisher connection are separate.

For a new package, npm requires an initial publication before configuring a
trusted publisher. From a reviewed checkout, build the local package and log
in with an npm account that can publish to `@mmux`:

```bash
make npm-package
cd npm/taskr
npm pack --dry-run
npm login --registry https://registry.npmjs.org
npm whoami --registry https://registry.npmjs.org
npm publish --access public --tag bootstrap
```

Confirm that `npm whoami` succeeds and reports an account allowed to create
packages in `@mmux`. A user-owned scope requires that user's account; an
organization scope requires the appropriate organization access. Access to an
existing package does not grant creation rights in another user's scope. If
`npm whoami` returns 401, renew the CLI login before retrying the publish.
[npm scope ownership](https://docs.npmjs.com/about-scopes/)

This first publication contains the current platform's binary and uses the
`bootstrap` tag. Use the release workflow for the first all-platform `latest`
version. A published version cannot be reused: if the bootstrap version is
`0.4.0`, the subsequent release must use a new version, such as `0.5.0`.
[npm package trust prerequisites](https://docs.npmjs.com/cli/v11/commands/npm-trust/)

Open `@mmux/taskr` on npm, select Settings → Trusted Publisher → GitHub Actions,
and configure:

| Field | Value |
| --- | --- |
| Organization or user | `ilijaljubicic` |
| Repository | `taskr` |
| Workflow filename | `release.yml` |
| Environment name | Leave empty; the release job has no GitHub environment. |
| Allowed actions | Enable direct publishing with `npm publish`. |

The release workflow uses Node.js 24 and `id-token: write` for OIDC, without an
npm publishing token. The publisher must match the repository and workflow
exactly. Commit/merge the package changes and version bump to `main`, then use
`make release-tag` from a clean `main` checkout. The workflow builds Linux and
both macOS binaries and publishes the complete package.
[npm trusted publishing](https://docs.npmjs.com/trusted-publishers/)

### Inspect the npm package locally

The `npm/taskr` package provides an `npx`/`yarn dlx` wrapper around the native
`taskr` binary. To inspect the package from this workstation:

```bash
make npm-pack-dry-run
```

`make npm-package` builds the current platform, writes
`npm/taskr/artifacts/taskr-<platform>.tar.gz`, and syncs the npm package version
from `[workspace.package].version`. The local package only contains the current
platform archive; public npm publishing is done by the GitHub release workflow
so the package contains all supported platform archives.

`make npm-pack` creates a `.tgz` without publishing. Set `NPM_CACHE=/path/to/cache`
if npm should use a cache directory other than `/tmp/taskr-npm-cache`.

The published package can be run with:

```bash
npx @mmux/taskr controller
yarn dlx @mmux/taskr controller
```

## Launch Profiles

Launch choices come from prepared native environments stored in `taskr.db`.
No controller `profiles.json` is needed. Herdr's live agent list reports running
processes; it does not discover configuration homes or native profiles.

Start the controller with `--enable-admin-tools` for setup. Python 3.11+ must be
installed on both ends, and the native coding CLI on the selected endpoint.
Installed-home discovery also needs the controller's CLI; imports may declare
their source CLI version in the manifest.
Remote setup also needs OpenSSH and a saved, enabled Herdr machine. The embedded
companion runs over that machine's saved SSH target; no daemon or companion
installation is required. It prepares files; Herdr remains the agent executor.

1. Call `admin_environment_discover` with `{}` to search local `.codex*` and
   `.claude*` homes (including nested `.claude` directories), or pass
   `homes: ["/home/user/.codex-work"]`. To import a native home or collection,
   use `source_path: "/path/to/environments"` or
   `source_path: "/path/to/environments.zip"` instead. Read the returned environment IDs,
   revisions, native profile names, and discovery issues.
2. Pick an `endpoint_id` from `list_endpoints`.
3. Call `admin_environment_sync` with the selected `source_environment_id`,
   `source_revision`, `endpoint_id`, and explicit `credential_policy`.
4. Poll `admin_environment_sync_status` using the returned `sync_job_id`.
   `queued` and `preparing` are in progress; `ready` returns prepared
   `launch_profile_ids`; `failed` reports setup errors. Cancel unfinished jobs
   with `admin_environment_sync_cancel`.
5. Call `list_launch_profiles({"endpoint_id":"local"})` (or the remote ID)
   and select a returned ID for `start_coding_session` or a task `run_spec`.

Folder/ZIP imports use the same sync tools and endpoint selection. Each detected
home retains its native profiles and supplied skills/configuration. Imported
sources do not inherit the controller user's skills or external files. An
optional `taskr-environments.json` manifest names environments and maps paths
from the original machine. ZIPs are extracted into a private, versioned cache
beside `taskr.db`; they can wrap the collection in one directory. Discovery
returns provenance and metadata. Source or archive changes require rediscovery;
prepared deployments retain their original settings. See
[folder and ZIP import format](docs/environment-imports.md) for examples,
path rules, limits, and cache retention.

Example sync using login already provisioned on the destination:

```json
{
  "source_environment_id": "<discovered environment ID>",
  "source_revision": "<discovered revision>",
  "endpoint_id": "local",
  "credential_policy": "endpoint",
  "endpoint_auth_home": "/home/user/.codex"
}
```

`credential_policy="endpoint"` excludes source login files and rejects recognized
embedded credentials in configuration. It copies the selected destination's
native login file into the managed home. `credential_policy="copy"` explicitly
permits transferring source login files and embedded configuration credentials.
There is no implicit credential transfer. OS keychains and provider/MCP OAuth
stores are not cloned. Provider access is not probed by sync; readiness confirms
files/dependencies, not that a provider will accept the login. Review source
configuration and provision target service credentials when needed.

Native `hooks.json` commands and their explicit script dependencies are included
and checked. Codex may request hook review after paths change during cloning.
Its `hooks.state` trust metadata can change without invalidating resume; launch
settings, hook commands and bundled scripts remain pinned.

`dry_run=true` validates preparation without publishing a home or launch choice.
`refresh=true` re-exports an already-ready selection, for credential rotation or
repair. Identical content reuses its deployment; changed content/credentials
create a new one. `deployment_root` optionally selects an absolute or `~/` managed
root on the endpoint; its default is `~/.local/share/taskr/environments`.

Discovery currently supports Codex homes with `config.toml` and Claude homes
with `settings.json`. Codex requires 0.134.0+ and the advertised `--profile`, `--no-daemon`, and
`--no-alt-screen` capabilities; native `*.config.toml` profiles
remain native and yield a base choice plus one choice per profile. Legacy inline
Codex profile tables require native migration first. The destination CLI must
be at least as new as the source. Claude currently yields its base settings
choice; Claude plugin installations are reported as unsupported rather than
silently omitted. OpenCode/Kimi environment adapters are not implemented yet;
already-running agents can still be adopted through MCP.

Bundles include native configuration, profile files, instructions, rules,
commands, agents, skills, declared file dependencies, and configured Codex plugin
installations. External Codex user skills are materialized under the deployed
home's supported `skills` directory. Repository skills stay with the repository.
Native sessions/history/logs and general home caches are excluded. Bundles are
limited to 64 MiB and 10,000 files; file symlinks are materialized, cycles and
special files are rejected. Missing MCP/hook executables, source-only localhost
services on remote sync, and unsupported state-path settings fail preparation.
Executables and whole remote servers are provisioned separately.

Managed homes publish atomically after validation, with private file permissions.
The SQLite registry stores metadata only; bundles and credentials travel through
companion stdin, never launch arguments or TASKR snapshots. Interrupted jobs are
reconciled after restart. Cancellation prevents publishing a launch selection;
an already-transferred deployment can remain for later reconciliation.

Normal launch verifies the selected deployment before allocating a pane. It
passes `CODEX_HOME` or `CLAUDE_CONFIG_DIR`, native profile arguments, and the
explicit permission policy to Herdr. Codex uses `--no-daemon --no-alt-screen`.
The process `HOME` is unchanged. Model, provider, permissions, hooks, skills, and
MCP settings are loaded by the native CLI from that deployed configuration and
its normal repository/managed layers. Sync does not grant permission bypass.

Choices are endpoint-specific and immutable: newer syncs retain previous IDs,
homes, and conversations. Executions pin the deployment through their selected
profile ID and frozen home/arguments. Resume uses that original home. Task exit
and ordinary prune never delete native conversations or deployed environments.
An empty endpoint catalog requires admin setup; there is no global-profile or
local-endpoint fallback. An explicit profile is required when several choices
exist; required MCP fields remain required.

The old `--launch-profiles-file`, `--enabled-launch-profiles`, and
`--default-launch-profile` flags are rejected. For existing stores, sync the
native home, clear conflicting project home overrides with `project_update`,
and explicitly update future task `run_spec.launch_profile_id` values with
`task_update`. Stored old preset IDs are not automatically remapped or historical
conversations relocated; pre-cutover sessions remain available in their original
native homes for direct native resume through Herdr.

## Per-project agent homes

Projects retain optional `codex_home`, `claude_home`, `opencode_home`, and
`kimi_home` metadata for existing stores and the retained project CLI. With a
prepared environment, a configured project home must exactly match its resolved
deployment home; a conflict is rejected. Normally leave these fields `null` and
let the prepared launch choice supply the home. `project_update` (admin) clears
an override with explicit `null`; omission preserves its value. Existing
executions keep their frozen environment.

```json
{"project_id":"example", "codex_home":null, "claude_home":null}
```

The endpoint owns the home path. TASKR does not reinterpret it on the controller
filesystem. The process `HOME` and task `workspace_path` remain separate from the
CLI configuration home.

## Executions

Every agent launch is a durable **task execution**: a binding between one
task, one Herdr endpoint, one launch profile, one workspace path, and the
Herdr-owned runtime facts (workspace id, tab id, pane id, agent name, agent
session). Executions are recorded in the SQLite store and survive controller
restarts.

- Endpoints are `local` (Herdr's local session) or a saved Herdr machine
  profile id. `--herdr-session` selects an explicit local Herdr session but
  never affects saved-machine endpoints.
- Launches are task-owned. `start_coding_session` requires an existing
  `task_id`; missing, malformed, or unknown task ids are rejected before taskr
  contacts Herdr. **No task, no execution.**
- Execution phases: `Pending` (launch intent persisted), `Allocating`
  (workspace/tab/pane allocation submitted), `Starting` (agent start
  submitted), `Live` (agent recognized on the endpoint), `Unavailable`
  (endpoint unreachable; the binding is kept), `Exited` (agent or pane gone),
  `Stopped` (stopped deliberately through TASKR-owned cleanup), and `Failed`
  (a launch failed with an observed error).
- Recovery states: `NeedsReconciliation` (default after a restart or gap),
  `Reconciled` (live facts confirmed), `OccupantMismatch` (the bound pane is
  live but occupied by a different agent; adopt it with `execution_adopt` or
  stop the record), `Abandoned` (deliberately given up). A surviving pane with
  no registered agent is proven-exited, not confirmed. Reconcile by observing
  the endpoint with `list_executions`, adopting the real pane/agent with
  `execution_adopt`, or stopping the record with `execution_stop`.
- `execution_stop` saves the report and native conversation reference, exits
  the coding agent, and closes its verified pane.

TASKR allocates one Herdr **space per plan and endpoint**. Tabs are created on
demand from the launch template; `start_coding_session.template` overrides the
stored run template, which otherwise defaults to `task`.

| Launch template | Tab group |
| --- | --- |
| `task` | Work |
| `validate` | Validation |
| `review` | Review |
| `quality-guard` | Quality |

Each execution gets a fresh pane with its own cwd and environment. A tab holds
at most two TASKR agent panes; further concurrent agents open `Work 2`, etc.
Roles and descriptive kinds do not select tabs. The group is frozen at launch;
later prompts do not move the pane. Spaces show `plan-N · Plan title`, and panes
show `task-N · Task title`. Persisted IDs establish ownership, so renaming a
label does not change routing. TASKR preserves user-added panes.

New Codex conversations receive the title `task-N · Task title · exec-ID`.
TASKR saves the native conversation ID, original launch arguments, configuration
environment, home, cwd, group, and report. `task_get.execution_history` exposes
previous attempts after replacement. Titles help discovery; the native ID
selects the exact conversation when resuming.

Inspect a live worker directly in Herdr by selecting the plan's space, group
tab, and task pane. To reopen a saved conversation after its pane has closed,
call MCP `execution_resume` with:

```json
{"execution_id":"<closed-execution-id>"}
```

Resume creates a fresh pane in Herdr. Select its plan space and group tab to
inspect it. Herdr owns terminal viewing and navigation. Controller MCP tools
(`coding_send`, `coding_read`, `check_state`, `execution_stop`) provide programmatic
control of the execution.

MCP `execution_resume` recreates a pane in the same plan,
group, and endpoint using the saved native session and frozen launch settings.
It can use an execution ID from `task_get.execution_history`. It fails if the
task already has a live/unresolved execution, the native ID is missing, or the
original profile is missing, disabled, or selects a different agent kind.
The owning task must still exist. A resume is an
explicit inspection: it does not replay a task prompt or change task status,
and automatic final-task cleanup leaves it open until `execution_stop`.
To repeat work in a new conversation, explicitly reopen the task and launch a
new attempt; its previous report and conversation remain in history.

To find a saved attempt, call `task_get` with the task ID. The current binding
is `task.execution`; previous attempts are in `execution_history`. Pass that
attempt's TASKR `execution_id` to resume. The result contains a **new**
`execution.execution_id`, `inspection=true`, and `resumed_from` pointing to the
source attempt. Use the new ID for runtime reads and explicit stop. An
active inspection appears in `list_executions(project_id)` even when its task
is finished; use `include_completed=true` to also list finished task bindings.

Call `execution_resume` on the running controller that owns the task. It opens
the pane without attaching a terminal client. The saved argument vector and
environment take precedence
over later profile or project-home edits. The original profile must still be
enabled and select the same agent kind. Native configuration, authentication,
and skills are read from the saved endpoint paths; their file contents are not
snapshotted by TASKR.

Each execution's `report` preserves its task status, outcome, and evidence when
cleanup first runs. Later status updates or fresh attempts can change the task's
current result while leaving that historical report intact. To repeat work,
move the task to `Planned`, then use `task_start` with its saved `run_spec` or
`start_coding_session` with explicit launch choices. A new launch submits the
task prompt and creates a separate native conversation.

`workspace_path` is the workspace/start directory interpreted by Herdr on the
selected endpoint. taskr stores and passes the string without canonicalizing it
against the controller host filesystem. Task scope is separate from runtime
placement: `include_paths` and `exclude_paths` define the task work
boundaries.

## Orchestration

taskr includes a durable orchestration layer for coordinating agent work:
projects contain Markdown plan briefs, plans contain executable tasks, and
each task can own executions. Projects are long-lived boundaries with required
descriptions. Plans carry enough context to derive tasks and may carry
optional plan-wide instructions that every task, validation, review, and
quality-guard prompt receives. Tasks carry objective, scope, gates, outcome,
blockers, dependency edges, and launch intent for the attached execution.

Tasks may also carry an optional `run_spec` with `endpoint_id`,
`launch_profile_id`, `workspace_path`, `bypass_permissions`, `role`, `kind`,
`skills`, prompt `template`, and `instruction`. `run_spec` describes how a
task can be started; the task's `auto_schedule` flag separately controls
whether explicit `orchestration_next` runs may start it automatically.

Operators create or select a project, create a plan, derive tasks from that
plan, then start executions against individual tasks. Initial delegation uses
`coding_task_send`, which renders deterministic task context before sending
the operator's instruction to the coding agent. Follow-up steering uses
`coding_send`.

Scheduling is explicit. An operator or external MCP controller observes
`orchestration_status`, records worker and validator results with
`task_status_update`, optionally calls `orchestration_report` to inspect
ready/skipped/error task classifications, then calls `orchestration_next` to
start all currently ready tasks in that plan whose `auto_schedule` is true. A
task is startable only when a `run_spec` exists, dependencies and required
validations are ready, the task is `Backlog` or `Planned`, its launch profile
is enabled, and it has no live execution. `orchestration_report` is read-only
and never starts executions. `orchestration_next` launches agents through
Herdr, records the executions on tasks, waits for coding readiness, sends task
prompts, and marks tasks `Running`. Use `task_start` to explicitly start one
task from its `run_spec` without requiring `auto_schedule=true`.

Task-owned `gates` are acceptance checks. `TaskEdgeKind::Validates` makes a
validator task operational: an edge from validator to target means downstream
tasks that `DependsOn` the target cannot start until that validator is `Passed`
or `Delivered`. A failed or blocked validator remains visible in
`orchestration_status` through `validation_blocked_by` and keeps dependent
auto-scheduled tasks from starting.

Supported task edges in v1 are `DependsOn`, `ParentOf`, `Validates`, `Audits`,
`Supersedes`, and `Related`. `DependsOn`, `ParentOf`, `Validates`, and
`Supersedes` have orchestration semantics. `Audits` is non-gating review
traceability; use `Validates` when an audit must approve gates before dependent
work can start. An unfinished `Audits` edge from audit to target also retains
the target's successful worker until the audit finishes. `Supersedes` marks the `to` task as replaced by the `from`
task, so the replaced task is skipped by scheduling and explicit starts.
`Related` is durable traceability/navigation only.

The orchestrator detects failed or blocked scheduler starts through
`orchestration_status`: startup/readiness/prompt failures are recorded as task
`Blocked` state with blockers. Worker-reported failure remains an operator
state update through `task_status_update`. Runtime detection of a long-running
task being "stuck" requires an additional timeout or heartbeat policy and is
not inferred from silence in v1.

Validation that must gate downstream work should be modeled as a task plus a
`Validates` edge. For example, the validation task should `DependsOn` the
implementation task, the validation task should `Validates` the implementation
task, and downstream work should `DependsOn` the implementation task. If
validation cannot run, mark the validator task `Blocked`; if validation rejects
the work, mark the validator task `Failed` with outcome evidence. The scheduler
will not start downstream `DependsOn` tasks while the dependency status is not
ready or while linked validators have not approved it.

`orchestration_status` is the compact source of truth for current project,
plan, task, edge, outcome, blocker, execution, cleanup, warning, and runtime
state. Use `task_get` when an exporter or operator needs one full stored task
body, including objective, scope, gates, outcome/evidence, run spec,
executions, and incoming/outgoing edges. Use `plan_get` for the analogous full
plan record, including the Markdown plan brief and optional plan instructions
that the summary listings omit. `orchestration_prune` is dry-run by default;
destructive cleanup requires explicit opt-in.

For the full operator workflow, use the bundled `taskr-operator` skill.

## MCP Surface

Orchestration tools:

| Tool | Purpose |
| ---- | ------- |
| `project_create` | Create a durable project (admin; requires `--enable-admin-tools`). |
| `project_update` | Update project configuration homes (admin). Omitted homes keep their value; explicit null clears. |
| `project_status_update` | Set project status to `Active` or `Archived` (admin). |
| `project_list` | List projects with plan/task counts. |
| `plan_create` | Create a plan inside a project. |
| `plan_update` | Update plan title/brief/instructions. |
| `plan_status_update` | Set plan status; outcome is required for `Delivered`. |
| `plan_get` | Get one plan with its tasks. |
| `plan_list` | List plans, optionally filtered by project. |
| `task_create` | Create a task inside a plan; optional `run_spec` pins the launch intent. |
| `task_update` | Update task fields; `run_spec` may be replaced or cleared with null. |
| `task_get` | Get one task with dependency blockers, superseding tasks, and previous execution history. |
| `task_status_update` | Set task status; outcome required before `Passed`/`Delivered` on gated tasks. |
| `task_edge_add` | Add a task edge (`ParentOf`, `DependsOn`, `Validates`, `Audits`, `Supersedes`, `Related`). |
| `task_edge_remove` | Remove a task edge. |
| `task_start` | Start one task now (or preview with `dry_run`); replaces non-active previous executions. |
| `orchestration_status` | Orchestration overview with live execution phases; optionally filtered. |
| `orchestration_report` | Report which tasks would start right now, without launching. |
| `orchestration_next` | Run one scheduler pass (`dry_run` previews, otherwise launches tasks). |
| `orchestration_prune` | Remove old retained worker terminals, stale execution records, and finished plans after observing live endpoints. |

Launch and execution tools:

| Tool | Purpose |
| ---- | ------- |
| `start_coding_session` | Launch a coding agent for one task on an explicit endpoint + launch profile + workspace, then deliver the task prompt. |
| `execution_adopt` | Adopt an already-running pane/agent as a task execution with observed provenance. |
| `execution_resume` | Reopen a saved native conversation in a fresh plan/group pane without a prompt or task-status change. |
| `execution_stop` | Save the report and native conversation reference, exit the coding agent, and close its verified pane. |
| `list_executions` | List durable executions for a project with live runtime facts. |
| `list_endpoints` | List Herdr endpoints (local + saved machine profiles) with live availability, plus the pending legacy-node migration counts. |
| `list_launch_profiles` | List prepared base/native-profile choices for a required `endpoint_id`. |
| `admin_environment_discover` | Admin: discover supported local native homes, profiles, and revisions. |
| `admin_environment_sync` | Admin: explicitly prepare an environment on one endpoint; returns a durable job. |
| `admin_environment_sync_status` | Admin: inspect sync progress and ready launch IDs. |
| `admin_environment_sync_cancel` | Admin: cancel unfinished preparation without publishing launch choices. |

Setting a task to `Passed`, `Delivered`, `Canceled`, or `Failed` automatically
exits its recorded coding agent through Herdr after verifying the current
occupant. TASKR interrupts the turn if necessary, submits the native `/exit`
command, verifies the return to a shell, then closes the owned pane. The outcome,
evidence, native conversation ID, and launch settings are saved before exit.
Closing the last pane may remove its tab and space through Herdr; a later launch
or resume recreates the layout. An unfinished task linked by an `Audits`
edge keeps a `Passed`/`Delivered` worker available until all linked audits
pass or end; the response reports `runtime_cleanup.state="held_for_audit"`
and the audit task IDs. Cancellation/failure requests exit immediately. Cleanup
retries every 10 seconds, including after restart.
If cleanup cannot finish immediately, `task_status_update` returns an MCP tool
error containing the saved final task and `runtime_cleanup.state="pending"`;
the final status is already committed. Unrelated panes and shared workspaces
remain available. A canceled startup cannot revive the task.

If a live worker's native conversation ID has not yet been observed, cleanup
remains pending to preserve resumability. Pane closure verifies the recorded
layout, immutable terminal ID, and sole shell foreground. Changed occupants,
manual commands, and unavailable endpoints remain untouched.

`orchestration_prune` or `taskr prune --execute` removes old execution metadata,
finished plans, and legacy leftover shell terminals after the configured age
(14 days by default). Dry-run previews the selection. The same ownership and
foreground checks protect remaining terminals. Native CLI conversation files
are never deleted by exit or prune; after TASKR records are pruned, use the native
CLI's resume picker or saved ID. Neither operation stops the Herdr server.

Coding interaction tools:

| Tool | Purpose |
| ---- | ------- |
| `coding_send` | Send a raw prompt to a task execution's coding agent. |
| `coding_task_send` | Render a task-aware prompt (template, gates, scope, context cards) and deliver it to a task execution. |
| `coding_read` | Read a task execution's coding-agent terminal output. |
| `capture_output` | Capture a task execution's raw pane output. |
| `check_state` | Truthful lifecycle and occupant facts for one task execution. |
| `send_input` | Send literal text (optionally followed by Enter) to a task execution's agent. |
| `send_key` | Send one key press to a task execution's agent. |
| `coding_action` | Send a semantic action (`approve`, `reject`, `cancel`, `escape`, `dismiss`, `continue`) to a task execution's agent. |
| `exec` | Run one shell command in an active execution's verified available shell pane. |

Wait tools:

| Tool | Purpose |
| ---- | ------- |
| `wait_start` | Start an async wait on a task execution (`stable`, `sentinel`, or `coding_ready`). |
| `wait_status` | Poll one async wait job. |
| `wait_cancel` | Cancel one async wait job. |

Admin debug tools:

| Tool | Purpose |
| ---- | ------- |
| `admin_list_endpoint_agents` | Admin: list every recognized live coding agent on one endpoint. Requires `--enable-admin-tools`. |
| `endpoint_migrate` | Admin: atomically replace unresolved legacy node references in stored run specs and execution endpoints with a selected Herdr endpoint ID. Creates no routing alias and does not launch/adopt workers. Requires `--enable-admin-tools`. |

`wait_start`, `wait_status`, and `wait_cancel` are the canonical orchestration
wait API. Wait jobs are runtime-only in v1 and are not persisted to SQLite.
Use `kind="coding_ready"` to wait for Herdr's `idle` or `done` lifecycle state.
`stability_seconds` applies to `stable` output waits. `check_state` reports
lifecycle and occupant facts; readiness does not mean the task was accepted.

Tool argument schemas reject unknown fields. Arguments named `node`,
`session`, or `coder_profile` are no longer part of the surface and are
rejected as unknown; use `endpoint_id`, execution ids, and
`launch_profile_id` instead.

## Resources and Prompts

Resources:

- `taskr://orchestration/status` returns the current orchestration status
  snapshot as JSON.
- `taskr://project/{project_id}/status` returns the per-project status snapshot
  (project UUID id or slug).

Prompts:

- `coding-task-send` — task prompt for a coding worker.
- `coding-validate-send` — validation prompt for a coding worker.
- `coding-review-send` — review prompt for a coding worker.
- `coding-quality-guard-send` — quality-guard prompt for a coding worker.

Each prompt takes one required argument, `task_id_or_slug`.

## Security

taskr is an agent controller with controller-host file access and the ability
to launch agents in arbitrary workspace paths. Giving a client access to a
writable taskr server is equivalent to giving that client broad control of the
controller host and of every configured Herdr endpoint. Use trusted clients
only.

The default bind host is loopback-only. A non-loopback MCP bind without a token
is rejected unless you deliberately pass `--allow-remote-without-mcp-token`.
That flag also ignores the default `TASKR_MCP_TOKEN` env fallback and is
mutually exclusive with explicit MCP token flags/files.

Authenticated requests must include:

```text
Authorization: Bearer <token>
```

The `x-mcp-token` header is accepted as an alternative. Cross-site browser
requests (`Sec-Fetch-Site: cross-site`) are rejected to blunt DNS-rebinding
and drive-by POSTs.

Controller credentials stay out of worker environments: launch profiles carry
non-secret configuration environment only, and per-project agent homes are
configuration paths, not credential stores. Do not put secrets into
`profiles[].env`; provision credentials on the endpoint through your own
mechanism.

TASKR filesystem access and file transfer are deferred. The `read_file` and
`save_file` MCP tools have been removed; calls to those names fail as unknown
tools. Their `--max-read-bytes` and `--max-write-bytes` flags are also removed
and rejected. Coding agents use their native file tools on the execution
endpoint; deployment tooling provisions repositories, configuration, and skills
there. Terminal output reads and prompt submission remain available.

Request and output limits are configurable with `--max-timeout-seconds`,
`--max-request-bytes`, and
`--max-capture-bytes`.

## Architecture

```text
.
├── src/main.rs                  # CLI dispatch: controller, project, prune
├── crates/
│   ├── taskr-controller/         # MCP server, auth, policy, orchestration actors, store
│   ├── taskr-core/              # storage-agnostic model, store interface, mutation service
│   ├── taskr-herdr/              # Herdr execution adapter, launch profiles, agent naming
│   └── taskr-environment/        # native environment discovery and endpoint deployment
├── taskr-cormilo-agent/          # bundled orchestration agent runtime
└── Makefile
```

Runtime flow:

1. `taskr controller` starts the MCP HTTP endpoint and opens the durable
   orchestration store (`taskr.db` in the store path).
2. An MCP client creates projects, plans, and tasks, and optionally attaches
   launch profiles and `run_spec`s.
3. A launch tool call (`start_coding_session`, `task_start`,
   `orchestration_next`) asks Herdr to allocate a workspace/tab/pane and start
   the configured agent, then records the durable execution binding.
4. Interaction tools (`coding_send`, `coding_read`, `check_state`, waits)
   drive the agent through Herdr.
5. Outcomes, blockers, and evidence are recorded back on tasks; pruning
   removes stale execution records and finished plans after observing live
   endpoints through Herdr.

## Health Check

```bash
curl "http://<controller-host>:3000/health"
```

The response body is:

```text
ok
```

## License

Apache-2.0 - see [LICENSE](LICENSE).
