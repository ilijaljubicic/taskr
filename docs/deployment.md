# Deploying taskr with Herdr

taskr owns durable orchestration (projects, plans, tasks, executions, gates)
and the MCP control plane. Herdr owns every terminal: it renders, spawns,
and supervises the coding agents on the local machine or on remote endpoints.

The default store is `~/.taskr/taskr.db`; `--store-path <directory>` selects
another store directory.

## Herdr endpoints

Configure local sessions and remote machines using the
[Herdr documentation](https://herdr.dev/docs/). Taskr uses the local Herdr
session or Herdr's saved machine profiles as execution endpoints. Containers
and microVMs are provisioned outside Taskr and expose the same endpoint interface.

### Environment setup

Taskr discovers Codex and Claude environments from installed homes, folders,
or ZIP files and syncs managed copies to a selected Herdr endpoint. Prepared
launch profiles supply the agent configuration and skills used for task execution.

Start the controller with `--enable-admin-tools` for setup. Use the admin MCP
tools to discover and sync an environment with an explicit credential policy,
then select a ready profile through `list_launch_profiles` when starting tasks.

For prerequisites, tool arguments, and setup recipes, see the
[MCP tools](mcp.md) and
[taskr-operator skill](../.codex/skills/taskr-operator/SKILL.md).

## Worker lifecycle

- Each plan has a space per endpoint, with Work, Validation, Review, and Quality tabs.
- Final task states trigger agent exit and pane closure; unfinished audits can retain successful workers.
- Saved task conversations can be resumed through MCP. Inspection sessions remain open until explicitly stopped.
- Pruning removes aged orchestration metadata while preserving native conversations.

Use the [MCP tools](mcp.md) and
[taskr-operator skill](../.codex/skills/taskr-operator/SKILL.md) for execution
commands, and [Herdr docs](https://herdr.dev/docs/) for terminal navigation.

## CI

CI installs the real Herdr CLI (same `install.sh`) as a provisioning guard.
The Rust test suite itself is hermetic: every herdr invocation in tests goes
through a per-test fake CLI fixture with canned responses, so no Herdr
server (and no tmux) is needed to run `cargo test --workspace`.

Portable transport, endpoint lifetime, generation recovery and container host
contracts are described in [execution ports](execution-ports.md).
