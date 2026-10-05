# Deploying taskr with Herdr

taskr owns durable orchestration (projects, plans, tasks, executions, gates)
and the MCP control plane. Herdr owns every terminal: it renders, spawns,
and supervises the coding agents on the local machine or on remote endpoints.
taskr never falls back to tmux, never spawns agents outside Herdr, and never
doubles as a terminal multiplexer.

This document covers provisioning Herdr for taskr, migrating a pre-Herdr
store, cutting over live workers, and rolling back.

The default store is `~/.taskr/taskr.db`; `--store-path <directory>` selects
another store directory. The completed MMUX-to-Taskr branding conversion was
one-off, and its helper script has been removed. Taskr refuses to create an
empty replacement when it detects a legacy `mmux.db`; use an already converted
Taskr store or explicitly select a separate store directory.

Saved project metadata, endpoint paths, conversation IDs and deployed homes
remain historical data. Retained deployments use their original ownership
markers and integrity checks; new syncs publish under
`~/.local/share/taskr/environments`. Keep source caches referenced by retained
imports available at their saved paths.

## 1. Provision Herdr

### Local endpoint

Install the Herdr CLI (Linux and macOS, x86_64 and aarch64):

```sh
curl -fsSL https://herdr.dev/install.sh | sh   # installs to ~/.local/bin
herdr --version                                 # 0.9.3 or newer
```

Start the persistent session once so the socket exists:

```sh
herdr session list        # shows the default session and its socket
herdr                     # launches/attaches the persistent session
```

Then start the controller pointing at that binary:

```sh
taskr controller \
  --herdr-bin "$HOME/.local/bin/herdr" \
  --herdr-session default \
  --enable-admin-tools \
  --store-path /var/lib/taskr/store
```

The controller probes the local endpoint (`herdr api snapshot`) at startup
and refuses to assume a running server from the binary's existence alone.

### Remote endpoints

Remote endpoints are Herdr's saved SSH machines, not taskr configuration.
Provision each machine with Herdr's own tooling, then let taskr discover it:

```sh
herdr machine add            # interactive: host, user, identity
herdr machine list --json    # taskr derives endpoint IDs from these profiles
herdr machine status <id> --json
```

taskr lists whatever Herdr reports via `list_endpoints`, including a live
availability probe per endpoint. Endpoint IDs in `run_spec` must be `local`
or one of these saved profile IDs; anything else is rejected at launch.

### Sandbox-hosted Herdr

Isolation (containers, microVMs, CPU/memory limits, mounts) is deployment
tooling territory. Run the Herdr server inside the sandbox with its socket
and workspace mounts pointed at the sandbox filesystem, install the coding
CLIs and any skills inside that environment, and register the sandbox to
taskr as a normal endpoint (local session inside the sandbox, or a saved
machine profile from outside). taskr has no sandbox-specific mode: it sees a
regular Herdr endpoint and applies the same launch, gating, and ownership
rules.

### Environment setup

Install Python 3.11+ on both ends and the native coding CLI on the endpoint.
The controller also needs the CLI unless an import manifest declares its version.
Codex needs
0.134.0+ native profile files; destination versions must be at least as new as
the source. Remote sync uses OpenSSH and the target returned by
`herdr machine list --json`; there is no second host catalog or helper daemon.
`--environment-python-bin` and `--environment-ssh-bin` select local executables.

Use admin MCP `admin_environment_discover`, choose a source ID/revision and Herdr
endpoint, then call `admin_environment_sync` with explicit `credential_policy`.
Poll `admin_environment_sync_status`; cancel with `admin_environment_sync_cancel`.
Only ready, non-dry-run jobs publish endpoint-specific launch choices in
`list_launch_profiles({"endpoint_id":"local"})`. Normal worker tools cannot sync.
The registry and durable jobs share `taskr.db` with orchestration; file contents
and credentials are excluded from SQLite.

Discovery accepts `source_path` for a supplied native home, collection folder,
or ZIP instead of installed homes. Do not combine it with `homes`. Archives are
cached privately under `<store-path>/agent-environment-sources`, survive restart,
and are never extracted over user homes. Preserve that directory and its path
for pending archive syncs. Moving the store requires rediscovering imported
sources; pending jobs retain their original source paths. Source archives must remain available
and unchanged until sync completes. Prepared endpoint environments are independent
of the source cache. See [the import format](environment-imports.md) for manifests,
original-path mapping, shared skills and archive limits. Imported dependencies
must stay inside the collection; controller user configuration is not merged.

`endpoint` policy requires `endpoint_auth_home` and uses its destination-native
login file; source login files are excluded and embedded credentials are
rejected. `copy` explicitly allows cloning source file credentials. OS keychains
and MCP OAuth stores require separate target setup. `dry_run` validates without
publishing; `refresh` rechecks a ready selection or rotates credentials into a
new immutable deployment. See [the full setup contract](../README.md#launch-profiles)
and [MCP recipes](../.claude/skills/taskr-operator/references/mcp-recipes.md).

The default managed root is `~/.local/share/taskr/environments` on each endpoint.
Configuration, native profiles, skills, declared files, and configured Codex
plugin content are staged, validated, and published atomically. History/session
state is not copied from source homes. Native CLI and MCP/hook executables must
already exist; source-only services and unsupported dependencies fail explicitly.
Claude plugin cloning and OpenCode/Kimi environment adapters remain unsupported.

Remove old profile-file flags from service definitions. Explicitly select new
launch IDs in task run specs and clear conflicting old project home overrides
with `project_update`. Existing executions are not retargeted; historical
pre-cutover conversations stay in their original native homes. No JSON preset
fallback or automatic alias map is maintained.

## 2. Migrate the store

Older taskr stores (pre-Herdr) carried tmux-shaped task sessions and remote
node registries. The v0.4 store keeps only projects/plans/tasks/edges plus
execution bindings, and its schemas use `deny_unknown_fields`: deprecated
`node`, `session`, and `coder_profile` names are rejected at the MCP
boundary and in durable state rather than silently ignored.

Migration steps:

1. Back up the old store directory (`cp -a store store.pre-herdr`).
2. Start the new controller against the same `--store-path`. The snapshot
   upgrades to version 6 and is persisted once, with an automatic
   `taskr.db.v<old-version>-<timestamp>.bak` backup. Domain records, results,
   evidence, and launch configuration are preserved; old tmux runtime IDs
   are not treated as Herdr bindings.
3. `list_endpoints` returns saved Herdr profile IDs and
   `pending_endpoint_migrations`, whose keys are unresolved legacy node IDs
   and whose values count affected tasks. Start the controller with
   `--enable-admin-tools`, then select the real target explicitly:

   ```
   endpoint_migrate { "legacy_node_id": "build-farm-1",
                      "endpoint_id": "<saved-Herdr-profile-ID>" }
   ```

   The target must be `local` or an ID in Herdr's saved-machine catalog.
   This operation atomically rewrites marked references in stored run specs
   and execution records. It returns updated counts and task IDs, preserves
   other launch/ownership/recovery fields, and starts no processes. Repeating
   it after completion updates zero records; it cannot retarget modern
   records that happen to share an old node's name.

4. Once migrated, all operations use stored Herdr endpoint IDs directly.
   No alias table or launch-time node matching remains. The provisional v2
   `endpoint_mappings` table is consumed during the version-3 upgrade;
   already-frozen Herdr execution bindings remain unchanged. Because v2
   did not record provenance for unbound remote run specs, ambiguous specs
   require explicit confirmation through `endpoint_migrate`.

Version 4 adds durable ownership for stopped execution layouts retained across
new task attempts, including the immutable Herdr terminal ID captured at
allocation/adoption. Existing version-3 records upgrade without endpoint
remapping. The previous database is backed up before the upgrade.

Version 5 adds plan/endpoint space IDs, fixed group tabs, closed-pane provenance,
inspection resumes, native session names, and per-attempt reports. Previous
attempts remain in execution history even after their panes close. Version-4
bindings preserve their endpoint, home, arguments, native ID, and terminal ID;
existing spaces are not guessed from labels or moved. Subsequent launches use
plan spaces. Final-task cleanup closes only positively verified TASKR panes;
manually added panes and unverifiable legacy terminals remain untouched.

## 3. Cut over active workers

Old tmux workers remain outside Herdr. Drain or stop them explicitly and
verify any replacement/adoption separately; `endpoint_migrate` only changes
stored placement. Old runtime records remain unavailable and require cutover.

For executions already running through Herdr:

- On startup the controller marks every active execution
  `needs_reconciliation`, observes the endpoints, then confirms live
  workers as `reconciled`, marks proven-exited workers `exited`, and leaves
  unobservable endpoints pending.
- A live pane is only confirmed as owned when both the agent name and the
  native conversation identity match the stored binding. A pane occupied by
  a different agent is flagged `occupant_mismatch` and must be adopted
  (`execution_adopt`) or stopped by an operator; taskr never prompts into,
  reads, or closes a pane whose occupant it has not verified.
- Frozen launch intent (endpoint, launch profile, agent homes, arguments,
  working directory, known native session identity) is preserved across
  restarts; reconciliation may fill a previously missing native identity.

Inspect workers directly in Herdr by selecting their plan space, group tab, and
task pane on the execution endpoint. Herdr owns terminal viewing and navigation.

### Plan spaces, worker exit, and resume

`Passed`, `Delivered`, `Canceled`, and `Failed` stop the recorded coding agent
through Herdr using its native `/exit` command after verifying ownership. The
report, evidence, native conversation ID, and launch settings are saved before
exit. TASKR verifies the sole shell foreground and immutable terminal identity,
then closes the pane. `execution_stop` uses the same behavior. Agent readiness
(`idle`/`done`) alone does not close a task.

Each plan has one space per endpoint, with on-demand Work (`task`), Validation
(`validate`), Review (`review`), and Quality (`quality-guard`) tabs. Every launch
gets a fresh pane; each tab holds at most two agent panes before an overflow tab
opens. Select the template at launch. The last pane's closure may remove its
tab and space; subsequent launches recreate the layout.

Call MCP `execution_resume {"execution_id":"exec-..."}` to reopen the
saved native conversation in a fresh pane with its original configuration.
The task status and report are preserved, and no task prompt is sent. A resume
is an explicit inspection and remains open until `execution_stop`, including
across controller restart. Missing native IDs, busy tasks, disabled profiles,
and changed agent kinds are rejected. `task_get.execution_history` includes
previous attempts; repeating work uses an explicitly reopened task and a new
normal launch.

Find the source TASKR execution ID in `task_get.task.execution` or
`task_get.execution_history`. Resume returns a new `execution.execution_id`;
use that new ID for `coding_read`, `check_state`, and
`execution_stop`. Active inspections remain visible in default project-scoped
`list_executions` even for finished tasks. The owning task and original prepared
deployment must still exist.

Call the running controller that owns the task. The tool creates a pane and
returns its binding; inspect the new pane directly in Herdr.
Saved arguments/environment and home paths are reused
despite later profile or project-home edits. The native CLI loads configuration,
authentication, and skills from those paths again; TASKR does not snapshot or
reprovision their contents. If the TASKR task or history has been pruned, use the
native CLI's saved session ID or resume picker instead.

Link an audit before setting a successful task to `Passed` or `Delivered`:
an `Audits` edge runs from the audit task to the reviewed task. An unfinished
audit holds the worker until all linked audits pass or end, reported as
`runtime_cleanup.state="held_for_audit"`. Cancellation/failure requests exit
immediately. If exit cannot be verified, the final task state remains committed
and the MCP error reports `runtime_cleanup.state="pending"`; cleanup retries
every 10 seconds and after controller restart.

Run `taskr prune --dry-run` or MCP `orchestration_prune` to preview removal.
`taskr prune --execute` applies the age policy (14 days by default), removing
old execution metadata, finished plans, and leftover legacy layouts matching
the recorded ownership and containing an available shell. A resumed agent,
running shell command, changed layout, or
unreachable endpoint prevents removal. Layouts without a recorded immutable
terminal ID are kept rather than claiming ownership from a label. Closing the
last legacy leftover pane removes its tab and space; shared spaces and other
workers remain. Pruning does not
stop Herdr or delete native agent conversation files. Old stopped attempts
retain conversation references, original report snapshots, and ownership even
when a task starts a new execution.

## 4. Roll back

- Stop the new controller before touching the store.
- Restore `store.pre-herdr` if you need the exact pre-migration state; the
  new store format is not readable by pre-Herdr binaries.
- Endpoint migration rewrites records permanently. To undo it, stop the
  controller and restore the appropriate store backup; there is no mutable
  alias table to clear.
- Herdr keeps running independently of taskr; stopping the controller never
  stops workers. To wind a worker down deliberately, use `execution_stop`
  (which saves its conversation reference, exits the agent, and closes its
  verified pane) before rolling back. Prune old records separately.

## 5. CI

CI installs the real Herdr CLI (same `install.sh`) as a provisioning guard.
The Rust test suite itself is hermetic: every herdr invocation in tests goes
through a per-test fake CLI fixture with canned responses, so no Herdr
server (and no tmux) is needed to run `cargo test --workspace`.

Portable transport, endpoint lifetime, generation recovery and container host contracts are described in [execution ports](execution-ports.md). Version 6 preserves older execution/layout records with unknown generations; it does not rebind them to the current runtime.
