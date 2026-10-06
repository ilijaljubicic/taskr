# MCP reference

Discover the current argument schemas through `tools/list`. Admin operations require a controller started with `--enable-admin-tools`.

## Tools

Orchestration tools:

| Tool | Purpose |
| ---- | ------- |
| `project_create` | Create a durable project (admin; requires `--enable-admin-tools`). |
| `project_update` | Assign `environment_ids` to a project (admin); omitted assignments are preserved and `[]` clears them. Also manages legacy home constraints. |
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
| `list_environments` | List known environments, prepared deployments, and discovery blockers; optional `project_id` restricts the list to assigned environments. |
| `list_launch_profiles` | List prepared choices for `endpoint_id`, optionally filtered by `project_id`, with model/effort options. `include_retained=true` includes earlier deployments. |
| `admin_environment_discover` | Admin: discover supported local native homes, profiles, and revisions. |
| `admin_environment_sync` | Admin: explicitly prepare an environment on one endpoint; returns a durable job. |
| `admin_environment_sync_status` | Admin: inspect sync progress, ready launch IDs and per-profile missing environment prerequisites. |
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

## Resources and prompts

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

These are worker-assignment templates. Select the agent environment before
launch through the task's `run_spec` or explicit launch arguments. The operator
skill covers mixed-profile plans and task-specific choices; templates do not
select a model or inherit a plan profile.

Projects allow source environment IDs. Each task selects a prepared profile
from an assigned environment; manual launches, scheduler passes, and inspection
resume check the assignment. Task `launch_hints` are advisory. Concrete settings
belong in `run_spec.launch_options` or `start_coding_session.launch_options`.
See [profile management](profiles.md) for examples and supported option metadata.

See the [Taskr operator skill](../.codex/skills/taskr-operator/SKILL.md) and its [MCP recipes](../.codex/skills/taskr-operator/references/mcp-recipes.md) for examples.
