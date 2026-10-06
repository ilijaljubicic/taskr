# Agent environments and launch profiles

Launch choices come from prepared native environments stored in `taskr.db`.
No controller `profiles.json` is needed. Herdr's live agent list reports running
processes; it does not discover configuration homes or native profiles.

Start the controller with `--enable-admin-tools` for setup. Python 3.11+ must be
installed on both ends, and the native coding CLI on the selected endpoint.
Installed-home discovery also needs the controller's CLI; imports may declare
their source CLI version in the manifest.
Remote setup also needs OpenSSH and a saved, enabled Herdr machine. The embedded
companion runs over that machine's saved SSH target; no daemon or companion
installation is required. It prepares files; Herdr remains the agent executor.

1. Call `admin_environment_discover` with `{}` to search local `.codex*` and
   `.claude*` homes (including nested `.claude` directories), or pass
   `homes: ["/home/user/.codex-work"]`. To import a native home or collection,
   use `source_path: "/path/to/environments"` or
   `source_path: "/path/to/environments.zip"` instead. Read the returned environment IDs,
   revisions, native profile names, dependency metadata, and discovery issues.
2. Pick an `endpoint_id` from `list_endpoints`.
3. Call `admin_environment_sync` with the selected `source_environment_id`,
   `source_revision`, `endpoint_id`, and explicit `credential_policy`.
4. Poll `admin_environment_sync_status` using the returned `sync_job_id`.
   `queued` and `preparing` are in progress; `ready` returns prepared
   `launch_profile_ids` and `prepared.profile_readiness`; `failed` reports setup errors. Cancel unfinished jobs
   with `admin_environment_sync_cancel`.
5. Call `list_launch_profiles({"endpoint_id":"local"})` (or the remote ID)
   and select a returned ID for `start_coding_session` or a task `run_spec`.

Folder/ZIP imports use the same sync tools and endpoint selection. Each detected
home retains its native profiles and supplied skills/configuration. Imported
sources do not inherit the controller user's skills or external files. An
optional `taskr-environments.json` manifest names environments and maps paths
from the original machine. ZIPs are extracted into a private, versioned cache
beside `taskr.db`; they can wrap the collection in one directory. Discovery
returns provenance and metadata. Source or archive changes require rediscovery;
prepared deployments retain their original settings. See
[folder and ZIP import format](environment-imports.md) for examples,
path rules, limits, and cache retention.

Example sync using login already provisioned on the destination:

```json
{
  "source_environment_id": "<discovered environment ID>",
  "source_revision": "<discovered revision>",
  "endpoint_id": "local",
  "credential_policy": "endpoint",
  "endpoint_auth_home": "/home/user/.codex"
}
```

`credential_policy="endpoint"` excludes source login/helper credential files and
rejects recognized embedded credentials in configuration. `endpoint_auth_home`
supplies destination credentials: native login files when the selected providers
need them, and helper credential files at the relative paths reported in
`dependencies`. A helper-only or environment-key provider does not need an
unrelated `auth.json`. `credential_policy="copy"` explicitly permits transferring
source login files, helper credential files, and embedded configuration credentials.
There is no implicit credential transfer. OS keychains and provider/MCP OAuth
stores are not cloned. Provider access is not probed by sync; readiness confirms
files/dependencies, not that a provider will accept the login. Review source
configuration and provision target service credentials when needed.

The dependency inventory follows nested agent configuration, model catalogs,
instructions, provider auth helpers, MCP commands, notification commands, and
Claude `apiKeyHelper`. Paths are rebased into the prepared home, including
absolute, `~/`, and configuration-relative references. Command file arguments
use an explicit command `cwd`; ambiguous relative arguments are rejected.
External working directories contain the declared dependencies, not a copy of
the whole source directory. Bare program names are checked on endpoint PATH;
custom executable scripts are copied. Configured absolute native binary paths
are preserved and checked on the endpoint rather than bundled or rebased.
Native binaries, interpreter packages,
services, keychains and OAuth logins must be provisioned on the endpoint.

Sync never executes helpers, hooks, MCP servers or notification scripts.
Inline shell/interpreter code and non-`cat` credential helpers require
[`taskr-dependencies.json`](environment-dependencies.md) declarations for their
files and required environment variables. Script bodies are not interpreted;
declare their indirect dependencies there. Missing source files, endpoint
programs, or helper credentials fail preparation. Graph cycles and excessive
depth are rejected rather than leaving partial configuration.

Required variables such as a selected provider's `env_key` are checked without
reading their values into the registry. `prepared.profile_readiness` identifies
blocked native profiles and their `missing_environment` names; only ready
profiles receive launch IDs. Requirements use the effective base/profile
configuration. Optional provider `env_http_headers` are not required. Make the
variables available to both the endpoint companion and the Herdr agent process,
then use `refresh=true` to publish newly ready choices. Variables are not copied
from the controller or saved as launch secrets. Launch verification rechecks the
selected profile's variables, executable availability, cwd, files and CLI version.

Claude user MCP definitions come from its native `.claude.json` registry. Sync
extracts MCP configuration and excludes sign-in, trust, history and unrelated
metadata. Project-local definitions retain project scope and require explicit
source-to-endpoint project mappings in the dependency manifest. Repository
`.mcp.json` files stay with the repository. Native trust/sign-in writes may change
the deployed registry; changing its MCP definitions still invalidates verification.

Native `hooks.json` commands and their explicit script dependencies are included
and checked. Codex may request hook review after paths change during cloning.
Its `hooks.state` trust metadata can change without invalidating resume; launch
settings, hook commands and bundled scripts remain pinned.

`dry_run=true` validates preparation without publishing a home or launch choice.
`refresh=true` re-exports an already-ready selection, for credential rotation or
repair. Identical content reuses its deployment; changed content/credentials
create a new one. `deployment_root` optionally selects an absolute or `~/` managed
root on the endpoint; its default is `~/.local/share/taskr/environments`.
Helper credential contents, like native login contents, do not change an
installed home's discovery revision; they do change the deployment digest when
exported with `copy`. Configuration and script changes require rediscovery.

Discovery currently supports Codex homes with `config.toml` and Claude homes
with `settings.json`. Codex requires 0.134.0+ and the advertised `--profile`, `--no-daemon`, and
`--no-alt-screen` capabilities; native `*.config.toml` profiles
remain native and yield a base choice plus one choice per ready profile. Legacy inline
Codex profile tables require native migration first. The destination CLI must
be at least as new as the source. Claude currently yields its base settings
choice; Claude plugin installations are reported as unsupported rather than
silently omitted. OpenCode/Kimi environment adapters are not implemented yet;
already-running agents can still be adopted through MCP.

Bundles include native configuration, profile files, instructions, rules,
commands, agents, skills, declared file dependencies, and configured Codex plugin
installations. External Codex user skills are materialized under the deployed
home's supported `skills` directory. Repository skills stay with the repository.
Native sessions/history/logs and general home caches are excluded. Bundles are
limited to 64 MiB and 10,000 files; file symlinks are materialized, cycles and
special files are rejected. Missing MCP/hook executables, source-only localhost
services on remote sync, and unsupported state-path settings fail preparation.
Executables and whole remote servers are provisioned separately.

Managed homes publish atomically after validation, with private file permissions.
The SQLite registry stores metadata only; bundles and credentials travel through
companion stdin, never launch arguments or TASKR snapshots. Interrupted jobs are
reconciled after restart. Cancellation prevents publishing a launch selection;
an already-transferred deployment can remain for later reconciliation.

Normal launch verifies the selected deployment before allocating a pane. It
passes `CODEX_HOME` or `CLAUDE_CONFIG_DIR`, native profile arguments, and the
explicit permission policy to Herdr. Codex uses `--no-daemon --no-alt-screen`.
The process `HOME` is unchanged. Model, provider, permissions, hooks, skills, and
MCP settings are loaded by the native CLI from that deployed configuration and
its normal repository/managed layers. Sync does not grant permission bypass.

Choices are endpoint-specific and immutable: newer syncs retain previous IDs,
homes, and conversations. Executions pin the deployment through their selected
profile ID and frozen home/arguments. Resume uses that original home. Task exit
and ordinary prune never delete native conversations or deployed environments.
An empty endpoint catalog requires admin setup; there is no global-profile or
local-endpoint fallback. An explicit profile is required when several choices
exist; required MCP fields remain required.

## Existing project home constraints

Projects retain optional `codex_home`, `claude_home`, `opencode_home`, and
`kimi_home` fields from earlier configuration. For new projects, leave these
fields `null`; the prepared launch profile supplies the agent's home. A stored
project home must exactly match the selected deployment home or the launch is
rejected. `project_update` (admin) clears
an override with explicit `null`; omission preserves its value. Existing
executions keep their frozen environment.

```json
{"project_id":"example", "codex_home":null, "claude_home":null}
```

The endpoint owns the home path. TASKR does not reinterpret it on the controller
filesystem. The process `HOME` and task `workspace_path` remain separate from the
CLI configuration home.

See the [MCP tools](mcp.md) and [Taskr operator skill](../.codex/skills/taskr-operator/SKILL.md) for commands and recipes.
