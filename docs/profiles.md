# Environments, profiles, and task settings

A source **environment** is a Codex or Claude configuration home. Its stable
`source_environment_id` survives sync revisions. A prepared deployment is an
immutable copy on a Herdr endpoint. A **launch profile** selects that deployment
and a native profile within it; Codex supplies base and `*.config.toml` choices,
while the current Claude adapter supplies a base choice per home.

Assign environments to a project. Every task in its plans can use any prepared
profile from those environments. Assignment does not sync files, start agents,
or change existing task selections. Each task still chooses an endpoint,
`launch_profile_id`, and workspace explicitly.

## Discover and assign

Use admin `admin_environment_discover` and the [environment setup](environments.md)
workflow to discover and prepare environments. `list_environments({})` lists
registered source metadata, deployments, model options, and discovery issues.
Invalid sources are reported as issues. Valid Claude homes with setup blockers
remain discoverable and assignable, with `sync_blockers` describing the needed
repair; assignment does not make them ready to launch.

Assign discovered source IDs through admin MCP:

```json
{
  "project_id": "<project-id-or-slug>",
  "environment_ids": ["<codex-source-environment-id>", "<claude-source-environment-id>"]
}
```

Pass these arguments to `project_update`, or include `environment_ids` when
creating a project. Omitted assignments retain their value; `[]` clears them.
New projects and upgraded projects start with no assignments. Configure them
before starting work. Removing an assignment preserves running executions and
history, and prevents new launches and inspection resumes from that environment.

`list_environments({"project_id":"<project-id>"})` lists assigned environments.
Then request eligible profiles on the intended endpoint:

```json
{
  "project_id": "<project-id>",
  "endpoint_id": "local"
}
```

Pass these arguments to `list_launch_profiles`. The returned `id` is the concrete
`launch_profile_id`; `source_environment_id` identifies its environment. Default
listing shows the newest ready choice for each environment/native-profile pair.
Use `include_retained:true` to inspect earlier deployments. Earlier IDs remain
valid for saved task selections and resume when their environment is assigned.

## Task preferences and concrete choices

A user can say:

> Use the main Codex environment for this project. Run implementation tasks
> with its GLM profile and deep reasoning; use its regular Codex profile for
> validation. Use lower reasoning for task Y.

The operator resolves environment/profile names against the catalogs, assigns
the environment, and stores a separate `run_spec` for each task. A plan has no
profile-default field. A task-specific instruction takes precedence over a
broader plan preference.

Tasks can retain advisory preferences through `task_create` or `task_update`:

```json
{
  "task_id": "<task-id>",
  "launch_hints": {
    "model": "Prefer GLM 5.3",
    "reasoning_effort": "Use deep reasoning for the implementation"
  }
}
```

Hints guide the orchestrator; they are not native CLI overrides. Inspect the
selected profile's `settings.model_options`, which lists model names and their
advertised `reasoning_efforts`. Resolve preferences into concrete options, such as:

```json
{
  "model": "glm-5.3",
  "reasoning_effort": "max"
}
```

Place this object in the full `run_spec.launch_options`, or pass it as
`start_coding_session.launch_options`. Updating a `run_spec` replaces the whole
specification: preserve the other task fields. Omitted options keep the profile
defaults. An explicit launch-options object overrides the stored object for
that attempt; `{}` explicitly selects profile defaults.

Codex model options come from its configured model catalog or OpenAI model cache;
Claude exposes configured model/alias choices and effort levels advertised by
its native CLI. Imported environments use their supplied metadata and configuration.
These choices describe configuration and CLI support; the native agent/provider
checks actual model access. If a retained deployment has no option metadata,
refresh its environment before requesting overrides.

The controller rejects empty profile IDs, unassigned environments, and explicit
model/effort choices absent from the selected profile's option metadata before
allocating terminals. Codex launches use `--model` and a native reasoning config
override; Claude uses `--model` and `--effort`. Profiles, providers, credentials,
and skills stay in the selected environment. Executions record known effective
settings and preserve their native arguments for resume.

See the [operator skill](../.codex/skills/taskr-operator/SKILL.md) and
[MCP recipes](../.codex/skills/taskr-operator/references/mcp-recipes.md) for full
launch specifications and scheduling calls.
