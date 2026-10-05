# Taskr CLI reference

| Command | Purpose |
| ------- | ------- |
| `taskr controller` | Runs the MCP control plane. Every terminal operation is delegated to Herdr. |
| `taskr create-project <title> --description <text>` | Creates a durable orchestration project in the local taskr store. Supports optional `--slug <slug>`. Agent environments are selected through endpoint launch profiles when starting tasks. |
| `taskr delete-project <id-or-slug>` | Deletes a durable orchestration project from the local taskr store, including all contained plans, task cards, and task edges. |
| `taskr list-projects` | Lists durable orchestration projects from the local taskr store so project ids/slugs are discoverable. |
| `taskr prune` | Removes old retained worker terminals, stale execution records, and finished plans after observing Herdr endpoints. Defaults to dry-run, all categories included, and `--older-than-days 14`; pass `--execute` to apply cleanup. |

Important controller flags:

| Flag | Default | Purpose |
| ---- | ------- | ------- |
| `--host` | `127.0.0.1` | Bind host for the MCP HTTP server. |
| `--port` | `3000` | Bind port. |
| `--mcp-token` | none | Explicit bearer token for MCP requests; otherwise read the selected token file or environment variable. |
| `--mcp-token-file` | none | Reads the MCP bearer token from a file. Prefer `/run/secrets` paths in containers. |
| `--mcp-token-env` | `TASKR_MCP_TOKEN` | Env var used when MCP token flags are omitted. |
| `--allow-remote-without-mcp-token` | false | Disables bearer auth, including the token environment fallback; conflicts with `--mcp-token` and `--mcp-token-file`. |
| `--store-path` | `~/.taskr` | Directory for durable state (`taskr.db`). |
| `--enable-admin-tools` | false | Enables project administration, environment discovery/sync, endpoint migration, and endpoint-agent debugging. |
| `--herdr-bin` | `herdr` | Herdr executable used for every terminal operation. |
| `--herdr-session` | none | Explicit local Herdr session selection. Never affects saved-machine endpoints. |
| `--environment-python-bin` | `python3` | Python 3.11+ executable for the local environment companion. |
| `--environment-ssh-bin` | `ssh` | OpenSSH client for environment sync to saved Herdr machines. |
| `--max-timeout-seconds` | `120` | Maximum wait timeout accepted by wait tools. |
| `--max-request-bytes` | `2097152` | Maximum MCP HTTP request body size. |
| `--max-capture-bytes` | `2097152` | Maximum bytes returned by terminal capture tools. |

Prune flags:

| Flag | Default | Purpose |
| ---- | ------- | ------- |
| `--dry-run` | default | Preview what would be pruned without mutating state. |
| `--execute` | false | Apply the prune. |
| `--older-than-days` | `14` | Age cutoff for stale execution records and finished plans. |
| `--include-stale-execution-records` | false | Scope the run to stale durable execution records only. |
| `--include-finished-plans` | false | Scope the run to finished plans only. |
| `--herdr-bin` | `herdr` | Herdr executable used to observe live endpoints. |
| `--herdr-session` | none | Explicit local Herdr session selection. |

Agent environments are selected through prepared endpoint launch profiles. See [environment setup](environments.md) and [MCP tools](mcp.md).
