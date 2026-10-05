# Developing Taskr

From a repo checkout, the development command is:

```bash
make run-local
```

## Build and test

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

See the [developer skill](../.codex/skills/taskr-developer/SKILL.md), [core library contracts](../crates/taskr-core/README.md), and [execution interfaces](execution-ports.md) for implementation guidance.
