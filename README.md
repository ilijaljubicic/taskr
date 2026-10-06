# taskr — durable agent orchestration over MCP

Taskr is the evolution of MMUX. For the original tmux and sandbox functionality,
use the [MMUX release](https://www.npmjs.com/package/@mmux/mmux).

Taskr coordinates coding agents through a durable **project → plan → task** model.
It tracks what needs doing, which work can start, and what evidence supports the
result. [Herdr](https://herdr.dev/docs/) runs the agents and provides their terminals;
Taskr exposes orchestration and agent control through MCP.

## Capabilities

- Organize work into projects, plans, and tasks with objectives, scope, shared instructions, and acceptance gates.
- Coordinate dependencies, validation tasks, and audits; inspect blockers before starting downstream work.
- Start individual tasks or schedule ready tasks through explicit MCP scheduling passes.
- Prepare Codex and Claude environments from installed homes, folders, or ZIPs, then select profiles on local or remote Herdr endpoints.
- Record outcomes, evidence, blockers, and execution history across controller restarts.
- Close finished workers, retain successful workers for pending audits, and reopen saved conversations for inspection.

## Projects, plans, and tasks

| Element | What it represents | What it contains |
| --- | --- | --- |
| **Project** | A lasting boundary for a repository, product, or body of work. | Description, status, and plans. |
| **Plan** | A goal or change within a project. | A Markdown brief, shared instructions, and tasks. |
| **Task** | A focused piece of work or a validation/review activity. | Objective, scope, gates, dependencies, results, and optional launch settings. |
| **Execution** | One agent attempt for a task. | Endpoint, launch profile, working directory, agent session, and saved report. |

Each plan belongs to a project, and each task belongs to a plan. A task keeps its
previous execution attempts and their reports. Every agent launch belongs to a task.

Dependencies determine work order. Validation tasks can gate downstream work;
audits provide review traceability and can keep a successful worker available
for inspection. Operators record outcomes and advance scheduling through MCP.

## How profiles map to work

Assign agent environments to a project, then select a profile from those
environments for each task execution. Different tasks can use different agents,
models, reasoning settings, machines, and working directories. Environments can
be shared by multiple projects.

Each launch selects three things:

| Selection | Purpose |
| --- | --- |
| **Herdr endpoint** | The local session or a saved remote machine where the agent runs. |
| **Taskr launch profile** | A prepared agent environment on that endpoint, including native configuration, profile settings, and skills. |
| **Workspace path** | The repository or worktree directory on that endpoint. |

A Herdr machine profile selects a remote machine. A Taskr launch profile selects
the agent environment on that machine. Task scope describes the work boundaries;
the workspace path selects the starting directory.

For example, the **Shop** project might contain a **Fix checkout** plan:

| Task | Launch profile | Placement |
| --- | --- | --- |
| Implement the fix | Codex work environment | Local endpoint, `/repos/shop` |
| Validate the fix | Claude review environment | Remote endpoint, `/work/shop` |

The tasks share a plan and project while using different environments. Save
launch choices on a task when it should be ready for later scheduling.

Tasks can carry model and reasoning hints for the orchestrator. Supported choices
become concrete launch options; executions save the selected settings for resume.
See [environment and profile selection](docs/profiles.md) for the MCP workflow.

## Work in Herdr

Taskr groups executions into one Herdr space per plan and endpoint. Work,
Validation, Review, and Quality tabs group agents by their initial prompt mode;
each execution gets a fresh pane.

Task completion triggers agent exit and pane closure, with audit holds for
successful workers. Saved conversations remain available for MCP resume;
inspection sessions stay open until explicitly stopped. Use Herdr to view and
navigate terminals.

## MCP and skills

Use the [Taskr operator skill](.codex/skills/taskr-operator/SKILL.md)
([Claude copy](.claude/skills/taskr-operator/SKILL.md)) to create projects and plans,
derive tasks, select profiles, supervise workers, and record results.
The [MCP reference](docs/mcp.md) lists orchestration, environment setup,
execution, and interaction tools; `tools/list` provides their current schemas.

Taskr is published on npm as [@mmux/taskr](https://www.npmjs.com/package/@mmux/taskr).

## Documentation

| Guide | Contents |
| --- | --- |
| [Install and connect](docs/installation.md) | Installation, MCP client setup, authentication, and health checks. |
| [CLI reference](docs/cli.md) | Controller options and retained project/prune commands. |
| [Agent environments](docs/environments.md) | Discovery, sync, launch profiles, configuration, and credentials. |
| [Folder and ZIP imports](docs/environment-imports.md) | Collection layouts and optional manifests. |
| [Orchestration](docs/orchestration.md) | Dependencies, validation gates, task states, and scheduling. |
| [Executions](docs/executions.md) | Worker lifecycle, saved attempts, and inspection resume. |
| [Deployment](docs/deployment.md) | Herdr endpoints and the basic operating model. |
| [MCP reference](docs/mcp.md) | Tools, resources, and prompts. |
| [Access and credentials](docs/security.md) | MCP authentication and credential boundaries. |
| [Development](docs/development.md) | Building, testing, and architecture. |

## Project status

Taskr is in early development. Interfaces, configuration, and runtime behavior
may change in breaking ways.

## License

Apache-2.0 — see [LICENSE](LICENSE).
