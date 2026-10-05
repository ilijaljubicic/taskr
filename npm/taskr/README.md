# Taskr

Task orchestration over MCP, powered by [Herdr](https://herdr.dev/).
Coordinate coding agents with durable projects, plans, tasks, dependencies,
validation gates and resumable conversations.

This package exposes the `taskr` command by extracting and running its bundled
native binary. Releases support Linux x86_64, macOS arm64 and macOS x86_64.
Herdr and the selected coding CLI must be installed separately.

After the first complete Taskr npm release:

```bash
npx --yes @mmux/taskr controller
```

The controller serves MCP at `http://127.0.0.1:3000/mcp` by default. It stores
orchestration state in `~/.taskr/taskr.db`; Herdr owns terminal execution.

See the [repository](https://github.com/ilijaljubicic/taskr) for setup,
environment preparation and MCP tool documentation. Maintainers can find the
[npm publishing setup](https://github.com/ilijaljubicic/taskr#configure-npm-publishing)
in the main README.
