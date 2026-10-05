# Task execution and saved conversations

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

See the [MCP tools](mcp.md) and [Taskr operator skill](../.codex/skills/taskr-operator/SKILL.md) for interaction recipes. Use [Herdr documentation](https://herdr.dev/docs/) for terminal navigation.
