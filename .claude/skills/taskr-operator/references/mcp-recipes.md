# taskr MCP Recipes

Use these request bodies against `http://127.0.0.1:3000/mcp` unless the user
names another endpoint.

## Headers

```bash
-H 'Accept: application/json, text/event-stream'
-H 'Content-Type: application/json'
-H 'MCP-Protocol-Version: 2025-11-25'
```

If MCP bearer auth is enabled, send the token explicitly and never print token
values:

```bash
-H "Authorization: Bearer $TASKR_MCP_TOKEN"
```

When calling MCP directly, check both JSON-RPC `error` and tool-level
`isError` before parsing the result as a success payload. Tool failures are
returned clearly inside the MCP envelope. Every `start_coding_session` call
requires a valid existing `task_id`, explicit `endpoint_id`,
`launch_profile_id`, and `workspace_path`; `bypass_permissions` defaults to
`false`, and `role`/`kind` default from the stored run spec and launch
profile when omitted. Pass `endpoint_id: "local"` for Herdr's local session;
other endpoint ids are saved Herdr machine profiles. Missing/invalid tasks are
rejected before Herdr access. `skills` defaults to an empty list.

## Discovery

```json
{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}
```

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_launch_profiles","arguments":{"endpoint_id":"local"}}}
```

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_endpoints","arguments":{}}}
```

```json
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"orchestration_status","arguments":{}}}
```

## Migrate legacy endpoints once

Use `list_endpoints` to inspect pending legacy node IDs and select an actual
Herdr endpoint ID. `endpoint_migrate` requires `--enable-admin-tools` and
permanently rewrites marked references in stored run specs and executions:

```json
{"jsonrpc":"2.0","id":81,"method":"tools/call","params":{"name":"endpoint_migrate","arguments":{"legacy_node_id":"build-farm-1","endpoint_id":"<saved-Herdr-profile-ID>"}}}
```

The result reports `run_specs_updated`, `executions_updated`, and `task_ids`.
It does not launch or adopt a worker. Resolved records use Herdr IDs directly;
there is no alias table to clear. Repeating a completed conversion updates zero
records. Restore a store backup to undo a conversion, and handle old live
workers through the separate cutover workflow.

## Projects

MCP `project_create` is advertised but requires `--enable-admin-tools` to execute.
It requires `title` and `description`. For offline
local store setup, use
`taskr create-project <title> --description <text> [--slug <slug>]`.

```json
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"project_create","arguments":{"title":"taskr orchestration","description":"Tasks for taskr orchestration work"}}}
```

```json
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"project_list","arguments":{}}}
```

For new projects, omit `codex_home`, `claude_home`, `opencode_home`, and
`kimi_home`. Select a prepared `launch_profile_id` for the execution endpoint
when starting a task; that profile supplies the agent home and configuration.
Stored project homes are legacy constraints: a value conflicting with the
selected deployment home causes launch rejection.

Clear legacy home constraints (requires `--enable-admin-tools`):

```json
{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"project_update","arguments":{"project_id":"example","codex_home":null,"claude_home":null,"opencode_home":null,"kimi_home":null}}}
```

Explicit `null` clears a field; omitted fields keep their current values. The
next launch uses the updated constraints without a restart. Existing executions
keep their frozen environment. `project_list` and `orchestration_status` return
the stored home fields.

## Plans

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"plan_create","arguments":{"project_id":"taskr","title":"Update orchestration docs plan","brief":"Document the orchestration workflow, update operator recipes, and validate examples. Tasks should cover README and skill recipe updates.","instructions":"Keep changes scoped to the documented plan and report changed files, validation commands, blockers, and unresolved questions."}}}
```

## Tasks

`include_paths` and `exclude_paths` are task scope boundaries. Relative paths
are interpreted from the execution workspace. `run_spec.workspace_path` is
runtime launch placement, not an extra scope path.

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"task_create","arguments":{"plan_id":"plan-0001","title":"Update orchestration docs","objective":"Document the orchestration workflow.","scope":{"include_paths":["README.md",".claude/skills/taskr-operator/SKILL.md",".claude/skills/taskr-operator/references/mcp-recipes.md"],"exclude_paths":["target"],"notes":"Documentation-only task."},"gates":["Docs updated","No unrelated files changed"]}}}
```

```json
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"task_update","arguments":{"task_id":"task-0001","title":"Update orchestration docs and recipes","scope":{"include_paths":["README.md",".claude/skills/taskr-operator"],"exclude_paths":[],"notes":"Docs task expanded to include recipes."},"gates":["Docs mention current workflow","Examples are usable"]}}}
```

```json
{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"task_update","arguments":{"task_id":"task-0001","auto_schedule":true,"run_spec":{"endpoint_id":"local","launch_profile_id":"codex","workspace_path":"/mnt/Radni/taskr","bypass_permissions":false,"role":"implementation-worker","kind":"implementation","skills":["taskr-developer"],"template":"task","instruction":"Implement this task. Report changed files, validation commands, blockers, and unresolved questions."}}}}
```

Read one full stored task body for export or exact inspection:

```json
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"task_get","arguments":{"task_id":"task-0001"}}}
```

Read one full stored plan body (including its Markdown brief):

```json
{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"plan_get","arguments":{"plan_id":"plan-0001"}}}
```

```json
{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"task_edge_add","arguments":{"from":"task-0001","to":"task-0002","kind":"DependsOn"}}}
```

Use `Validates` from validator task to target task when the validator result
should gate downstream tasks that depend on the target:

```json
{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"task_edge_add","arguments":{"from":"task-0002","to":"task-0001","kind":"Validates"}}}
```

```json
{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"task_edge_remove","arguments":{"from":"task-0001","to":"task-0002","kind":"DependsOn"}}}
```

```json
{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"orchestration_report","arguments":{"plan_id":"plan-0001"}}}
```

After committing worker or validator results, an external controller advances a
plan with `orchestration_next`; it starts all currently ready tasks in the
plan by default:

```json
{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"orchestration_next","arguments":{"plan_id":"plan-0001"}}}
```

```json
{"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"orchestration_next","arguments":{"plan_id":"plan-0001","dry_run":true}}}
```

```json
{"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"task_start","arguments":{"task_id_or_slug":"task-0001","dry_run":false}}}
```

## Launch and Executions

Create/select the task before launching. No task, no execution:
`start_coding_session` rejects a profile and directory without `task_id`.
Every successful launch returns a durable execution binding on that task with
an `execution_id`. `exec` requires an existing execution and never creates
one; it accepts no `workspace_path` argument.

```json
{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"start_coding_session","arguments":{"endpoint_id":"local","launch_profile_id":"codex","bypass_permissions":false,"task_id":"task-0001","role":"editable-worker","kind":"codex","skills":["docs","taskr"],"workspace_path":"/mnt/Radni/taskr","template":"task"}}}
```

Use the returned `execution_id` for `coding_task_send`, follow-up
`coding_send`, wait tools, `coding_read`, and `check_state`. Task updates use
the task's `task_id`.

Launch performs bounded startup and submits the rendered task prompt before
returning. Wait and read that result before sending another assignment; an
immediate second `coding_task_send` would duplicate the initial work.

Prepare environments through admin MCP when the endpoint has no launch choices.
Start setup with:

```bash
make run-local LOCAL_ARGS="--enable-admin-tools"
```

Pass flags through `LOCAL_ARGS` (`CONTROLLER_ARGS` for `make run-controller`).
Python 3.11+ is required on both ends, with the native coding CLI on the endpoint.
Discovery needs the controller CLI unless an import manifest declares its source
version. Remote sync also uses OpenSSH and Herdr's saved SSH target.

```json
{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"admin_environment_discover","arguments":{"homes":["/home/user/.codex-work"]}}}
```

Alternatively import a supplied native home, collection folder or ZIP:

```json
{"jsonrpc":"2.0","id":24,"method":"tools/call","params":{"name":"admin_environment_discover","arguments":{"source_path":"/path/to/environments.zip"}}}
```

Do not combine `source_path` with `homes`. The folder/ZIP can contain multiple
Codex and Claude homes and an optional `taskr-environments.json` manifest for
names, original-path mappings and shared skills. Imports use supplied dependencies
without controller user-home fallback. The controller keeps a private archive
cache beside its store; keep the original ZIP unchanged through sync. Changed
sources require rediscovery. Select and sync each returned environment separately;
the following sync/status/launch flow is identical for all source types.

Choose the returned source ID/revision and a destination from `list_endpoints`:

```json
{"jsonrpc":"2.0","id":25,"method":"tools/call","params":{"name":"admin_environment_sync","arguments":{"source_environment_id":"<source ID>","source_revision":"<revision>","endpoint_id":"local","credential_policy":"endpoint","endpoint_auth_home":"/home/user/.codex","dry_run":true}}}
```

```json
{"jsonrpc":"2.0","id":26,"method":"tools/call","params":{"name":"admin_environment_sync_status","arguments":{"sync_job_id":"<job ID>"}}}
```

Use `dry_run:false` for publication. `ready` then returns `launch_profile_ids`;
select one from `list_launch_profiles({"endpoint_id":"local"})`. A dry run
creates no launch choice. `refresh:true` rechecks a ready selection or rotates
credentials into a new version while retaining the old deployed home.

Cancel unfinished setup:

```json
{"jsonrpc":"2.0","id":27,"method":"tools/call","params":{"name":"admin_environment_sync_cancel","arguments":{"sync_job_id":"<job ID>"}}}
```

`endpoint` uses required destination login/helper credential files beneath
`endpoint_auth_home` and rejects recognized embedded source
credentials. `copy` explicitly permits cloning source file credentials; never
select it implicitly. Credentials and bundles stay out of SQLite and tool
responses. Provider connectivity and OAuth/keychain migration are separate.

Inspect `prepared.profile_readiness` in sync status: blocked profiles report
`missing_environment` names and receive no launch IDs. Supply these variables
to both the endpoint companion and Herdr agent process, then refresh. Launch
verification rechecks the selected native profile. Never persist secret values
in run-spec or launch-profile environment metadata.

For a Codex profile using `cat /absolute/path/to/key` as its provider helper,
copy policy includes that private key file and rebases the helper argument.
Endpoint policy excludes it and expects the reported managed relative path
beneath `endpoint_auth_home`; it requires no `auth.json` for a helper-only
provider. Helper-key rotation changes the deployment digest; refresh preserves
the old home and conversation. Config/script edits require rediscovery.

Opaque commands and non-cat auth helpers require a native-home
`taskr-dependencies.json` declaration for indirect files and required variables.
No helper/MCP script runs during sync. Nested role configs, catalogs, scripts
and explicit cwd dependencies are followed and rebased; ambiguous relative
command files fail rather than guessing the task workspace. Claude registry
MCP configuration is extracted without login/trust metadata; project-local
definitions require explicit project mappings and never become global. See
`docs/environment-dependencies.md` in the TASKR repository for the manifest.

Codex homes contain `config.toml` and optional native `*.config.toml` profiles;
Claude homes contain `settings.json`. Skills and configured Codex plugin content
are included. Claude plugin cloning and OpenCode/Kimi adapters are unsupported.
Native histories are excluded. Normal launches use the prepared home and native
profile without another sync. Clear conflicting legacy project home overrides
with admin `project_update` and update future task run-spec IDs explicitly.

Adopt an already-running pane/agent instead of launching one:

```json
{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"execution_adopt","arguments":{"task_id":"task-0001","endpoint_id":"local","agent_name":"taskr-docs-worker","role":"editable-worker","kind":"codex"}}}
```

List durable executions for a project:

```json
{"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"list_executions","arguments":{"project_id":"taskr"}}}
```

Truthful lifecycle and occupant facts for one execution:

```json
{"jsonrpc":"2.0","id":33,"method":"tools/call","params":{"name":"check_state","arguments":{"execution_id":"exec-..."}}}
```

Save the native conversation/report, exit one coding agent, and close its pane:

```json
{"jsonrpc":"2.0","id":34,"method":"tools/call","params":{"name":"execution_stop","arguments":{"execution_id":"exec-...","dry_run":false}}}
```

To change worker, stop the current live execution first. Only `Exited`,
`Stopped`, or `Failed` bindings can be replaced; inspect unavailable or
unresolved bindings before taking action. Launch or adopt the new worker with
the same `task_id`. A launch already sends the initial task prompt; read its
result before sending follow-up steering. Previous attempts remain in history.

An allocation timeout can leave `phase:"allocating"` without a pane receipt.
Do not force replacement or prune it: an empty agent inventory is not proof that
allocation failed. Recovery needs a receipt, positive ownership evidence, or a
positively observed replacement runtime generation. Failed endpoint probes leave
the binding unresolved. Endpoint discovery does not start a stopped server.

One plan has one space per endpoint. Launch with `template:"task"` for Work,
`"validate"` for Validation, `"review"` for Review, or `"quality-guard"` for
Quality. Select it at launch; later prompts do not move the pane. Each tab holds
at most two agents before an overflow tab opens. Every attempt gets a fresh pane.

Read the task to find its current `task.execution` and previous
`execution_history` entries:

```json
{"jsonrpc":"2.0","id":35,"method":"tools/call","params":{"name":"task_get","arguments":{"task_id":"task-0001"}}}
```

Pass the chosen entry's TASKR `execution_id` to reopen its saved conversation:

```json
{"jsonrpc":"2.0","id":36,"method":"tools/call","params":{"name":"execution_resume","arguments":{"execution_id":"exec-previous"}}}
```

Resume uses the saved native ID, arguments, home/env, cwd, endpoint, and group.
It submits no prompt and changes no task status. It remains open for inspection,
including after restart, until `execution_stop` on the newly returned execution
ID. Successful `structuredContent` includes these fields:

```json
{"execution":{"execution_id":"exec-inspection","inspection":true,"resumed_from":"exec-previous"},"prompt_submitted":false,"task_status_changed":false}
```

Use `exec-inspection` for runtime tools. Inspect its pane directly in Herdr by
selecting the plan space and group tab. Call `execution_resume` on the running
controller that owns the task. The current
profile must exist, be enabled, and select the original agent kind; resume uses
saved args/env despite later profile or project-home edits. The native CLI reads
the current configuration/authentication/skills at those saved endpoint paths.
The owning task must still exist and have no live/unresolved execution, and the
source must have a saved native session ID. If TASKR records have been pruned,
use the native CLI's resume picker or saved ID.

Before pane allocation, resume verifies the pinned deployment, original native
history/home/data and workspace on that endpoint. Missing history or workspace
is an error; restore it before retrying. Environment sync transfers configuration
and skills, not conversation history. Runtime generation replacement preserves
the saved conversation/report but does not restore endpoint files.

Cloned hooks can need native review after their command paths change. Review
the referenced script in the selected endpoint home and dismiss the hook screen
before submitting work. Use `send_key` with `key:"esc"` to go back; nested hook
screens may need more than one press. Codex's native `hooks.state` writes are
allowed by semantic integrity checks; hook declarations, scripts and other
launch settings remain pinned. Older deployments lacking that metadata keep
strict byte checks until explicitly repaired or replaced.

Active inspection resumes appear in `list_executions(project_id)` even for
finished tasks. Use `include_completed:true` to also inspect finished task
bindings. `task_get.execution_history` remains the source for older attempts.

Stop the new inspection explicitly; its task status and results stay unchanged:

```json
{"jsonrpc":"2.0","id":37,"method":"tools/call","params":{"name":"execution_stop","arguments":{"execution_id":"exec-inspection"}}}
```

To repeat work in a separate conversation, first stop any active inspection,
then explicitly reopen the task:

```json
{"jsonrpc":"2.0","id":38,"method":"tools/call","params":{"name":"task_status_update","arguments":{"task_id":"task-0001","status":"Planned","outcome":"Repeat requested."}}}
```

If it has a `run_spec`, start the fresh attempt with:

```json
{"jsonrpc":"2.0","id":39,"method":"tools/call","params":{"name":"task_start","arguments":{"task_id_or_slug":"task-0001"}}}
```

Otherwise use `start_coding_session` with explicit choices as above. This sends
the task prompt and creates a new native conversation. Prior native IDs and
report snapshots stay in history even when the current task result changes.

## Send Work

```json
{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{"name":"coding_task_send","arguments":{"execution_id":"exec-...","task_id_or_slug":"task-0001","template":"task","prompt":"Implement only this task. Run focused validation. Report: outcome, changed_files, validations_run, blockers, unresolved_questions. Do not mutate taskr task state directly."}}}
```

Use `template:"quality-guard"` when the worker should check project/operator
quality preferences rather than validate gates or perform a general review:

```json
{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{"name":"coding_task_send","arguments":{"execution_id":"exec-...","task_id_or_slug":"task-0001","template":"quality-guard","prompt":"Check for over-generalization, hidden runtime assumptions, unclear ownership, obsolete fallback paths, and abstractions that do not reduce complexity. Report proceed|revise|escalate with evidence and recommended corrections."}}}
```

For validation of a task set, pass the reviewed task ids as `context_task_ids`.
This makes taskr render field-complete operator task cards into the validator
prompt. Do not ask the validator execution to call taskr to discover prior task
results.

```json
{"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"coding_task_send","arguments":{"execution_id":"exec-...","task_id_or_slug":"task-0009","template":"validate","context_task_ids":["task-0001","task-0002","task-0003"],"prompt":"Validate the supplied task-card bundle. For each card, check id, status, gates, outcome/evidence, scope, blockers, and edges. Report findings first, then field_coverage_table, gate_results, evidence, commands_or_checks_run, residual caveats, and recommended_status. Do not call taskr internally."}}}
```

```json
{"jsonrpc":"2.0","id":43,"method":"tools/call","params":{"name":"wait_start","arguments":{"execution_id":"exec-...","kind":"coding_ready","timeout_seconds":120,"poll_seconds":0.5}}}
```

```json
{"jsonrpc":"2.0","id":44,"method":"tools/call","params":{"name":"wait_status","arguments":{"wait_id":"wait-..."}}}
```

```json
{"jsonrpc":"2.0","id":45,"method":"tools/call","params":{"name":"coding_read","arguments":{"execution_id":"exec-...","source":"recent","lines":80}}}
```

## Status

```json
{"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"task_status_update","arguments":{"task_id":"task-0001","status":"Running","outcome":"Docs worker launched with scoped prompt."}}}
```

```json
{"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"task_status_update","arguments":{"task_id":"task-0001","status":"Blocked","outcome":"Validation cannot complete because the docs checker command is missing.","blockers":["No markdown checker target found"]}}}
```

```json
{"jsonrpc":"2.0","id":52,"method":"tools/call","params":{"name":"task_status_update","arguments":{"task_id":"task-0001","status":"Passed","outcome":"Validation passed and scoped diff reviewed.","evidence":["cargo test -p taskr-controller passed"]}}}
```

```json
{"jsonrpc":"2.0","id":53,"method":"tools/call","params":{"name":"task_status_update","arguments":{"task_id":"task-0001","status":"Delivered","outcome":"Delivered after validation passed and no unrelated changes were found."}}}
```

`Passed`, `Delivered`, `Canceled`, and `Failed` automatically exit the task's
verified coding agent, then close its owned pane after saving the native ID,
report, and configuration. Link an unfinished audit with `Audits` before setting
`Passed`/`Delivered` to keep its worker open until all such audits pass or end;
the response reports `runtime_cleanup.state="held_for_audit"` and audit IDs.
Cancellation/failure requests exit immediately. A cleanup failure
returns an MCP tool error containing the committed task and
`runtime_cleanup.state="pending"`; cleanup retries every 10 seconds and after
restart. Inspect pending cleanup before taking further action. Reports and
evidence and native conversation IDs remain in TASKR after the agent exits.
Closing the last pane may remove its tab and space; launch/resume recreates them.
Pruning later removes old metadata and leftover legacy shell layouts (14 days
by default), protecting changed occupants and manual commands. Native CLI
conversation files remain available after TASKR metadata is pruned.

## Cleanup

Start with dry-run cleanup:

```json
{"jsonrpc":"2.0","id":60,"method":"tools/call","params":{"name":"orchestration_prune","arguments":{}}}
```

Run explicit cleanup only after reviewing the dry-run result:

```json
{"jsonrpc":"2.0","id":61,"method":"tools/call","params":{"name":"orchestration_prune","arguments":{"dry_run":false,"older_than_days":14}}}
```

Inspect state again after cleanup:

```json
{"jsonrpc":"2.0","id":62,"method":"tools/call","params":{"name":"orchestration_status","arguments":{}}}
```

## Troubleshooting

```json
{"jsonrpc":"2.0","id":70,"method":"tools/call","params":{"name":"list_executions","arguments":{"project_id":"taskr","include_completed":true}}}
```

Admin visibility of every recognized live agent on one endpoint:

```json
{"jsonrpc":"2.0","id":71,"method":"tools/call","params":{"name":"admin_list_endpoint_agents","arguments":{"endpoint_id":"local"}}}
```

```json
{"jsonrpc":"2.0","id":72,"method":"tools/call","params":{"name":"check_state","arguments":{"execution_id":"exec-..."}}}
```

```json
{"jsonrpc":"2.0","id":73,"method":"tools/call","params":{"name":"coding_action","arguments":{"execution_id":"exec-...","action":"approve"}}}
```

A binding left in `NeedsReconciliation` after a controller restart is
reconciled by confirming live facts (`check_state`, `list_executions`),
adopting the real pane/agent (`execution_adopt`), or closing the binding
(`execution_stop`).
