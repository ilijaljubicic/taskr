# Install and connect Taskr

## Prerequisites

| Dependency | Required for | Notes |
| ---------- | ------------ | ----- |
| Node.js and npm | `npx @mmux/taskr` quick start | The npm package extracts and runs the bundled native `taskr` binary for the current platform. |
| Rust and Cargo | Build, test, run | Install with rustup or your system package manager. |
| Herdr binary | All terminal/agent execution | taskr shells out to Herdr for every terminal operation (`--herdr-bin`, default `herdr` on `PATH`). Herdr owns its own terminal dependencies. Install: `curl -fsSL https://herdr.dev/install.sh \| sh`. |

Provisioning and worker lifecycle are
documented in the [deployment guide](deployment.md).

## Setup

### Run from npm

The package is published on npm as [@mmux/taskr](https://www.npmjs.com/package/@mmux/taskr).

For a local loopback-only MCP server driving the local Herdr session without
bearer authentication:

```bash
npx --yes @mmux/taskr controller --allow-remote-without-mcp-token
```

The MCP endpoint is:

```text
http://127.0.0.1:3000/mcp
```

Register that HTTP MCP server with codex:

```bash
codex mcp add taskr --url http://127.0.0.1:3000/mcp
```

Register it with claude code:

```bash
claude mcp add --transport http taskr http://127.0.0.1:3000/mcp
```

For authenticated local setup, start taskr with an MCP bearer token and register
the same token with each MCP client:

```bash
export TASKR_MCP_TOKEN="$(openssl rand -hex 32)"
npx --yes @mmux/taskr controller --mcp-token-env TASKR_MCP_TOKEN
```

In another shell with `TASKR_MCP_TOKEN` set, register codex:

```bash
codex mcp add taskr \
  --url http://127.0.0.1:3000/mcp \
  --bearer-token-env-var TASKR_MCP_TOKEN
```

Register claude code by adding the bearer header:

```bash
claude mcp add --transport http taskr http://127.0.0.1:3000/mcp \
  --header "Authorization: Bearer $TASKR_MCP_TOKEN"
```

The controller needs at least one launch profile before it can start agents.
Use [environment setup](environments.md) to discover and sync a native environment
through the admin MCP tools, then select one prepared launch choice.

### Install native binary

Install the latest released `taskr` binary:

```bash
curl -fsSL https://raw.githubusercontent.com/ilijaljubicic/taskr/main/scripts/install.sh | bash
```

Pin a specific release:

```bash
curl -fsSL https://raw.githubusercontent.com/ilijaljubicic/taskr/main/scripts/install.sh | VERSION=vX.Y.Z bash
```

Then run a local controller:

```bash
taskr controller
```

### Local MCP access

Default local runtime paths:

```text
store path:  ~/.taskr
MCP path:    http://127.0.0.1:3000/mcp
```

With `--store-path <path>`, taskr uses that path for durable state. Pass the
same path to store-backed CLI commands:

```bash
taskr controller --store-path /tmp/taskr-dev
taskr --store-path /tmp/taskr-dev create-project "Release hardening" --description "..." --slug release-hardening
taskr --store-path /tmp/taskr-dev list-projects
taskr --store-path /tmp/taskr-dev prune --dry-run
```

If you are calling the MCP endpoint directly, include both accepted response
types:

```bash
curl -X POST "http://<controller-host>:3000/mcp" \
  -H "Accept: application/json, text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

Raw MCP clients must check for JSON-RPC `error` and tool-level `isError`
before parsing a response as a successful tool result. Tool failures are clear
but still returned in the MCP response envelope.

## Health check

```bash
curl "http://<controller-host>:3000/health"
```

The response body is:

```text
ok
```

For command options, see the [CLI reference](cli.md). For building from source, see the [developer guide](development.md).
