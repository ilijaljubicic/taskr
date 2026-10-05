# MCP access and credentials

taskr is an agent controller with controller-host file access and the ability
to launch agents in arbitrary workspace paths. Giving a client access to a
writable taskr server is equivalent to giving that client broad control of the
controller host and of every configured Herdr endpoint. Use trusted clients
only.

The default bind host is loopback-only. A non-loopback MCP bind without a token
is rejected unless you deliberately pass `--allow-remote-without-mcp-token`.
That flag also ignores the default `TASKR_MCP_TOKEN` env fallback and is
mutually exclusive with explicit MCP token flags/files.

Authenticated requests must include:

```text
Authorization: Bearer <token>
```

The `x-mcp-token` header is accepted as an alternative. Cross-site browser
requests (`Sec-Fetch-Site: cross-site`) are rejected to blunt DNS-rebinding
and drive-by POSTs.

Controller credentials stay out of worker environments. Prepared launch profiles
supply configuration; native credentials are provisioned on the endpoint or
transferred through an explicitly selected environment-sync credential policy.

TASKR filesystem access and file transfer are deferred. The `read_file` and
`save_file` MCP tools have been removed; calls to those names fail as unknown
tools. Their `--max-read-bytes` and `--max-write-bytes` flags are also removed
and rejected. Coding agents use their native file tools on the execution
endpoint; deployment tooling provisions repositories, configuration, and skills
there. Terminal output reads and prompt submission remain available.

Request and output limits are configurable with `--max-timeout-seconds`,
`--max-request-bytes`, and
`--max-capture-bytes`.

See [installation](installation.md) for authentication setup and [agent environments](environments.md) for credential policies.
