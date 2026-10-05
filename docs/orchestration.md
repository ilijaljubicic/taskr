# Task orchestration

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

See the [MCP tools](mcp.md) and [Taskr operator skill](../.codex/skills/taskr-operator/SKILL.md) for the operating workflow.
