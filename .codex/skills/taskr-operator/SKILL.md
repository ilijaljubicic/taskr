---
name: taskr-operator
description: Use when operating taskr MCP task executions, delegating work to opencode/codex/kimi/claude through taskr and Herdr, preserving the primary agent context budget, or coordinating expensive/secret-bearing local model and provider workflows.
---

# taskr Operator

## Skill Definition

Use this skill to operate taskr as the controller, not the worker. Create and
supervise task executions, assign focused tasks, read concise results, and
intervene only when needed.

Projects contain plans, and plans contain tasks. Each task execution selects a
Herdr endpoint, a prepared launch profile on that endpoint, and a working
directory. Tasks in one project can use different profiles and machines; a
profile can serve tasks from multiple projects.

Primary use cases:

- Drive coding-agent CLIs through taskr MCP tools; Herdr owns the terminals.
- Coordinate durable plans, tasks, executions, gates, and status inside
  a selected project boundary.
- Preserve this agent's context by delegating broad exploration or long-running
  implementation to worker executions.
- Inspect or recover durable execution bindings without bypassing taskr state.

## Selecting profiles for tasks

A user can request a mixed-profile plan in plain language:

> Use the taskr-operator skill. For plan X, use `glm-5-3-zai` for
> implementation tasks on `local`, working in `/path/to/repository`.
> Use my regular Codex profile for validation and review.

- Resolve profile names through endpoint-scoped `list_launch_profiles` and
  use the returned `id` as `launch_profile_id`. Inspect native profile,
  agent kind, and deployment to distinguish choices; clarify ambiguous names
  such as "my regular Codex profile" if existing context does not resolve them.
  Never substitute a native profile name, model name, or example ID for that ID.
- Tasks may remain without a `run_spec` while planning. Before starting work,
  select an explicit, nonempty profile ID, endpoint, and workspace for each
  task. Save these in its `run_spec` for `task_start` or scheduling; an explicit
  `start_coding_session` request supplies them for that attempt instead.
- Plans have no profile-default field or automatic profile inheritance. Apply
  a plan-wide request to each affected task, including later tasks created
  under that request. An explicit choice for one task takes precedence over
  a broader plan or role preference. Preserve unrelated task selections.
- Assign stable source `environment_ids` to the project through admin
  `project_create` or `project_update`. Tasks can select any prepared profile
  from those environments. Use `list_environments(project_id)` and
  `list_launch_profiles(endpoint_id, project_id)` for project-scoped choices.
  Unassigned environments and empty profile IDs are rejected before launch.
- Task `launch_hints` are advisory model/reasoning preferences. Resolve them
  against the selected profile's `settings.model_options`, then save concrete
  `model`/`reasoning_effort` in `run_spec.launch_options` or pass launch options
  explicitly for that attempt. A worker prompt does not configure native model
  settings. Do not claim a hint was applied unless the execution's recorded
  options show that selection. Preserve profile/provider/credential boundaries.
- `role`, `kind`, `skills`, and prompt `template` do not route models or select
  launch profiles. Implementation, validation, and review tasks can each use
  different profiles; select the appropriate template separately.
- Editing launch intent affects future attempts. Live executions and
  `execution_resume` keep their frozen launch settings. Updating a `run_spec`
  configures the task without launching it; start work only when requested.

See [profile selection examples](references/mcp-recipes.md#profile-selection-by-task)
for the MCP calls that implement this request.

## Catalog

Core MCP endpoint:

- Default URL: `http://127.0.0.1:3000/mcp`.
- Health check: `GET /health`.
- If MCP bearer auth is enabled, pass `Authorization: Bearer $TASKR_MCP_TOKEN`
  and never print token values.

Discovery and state tools:

- `tools/list`: discover the current MCP surface.
- `list_environments(project_id?)`: inspect discovered environments, prepared
  deployments, source model options, and discovery issues. With a project, list
  only assigned environments. Without a project, inspect setup choices/blockers.
- `list_launch_profiles(endpoint_id)`: inspect prepared endpoint-specific base
  and native-profile choices, including their deployment/revision.
  Pass `project_id` for project-scoped choices. Default listing shows the newest
  ready deployment per environment/native-profile pair; use `include_retained`
  for historical choices. Existing run specs and resumes keep their pinned IDs.
- `list_endpoints`: list Herdr endpoints (local + saved machine profiles) with
  availability. Observation is read-only and does not start stopped endpoints.
- `orchestration_status`: inspect projects, plans, tasks, outcomes, blockers,
  executions, warnings, cleanup candidates, and runtime state.
- `task_get`: read one full stored task body, including objective, scope,
  gates, outcome/evidence, run spec, current `task.execution`, previous
  `execution_history`, and incoming/outgoing edges.
- `plan_get`: read one full stored plan body, including the Markdown brief and
  optional plan-wide instructions that `plan_list` omits.
- `project_list`, `list_executions(project_id)`, `check_state`: inspect
  durable and live runtime state.

Project, plan, and task tools:

- `project_create`: create a project boundary through MCP; select agent environments through prepared launch profiles when starting tasks.
- `project_update`: assign `environment_ids` and clear legacy project home constraints through admin MCP; omitted fields retain their values, `environment_ids:[]` clears assignments, and live executions keep their frozen environment.
- `plan_create`, `plan_list`, `plan_update`, `plan_status_update`, `plan_get`:
  manage plan work-package documents and status; `plan_get` returns one full
  stored plan body including its brief and optional instructions.
- `task_create`, `task_update`, `task_status_update`: manage
  task metadata, optional scheduler `run_spec`, executions, and state.
- `task_edge_add`, `task_edge_remove`: maintain task relationships.
- `orchestration_report`: read-only report of ready/skipped/error task
  classifications for automatic orchestration.
- `orchestration_next`: advance one plan by starting all ready
  auto-scheduled tasks after worker/validator results have been recorded.

Launch and execution tools:

- `start_coding_session`: launch a coding agent for a required existing task on
  an explicit endpoint + launch profile + workspace, then deliver the task
  prompt. No task, no execution.
- `execution_adopt`: adopt an already-running pane/agent as a task execution
  with observed provenance.
- `execution_resume`: reopen a saved native conversation in a fresh plan/group
  pane with its frozen launch settings; no task prompt or status change.
- `execution_stop`: save the report/native ID, exit one coding agent, and close
  its verified pane. Explicitly stops inspection resumes.
- `list_executions`: list durable executions for a project with live runtime
  facts. Active inspections of finished tasks are included by default; use
  `include_completed=true` for finished task bindings.
- `admin_list_endpoint_agents`: admin/debug view of every recognized live
  coding agent on one endpoint.
- `endpoint_migrate`: admin-only, one-time conversion of unresolved legacy
  node references in stored run specs/executions to a selected Herdr endpoint.
  It launches no workers and creates no routing alias. `list_endpoints` reports
  pending source IDs and affected task counts.

Interaction tools (all target one `execution_id`):

- `coding_task_send`: send initial task-aware work using rendered task context.
- `coding_send`: send follow-up steering.
- `wait_start`, `wait_status`, `wait_cancel`: supervise waits.
- `coding_read`, `capture_output`: read selected terminal output.
- `coding_action`, `send_input`, `send_key`: handle prompts, interrupts, and
  steering.
- `exec`: run one shell command in an active execution's verified available
  shell pane.

Environment setup and launch selection:

- `admin_environment_discover` returns local native home IDs, revisions, and
  native profiles. `admin_environment_sync` explicitly prepares a selection on
  one Herdr endpoint; status/cancel tools operate on its durable `sync_job_id`.
  These tools require `--enable-admin-tools`; sync is setup, not an agent task.
- Discovery also accepts `source_path` for a native home, collection folder or
  ZIP, mutually exclusive with `homes`. Imported sources use provided skills and
  dependencies, without controller user-home fallback. Optional manifests map
  original paths; consult `docs/environment-imports.md` in the TASKR repository.
  Keep archives/cache available until sync completes; rediscover changed sources.
- `credential_policy="endpoint"` uses `endpoint_auth_home` for the required
  native login and helper credential files; helper/env-only providers do not
  require unrelated native login files. Source credentials are excluded.
  `copy` explicitly allows source
  credentials. Never choose copy implicitly. Use `dry_run` for validation and
  `refresh` for a ready selection's credential rotation or repair.
- Discovery reports dependency metadata. Sync status's
  `prepared.profile_readiness` reports per-profile missing variable names; only
  ready profiles publish launch IDs. Provide variables to the endpoint companion
  and Herdr agent environment, then refresh. Values are not cloned or stored.
  Launch/resume rechecks the selected profile's pinned files and prerequisites.
- Helpers are inspected, never run during sync. Opaque commands and non-cat auth
  helpers require `taskr-dependencies.json` in the native home for indirect files
  and required variables. Consult `docs/environment-dependencies.md`; do not
  silently omit a script, credential, cwd or nested configuration dependency.
- Absolute native binary/interpreter paths are preserved and checked on the
  endpoint, including runtimes outside PATH. A missing executable needs endpoint
  provisioning; do not shorten its configured path or copy a native binary.
- Claude `.claude.json` user MCP definitions are extracted without trust/login
  metadata. Project-local MCP definitions need explicit project path mappings
  and keep their scope; repository `.mcp.json` remains repository-owned.
- `list_launch_profiles` requires `endpoint_id`. Select a returned prepared ID;
  an empty list needs admin sync. The old profile-file/default/enabled CLI flags
  are removed. No implicit sync, endpoint fallback, or global default is used.
- Codex base/profile choices preserve native `*.config.toml` settings and use
  `CODEX_HOME`; Claude base choices use `CLAUDE_CONFIG_DIR`. The process `HOME`
  is unchanged. Model/provider/hooks/MCP configuration loads natively.
- Sync supports Codex and Claude environments, including configured installed
  plugins and their dependencies. Claude plugin registries and marketplace paths
  are rebased; versions are pinned and project/local scopes require explicit
  project mappings. Valid homes with `sync_blockers` remain discoverable and
  assignable, but must be repaired and rediscovered before sync. Never remove an
  enabled plugin or borrow another environment's payload to hide a setup error.
  Unsupported dependencies fail before launch;
  readiness does not prove provider authentication or service connectivity.
- A cloned Codex hook may need native review after its path changes. Inspect
  the execution in Herdr, review the referenced script and handle that dialog
  before sending work. Use `send_key` with `esc` for Escape; `coding_ready`
  alone does not prove a hook dialog is closed. Native hook-trust writes are
  allowed by deployment verification; launch settings and scripts stay pinned.

Read `references/mcp-recipes.md` when exact JSON-RPC request bodies, headers,
or troubleshooting examples are needed.

## Bootstrap

1. Confirm the controller is reachable with `GET /health`.
2. Discover the loaded tool surface with `tools/list`.
3. Call `list_endpoints` to pick the `endpoint_id` (`local` or a saved Herdr
   machine profile).
4. Call `list_launch_profiles` with that endpoint; select a ready choice. If
   none exist, an admin prepares one using the environment setup tools.
   Assign its source environment to the project before configuring/launching
   tasks. New and upgraded projects have empty assignments until configured.
5. Inspect durable state with `orchestration_status`.
6. For project-scoped executions, call `list_executions(project_id)` with a
   project UUID id or globally unique slug. Use `admin_list_endpoint_agents`
   only for admin debugging.
7. For every launch, create/select the task first and choose the runtime
   values: `endpoint_id`, `launch_profile_id`, `workspace_path`,
   `bypass_permissions`, `task_id`, `role`, `kind`, `skills`, and initial `template`.

## Core Rules

- Treat this agent's context as expensive. Do not spend it reading broad file
  trees or long logs when a worker execution can inspect and summarize.
- Keep secrets out of prompts, transcripts, and final answers. Refer to secret
  env vars by name only. Never ask a worker to print API keys or tokens.
- Prefer taskr MCP tools over direct terminal driving for interactive coder CLIs.
- General filesystem/file-transfer tools are deferred. The environment companion
  deploys only scoped configuration bundles; repositories and native binaries
  still need endpoint provisioning.
- Keep project agent home fields null when using prepared launch choices. An
  old override conflicting with the selected deployed home is rejected; admin
  `project_update` clears it with explicit `null`. Omitted fields are preserved.
  Existing executions retain their recorded endpoint, deployment, home, and args.
- New sync revisions retain previous launch choices and native conversations.
  Resume stays on the original deployment; cleanup/prune do not delete its files.
  For old manual preset IDs, explicitly update future task run specs; do not
  retarget historical conversations to a newly cloned home.
- A timeout after allocation can leave an `allocating` binding without a pane
  receipt. Do not relaunch, stop or prune it to force a retry; inspect recovery
  and obtain positive placement or namespace-loss evidence. Empty agent lists
  are insufficient. Unavailable endpoints remain unresolved. A positively
  observed new runtime generation marks old resources closed while preserving
  native IDs/reports, without closing replacement panes.
- Inspect live worker panes directly in Herdr using the plan's space and group
  tab on the selected endpoint. Herdr owns terminal viewing and navigation.
- Use `taskr create-project` for offline project setup. Use
  `taskr list-projects` to find project ids/slugs from the CLI.
- Keep worker prompts specific: objective, constraints, expected output, and
  stop condition.
- Treat `coding_send` as submit-only. Coding prompts can run for minutes or
  hours; do not wait for completion inside `coding_send`. Track progress with
  `wait_start(kind = "coding_ready")`, poll `wait_status`, use `coding_read`
  for output, and `wait_cancel`/`coding_action`/`send_key` only when steering
  is needed.
- Be patient with work that is already running under a wait job. Long-running
  work should be supervised by polling `wait_status` and reading the execution,
  not abandoned just because it takes minutes.
- After each significant worker action, capture output and check state before
  sending the next instruction. `check_state` reports truthful lifecycle and
  occupant facts (`phase`, `recovery`, `runtime_state`, `agent_status`,
  `foreground_cwd`); it never implies task acceptance.
- Wait for `coding_ready` (Herdr reports the agent `idle` or `done`) or a completed
  `stable`/`sentinel` wait before sending a new independent prompt, new task
  prompt, validator prompt, review prompt, or quality-guard prompt.
- Do not leave accidental test executions running. Keep intentional workers
  alive when the user asks to wait for further instructions; stop disposable
  ones with `execution_stop`.
- Setting `Passed`, `Delivered`, `Canceled`, or `Failed` automatically exits
  the task's recorded coding agent with occupant verification and closes its
  verified pane after saving the native ID, report, and launch settings. An unfinished
  `Audits` task keeps a `Passed`/`Delivered` worker open until all linked audits
  pass or end; link the audit before closing the task. The response names held
  audit IDs in `runtime_cleanup.state="held_for_audit"`. Cancellation/failure
  requests exit immediately. Outcomes/evidence stay saved. If the
  tool returns `runtime_cleanup.state="pending"` with an MCP error, the final
  task state is committed and cleanup retries every 10 seconds and after
  restart. Inspect the task and execution; never exit a replacement occupant.
  Missing native IDs on live workers leave cleanup pending to preserve
  resumability. The last pane's closure may remove its tab/space; they are
  recreated on demand. Pruning removes old metadata and leftover legacy shell
  layouts after the age policy (14 days by default), verifying immutable
  ownership and shell foreground. Native conversations are never deleted.
- One plan has one Herdr space per endpoint. Select the launch template for
  on-demand Work (`task`), Validation (`validate`), Review (`review`), or Quality
  (`quality-guard`) tabs. Each execution gets a fresh pane; a tab holds two
  agent panes before an overflow tab opens. Roles/kinds and later prompts do
  not change the frozen group. Labels are display text; IDs establish ownership.
- New Codex conversations are named `task-N · Task title · exec-ID`. Use
  `task_get.execution_history` to find previous attempts and native IDs.
  `execution_resume` reopens the exact saved session
  using its original profile arguments, home/env, cwd, endpoint, and group.
  Resume is inspection: it sends no prompt and leaves status/results unchanged.
  It survives final-task cleanup and restart until explicit `execution_stop`.
  Use the newly returned `execution.execution_id` for runtime reads and stop;
  `resumed_from` identifies the source attempt. Call MCP on the running
  controller that owns the task. Inspect its new pane directly in Herdr.
  The owning task and original enabled profile must still exist; the profile
  must select the same agent kind and belong to an environment still assigned
  to the project. Resume verifies and reuses the original prepared deployment
  and saved args/env; the native CLI loads configuration and skills from that home.
  The pinned deployment, native conversation history and workspace must exist
  on the selected endpoint before a fresh pane is allocated. Missing history
  requires restoration; syncing configuration alone cannot recreate a session.
  Stop a current live execution before resuming another. To repeat a task in
  a new session, move it to `Planned`, then use `task_start` with its `run_spec`
  or `start_coding_session` with explicit choices. Old report snapshots and
  native IDs remain in history. After task/history pruning, use native resume.
- Worker executions report findings, evidence, blockers, and proposed changes.
  The operator records accepted task mutations unless authority is explicitly
  delegated outside this skill.

## Default Workflow

1. Create or select an existing task, then launch its worker:
   `start_coding_session(task_id, endpoint_id, launch_profile_id,
   workspace_path, bypass_permissions, role, kind, skills, template)`.
   A valid `task_id` and explicit runtime choices are mandatory. `skills`
   defaults to an empty list. `template` selects the initial prompt and group;
   otherwise the stored run template or `task` applies. The tool launches
   through Herdr, records the durable execution on the task, waits within its
   startup bounds, and submits
   the rendered task prompt. It returns the `execution_id` after submission,
   without waiting for the agent to finish the task. A profile and directory
   alone are rejected. `exec` cannot create executions and only operates inside
   an existing execution's pane.
2. Supervise the submitted task:
   `wait_start` with `kind = "coding_ready"` on the returned `execution_id`,
   then `wait_status`; for quick checks use `check_state` or `capture_output`.
   A `coding_ready` wait completes when Herdr reports the agent `idle` or `done`.
   Herdr uses `done` for an unseen completion; both states accept input.
3. Read the result. The launch already submitted the task; send another
   `coding_task_send` only for an intentional new assignment or handoff. Use
   `coding_send` for follow-up steering after the agent is ready.
4. Wait, read, and steer:
   `wait_start`, `wait_status`, `wait_cancel`, `coding_read`,
   `coding_action`, `capture_output`.
   Do not use blocking readiness calls for long-running agent work unless the
   user explicitly wants a synchronous call.
5. Verify with targeted commands or ask the worker for a short verification
   report.
6. Record the accepted result with `task_status_update`; final task states close
   the worker pane unless an audit holds it. Use `execution_stop` for disposable
   workers and inspection resumes. Keep intentionally unfinished workers alive
   when the user requests that.

## Orchestration Workflow

Use this flow when coordinating tasks through the orchestration tools:

1. Discover the current MCP surface with `tools/list` or equivalent client
   discovery, then call `list_endpoints` and endpoint-scoped `list_launch_profiles`.
2. Inspect current orchestration state with `orchestration_status`; use it for
   projects, plans, task graph, task outcomes, blockers, execution summaries,
   cleanup candidates, warnings, and runtime states instead of scraping
   terminal output. Use `task_get` for one full stored task
   body when exporting or inspecting exact task fields, and `plan_get` for one
   full stored plan brief plus optional plan-wide instructions.
3. Create or select a project with CLI
   `taskr create-project <title> --description <text>` for offline setup or MCP
   `project_create`/`project_list` through a running controller. Projects have
   required descriptions. MCP `project_create`, `project_update`, and
   `project_status_update` are advertised in the catalog but reject calls unless
   the controller starts with `--enable-admin-tools`; `project_list` is always
   available.
4. Create a plan with `plan_create`: required `project_id`, `title`, and
   Markdown `brief` with enough context and detail to derive tasks. Optional
   Markdown `instructions` are rendered into every task, validation, review,
   and quality-guard prompt for that plan; use `plan_update` to change them,
   or set `instructions` to an empty string to clear them. Create tasks with
   `task_create`: required `plan_id`, `title`, `objective`, optional scope
   (`include_paths`, `exclude_paths`, `notes`), optional `gates`, optional
   `slug`, `auto_schedule`, and optional scheduler `run_spec`. The response is
   the created task object directly; read `id` from the top level.
   Created tasks start in `Backlog`; move them to `Planned` only when
   dependencies and scope are ready.
   A scheduler `run_spec` contains `endpoint_id`, `launch_profile_id`,
   `workspace_path`, boolean `bypass_permissions`, `role`, `kind`, `skills`,
   prompt `template`, and `instruction`. `run_spec` describes how a task can
   be started; top-level `auto_schedule` separately controls whether explicit
   `orchestration_next` runs may start it automatically.
5. Correct mutable task metadata with `task_update`; pass `run_spec = null` to
   clear scheduler launch intent.
   Scope paths define task work boundaries, not runtime placement. Relative
   `include_paths` and `exclude_paths` are interpreted from the execution
   workspace. Prefer scope paths inside the workspace unless the operator
   intentionally scopes external files.
6. Maintain dependency edges with `task_edge_add` and `task_edge_remove`.
7. Launch workers with `start_coding_session`. Provide explicit `endpoint_id`,
   `launch_profile_id`, `workspace_path`, boolean `bypass_permissions`,
   `task_id`, `role`, `kind`, optional `skills`, and initial `template`.
   Treat `workspace_path` as launch placement, not task scope.
   Only a terminal execution (`Exited`, `Stopped`, or `Failed`) can be replaced.
   Stop a live worker before switching it; unavailable/unresolved bindings need
   inspection and reconciliation first. Previous attempts remain in history.
8. Adopt an already-running pane/agent with `execution_adopt` (requires
   `task_id` and `endpoint_id`, plus `pane_id` or `agent_name` when the
   endpoint hosts several agents). Adoption records observed provenance
   instead of launching anything.
9. Inspect lifecycle with `list_executions(project_id)` and
   `check_state(execution_id)`. Phases are `Pending`, `Allocating`,
   `Starting`, `Live`, `Unavailable`, `Exited`, `Stopped`, and `Failed`.
   Recovery states are `NeedsReconciliation`, `Reconciled`, `Abandoned`, and
   `OccupantMismatch`. Restart reconciliation observes existing executions
   without relaunching them; unavailable endpoints retain pending bindings.
   Inspect unresolved ownership before adopting or stopping an execution.
10. Use `coding_task_send` for task-aware delegation or a handoff after the
    previous assignment finishes. Launch already sends the initial task prompt.
    Pass `task_id_or_slug`
    and a concrete instruction; taskr builds deterministic task context from
    orchestration state, includes plan-wide instructions when configured, and
    appends your instruction before sending. Use
    `template = "task"` for implementation/delegation, `template = "validate"`
    for gate validation, `template = "review"` for bug/risk review, and
    `template = "quality-guard"` for maintainability, architecture fit, naming,
    boundaries, lifecycle, API shape, and operator/project quality preferences.
    For validation or review of a task set, pass `context_task_ids` with every
    task id/slug whose result is in scope. This is the required operator-side
    task-card export path: taskr renders each card with status, gates,
    outcome/evidence, scope, blockers, and edges. Do
    not rely on the validator's primary task context, local artifacts alone, or
    worker-side taskr calls to recover prior task results.
    Use `coding_send` only for follow-up prompts, steering, or corrections.
11. Use `task_start` to explicitly start one task from its `run_spec`.
    `task_start` does not require `auto_schedule = true`.
12. Use `orchestration_report` when the operator or external MCP controller
    needs a read-only classification of ready/skipped/error tasks for automatic
    orchestration. It accepts optional `project_id` and `plan_id` filters and
    never starts executions. Use `orchestration_next` for scheduler-driven
    startup after the operator or external MCP controller records
    worker/validator results for a plan. It requires `plan_id`, which accepts a
    plan id or unique plan slug, and executes by default; pass `dry_run = true`
    to preview. There is no background scheduler loop; the operator or
    external MCP controller observes `orchestration_status`, commits
    worker/validator results, optionally calls `orchestration_report`, then
    calls `orchestration_next` to advance work. Scheduler startup is
    conservative: it requires top-level `auto_schedule = true`, a `run_spec`,
    `Backlog` or `Planned` status, ready `DependsOn` dependencies and required
    validations, an enabled launch profile, and no live execution.
    Scheduler startup/readiness/prompt failures are visible in
    `orchestration_status` as `Blocked` tasks with blockers; worker-reported
    failures still need `task_status_update`. taskr does not infer stuck running
    tasks from silence without a separate timeout or heartbeat policy.
    `Task.gates` are the acceptance checklist. A `Validates` edge from a
    validator task to a target task is operational: downstream tasks that
    `DependsOn` the target cannot start until every linked validator is
    `Passed` or `Delivered`. If validation cannot run, mark the validator task
    `Blocked`; if validation rejects the target, mark the validator task
    `Failed` with outcome and evidence. `orchestration_status` exposes
    `validation_blocked_by`, `unapproved_validator_count`, and
    `failed_validator_count`.
    Supported task edges in v1 are `DependsOn`, `ParentOf`, `Validates`,
    `Audits`, `Supersedes`, and `Related`. `DependsOn`, `ParentOf`,
    `Validates`, and `Supersedes` have orchestration semantics. `Audits` is
    non-gating review traceability; use `Validates` when an audit must approve
    gates before dependent work can start. `Supersedes` marks the `to` task as
    replaced by the `from` task, so the replaced task is skipped by scheduling
    and explicit starts. `Related` is durable traceability/navigation only.
13. Update task state with `task_status_update`. Include `status` and a concise
    `outcome`; include `blockers` when blocked and `evidence` when committing
    results. For gated moves to `Passed` or `Delivered`, include evidence in
    the outcome.

Task-owned `gates` are validation checks. Moving a gated task to `Passed` or
`Delivered` requires an operator-recorded outcome. `Validates` edges give those
checks orchestration meaning for downstream scheduling.

Plan-wide `instructions` are durable guidance shared by every task in the plan.
Use them for recurring constraints such as reporting style, scope discipline, or
project-specific operating rules. Do not use them as substitutes for task-owned
`gates`; gates remain the concrete acceptance checks for a specific task.

`coding_task_send` templates answer different orchestration questions:

- `template = "task"` answers: "What work should this agent perform for this
  task?" Use it for the initial implementation/delegation prompt. Put concrete
  execution instructions in `prompt`: scope, constraints, expected report, and
  what not to mutate.
- `template = "validate"` answers: "Does the result satisfy the task gates and
  objective?" Use it for validator executions. Put the validation focus in
  `prompt`: which gates, evidence, commands, files, or reports to inspect. For
  multi-task validation, include `context_task_ids` and require a
  `field_coverage_table`; if a task card, gate, or outcome/evidence is missing,
  the worker must report the result as inconclusive.
- `template = "review"` answers: "Are there correctness risks, regressions,
  missing tests, contract breaks, or scope drift?" Use it for reviewer/auditor
  executions. Put the changed files, suspected risks, or review angle in
  `prompt`.
- `template = "quality-guard"` answers: "Does the change conform to the
  project/operator quality bar?" Use it for maintainability and design-quality
  checks. Put operator-specific guard points in `prompt`, such as architecture
  boundaries, naming preferences, lifecycle clarity, API shape,
  canonical-only policy, or abstraction discipline.

Templates provide the operating mode and task context. The `prompt` provides
the specific assignment. Do not rely on a task id alone; include the concrete
focus the worker should act on.

For validation, prefer `coding_task_send` with `template = "validate"`, for
example:

```text
Validate this task against its gates. Report pass/fail findings, evidence
references, blockers, unresolved questions, and recommended status. Do not
mutate taskr task state directly.
```

For validation that spans prior tasks, the operator must pass the prior ids in
`context_task_ids`, for example `["task-71", "task-72", "task-73"]`, and the
instruction must tell the validator to check every card field. A separate
export file is acceptable only when the prompt names the path and carries the
same checklist. Never tell a worker execution to query taskr for missing task
cards; missing cards are a prompt/context defect.

Do not send empty prompts or placeholder prompt text such as `null` or
`undefined`; these indicate a bad extraction/parsing path.

Use cancellable runtime wait jobs as the canonical orchestration wait API:
`wait_start`, `wait_status`, and `wait_cancel`, each targeting one
`execution_id`. Supported wait kinds are `stable`, `sentinel`, and
`coding_ready`; `coding_ready` completes when Herdr reports the agent `idle` or `done`,
`sentinel` requires `sentinel` text, and `stable` requires output to stop
changing for the stability window. If a wait job remains pending, inspect with
`check_state` and `coding_read`; cancel the wait job with `wait_cancel`
before interrupting the agent.

`coding_read` returns source-selected terminal output: `visible` (default
viewport), `recent`, `recent_unwrapped`, or `detection`. Use `capture_output`
for the raw pane and pass `lines` to bound either tool.

Use `orchestration_prune` when intentionally removing aged retained terminals,
stale execution records, or finished plans. Start with the default dry-run
behavior; inspect warnings for reused or unobservable layouts.

## Delegation Prompts

Good prompts are short and bounded:

```text
Inspect this repo for where <feature> is implemented. Do not edit files.
Return: 3-6 bullet findings, exact files/functions, and one recommended next step.
Do not print secrets or full file contents.
```

```text
Implement <change>. Keep edits minimal and aligned with existing patterns.
Run the narrowest relevant tests. Report changed files, tests run, and blockers.
Do not touch unrelated worktree changes.
```

## Orchestration Role Prompt Examples

These are copy/edit examples for the `prompt` field of `coding_task_send` or
follow-up `coding_send`; they are not MCP prompts and not controller runtime
features. With `coding_task_send`, taskr supplies the task context. Worker
executions should propose and report; the operator records accepted task
mutations.

```text
Role: planner
Task: <task_id> - <title>
Objective: <objective>
Context: <scope, dependencies, gates, blockers, execution hints>
Execution: <endpoint_id/launch_profile_id/workspace_path/role/kind/skills>
Create a concise execution plan. Report: planning_outcome, proposed_plan,
proposed_tasks, dependencies, gates, blockers, unresolved_questions.
Do not edit files or mutate taskr task state.
```

```text
Role: task-manager
Task: <task_id> - <title>
Current state: <status, executions, dependencies, blockers, gates>
Execution: <endpoint_id/launch_profile_id/workspace_path/role/kind/skills>
Decide the next orchestration action. Report: decision_outcome,
proposed_status_changes, proposed_tasks, needs_planner, needs_task_writer,
needs_validator, needs_auditor, blocked_on, unresolved_questions.
Do not call taskr task tools unless explicitly instructed.
```

```text
Role: task-writer
Task goal: <goal or capability>
Known context: <facts, scope paths, dependencies, constraints>
Launch hints: <endpoint_id/launch_profile_id/workspace_path if already chosen>
Draft concrete orchestration tasks with clear objectives, allowed scope paths,
dependencies, and validation gates. Scope paths are task work boundaries;
workspace_path is launch placement. Report:
task_writing_outcome, proposed_tasks, scope_paths, gates, dependencies,
blockers, unresolved_questions.
```

```text
Role: editable-worker
Task: <task_id> - <title>
Objective: <objective>
Task outcome and current status: <outcome/status/blockers>
Scope: include <paths>; exclude <paths>; relative paths are from the execution workspace
Runtime placement: workspace_path <workspace_path>
Dependencies and gates: <dependency status; gate list>
Execution: <endpoint_id/launch_profile_id/workspace_path/role/kind/skills>
Implement only this task. Run focused verification. Report: implementation_outcome,
changed_files, tests_run, blockers, unresolved_questions, proposed_tasks,
needs_planner, needs_task_writer, needs_validator, needs_auditor, blocked_on.
```

```text
Role: validator
Task context:
- <task_id>: <title>; status <status>; outcome <outcome>; blockers <blockers>
Changed-file groups: <paths grouped by purpose>
Gates to validate: <gate list with expected outcome>
Evidence to inspect: <tests, files, execution reports copied or summarized here>
Review the current implementation against each gate. Report: verdict
pass|fail, gate_results, findings with severity and evidence reference,
changed_files reviewed, blockers, unresolved_questions.
Do not edit files.
```

```text
Role: auditor
Task/execution context:
- <task_id/execution_id>: <title/objective>; status <status>; outcome <outcome>
Scope, changed-file groups, dependencies, and gates: <copied compact details>
Audit for correctness, safety boundaries, missing evidence, and cleanup risk.
Report: verdict pass|fail, gate_results if gates exist, findings with severity
and evidence reference, changed_files reviewed, blockers, unresolved_questions.
Do not edit files.
```

```text
Role: extractor
Source context: <files, logs, execution output, task graph>
Question and scope: <question, include/exclude paths, dependencies>
Extract only facts relevant to <question>. Report: extraction_outcome, extracted_facts,
evidence_refs, proposed_tasks, blockers, unresolved_questions.
Do not infer beyond the evidence.
```

```text
Role: reviewer
Change under review: <files/commit/execution/task with copied summaries>
Changed-file groups: <paths grouped by purpose>
Gates and expected behavior: <gate list, acceptance criteria, expected outcomes>
Review for regressions, contract breaks, stale docs, and missing tests. Report:
verdict pass|fail, findings with severity and evidence reference,
changed_files reviewed, blockers, unresolved_questions.
Do not edit files.
```

```text
Role: quality-guard
Task/change context: <task, files, summaries, operator preferences>
Guard points: <project/operator-specific quality concerns>
Check maintainability, architecture fit, naming, boundaries, lifecycle, state
ownership, API coherence, and project conventions. Report: overall
recommendation proceed|revise|escalate, relevant built-in heuristic concerns,
operator guard point results, evidence refs, recommended corrections, blockers.
Do not edit files.
```

## Context Budget Discipline

Use direct file reads only for:

- checking exact code before making a patch;
- validating a worker report;
- inspecting small config files;
- reading test failures or short diffs.

Use worker executions for:

- repo exploration;
- tracing call graphs;
- broad search and summarization;
- implementation attempts that may take many tool calls;
- comparing alternatives before this agent patches.

Ask the worker to return concise summaries, not copied files.

## References

For exact MCP request examples and recovery commands, read
`references/mcp-recipes.md`.
