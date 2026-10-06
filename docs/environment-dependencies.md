# Declaring environment dependencies

Taskr follows native configuration file references and static command arguments.
It does not evaluate shell code or run credential helpers during setup. Put
`taskr-dependencies.json` in a Codex or Claude home to declare dependencies hidden
inside scripts or inline commands. This works for installed homes, folders and
ZIP imports. It is separate from a collection's `taskr-environments.json`.

```json
{
  "version": 1,
  "commands": [
    {
      "config": "config.toml",
      "pointer": "/model_providers/ZAI/auth",
      "files": [
        {"path": "credentials/zai-api-key", "credential": true}
      ],
      "environment": ["PROVIDER_ACCOUNT"]
    }
  ]
}
```

`config` names a file relative to the native home. `pointer` is a JSON Pointer
to the command object or command value in that file; escape `~` as `~0` and `/`
as `~1` in a field name. For example, `/mcp_servers/worker` selects a Codex MCP
server; `/notify` selects its notification command; `/apiKeyHelper` selects a
Claude credential helper; `/hooks/SessionStart/0/hooks/0` selects a command hook.
The pointer must match configuration. Unknown fields and duplicate pointers fail
discovery. Native profile declarations use that profile's `name.config.toml`.
Inherited base commands keep their base declarations.

`files` lists indirect dependencies. Paths resolve against an explicit command
`cwd`, or the declaring configuration directory when no cwd is supplied. Absolute
and `~/` paths also work. Mark private key/token data `credential: true`; omitted
or false means configuration/script content. Command executables and static file
arguments are still followed automatically. Explicit declarations also allow
relative command arguments whose source location would otherwise be ambiguous.
Only declared files are packaged from an external cwd. Do not assume that a
script's imported modules, subprocesses or undeclared file reads are discovered.
Provision interpreter packages and additional programs separately on the endpoint.

`environment` lists required variable names; it never contains their values.
Variables supplied by the native command's own `env` do not become endpoint
requirements. Do not put secret values into a dependency manifest. For opaque
commands with no indirect dependencies, explicitly use empty `files` and
`environment` lists. This declaration is an operator assertion of completeness.

A direct provider helper such as `command = "/usr/bin/cat"` with an absolute key
file in `args` needs no manifest: Taskr identifies that file as credential data.
Other provider helpers and Claude `apiKeyHelper` require declarations because
their file/environment dependencies cannot be inferred from their body.

With `credential_policy="copy"`, declared credential bytes are transferred
privately through stdin and included in the deployment digest. With `endpoint`,
source credential bytes are excluded and the corresponding relative paths must
exist beneath `endpoint_auth_home`; source credential files may be absent.
Discovery reports these managed relative paths in `dependencies`. Refresh
rotates credentials into a new home without altering previous conversations.

Imports must keep configuration/script dependencies inside the collection,
including declared files and symlink targets. Original-machine path aliases use
the [collection manifest](environment-imports.md). Native conversation history
is not an environment dependency and is never cloned by sync.

## Claude project-local MCP scope

Claude's `.claude.json` mixes user MCP definitions, project-local definitions,
sign-in and trust metadata. Taskr extracts only MCP configuration. Add explicit
project mappings to preserve local scope on the endpoint:

```json
{
  "version": 1,
  "claude_projects": {
    "/home/alice/repos/app": "/srv/work/app"
  }
}
```

Every project with local MCP configuration needs a mapping, even for local sync
where both paths are the same. Distinct source projects cannot map into one
destination scope. The repository/workspace itself must exist separately on the
endpoint; Taskr does not transfer it or widen project definitions into user scope.
Project `.mcp.json` files remain repository-owned.

## Claude plugin dependencies

Configured installed plugins use the same dependency inspection for hooks, MCP
servers, and LSP commands. A plugin can carry its own `taskr-dependencies.json`
with `version: 1` and `commands`. Its `config` paths are relative to the plugin
root; file paths retain the command cwd/configuration-directory rules above.
For example, a declaration for `hooks/hooks.json` can use
`/hooks/SessionStart/0/hooks/0` to identify a command hook. A home-level manifest
can also declare commands by their original installed plugin configuration paths.

Native `${CLAUDE_PLUGIN_ROOT}` references are inspected against the installed
payload and remain native references in the prepared configuration.
`${CLAUDE_PLUGIN_DATA}`, `${CLAUDE_PROJECT_DIR}`, and native user-configuration
references describe runtime inputs; provision those inputs on the endpoint.
Plugin installers, hooks, and servers are never executed during sync.

The home's `claude_projects` mappings also apply to project/local plugin
installation records. Such installations retain their original scope. Valid
homes with missing plugin payloads or project mappings remain discoverable with
`sync_blockers`; repair the setup and rediscover before syncing. See
[Claude plugin snapshots](environments.md#claude-plugins) for registry and version
handling.

See [environment setup](environments.md) for discovery, sync, readiness and launch.
