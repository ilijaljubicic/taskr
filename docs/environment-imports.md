# Import native environments from a folder or ZIP

Enable admin tools, then call `admin_environment_discover` with one source:

```json
{"source_path":"/path/to/environments"}
```

```json
{"source_path":"/path/to/environments.zip"}
```

The path is read on the TASKR controller. A single native home also works.
`source_path` and `homes` are mutually exclusive; omitting both searches the
installed `.codex*` and `.claude*` homes. ZIP imports accept a collection directly
or inside a single wrapping directory.

Without a manifest, discovery searches the collection recursively for Codex
`config.toml` and Claude `settings.json`. It stops below each detected home,
so plugin example configurations do not become extra environments. Ambiguous
homes need a manifest. Git metadata, node modules, sessions, logs, and `.agents`
are excluded from the search for homes.

For example:

```text
environments/
  .agents/skills/shared/SKILL.md
  codex/work/
    config.toml
    review.config.toml
    skills/reviewer/SKILL.md
  codex/personal/config.toml
  claude/work/settings.json
```

Each home returns its own environment ID, revision, display name, native
profiles, and `source_location`. Select one ID/revision and use
`admin_environment_sync` with an explicit endpoint and credential policy,
then poll status and choose a returned launch profile. This prepares one
environment per invocation; repeat for other homes or endpoints. No coding
agent starts during discovery or sync. See [launch setup](environments.md).

## Optional manifest

Place `taskr-environments.json` at the collection root to declare custom layouts,
names, source CLI versions, and original machine paths:

```json
{
  "version": 1,
  "environments": [
    {
      "home": "codex/work",
      "kind": "codex",
      "name": "Work",
      "original_home": "/home/alice/.codex-work",
      "original_user_home": "/home/alice",
      "user_skills": ".agents/skills",
      "cli_version": "0.160.0"
    },
    {
      "home": "claude/work",
      "kind": "claude",
      "name": "Claude work"
    }
  ]
}
```

Only `home` and `kind` are required per entry. Paths for `home` and `user_skills`
are relative to the collection, must exist there, and cannot contain traversal.
`home: "."` selects the collection root. Unknown fields and duplicate homes are
rejected. The manifest is the complete selection when present. Native profile
names still come from the home's files; no TASKR launch presets are defined here.

`original_home` maps original absolute references into the selected packaged
home. `original_user_home` maps references into a mirrored collection root:
`/home/alice/instructions.md` needs `environments/instructions.md`. Relative
configuration file dependencies resolve from the declaring file's directory;
command arguments use their explicit cwd or dependency declaration. `~/` resolves from the collection
root. Dependencies outside the collection and escaping symlinks are rejected.
TASKR does not read an old absolute path from the controller as a fallback.

Native `hooks.json` and declarative hooks in settings are included. Direct hook
executables and script arguments to common interpreters are bundled and checked
without running them. Use absolute or `~/` script paths, with manifest aliases
mapping original locations into the collection, or declare an explicit cwd and
dependency list for relative arguments. Wrapper commands and
inline shell code require explicit file/environment declarations in a
home-local [taskr-dependencies.json](environment-dependencies.md). Credential
helpers obey the same explicit credential policy as native login files.

Codex shared user skills come from the supplied `user_skills` directory or the
collection's `.agents/skills`. They are materialized under the deployed home's
`skills/_user_agents`. Controller user skills are excluded from imports. Native
CLI runtime layers on the endpoint, including repository and managed settings,
continue to apply normally.

`cli_version` must be a supported `major.minor.patch` version. If omitted,
discovery checks the controller's installed native CLI. A declared version lets
discovery package a source without that CLI on the controller; preparation still
requires a compatible CLI on the endpoint. No executable from a collection runs
during discovery. Codex needs 0.134.0+ native profile support. Current adapters
support Codex and Claude, including configured installed plugins; other adapters
remain unsupported.

## Revisions, credentials and cache

Folder configuration, skills, dependencies, or manifest changes require
rediscovery before syncing. Replacing a ZIP keeps environment identity for the
same path/home but creates a new source revision and private cache. Archive
changes or cache tampering reject a sync of the old revision. Already-prepared
homes and execution histories remain pinned and usable.

ZIP caches live under `<store-path>/agent-environment-sources`. Directories use
mode 0700; files use 0600, or 0700 for executable dependencies. A selected archive
can contain login files, so its private cache can too. `credential_policy="endpoint"`
excludes source login and helper credential files from endpoint transfer; `copy` explicitly permits
them. Credentials and file contents never enter the SQLite catalog or discovery
responses. Source histories may exist in an archive cache but are excluded from
environment deployment.

ZIPs are limited to 256 MiB compressed and expanded, 50,000 entries, and 64 MiB
per file. Unsafe paths, duplicate members, symlinks, special files, encrypted ZIPs
and file/directory conflicts are rejected. Extraction publishes atomically;
incomplete imports do not become sources. Folder search is bounded to 32 levels
and 50,000 entries. Each deployed environment remains limited to 64 MiB and
10,000 files.

Archives and their caches must remain available for pending syncs, including
after restart. They are not needed to launch an already-prepared endpoint home.
Ordinary orchestration prune retains source caches and prepared homes; cache
garbage collection is not implemented yet.
