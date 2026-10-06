"""Native environment provisioning companion. One JSON request/response on stdio.

Executed locally or over the SSH target owned by Herdr. No daemon, terminal,
task scheduler, or general-purpose file API. Requires Python 3.11+ on both ends.
"""
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import shlex
import subprocess
import sys
import stat
import tempfile
import tomllib
import zipfile

MAX_BYTES = 64 * 1024 * 1024
MAX_FILES = 10000
MAX_COLLECTION_BYTES = 256 * 1024 * 1024
MAX_COLLECTION_FILES = 50000
AUTH_FILES = {"codex": "auth.json", "claude": ".credentials.json"}
SECRET_KEY = re.compile(r"(^|_)(token|secret|password|api_key|bearer|access_key|authorization)(_|$)", re.I)
PATH_KEYS = {"model_instructions_file", "model_catalog_json", "config_file", "path"}
HOOK_INTERPRETERS = {"sh", "bash", "dash", "zsh", "python", "python3", "node", "ruby", "perl", "bun"}
SYSTEM_COMMANDS = HOOK_INTERPRETERS | {"cat", "env", "npx", "uv", "uvx", "echo", "printf"}
FILE_ARGUMENT = re.compile(r"\.(py|js|mjs|cjs|sh|rb|pl|json|toml|yaml|yml|pem|crt|key)$", re.I)
CLAUDE_PROJECT_MCP_KEYS = {"mcpServers", "enabledMcpjsonServers", "disabledMcpjsonServers", "enableAllProjectMcpServers"}


def overlay(base, override):
    result = dict(base)
    for key, value in override.items():
        result[key] = overlay(result[key], value) if isinstance(value, dict) and isinstance(result.get(key), dict) else value
    return result


def native_reasoning_efforts(kind):
    efforts = []
    if kind == "claude":
        try:
            help_result = subprocess.run([kind, "--help"], capture_output=True, timeout=15)
            text = help_result.stdout.decode("utf-8", errors="replace")
            match = re.search(r"--effort[^\n]*\n?[^\n]*\(([^)]+)\)", text)
            if match:
                efforts = [value.strip() for value in match[1].split(",")
                           if value.strip() in {"low", "medium", "high", "xhigh", "max"}]
        except (OSError, subprocess.TimeoutExpired):
            pass  # Imported source versions need not have a controller-local CLI.
    return efforts


def profile_launch_settings(home, kind, config_entries, profiles, resolve):
    """Non-secret configuration/capability metadata; never calls a provider."""
    efforts = native_reasoning_efforts(kind)
    rows = []
    for name, (path, _, settings) in zip([None, *profiles], config_entries):
        config = settings if name is None else overlay(config_entries[0][2], settings)
        model = config.get("model")
        effort = config.get("model_reasoning_effort") if kind == "codex" else config.get("effortLevel")
        options = {}
        if kind == "codex":
            catalog = None
            if config.get("model_catalog_json"):
                catalog = read_config(resolve(config["model_catalog_json"], path.parent))
            elif config.get("model_provider", "openai") == "openai" and (home / "models_cache.json").is_file():
                catalog = read_config(resolve(str(home / "models_cache.json")))
            for item in (catalog or {}).get("models", []):
                if not isinstance(item, dict) or not isinstance(item.get("slug"), str):
                    continue
                options[item["slug"]] = [entry["effort"] for entry in item.get("supported_reasoning_levels", [])
                                         if isinstance(entry, dict) and isinstance(entry.get("effort"), str)]
        else:
            environment = config.get("env", {})
            model = model or environment.get("ANTHROPIC_MODEL")
            effort = effort or environment.get("CLAUDE_CODE_EFFORT_LEVEL")
            for alias in ("haiku", "sonnet", "opus", "fable"):
                if not environment.get("ANTHROPIC_BASE_URL") or environment.get("ANTHROPIC_DEFAULT_" + alias.upper() + "_MODEL"):
                    options[alias] = efforts
        if isinstance(model, str):
            options.setdefault(model, [effort] if isinstance(effort, str) else efforts)
        rows.append({"native_profile": name, "model": model, "reasoning_effort": effort,
                     "model_options": [{"model": value, "reasoning_efforts": supported}
                                       for value, supported in sorted(options.items())]})
    return rows


def pointer_part(value):
    return value.replace("~", "~0").replace("/", "~1")


class DependencyInventory:
    """One bounded graph for export, path adaptation and endpoint preflight.

    Command bodies are never executed. Only declared native command fields are
    inspected; opaque commands require an explicit dependency declaration.
    """
    def __init__(self, home, source, files, checked, resolve, configs, profiles):
        self.home, self.source, self.files = home, source, files
        self.checked, self.resolve = checked, resolve
        self.records, self.credentials, self.mappings, self.config_files = [], {}, {}, []
        self.directories = {}
        self.visiting, self.visited = set(), set()
        self.runtime_configs = set()
        self.effective = {None: configs[0][2]}
        self.effective.update({name: overlay(configs[0][2], config) for name, (_, _, config) in zip(profiles, configs[1:])})
        self.declarations, self.used_declarations, self.project_mappings = {}, set(), {}
        manifest = home / "taskr-dependencies.json"
        if manifest.exists():
            value = read_config(checked(manifest))
            if value.get("version") != 1 or set(value) - {"version", "commands", "claude_projects"}:
                raise ProvisionError("Invalid taskr-dependencies.json manifest")
            self.add_commands(value)
            self.project_mappings = value.get("claude_projects", {})
            if not isinstance(self.project_mappings, dict) or any(
                    not isinstance(k, str) or not isinstance(v, str) or not k.startswith("/") or not v.startswith("/")
                    for k, v in self.project_mappings.items()):
                raise ProvisionError("Claude project mappings must use absolute paths")
            if len(set(self.project_mappings.values())) != len(self.project_mappings):
                raise ProvisionError("Claude project mappings must not merge scopes")
            files.append(file_entry(checked(manifest), "home/taskr-dependencies.json"))

    def add_commands(self, value, prefix=""):
        if not isinstance(value.get("commands", []), list) or len(value.get("commands", [])) > MAX_FILES:
            raise ProvisionError("Invalid command dependency declaration list")
        for entry in value.get("commands", []):
            if not isinstance(entry, dict) or set(entry) - {"config", "pointer", "files", "environment"}:
                raise ProvisionError("Invalid command dependency declaration")
            if not isinstance(entry.get("files", []), list) or not isinstance(entry.get("environment", []), list):
                raise ProvisionError("Command dependencies must use file and environment lists")
            config = entry.get("config", "")
            if not isinstance(config, str) or not config or PurePosixPath(config).is_absolute() or ".." in PurePosixPath(config).parts:
                raise ProvisionError("Dependency declarations require a home-relative config")
            key = (prefix + config, entry.get("pointer", ""))
            if not isinstance(key[1], str) or not key[1].startswith("/") or key in self.declarations:
                raise ProvisionError("Invalid or duplicate command dependency pointer")
            self.declarations[key] = entry

    def destination(self, path):
        for directory in sorted(self.directories, key=lambda p: len(p.parts), reverse=True):
            if path.is_relative_to(directory):
                return str(PurePosixPath(self.directories[directory]) / path.relative_to(directory))
        return str(path.relative_to(self.home)) if path.is_relative_to(self.home) else (
            "dependencies/files/" + digest(str(path.parent))[:20] + "/" + path.name)

    def record(self, kind, reference, profiles, context):
        if kind == "environment" and not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", reference):
            raise ProvisionError("Invalid required environment variable name")
        record = {"kind": kind, "reference": reference, "profiles": profiles, "context": context}
        if record not in self.records:
            if len(self.records) >= MAX_FILES:
                raise ProvisionError("Too many configuration dependencies")
            self.records.append(record)

    def collect(self, value, base, destination, profiles, context, credential=False, directory=False):
        path = self.resolve(value, base, allow_missing=credential)
        relative = self.destination(path)
        self.mappings.setdefault(destination, {})[value] = relative
        self.mappings[destination][str(path)] = relative
        self.record("directory" if directory else "credential" if credential else "file", relative, profiles, context)
        if directory:
            if not path.is_dir():
                raise ProvisionError("Command cwd is not a directory at the source")
            self.directories[path] = relative
        elif credential:
            if path.exists() and not path.is_file():
                raise ProvisionError("Credential dependencies must be files")
            self.credentials[relative] = str(path)
        else:
            self.files.extend(tree_files(path, "home/" + relative, Path(self.source["root"]) if self.source else None))
        return path, relative

    def active_profiles(self, destination, pointer, inherited):
        if destination not in {"config.toml", *[str(n) + ".config.toml" for n in self.effective if n]}:
            return inherited
        names = inherited if inherited is not None else (list(self.effective) if destination == "config.toml" else [destination[:-len(".config.toml")]])
        parts = pointer.split("/")
        if len(parts) > 2 and parts[1] == "model_providers":
            provider = parts[2].replace("~1", "/").replace("~0", "~")
            names = [n for n in names if self.effective[n].get("model_provider", "openai") == provider]
        return names

    def command(self, value, path, destination, pointer, profiles, auth=False, context="command"):
        declaration = self.declarations.get((destination, pointer))
        declaration_key = (destination, pointer)
        if declaration is None and getattr(self, "effective_scan", False):
            declaration_key = ("config.toml", pointer)
            declaration = self.declarations.get(declaration_key)
        if declaration is not None:
            self.used_declarations.add(declaration_key)
        if isinstance(value, dict):
            program, args = value.get("command"), value.get("args", [])
            cwd = value.get("cwd")
            command_env = value.get("env", {})
            if context == "hooks":
                if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
                    raise ProvisionError("Invalid native hook arguments")
                try:
                    words = shlex.split(program)
                except (ValueError, TypeError):
                    raise ProvisionError("Invalid native hook command")
                program, args = (words[0] if words else None), words[1:] + args
        elif isinstance(value, list):
            program, args, cwd, command_env = (value[0] if value else None), value[1:], None, {}
        else:
            try:
                words = shlex.split(value)
            except (ValueError, TypeError):
                raise ProvisionError("Invalid native command declaration")
            program, args, cwd, command_env = (words[0] if words else None), words[1:], None, {}
        if not isinstance(program, str) or not program or not isinstance(args, list) or not all(isinstance(a, str) for a in args):
            raise ProvisionError("Invalid native command declaration")
        base = path.parent
        if cwd:
            base, _ = self.collect(cwd, base, destination, profiles, context, directory=True)
        name = Path(program).name
        absolute = program.startswith(("/", "~/", "./", "../")) or bool(FILE_ARGUMENT.search(program))
        if not absolute and not re.fullmatch(r"[A-Za-z0-9_+.-]+", program):
            raise ProvisionError("Shell assignments and expressions require an explicit executable wrapper")
        if absolute and name not in SYSTEM_COMMANDS:
            executable = self.resolve(program, base)
            if not executable.stat().st_mode & 0o111:
                raise ProvisionError("A bundled command executable is not executable")
            with executable.open("rb") as stream:
                head = stream.read(4096)
            binary = head[:4] in {b"\x7fELF", b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf",
                                   b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xce",
                                   b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"} or b"\0" in head
            if binary:
                # This is an endpoint-installed program, not a portable script.
                # Preserve its configured endpoint path; never package its bytes
                # or redirect it into the managed home through prefix rebasing.
                reference = program if program.startswith(("/", "~/")) else str(executable)
                self.record("executable", reference, profiles, context)
                self.mappings.setdefault(destination, {})[program] = "@program:" + reference
            else:
                _, relative = self.collect(program, base, destination, profiles, context)
                self.record("bundled_executable", relative, profiles, context)
            header = head.split(b"\n", 1)[0]
            if not binary and header.startswith(b"#!"):
                try:
                    interpreter = shlex.split(header[2:].decode())
                except (UnicodeError, ValueError):
                    raise ProvisionError("Invalid bundled executable interpreter")
                if not interpreter:
                    raise ProvisionError("Invalid bundled executable interpreter")
                self.record("executable", interpreter[0], profiles, context)
                if Path(interpreter[0]).name == "env":
                    if len(interpreter) != 2 or interpreter[1].startswith("-"):
                        raise ProvisionError("Unsupported bundled executable env interpreter")
                    self.record("executable", interpreter[1], profiles, context)
        else:
            self.record("executable", program, profiles, context)
            if absolute:
                self.mappings.setdefault(destination, {})[program] = "@program:" + program
        opaque = any(a in {"-c", "-e", "--eval", "-m"} for a in args) or any(
            re.search(r"[|;&`]|\$\(", a) for a in [program, *args])
        if (opaque or (auth and name != "cat")) and declaration is None:
            raise ProvisionError("Opaque command requires taskr-dependencies.json: " + destination + pointer)
        if not opaque:
            script_index = next((i for i, a in enumerate(args) if not a.startswith("-")), None) if name in HOOK_INTERPRETERS else None
            for i, arg in enumerate(args):
                reference = arg.split("=", 1)[1] if arg.startswith("--") and "=" in arg else arg
                if path in self.runtime_configs and reference.startswith(("${CLAUDE_PLUGIN_DATA}", "${CLAUDE_PROJECT_DIR}", "${user_config.")):
                    continue  # Native runtime/user configuration, not source files.
                if "://" in reference or reference.startswith("-"):
                    continue
                script = i == script_index and name in HOOK_INTERPRETERS
                looks_file = reference.startswith(("/", "~/", "./", "../")) or bool(FILE_ARGUMENT.search(reference))
                if script or looks_file or (auth and name == "cat"):
                    if not reference.startswith(("/", "~/")) and cwd is None and declaration is None:
                        raise ProvisionError("Relative command dependencies require an explicit cwd or taskr-dependencies.json")
                    _, relative = self.collect(reference, base, destination, profiles, context, credential=auth and not script)
                    if reference != arg:
                        self.mappings.setdefault(destination, {})[arg] = "@argument:" + arg.split("=", 1)[0] + "=" + relative
        for env_name in (declaration or {}).get("environment", []):
            if not isinstance(env_name, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", env_name):
                raise ProvisionError("Invalid required environment variable name")
            if env_name not in command_env:
                self.record("environment", env_name, profiles, context)
        for entry in (declaration or {}).get("files", []):
            if not isinstance(entry, dict) or set(entry) - {"path", "credential"} or not isinstance(entry.get("path"), str):
                raise ProvisionError("Invalid declared dependency file")
            if not isinstance(entry.get("credential", False), bool):
                raise ProvisionError("Dependency credential classification must be boolean")
            self.collect(entry["path"], base, destination, profiles, context, credential=entry.get("credential", False))

    def scan(self, path, destination, config=None, inherited=None):
        path = self.checked(path)
        if path in self.visiting:
            raise ProvisionError("A native configuration dependency cycle is unsupported")
        key = (path, tuple(inherited or []))
        if key in self.visited:
            return
        if len(self.visited) >= 256 or len(self.visiting) >= 32:
            raise ProvisionError("Native configuration dependency graph exceeds its limit")
        self.visiting.add(path)
        self.config_files.append(destination)
        config = config if config is not None else read_config(path)
        def visit(value, pointer="", context=""):
            profiles = self.active_profiles(destination, pointer, inherited if inherited is not None else list(self.effective))
            if isinstance(value, dict):
                if "command" in value and isinstance(value["command"], str) and (
                        context in {"mcp_servers", "mcpServers", "lspServers", "auth", "hooks"}):
                    self.command(value, path, destination, pointer, profiles, auth=context == "auth", context=context)
                for name, child in value.items():
                    child_context = name if name in {"mcp_servers", "mcpServers", "lspServers", "auth", "hooks", "env_http_headers"} else context
                    child_pointer = pointer + "/" + pointer_part(name)
                    if name in PATH_KEYS and isinstance(child, str):
                        nested, relative = self.collect(child, path.parent, destination, profiles, "configuration")
                        if name == "config_file":
                            self.scan(nested, relative, inherited=profiles)
                    elif name in {"notify", "apiKeyHelper"} and isinstance(child, (str, list)):
                        self.command(child, path, destination, child_pointer, profiles, auth=name == "apiKeyHelper", context=name)
                    elif name in {"env_key", "bearer_token_env_var", "apiKeyHelperEnvVar"} and isinstance(child, str):
                        self.record("environment", child, profiles, "provider" if name == "env_key" else context)
                    else:
                        visit(child, child_pointer, child_context)
            elif isinstance(value, list):
                for index, child in enumerate(value):
                    visit(child, pointer + "/" + str(index), context)
            elif isinstance(value, str) and context != "env_http_headers":
                for variable in re.findall(r"\$\{(?:env:)?([A-Za-z_][A-Za-z0-9_]*)\}", value):
                    if path in self.runtime_configs and variable in {"CLAUDE_PLUGIN_ROOT", "CLAUDE_PLUGIN_DATA", "CLAUDE_PROJECT_DIR"}:
                        continue
                    self.record("environment", variable, profiles, context or "configuration")
        visit(config)
        self.visiting.remove(path)
        self.visited.add(key)

    def finish(self):
        if set(self.declarations) != self.used_declarations:
            raise ProvisionError("A taskr-dependencies.json command pointer does not match native configuration")
        if set(self.config_files) & self.credentials.keys():
            raise ProvisionError("Native configuration cannot be classified as helper credential data")
        # Credentials encountered via trees or a second config reference must
        # still obey policy, including during metadata-only discovery.
        return [entry for entry in self.files if entry["path"] not in {"home/" + p for p in self.credentials}]


class ProvisionError(Exception):
    pass


class EnvironmentSetupError(ProvisionError):
    """A valid native home whose dependencies need endpoint setup."""


class PluginProvisionError(EnvironmentSetupError):
    """A valid Claude home whose installed plugin setup needs repair."""


def json_entry(value, destination):
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True).encode()
    return {"path": "home/" + destination, "data": base64.b64encode(raw).decode(),
            "sha256": hashlib.sha256(raw).hexdigest(), "executable": False}


def plugin_root_values(value, root):
    """Resolve native plugin-root references for inspection, never execution."""
    if isinstance(value, dict):
        return {key: plugin_root_values(child, root) for key, child in value.items()}
    if isinstance(value, list):
        return [plugin_root_values(child, root) for child in value]
    if isinstance(value, str):
        return value.replace("${CLAUDE_PLUGIN_ROOT}", str(root)).replace("$CLAUDE_PLUGIN_ROOT/", str(root) + "/")
    return value


def claude_plugins(home, config, files, inventory, checked, resolve, boundary, path_mappings):
    """Clone configured installed plugins and a pinned native marketplace catalog.

    Payloads live below the marketplace snapshot; installation records point at
    those exact roots. No downloads, installation commands, or plugin code run.
    Unmapped project/local installations never become user installations.
    """
    enabled = config.get("enabledPlugins", {})
    if not isinstance(enabled, dict) or any(not isinstance(value, bool) for value in enabled.values()):
        raise ProvisionError("Invalid Claude enabledPlugins configuration")
    pending = sorted(name for name, active in enabled.items() if active)
    if not pending:
        return {}
    configured_root = config.get("env", {}).get("CLAUDE_CODE_PLUGIN_CACHE_DIR")
    registry_root = resolve(configured_root) if configured_root else home / "plugins"
    path_mappings[str(registry_root)] = "plugins"
    installed_path = registry_root / "installed_plugins.json"
    if not installed_path.is_file():
        raise PluginProvisionError("Claude enabled plugins have no installed_plugins.json; install them in this source home")
    installed = read_config(checked(installed_path))
    if installed.get("version") != 2 or not isinstance(installed.get("plugins"), dict):
        raise PluginProvisionError("Claude installed plugin registry needs the native version-2 format")
    known_path = registry_root / "known_marketplaces.json"
    known = read_config(checked(known_path)) if known_path.is_file() else {}
    selected, markets, plugin_configs = {}, {}, {}

    def installed_path(value):
        try:
            return resolve(value)
        except ProvisionError as error:
            if "escapes" in str(error):
                raise
            raise PluginProvisionError("Claude installed plugin or marketplace payload is missing at the source") from error

    def scan_plugin(root, destination, entry):
        manifest_path = root / ".claude-plugin/plugin.json"
        own = read_config(checked(manifest_path)) if manifest_path.is_file() else None
        manifest = dict(own if own is not None else entry)
        component_keys = {"commands", "agents", "skills", "hooks", "outputStyles", "themes"}
        if own is not None:
            if not entry.get("strict", True) and any(key in entry for key in component_keys):
                raise PluginProvisionError("Claude plugin has conflicting cache and marketplace component declarations")
            for key in component_keys & entry.keys():
                if key == "hooks" or key not in manifest:
                    manifest[key] = entry[key]
                else:
                    first, second = manifest[key], entry[key]
                    manifest[key] = (first if isinstance(first, list) else [first]) + (second if isinstance(second, list) else [second])
        scan_path = manifest_path if own is not None else markets[entry['_market']]['manifest_path']
        scan_relative = destination + "/.claude-plugin/plugin.json" if own is not None else markets[entry['_market']]['manifest_relative']

        def scan_file(path, context):
            path = checked(path)
            if not path.is_relative_to(root):
                raise ProvisionError("Claude plugin component escapes its plugin root")
            relative = destination + "/" + str(path.relative_to(root))
            data = read_config(path)
            inventory.runtime_configs.add(path)
            if context in {"mcpServers", "lspServers"} and context not in data:
                data = {context: data}
            inventory.scan(path, relative, plugin_root_values(data, root))
            plugin_configs[relative] = destination

        for relative, context in [("hooks/hooks.json", "hooks"), (".mcp.json", "mcpServers"), (".lsp.json", "lspServers")]:
            path = root / relative
            if path.is_file():
                scan_file(path, context)
        inline = {}
        for key in ("skills", "commands", "agents", "outputStyles", "themes", "hooks", "mcpServers", "lspServers"):
            values = manifest.get(key, [])
            for value in values if isinstance(values, list) else [values]:
                if isinstance(value, str):
                    path = resolve(value, root)
                    if not path.is_relative_to(root):
                        raise ProvisionError("Claude plugin component escapes its plugin root")
                    if key in {"hooks", "mcpServers", "lspServers"}:
                        scan_file(path, key)
                elif isinstance(value, dict) and key in {"hooks", "mcpServers", "lspServers"}:
                    inline.setdefault(key, {}).update(value)
        if inline:
            # One marketplace can describe several plugins; inspect each in its
            # own root context instead of deduplicating on the catalog path.
            inventory.visited.discard((scan_path, ()))
            inventory.runtime_configs.add(scan_path)
            inventory.scan(scan_path, scan_relative, plugin_root_values(inline, root))
            if own is not None:
                plugin_configs[scan_relative] = destination
        return manifest

    while pending:
        identifier = pending.pop(0)
        if identifier in selected:
            continue
        if enabled.get(identifier) is False:
            raise PluginProvisionError("A required Claude plugin dependency is explicitly disabled: " + identifier)
        parts = identifier.split("@")
        if len(parts) != 2 or any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", part) or ".." in part for part in parts):
            raise ProvisionError("Invalid Claude plugin identifier")
        name, market = parts
        records = installed["plugins"].get(identifier, [])
        if not isinstance(records, list):
            raise PluginProvisionError("Invalid Claude installed plugin record")
        scoped = []
        for record in records:
            if not isinstance(record, dict):
                raise PluginProvisionError("Invalid Claude installed plugin record")
            scope = record.get("scope")
            if scope == "user":
                scoped.append(dict(record))
            elif scope in {"project", "local"}:
                target = inventory.project_mappings.get(record.get("projectPath"))
                if target:
                    scoped.append({**record, "projectPath": target})
            else:
                raise PluginProvisionError("Unknown Claude plugin installation scope")
        if not scoped:
            raise PluginProvisionError("Claude enabled plugin '" + identifier + "' has no installed user payload or explicitly mapped project installation")
        if market not in markets:
            registration = known.get(market)
            if not isinstance(registration, dict) or not isinstance(registration.get("installLocation"), str):
                raise PluginProvisionError("Claude plugin marketplace '" + market + "' is not installed in this source home")
            marketplace_root = installed_path(registration["installLocation"])
            manifest_path = checked(marketplace_root / ".claude-plugin/marketplace.json")
            catalog = read_config(manifest_path)
            if catalog.get("name") != market or not isinstance(catalog.get("plugins"), list):
                raise PluginProvisionError("Claude plugin marketplace identity or entries are invalid")
            relative = "plugins/marketplaces/" + market
            path_mappings[str(marketplace_root)] = relative
            inventory.directories[marketplace_root] = relative
            markets[market] = {"registration": registration, "catalog": catalog, "entries": [],
                              "manifest_path": manifest_path, "manifest_relative": relative + "/.claude-plugin/marketplace.json"}
        catalog = markets[market]
        matches = [entry for entry in catalog["catalog"]["plugins"] if isinstance(entry, dict) and entry.get("name") == name]
        if len(matches) != 1:
            raise PluginProvisionError("Claude installed plugin '" + identifier + "' needs one matching marketplace entry")
        entry = dict(matches[0])
        selected[identifier] = []
        for record in scoped:
            if not isinstance(record.get("installPath"), str):
                raise PluginProvisionError("Claude plugin installation has no payload path")
            root = installed_path(record["installPath"])
            if not root.is_dir():
                raise PluginProvisionError("Claude plugin installation payload is missing")
            destination = "plugins/marketplaces/" + market + "/plugins/" + name + "/" + digest(str(root))[:16]
            path_mappings[record["installPath"]] = destination
            path_mappings[str(root)] = destination
            inventory.directories[root] = destination
            if root.is_relative_to(home):
                old_prefix = str(root.relative_to(home)) + "/"
                for key in list(inventory.declarations):
                    if key[0].startswith(old_prefix):
                        replacement = (destination + "/" + key[0][len(old_prefix):], key[1])
                        if replacement in inventory.declarations:
                            raise ProvisionError("Duplicate plugin dependency declaration")
                        inventory.declarations[replacement] = inventory.declarations.pop(key)
            declarations = root / "taskr-dependencies.json"
            if declarations.is_file():
                value = read_config(checked(declarations))
                if value.get("version") != 1 or set(value) - {"version", "commands"}:
                    raise ProvisionError("Invalid plugin taskr-dependencies.json manifest")
                inventory.add_commands(value, destination + "/")
            files.extend(tree_files(root, "home/" + destination, boundary))
            manifest = scan_plugin(root, destination, {**entry, "_market": market})
            selected[identifier].append(record)
            if not any(e["name"] == name for e in catalog["entries"]):
                catalog["entries"].append({**entry, "version": record.get("version", entry.get("version", "unknown")),
                                           "source": "./plugins/" + name + "/" + digest(str(root))[:16]})
            for dependency in manifest.get("dependencies", []):
                value = dependency.get("name") if isinstance(dependency, dict) else dependency
                if not isinstance(value, str):
                    raise PluginProvisionError("Invalid Claude plugin dependency")
                pending.append(value if "@" in value else value + "@" + market)
    files.append(json_entry({"version": 2, "plugins": selected}, "plugins/installed_plugins.json"))
    registrations = {}
    for name, market in markets.items():
        catalog = {key: value for key, value in market["catalog"].items()
                   if key not in {"renames", "forceRemoveDeletedPlugins"}}
        if isinstance(catalog.get("metadata"), dict):
            catalog["metadata"] = {key: value for key, value in catalog["metadata"].items() if key != "pluginRoot"}
        files.append(json_entry({**catalog, "plugins": market["entries"]}, market["manifest_relative"]))
        registrations[name] = {**market["registration"], "autoUpdate": False}
    files.append(json_entry(registrations, "plugins/known_marketplaces.json"))
    return plugin_configs


class BundleFiles(list):
    """Bound memory while packaging, rather than after collecting every tree."""
    def __init__(self):
        super().__init__()
        self.paths = {}
        self.bytes = 0

    def append(self, entry):
        previous = self.paths.get(entry["path"])
        if previous:
            if previous["sha256"] != entry["sha256"] or previous["executable"] != entry["executable"]:
                raise ProvisionError("Conflicting environment bundle paths")
            return
        size = len(entry["data"]) * 3 // 4 - (len(entry["data"]) - len(entry["data"].rstrip("=")))
        if self.bytes + size > MAX_BYTES or len(self) >= MAX_FILES:
            raise ProvisionError("Environment exceeds the bundle size/file limit")
        self.bytes += size
        self.paths[entry["path"]] = entry
        super().append(entry)

    def extend(self, entries):
        for entry in entries:
            self.append(entry)

    def __iadd__(self, entries):
        self.extend(entries)
        return self


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def read_config(path):
    try:
        if path.stat().st_size > MAX_BYTES:
            raise ProvisionError("Configuration exceeds the file size limit")
        raw = path.read_text()
        config = tomllib.loads(raw) if path.suffix == ".toml" else json.loads(raw)
        if not isinstance(config, dict):
            raise ProvisionError("Native configuration must be an object: " + path.name)
        return config
    except (OSError, ValueError):
        raise ProvisionError("Invalid native configuration file: " + path.name)


def hook_commands(config):
    def visit(value):
        if isinstance(value, dict):
            for key, item in value.items():
                if key == "command" and isinstance(item, str):
                    yield item
                else:
                    yield from visit(item)
        elif isinstance(value, list):
            for item in value:
                yield from visit(item)
    yield from visit(config.get("hooks", {}))


def hook_command_parts(command):
    try:
        words = shlex.split(command)
    except ValueError:
        raise ProvisionError("Invalid native hook command")
    if not words:
        raise ProvisionError("Empty native hook command")
    program = words[0]
    script = words[1] if (Path(program).name in HOOK_INTERPRETERS and len(words) > 1
                          and not words[1].startswith("-")) else None
    if script and not (script.startswith("/") or script.startswith("~/")):
        raise ProvisionError("Native hook script paths must be absolute")
    return program, script


def pinned_config(config):
    # Codex writes hook review decisions here. They are endpoint-native state;
    # declarative hook commands and all other settings remain pinned.
    config = json.loads(json.dumps(config))
    if isinstance(config.get("hooks"), dict):
        config["hooks"].pop("state", None)
        if not config["hooks"]:
            config.pop("hooks")
    return config


def pinned_claude_registry(config):
    result = {"mcpServers": config.get("mcpServers", {})}
    for project, settings in config.get("projects", {}).items():
        selected = {key: settings[key] for key in CLAUDE_PROJECT_MCP_KEYS if key in settings}
        if selected:
            result.setdefault("projects", {})[project] = selected
    return result


def native_version(kind):
    if not shutil.which(kind):
        raise ProvisionError("Required coding CLI is not on the endpoint PATH: " + kind)
    try:
        result = subprocess.run([kind, "--version"], capture_output=True, timeout=15)
        match = re.search(rb"\b(\d+)\.(\d+)\.(\d+)\b", result.stdout)
        if result.returncode or not match:
            raise ProvisionError("Cannot determine coding CLI version: " + kind)
        version = tuple(int(x) for x in match.groups())
        if kind == "codex" and version < (0, 134, 0):
            raise ProvisionError("Codex 0.134.0+ is required for native profile files")
        if kind == "codex":
            help_result = subprocess.run([kind, "--help"], capture_output=True, timeout=15)
            if help_result.returncode:
                raise ProvisionError("Cannot determine Codex launch capabilities")
            for flag in (b"--profile", b"--no-daemon", b"--no-alt-screen"):
                if flag not in help_result.stdout:
                    raise ProvisionError("Required Codex launch capability is missing: " + flag.decode())
        return ".".join(map(str, version))
    except (OSError, subprocess.TimeoutExpired):
        raise ProvisionError("Cannot determine coding CLI version: " + kind)


def file_entry(source, destination, include_login=False):
    if not include_login and source.resolve().name in AUTH_FILES.values():
        raise ProvisionError("Native login files cannot be configuration dependencies; use an explicit credential policy")
    if source.stat().st_size > MAX_BYTES:
        raise ProvisionError("Environment file exceeds the bundle size limit")
    content = source.read_bytes()
    if len(content) > MAX_BYTES:
        raise ProvisionError("Environment file exceeds the bundle size limit")
    return {"path": destination, "data": base64.b64encode(content).decode(),
            "sha256": hashlib.sha256(content).hexdigest(),
            "executable": bool(source.stat().st_mode & 0o111)}


def tree_files(root, destination, boundary=None):
    if not root.exists():
        return []
    output = BundleFiles()
    # Resolve symlinks to content, but reject directory cycles and special files.
    def visit(path, relative, parents):
        resolved = path.resolve(strict=True)
        if boundary is not None and not resolved.is_relative_to(boundary):
            raise ProvisionError("Imported dependency escapes the collection folder")
        if resolved in parents:
            raise ProvisionError("A selected environment contains a symlink cycle")
        if resolved.is_dir():
            for child in sorted(path.iterdir()):
                if child.name in {".git", "__pycache__", ".DS_Store", ".in_use", "sessions", "archived_sessions", "logs", *AUTH_FILES.values()}:
                    continue
                visit(child, relative / child.name, parents | {resolved})
        elif resolved.is_file():
            output.append(file_entry(resolved, str(PurePosixPath(destination) / relative)))
            if len(output) > MAX_FILES:
                raise ProvisionError("Environment contains too many files")
        else:
            raise ProvisionError("A selected environment contains a special file")
    visit(root, PurePosixPath(), set())
    return output


def export(home, kind, source=None):
    boundary = Path(source["root"]).resolve(strict=True) if source else None
    user_home = boundary if source else Path.home()
    original_home = source.get("original_home") if source else None
    original_user = source.get("original_user_home") if source else None
    def checked(path):
        resolved = path.resolve(strict=True)
        if boundary is not None and not resolved.is_relative_to(boundary):
            raise ProvisionError("Imported dependency escapes the collection folder")
        return resolved
    def dependency_path(value, base=None, allow_missing=False):
        if source:
            path = Path(value)
            if value.startswith("~/"):
                path = boundary / value[2:]
            elif original_home and path.is_relative_to(original_home):
                path = home / path.relative_to(original_home)
            elif original_user and path.is_relative_to(original_user):
                path = boundary / path.relative_to(original_user)
            elif not path.is_absolute():
                path = (base or home) / path
        else:
            path = Path(value).expanduser()
            path = (base or home) / path if not path.is_absolute() else path
        if not path.exists():
            if allow_missing:
                resolved = path.resolve()
                if boundary is not None and not resolved.is_relative_to(boundary):
                    raise ProvisionError("Imported dependency escapes the collection folder")
                return resolved
            raise ProvisionError("A referenced configuration dependency is missing at the source")
        return checked(path)
    config_name = "config.toml" if kind == "codex" else "settings.json"
    config = read_config(checked(home / config_name))
    version = source.get("cli_version") if source else None
    version = version or native_version(kind)
    if not re.fullmatch(r"\d+\.\d+\.\d+", version) or (kind == "codex" and tuple(map(int, version.split("."))) < (0, 134, 0)):
        raise ProvisionError("Invalid or unsupported source CLI version")
    if kind == "codex" and config.get("profiles"):
        raise ProvisionError("Legacy Codex inline profiles must be migrated to *.config.toml")
    files = BundleFiles()
    files.append(file_entry(checked(home / config_name), "home/" + config_name))
    configs = [config]
    config_entries = [(home / config_name, config_name, config)]
    path_mappings = {original_home: "."} if original_home else {}
    profiles = []
    if kind == "codex":
        for profile in sorted(home.glob("*.config.toml")):
            name = profile.name[:-len(".config.toml")]
            if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
                raise ProvisionError("Invalid native profile filename")
            configs.append(read_config(checked(profile)))
            config_entries.append((profile, profile.name, configs[-1]))
            profiles.append(name)
            files.append(file_entry(checked(profile), "home/" + profile.name))
    for name in ("AGENTS.md", "CLAUDE.md", "hooks.json"):
        if (home / name).is_file():
            files.append(file_entry(checked(home / name), "home/" + name))
            if name == "hooks.json":
                configs.append(read_config(checked(home / name)))
                config_entries.append((home / name, name, configs[-1]))
    for name in ("rules", "agents", "skills", "commands"):
        files += tree_files(home / name, "home/" + name, boundary)
    # Native plugin installations are configuration dependencies, even though
    # Codex calls their directory a cache. Include only configured plugins.
    if kind == "codex":
        for settings in configs:
            for plugin_id, settings_entry in settings.get("plugins", {}).items():
                if settings_entry.get("enabled", True) is False:
                    continue
                parts = plugin_id.split("@")
                if len(parts) != 2 or any(not re.fullmatch(r"[A-Za-z0-9_.-]+", p) for p in parts):
                    raise ProvisionError("Unsupported native plugin identifier")
                plugin, marketplace = parts
                cache = home / "plugins/cache" / marketplace / plugin
                versions = sorted(p for p in cache.iterdir() if p.is_dir()) if cache.is_dir() else []
                if not versions:
                    raise ProvisionError("A configured plugin is not installed in the selected home")
                active = cache / "local" if (cache / "local").is_dir() else versions[-1]
                files += tree_files(active, "home/" + str(active.relative_to(home)), boundary)
            for name, marketplace in settings.get("marketplaces", {}).items():
                if marketplace.get("source_type") == "local":
                    marketplace_source = dependency_path(marketplace["source"])
                    manifest = marketplace_source / ".agents/plugins/marketplace.json"
                    if not manifest.is_file():
                        raise ProvisionError("A configured local marketplace manifest is missing")
                    destination = "dependencies/marketplaces/" + digest(str(marketplace_source))[:20]
                    path_mappings[str(marketplace_source)] = destination
                    path_mappings[marketplace["source"]] = destination
                    files.append(file_entry(checked(manifest), "home/" + destination + "/.agents/plugins/marketplace.json"))
    inventory = DependencyInventory(home, source, files, checked, dependency_path, config_entries, profiles)
    plugin_configs = claude_plugins(home, config, files, inventory, checked, dependency_path, boundary, path_mappings) if kind == "claude" else {}
    plugin_dependencies = list(inventory.records)
    if kind == "claude":
        registry = home / ".claude.json"
        if not registry.exists() and not source and home == Path.home() / ".claude":
            registry = Path.home() / ".claude.json"
        if registry.exists():
            native = pinned_claude_registry(read_config(checked(registry)))
            sanitized = {"mcpServers": native["mcpServers"]}
            for project, settings in native.get("projects", {}).items():
                target = inventory.project_mappings.get(project)
                if not target:
                    raise EnvironmentSetupError("Claude project MCP definitions require an explicit project path mapping")
                sanitized.setdefault("projects", {})[target] = settings
            raw = json.dumps(sanitized).encode()
            files.append({"path": "home/.claude.json", "data": base64.b64encode(raw).decode(),
                          "sha256": hashlib.sha256(raw).hexdigest(), "executable": False})
            config_entries.append((registry, ".claude.json", sanitized))
    for path, relative, settings in config_entries:
        inventory.scan(path, relative, settings)
    # Configured plugin hooks/MCP declarations also belong to the graph.
    for entry in list(files):
        relative = str(PurePosixPath(entry["path"]).relative_to("home"))
        if relative not in inventory.config_files and PurePosixPath(relative).name in {"hooks.json", ".mcp.json"}:
            path = home / relative
            if path.is_file() and relative not in plugin_configs:
                inventory.scan(path, relative)
    # Derive prerequisites from each fully overlaid profile, so an override of
    # a provider helper, MCP command or env_key does not retain base requirements.
    inventory.records = plugin_dependencies
    inventory.visited = set()
    inventory.effective_scan = True
    for name, effective in inventory.effective.items():
        relative = name + ".config.toml" if name is not None else config_name
        inventory.scan(home / relative, relative, effective, inherited=[name])
    for path, relative, settings in config_entries:
        if relative not in {config_name, *[p + ".config.toml" for p in profiles]}:
            inventory.scan(path, relative, settings)
    for relative in list(inventory.config_files):
        if PurePosixPath(relative).name in {"hooks.json", ".mcp.json"} and (home / relative).is_file():
            inventory.scan(home / relative, relative)
    files = inventory.finish()
    # Absolute declarations also adapt script bodies without interpreting them.
    # Relative references remain local to their declaring configuration file.
    for mappings in inventory.mappings.values():
        path_mappings.update({old: target for old, target in mappings.items()
                              if old.startswith(("/", "~/")) and not target.startswith("@program:")})
    # User skills outside CODEX_HOME are explicitly bundled into the native
    # home/skills location still supported by the installed CLI. No HOME override.
    if kind == "codex":
        extra = user_home / (source.get("user_skills") or ".agents/skills") if source else user_home / ".agents/skills"
        if extra.resolve() != (home / "skills").resolve():
            files += tree_files(extra, "home/skills/_user_agents", boundary)
            if source and extra.exists():
                path_mappings[str(extra)] = "skills/_user_agents"
                if original_user:
                    path_mappings[original_user + "/.agents/skills"] = "skills/_user_agents"
    unique = {f["path"]: f for f in files}
    files = sorted(unique.values(), key=lambda f: f["path"])
    if sum(len(base64.b64decode(f["data"])) for f in files) > MAX_BYTES:
        raise ProvisionError("Environment exceeds the bundle size limit")
    if len(files) > MAX_FILES:
        raise ProvisionError("Environment contains too many files")
    revision_data = {"kind": kind, "version": version, "profiles": profiles, "path_mappings": path_mappings,
                     "dependencies": inventory.records, "config_mappings": inventory.mappings,
                     "credential_files": inventory.credentials,
                     "files": [{k: f[k] for k in ("path", "sha256", "executable")} for f in files]}
    settings = profile_launch_settings(home, kind, config_entries, profiles, dependency_path)
    revision_data["profile_settings"] = settings
    if source:
        revision_data["source"] = source
    revision = digest(revision_data)
    label = home.parent.name + "/.claude" if home.name == ".claude" and home.parent.name.startswith(".claude") else home.name
    if source:
        label = source.get("name") or source["relative_home"]
        if label == ".":
            label = Path(source["path"]).stem if source["kind"] == "zip" else Path(source["path"]).name
    return {"bundle_version": 1, "kind": kind, "display_name": kind + " / " + label,
            "source_home": str(home), "source_user_home": str(user_home),
            "cli_version": version, "native_profiles": profiles, "source_revision": revision,
            "source_environment_id": "env-" + digest([kind, source["kind"], source["path"], source["relative_home"]] if source else [kind, str(home)])[:24], "files": files,
            "path_mappings": path_mappings, "config_mappings": inventory.mappings,
            "dependencies": inventory.records, "credential_files": inventory.credentials,
            "profile_settings": settings, "plugin_configs": plugin_configs,
            "config_files": sorted(set(inventory.config_files)),
            "native_login_required": any(
                ((c.get("model_provider", "openai") == "openai" and not any(
                    c.get("model_providers", {}).get("openai", {}).get(k) for k in ("env_key", "auth", "experimental_bearer_token"))) or
                 c.get("model_providers", {}).get(c.get("model_provider"), {}).get("requires_openai_auth"))
                for c in inventory.effective.values()) if kind == "codex" else not config.get("apiKeyHelper") and not config.get("env", {}).get("ANTHROPIC_API_KEY") and not config.get("env", {}).get("ANTHROPIC_AUTH_TOKEN")}


def contains_credentials(value, context=""):
    if isinstance(value, dict):
        for key, item in value.items():
            normalized = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", key).replace("-", "_").lower()
            # Native settings can name an env var/helper without carrying its
            # credential. Keep these references intact for destination setup.
            declaration = context == "env_http_headers" or normalized.endswith(("_env_var", "_env_key")) or normalized in {"env_key", "api_key_helper"}
            if SECRET_KEY.search(normalized) and isinstance(item, str) and item and not declaration:
                remaining = re.sub(r"\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}|\$[A-Za-z_][A-Za-z0-9_]*", "", item)
                if remaining.strip().lower() not in {"", "bearer", "basic"}:
                    return True
            if contains_credentials(item, normalized):
                return True
        return False
    if isinstance(value, list):
        for index, item in enumerate(value[:-1]):
            if isinstance(item, str) and item.startswith("--") and SECRET_KEY.search(item[2:].replace("-", "_")):
                following = value[index + 1]
                if isinstance(following, str) and following and not following.startswith("$"):
                    return True
        return any(contains_credentials(v, context) for v in value)
    return False


def collection_member(name):
    if not isinstance(name, str) or not name or "\x00" in name or "\\" in name:
        raise ProvisionError("Invalid collection member path")
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or re.match(r"^[A-Za-z]:", name):
        raise ProvisionError("Invalid collection member path")
    return path


def file_hash(path, limit=MAX_COLLECTION_BYTES):
    if path.stat().st_size > limit:
        raise ProvisionError("Collection exceeds the size limit")
    sha = hashlib.sha256()
    total = 0
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            total += len(chunk)
            if total > limit:
                raise ProvisionError("Collection exceeds the size limit")
            sha.update(chunk)
    return sha.hexdigest()


def verify_source_cache(root, path, archive_sha):
    try:
        marker = root / ".taskr-source.json"
        if not marker.exists():
            marker = root / ".mmux-source.json"
        manifest = read_config(marker)
        if manifest.get("version") != 1 or manifest.get("path") != str(path) or manifest.get("archive_sha256") != archive_sha:
            raise ProvisionError("Source cache is not owned by this import")
        for entry in manifest["files"]:
            member = collection_member(entry["path"])
            file = root.joinpath(*member.parts)
            if file.is_symlink() or not file.resolve(strict=True).is_relative_to(root) or file_hash(file, MAX_BYTES) != entry["sha256"] or bool(file.stat().st_mode & 0o111) != entry["executable"]:
                raise ProvisionError("Imported source cache changed; rediscover the archive")
        if any(p.is_symlink() for p in root.rglob("*")):
            raise ProvisionError("Imported source cache changed; rediscover the archive")
        actual = {str(p.relative_to(root)) for p in root.rglob("*") if not p.is_dir() and p != marker}
        if actual != {e["path"] for e in manifest["files"]}:
            raise ProvisionError("Imported source cache changed; rediscover the archive")
    except (OSError, KeyError):
        raise ProvisionError("Imported source cache is incomplete; rediscover the archive")


def import_zip(path, cache_root):
    archive_sha = file_hash(path)
    cache_root = cache_root.absolute()
    if any(p.is_symlink() for p in [cache_root, *cache_root.parents]):
        raise ProvisionError("Source cache root must not contain symlinks")
    cache_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    cache_root.chmod(0o700)
    final = cache_root / ("zip-" + digest([str(path), archive_sha])[:32])
    if final.exists():
        verify_source_cache(final, path, archive_sha)
        return final, archive_sha
    stage = Path(tempfile.mkdtemp(prefix=".import-", dir=cache_root))
    try:
        files, seen, total = [], set(), 0
        with zipfile.ZipFile(path) as archive:
            entries = archive.infolist()
            if len(entries) > MAX_COLLECTION_FILES:
                raise ProvisionError("Collection exceeds the file limit")
            # Validate everything before writing any archive member.
            for entry in entries:
                member = collection_member(entry.filename)
                canonical = str(member)
                mode = entry.external_attr >> 16
                if canonical == "." or member.parts[0] == ".taskr-source.json" or canonical in seen or entry.flag_bits & 1 or stat.S_IFMT(mode) not in {0, stat.S_IFREG, stat.S_IFDIR}:
                    raise ProvisionError("Unsupported, encrypted or duplicate ZIP member")
                if entry.file_size > MAX_BYTES:
                    raise ProvisionError("Collection file exceeds the size limit")
                total += entry.file_size
                if total > MAX_COLLECTION_BYTES:
                    raise ProvisionError("Collection exceeds the expanded size limit")
                seen.add(canonical)
            total = 0
            for entry in entries:
                member = collection_member(entry.filename)
                destination = stage.joinpath(*member.parts)
                destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                if entry.is_dir():
                    destination.mkdir(exist_ok=True, mode=0o700)
                    continue
                sha, size = hashlib.sha256(), 0
                with archive.open(entry) as incoming, destination.open("xb") as output:
                    for chunk in iter(lambda: incoming.read(1024 * 1024), b""):
                        size += len(chunk)
                        total += len(chunk)
                        if size > MAX_BYTES or total > MAX_COLLECTION_BYTES:
                            raise ProvisionError("Collection exceeds the expanded size limit")
                        sha.update(chunk)
                        output.write(chunk)
                executable = bool((entry.external_attr >> 16) & 0o111)
                destination.chmod(0o700 if executable else 0o600)
                files.append({"path": str(member), "sha256": sha.hexdigest(), "executable": executable})
        if file_hash(path) != archive_sha:
            raise ProvisionError("Source archive changed during import; discover it again")
        marker = stage / ".taskr-source.json"
        marker.write_text(json.dumps({"version": 1, "path": str(path), "archive_sha256": archive_sha, "files": files}))
        marker.chmod(0o600)
        for directory in stage.rglob("*"):
            if directory.is_dir():
                directory.chmod(0o700)
        try:
            stage.rename(final)
        except FileExistsError:
            verify_source_cache(final, path, archive_sha)
        return final, archive_sha
    except (zipfile.BadZipFile, OSError, RuntimeError):
        raise ProvisionError("Invalid ZIP collection or conflicting member paths")
    finally:
        if stage.exists():
            shutil.rmtree(stage)


def imported_candidates(path, root, archive_sha, cache_root=None):
    manifest = root / "taskr-environments.json"
    if not manifest.exists() and cache_root is not None:
        manifest = root / "mmux-environments.json"
    records = []
    if manifest.is_file():
        if not manifest.resolve().is_relative_to(root):
            raise ProvisionError("Import manifest escapes the collection folder")
        config = read_config(manifest)
        if set(config) != {"version", "environments"} or config["version"] != 1 or not isinstance(config["environments"], list):
            raise ProvisionError("Invalid taskr-environments.json manifest")
        if len(config["environments"]) > MAX_FILES:
            raise ProvisionError("Collection contains too many environments")
        seen = set()
        for entry in config["environments"]:
            if not isinstance(entry, dict) or set(entry) - {"home", "kind", "name", "original_home", "original_user_home", "user_skills", "cli_version"} or not isinstance(entry.get("kind"), str) or entry["kind"] not in AUTH_FILES or not isinstance(entry.get("home"), str):
                raise ProvisionError("Invalid environment manifest entry")
            if any(not isinstance(v, str) or not v for v in entry.values()):
                raise ProvisionError("Manifest entry values must be nonempty strings")
            relative = collection_member(entry["home"])
            home = root.joinpath(*relative.parts).resolve(strict=True)
            if not home.is_dir() or not home.is_relative_to(root) or home in seen:
                raise ProvisionError("Invalid or duplicate manifest home")
            seen.add(home)
            for key in ("original_home", "original_user_home"):
                if key in entry and not Path(entry[key]).is_absolute():
                    raise ProvisionError("Original home paths must be absolute")
            if "user_skills" in entry:
                extra = root.joinpath(*collection_member(entry["user_skills"]).parts).resolve(strict=True)
                if not extra.is_dir() or not extra.is_relative_to(root):
                    raise ProvisionError("Manifest user skills escape the collection folder")
            records.append((home, entry["kind"], entry))
    else:
        count = 0
        def scan(directory, depth=0):
            nonlocal count
            count += 1
            if count > MAX_COLLECTION_FILES or depth > 32:
                raise ProvisionError("Collection exceeds the discovery depth/file limit")
            kinds = [kind for kind, name in (("codex", "config.toml"), ("claude", "settings.json")) if (directory / name).is_file()]
            if len(kinds) > 1:
                raise ProvisionError("Ambiguous native home; use taskr-environments.json")
            if kinds:
                records.append((directory, kinds[0], {}))
                return
            for child in directory.iterdir():
                count += 1
                if count > MAX_COLLECTION_FILES:
                    raise ProvisionError("Collection exceeds the discovery depth/file limit")
                if child.name in {".git", "node_modules", "__pycache__", ".agents", "sessions", "logs"}:
                    continue
                if child.is_dir() and not child.is_symlink():
                    scan(child, depth + 1)
        scan(root)
    if not records:
        raise ProvisionError("No native environments found in the collection")
    return [(home, kind, {"kind": "zip" if archive_sha else "folder", "path": str(path), "root": str(root),
        **({"cache_root": str(cache_root)} if cache_root else {}),
        "relative_home": str(home.relative_to(root)) or ".", **({"archive_sha256": archive_sha} if archive_sha else {}),
        **{key: entry[key] for key in ("name", "original_home", "original_user_home", "user_skills", "cli_version") if key in entry}})
        for home, kind, entry in sorted(records, key=lambda r: str(r[0]))]


def validate_import(source, home):
    root = Path(source["root"]).resolve(strict=True)
    expected = root.joinpath(*collection_member(source["relative_home"]).parts).resolve(strict=True)
    if expected != home or not home.is_relative_to(root):
        raise ProvisionError("Invalid imported source location")
    if source["kind"] == "zip":
        archive = Path(source["path"])
        if file_hash(archive) != source["archive_sha256"]:
            raise ProvisionError("Source archive changed; discover it again before syncing")
        cache_root = Path(source.get("cache_root") or source["root"]).resolve(strict=True)
        if not root.is_relative_to(cache_root):
            raise ProvisionError("Invalid imported source cache location")
        verify_source_cache(cache_root, archive, source["archive_sha256"])
    elif source["kind"] != "folder" or root != Path(source["path"]).resolve(strict=True):
        raise ProvisionError("Invalid imported source location")
    candidates = imported_candidates(Path(source["path"]), root, source.get("archive_sha256"), source.get("cache_root"))
    if not any(candidate == source for _, _, candidate in candidates):
        raise ProvisionError("Source collection manifest changed; discover it again before syncing")


def discover(request):
    explicit = request.get("homes")
    source_path = request.get("source_path")
    if explicit is not None and source_path is not None:
        raise ProvisionError("homes and source_path are mutually exclusive")
    if source_path is not None:
        if not source_path.strip():
            raise ProvisionError("source_path must not be empty")
        try:
            path = Path(source_path).expanduser().resolve(strict=True)
        except OSError:
            raise ProvisionError("Source collection is missing or unreadable")
        if path.is_dir():
            root, archive_sha, cache_root = path, None, None
        elif path.is_file() and path.suffix.lower() == ".zip":
            root, archive_sha = import_zip(path, Path(request["cache_root"]))
            cache_root = root
            # A usual archive wraps the collection in a single directory.
            for _ in range(32):
                if any((root / name).is_file() for name in ("taskr-environments.json", "mmux-environments.json", "config.toml", "settings.json")):
                    break
                children = [p for p in root.iterdir() if p.name not in {".taskr-source.json", ".mmux-source.json"}]
                if len(children) != 1 or not children[0].is_dir():
                    break
                root = children[0]
        else:
            raise ProvisionError("source_path must be a folder or ZIP archive")
        candidates = imported_candidates(path, root, archive_sha, cache_root)
    else:
        candidates = None
    homes = [] if candidates is not None else [Path(p).expanduser().resolve() for p in explicit] if explicit is not None else sorted(
        {q.resolve() for pattern in (".codex*", ".claude*") for p in Path.home().glob(pattern)
         for q in (p, p / ".claude") if q.is_dir()})
    environments, issues = [], []
    for home, kind, source in candidates if candidates is not None else [(h, None, None) for h in homes]:
        kind = kind or ("codex" if (home / "config.toml").is_file() else "claude" if (
            home / "settings.json").is_file() else None)
        if not kind:
            continue
        try:
            bundle = export(home, kind, source)
            environment = {k: bundle[k] for k in ("source_environment_id", "source_home", "display_name",
                "source_revision", "kind", "cli_version", "native_profiles", "dependencies", "profile_settings")}
            if source:
                environment["source_location"] = source
            environments.append(environment)
        except EnvironmentSetupError as error:
            # Assignment is a domain choice, not a claim that files can already
            # be deployed. Keep valid homes visible with their concrete blocker.
            config_path = home / "settings.json"
            config = read_config(config_path)
            version = (source or {}).get("cli_version") or native_version(kind)
            identity = [kind, source["kind"], source["path"], source["relative_home"]] if source else [kind, str(home)]
            label = (source or {}).get("name") or (home.parent.name + "/.claude" if home.name == ".claude" and home.parent.name.startswith(".claude") else home.name)
            settings = profile_launch_settings(home, kind, [(config_path, "settings.json", config)], [], lambda value, base=None: home / value)
            environment = {"source_environment_id": "env-" + digest(identity)[:24],
                           "source_home": str(home), "display_name": kind + " / " + label,
                           "kind": kind, "cli_version": version, "native_profiles": [], "dependencies": [],
                           "profile_settings": settings, "sync_blockers": [str(error)],
                           "source_revision": digest([identity, file_hash(config_path, MAX_BYTES), str(error)])}
            if source:
                environment["source_location"] = source
            environments.append(environment)
            issues.append({"source_environment_id": environment["source_environment_id"],
                           "source_home": str(home), "error": str(error)})
        except (ProvisionError, OSError) as error:
            if isinstance(error, OSError):
                error = ProvisionError("Source environment is missing or unreadable")
            issues.append({"source_home": str(home), "error": str(error)})
    return {"environments": environments, "issues": issues}


def export_request(request):
    home = Path(request["source_home"]).expanduser().resolve()
    source = request.get("source_location")
    if source:
        validate_import(source, home)
    bundle = export(home, request["kind"], source)
    if bundle["source_revision"] != request["source_revision"]:
        raise ProvisionError("Source environment changed; discover it again before syncing")
    policy = request["credential_policy"]
    if policy not in {"endpoint", "copy"}:
        raise ProvisionError("Unknown credential policy")
    if policy == "endpoint":
        for entry in bundle["files"]:
            if entry["path"].endswith((".toml", ".json")):
                raw = base64.b64decode(entry["data"]).decode()
                try:
                    config = tomllib.loads(raw) if entry["path"].endswith(".toml") else json.loads(raw)
                except ValueError:
                    continue
                if contains_credentials(config):
                    raise ProvisionError("Configuration embeds credentials; explicitly select copy policy or remove them")
    else:
        for relative, reference in bundle.get("credential_files", {}).items():
            if not Path(reference).is_file():
                raise ProvisionError("Source helper credential file is missing; supply it or select endpoint policy")
            entry = file_entry(Path(reference), "home/" + relative, include_login=True)
            entry["executable"] = False
            bundle["files"].append(entry)
        if (home / AUTH_FILES[bundle["kind"]]).is_file():
            auth_file = home / AUTH_FILES[bundle["kind"]]
            if source and not auth_file.resolve().is_relative_to(Path(source["root"])):
                raise ProvisionError("Imported login file escapes the collection folder")
            bundle["files"].append(file_entry(auth_file,
                                               "home/" + AUTH_FILES[bundle["kind"]], include_login=True))
    bundle["credential_policy"] = policy
    bundle["bundle_digest"] = digest(bundle)
    return bundle


def safe_member(name):
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "home" or "\\" in name:
        raise ProvisionError("Invalid environment bundle member")
    return path


def adapt_text(raw, source, final_home, suffix, relative=None):
    pairs = {source["source_home"]: str(final_home),
             source["source_user_home"] + "/.agents/skills": str(final_home / "skills/_user_agents")}
    pairs.update({old: str(final_home / relative) for old, relative in source.get("path_mappings", {}).items()})
    def mapped(value):
        if value.startswith("@program:"):
            program = value[len("@program:"):]
            return str(Path(program).expanduser()) if program.startswith("~/") else program
        if value.startswith("@argument:"):
            flag, path = value[len("@argument:"):].split("=", 1)
            return flag + "=" + str(final_home / path)
        return str(final_home / value)
    pairs.update({old: mapped(value) for old, value in source.get("config_mappings", {}).get(relative, {}).items()})
    ordered = sorted(pairs, key=len, reverse=True)
    absolute = [p for p in ordered if p.startswith(("/", "~/", "--"))]
    pattern = re.compile(r"(?<![A-Za-z0-9_./-])(?:" + "|".join(re.escape(p) for p in absolute) + r")(?=$|[/\s\"'])")
    def replace(value):
        for old in ordered:
            if value == old or value.startswith(old + "/"):
                return pairs[old] + value[len(old):]
        return pattern.sub(lambda m: pairs[m.group()], value)
    if suffix == ".json":
        def visit(value):
            if isinstance(value, dict):
                return {replace(k): visit(v) for k, v in value.items()}
            if isinstance(value, list):
                return [visit(v) for v in value]
            return replace(value) if isinstance(value, str) else value
        return json.dumps(visit(json.loads(raw)), ensure_ascii=False, indent=2)
    if suffix == ".toml":
        # Rewrite string tokens, not TOML syntax; convert literal strings to
        # basic strings when the destination needs escaping. Comments survive.
        tokens = re.compile(r'"""[\s\S]*?"""|\x27\x27\x27[\s\S]*?\x27\x27\x27|"(?:\\.|[^"\\])*"|\x27[^\x27]*\x27')
        def token(match):
            value = tomllib.loads("value = " + match.group())["value"]
            adapted = replace(value)
            return json.dumps(adapted, ensure_ascii=False) if adapted != value else match.group()
        return tokens.sub(token, raw)
    return replace(raw)


def validate_dependencies(config, final_home, included, remote):
    # Native state must remain beside the deployed config, not an old shared DB.
    for key in ("sqlite_home", "history_file", "log_dir"):
        if key in config:
            raise ProvisionError("Explicit native state path needs removal/adaptation before sync: " + key)
    def visit(value, key=""):
        if isinstance(value, dict):
            for k, v in value.items():
                visit(v, k)
        elif isinstance(value, list):
            for v in value:
                visit(v, key)
        elif isinstance(value, str):
            if key in PATH_KEYS:
                p = Path(value).expanduser()
                if not p.is_absolute():
                    p = final_home / p
                if p.is_relative_to(final_home):
                    relative = str(p.relative_to(final_home))
                    if relative not in included and not any(name.startswith(relative + "/") for name in included):
                        raise ProvisionError("A bundled configuration dependency is missing")
                elif not p.exists():
                    raise ProvisionError("A referenced configuration dependency is missing on the endpoint")
            if remote and key in {"url", "base_url"} and re.search(r"https?://(localhost|127\.0\.0\.1|\[::1\])([:/]|$)", value):
                raise ProvisionError("A localhost service requires explicit destination configuration before remote sync")
    visit(config)
    for server in config.get("mcp_servers", {}).values():
        command = server.get("command")
        if command and not shutil.which(command) and not (
                Path(command).is_relative_to(final_home) and str(Path(command).relative_to(final_home)) in included):
            raise ProvisionError("An MCP executable is missing on the endpoint")
    # Inspect native hook executables and interpreter scripts without running them.
    for command in hook_commands(config):
        program, script = hook_command_parts(command)
        for reference in [program] + ([script] if script else []):
            path = Path(reference).expanduser()
            if path.is_absolute():
                if path.is_relative_to(final_home):
                    if str(path.relative_to(final_home)) not in included:
                        raise ProvisionError("A bundled hook dependency is missing")
                elif not path.is_file():
                    raise ProvisionError("A hook dependency is missing on the endpoint")
            elif reference == program and not shutil.which(program):
                raise ProvisionError("A hook executable is missing on the endpoint")


def preflight_dependencies(dependencies, home, included=None, selected=None, all_profiles=False):
    """Check metadata only: never evaluate helper bodies or fetch a provider."""
    missing_environment = set()
    for item in dependencies:
        if not item["profiles"] or (not all_profiles and selected not in item["profiles"]):
            continue
        kind, reference = item["kind"], item["reference"]
        if kind == "environment":
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", reference):
                raise ProvisionError("Invalid required environment variable name")
            if not os.environ.get(reference):
                missing_environment.add(reference)
        elif kind == "executable":
            program = str(Path(reference).expanduser()) if reference.startswith("~/") else reference
            if not shutil.which(program):
                context = "MCP" if item["context"] in {"mcp_servers", "mcpServers"} else "Command"
                raise ProvisionError(context + " executable is missing on the endpoint: " + reference)
        else:
            safe_member("home/" + reference)
            path = home / reference
            if included is not None:
                exists = reference in included or any(p.startswith(reference + "/") for p in included)
                if kind == "directory":
                    exists = True  # created explicitly during publication
            else:
                exists = path.is_dir() if kind == "directory" else path.is_file() and not path.is_symlink()
            if not exists:
                raise ProvisionError("A bundled " + ("hook" if item["context"] == "hooks" else "command") + " dependency is missing")
            if kind == "bundled_executable" and included is None and not os.access(path, os.X_OK):
                raise ProvisionError("A bundled command executable is not executable")
    return sorted(missing_environment)


def prepare(request):
    bundle = request["bundle"]
    if bundle.get("bundle_version") != 1:
        raise ProvisionError("Unsupported environment bundle version")
    expected = bundle["bundle_digest"]
    if digest({k: v for k, v in bundle.items() if k != "bundle_digest"}) != expected:
        raise ProvisionError("Environment bundle digest mismatch")
    version = native_version(bundle["kind"])
    if tuple(map(int, version.split("."))) < tuple(map(int, bundle["cli_version"].split("."))):
        raise ProvisionError("Destination coding CLI is older than the selected source environment")
    # Caller chooses a managed root, never an arbitrary destination home.
    root = Path(request.get("deployment_root") or "~/.local/share/taskr/environments").expanduser().absolute()
    if any(p.is_symlink() for p in [root, *root.parents]):
        raise ProvisionError("Managed deployment root must not contain symlinks")
    auth = None
    endpoint_credentials = {}
    auth_name = AUTH_FILES[bundle["kind"]]
    if bundle["credential_policy"] == "endpoint":
        auth_home = request.get("endpoint_auth_home")
        if not auth_home:
            raise ProvisionError("endpoint credential policy requires endpoint_auth_home")
        auth_root = Path(auth_home).expanduser()
        if bundle.get("native_login_required", True):
            auth_file = auth_root / auth_name
            if not auth_file.is_file():
                raise ProvisionError("Selected endpoint login file is missing; authenticate that home first")
            if auth_file.stat().st_size > MAX_BYTES:
                raise ProvisionError("Native login file exceeds the bundle size limit")
            auth = auth_file.read_bytes()
            if len(auth) > MAX_BYTES:
                raise ProvisionError("Native login file exceeds the bundle size limit")
        for relative in bundle.get("credential_files", {}):
            safe_member("home/" + relative)
            credential = auth_root / relative
            if not credential.is_file() or not credential.resolve().is_relative_to(auth_root.resolve()):
                raise ProvisionError("Selected endpoint helper credential file is missing or escapes its home")
            if credential.stat().st_size > MAX_BYTES:
                raise ProvisionError("Credential file exceeds the bundle size limit")
            endpoint_credentials[relative] = credential.read_bytes()
    deployment_id = "dep-" + digest([expected, str(root), request.get("endpoint_auth_home"),
                                    hashlib.sha256(auth).hexdigest() if auth is not None else None,
                                    {p: hashlib.sha256(v).hexdigest() for p, v in endpoint_credentials.items()}])[:32]
    final = root / deployment_id
    manifest_path = final / ".taskr-environment.json"
    # Dry-run validates without creating the managed root or staging files.
    total = 0
    seen = set()
    contents = []
    configs = []
    pinned_marketplaces = set()
    if bundle["kind"] == "claude":
        for entry in bundle["files"]:
            if entry["path"] == "home/plugins/known_marketplaces.json":
                pinned_marketplaces.update(json.loads(base64.b64decode(entry["data"])))
    credential_paths = set(bundle.get("credential_files", {})) | {auth_name}
    for entry in bundle["files"]:
        member = safe_member(entry["path"])
        if str(member) in seen or len(seen) >= MAX_FILES:
            raise ProvisionError("Duplicate or excessive bundle members")
        seen.add(str(member))
        raw = base64.b64decode(entry["data"], validate=True)
        total += len(raw)
        if total > MAX_BYTES or hashlib.sha256(raw).hexdigest() != entry["sha256"]:
            raise ProvisionError("Invalid or excessive bundle contents")
        destination = final.joinpath(*member.parts[1:])
        relative = str(member.relative_to("home"))
        if relative not in credential_paths and destination.suffix in {".toml", ".json", ".md", ".sh", ".py", ".js", ".mjs", ".cjs"}:
            raw = adapt_text(raw.decode(), bundle, final, destination.suffix, relative).encode()
        if bundle["kind"] == "claude" and relative == "settings.json":
            settings = json.loads(raw)
            for name, declaration in settings.get("extraKnownMarketplaces", {}).items():
                if name in pinned_marketplaces and isinstance(declaration, dict):
                    declaration["autoUpdate"] = False
            raw = json.dumps(settings, ensure_ascii=False, indent=2).encode()
        if relative in bundle.get("config_files", []) and member.suffix == ".toml":
            configs.append((tomllib.loads(raw.decode()), relative))
        elif member.name == "config.toml" or member.name.endswith(".config.toml"):
            configs.append((tomllib.loads(raw.decode()), relative))
        elif relative in bundle.get("config_files", []) or member.name in {"settings.json", "hooks.json"}:
            configs.append((json.loads(raw.decode()), relative))
        contents.append((member, raw, entry["executable"]))
    for relative, raw in endpoint_credentials.items():
        if "home/" + relative in seen:
            raise ProvisionError("Source credentials must be excluded under endpoint policy")
        contents.append((PurePosixPath("home/" + relative), raw, False))
        total += len(raw)
    if total > MAX_BYTES or len(contents) > MAX_FILES:
        raise ProvisionError("Environment exceeds the bundle size/file limit")
    included = {str(member.relative_to("home")) for member, _, _ in contents}
    dependencies = bundle.get("dependencies", [])
    preflight_dependencies(dependencies, final, included, all_profiles=True)
    for settings, relative in configs:
        if relative in bundle.get("plugin_configs", {}):
            settings = plugin_root_values(settings, final / bundle["plugin_configs"][relative])
        validate_dependencies(settings, final, included, request.get("remote", False))
    readiness = [{"native_profile": name,
                  "missing_environment": preflight_dependencies(dependencies, final, included, selected=name)}
                 for name in [None, *bundle["native_profiles"]]]
    for entry in readiness:
        entry["state"] = "blocked" if entry["missing_environment"] else "ready"
    profile_settings = json.loads(json.dumps(bundle.get("profile_settings", [])))
    if bundle["kind"] == "claude":
        # Capability flags belong to the destination CLI, including imports
        # whose declared source version differs from the controller's CLI.
        supported = native_reasoning_efforts("claude")
        for settings in profile_settings:
            for option in settings.get("model_options", []):
                option["reasoning_efforts"] = supported
    policy = bundle["credential_policy"]
    # Copy policy may use provider credentials embedded in config rather than a
    # login file. Readiness does not claim provider connectivity or authorization.
    result = {"deployment_id": deployment_id, "bundle_digest": expected,
              "source_environment_id": bundle["source_environment_id"],
              "source_revision": bundle["source_revision"], "kind": bundle["kind"],
              "display_name": bundle["display_name"],
              "native_profiles": bundle["native_profiles"], "home": str(final),
              "cli_version": version, "credential_policy": policy,
              "dependencies": dependencies, "profile_readiness": readiness,
              "profile_settings": profile_settings,
              "authentication": "file_provisioned" if auth is not None or "home/" + auth_name in seen else "native_provider_config"}
    if final.exists():
        previous = verify({"home": str(final), "deployment_id": deployment_id, "preparing": True})
        if previous.get("bundle_digest") != expected:
            raise ProvisionError("Owned environment identity mismatch")
        previous["profile_readiness"] = readiness
        if request.get("dry_run"):
            return {**previous, "dry_run": True, "file_count": len(contents), "bytes": total}
        return previous
    if request.get("dry_run"):
        return {**result, "dry_run": True, "file_count": len(contents), "bytes": total}
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    if root.is_symlink():
        raise ProvisionError("Managed deployment root must not be a symlink")
    stage = Path(tempfile.mkdtemp(prefix=".staging-", dir=root))
    try:
        for item in dependencies:
            if item["kind"] == "directory":
                safe_member("home/" + item["reference"])
                (stage / item["reference"]).mkdir(parents=True, exist_ok=True, mode=0o700)
        for member, raw, executable in contents:
            dest = stage.joinpath(*member.parts[1:])
            dest.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            dest.write_bytes(raw)
            dest.chmod(0o700 if executable else 0o600)
        if auth is not None:
            (stage / auth_name).write_bytes(auth)
            (stage / auth_name).chmod(0o600)
        integrity = {str(member.relative_to("home")): hashlib.sha256(raw).hexdigest()
                     for member, raw, _ in contents if member.name != auth_name}
        config_integrity = {str(member.relative_to("home")): digest(pinned_config(tomllib.loads(raw.decode())))
                            for member, raw, _ in contents if bundle["kind"] == "codex" and len(member.parts) == 2
                            and (member.name == "config.toml" or member.name.endswith(".config.toml"))}
        if bundle["kind"] == "claude":
            config_integrity.update({str(member.relative_to("home")): digest(pinned_claude_registry(json.loads(raw.decode())))
                                     for member, raw, _ in contents if str(member) == "home/.claude.json"})
        manifest = {**result, "integrity": integrity, "config_integrity": config_integrity}
        (stage / ".taskr-environment.json").write_text(json.dumps(manifest))
        (stage / ".taskr-environment.json").chmod(0o600)
        # Rename publishes only a complete environment. Concurrent identical
        # requests may have published first; verify ownership in that case.
        try:
            stage.rename(final)
        except OSError:
            if not final.is_dir() or not manifest_path.is_file():
                raise
            if json.loads(manifest_path.read_text()) != manifest:
                raise ProvisionError("Concurrent environment publication mismatch")
        return result
    finally:
        if stage.exists():
            shutil.rmtree(stage)


def verify(request):
    path = Path(request["home"])
    manifest = path / ".taskr-environment.json"
    # Retained deployments keep their original home, files and conversations.
    # Read the historical ownership marker; all new publications use Taskr.
    if not manifest.exists():
        manifest = path / ".mmux-environment.json"
    if path.is_symlink() or not manifest.is_file():
        raise ProvisionError("Prepared environment is missing or no longer owned")
    result = json.loads(manifest.read_text())
    if result["deployment_id"] != request["deployment_id"] or result["home"] != str(path):
        raise ProvisionError("Prepared environment identity mismatch")
    version = native_version(result["kind"])
    if tuple(map(int, version.split("."))) < tuple(map(int, result["cli_version"].split("."))):
        raise ProvisionError("Destination coding CLI is older than the prepared environment")
    for relative, expected in result.get("integrity", {}).items():
        member = safe_member("home/" + relative)
        file = path.joinpath(*member.parts[1:])
        if file.is_symlink() or not file.is_file():
            raise ProvisionError("Prepared configuration or skills changed; sync a new revision")
        if hashlib.sha256(file.read_bytes()).hexdigest() != expected:
            semantic = result.get("config_integrity", {}).get(relative)
            semantic_match = semantic is not None and (
                (result["kind"] == "codex" and file.suffix == ".toml" and digest(pinned_config(read_config(file))) == semantic)
                or (result["kind"] == "claude" and file.name == ".claude.json" and digest(pinned_claude_registry(read_config(file))) == semantic))
            if not semantic_match:
                raise ProvisionError("Prepared configuration or skills changed; sync a new revision")
    if result["authentication"] == "file_provisioned" and not (path / AUTH_FILES[result["kind"]]).is_file():
        raise ProvisionError("Prepared native login file is missing")
    missing = preflight_dependencies(result.get("dependencies", []), path, selected=request.get("native_profile"))
    if missing and not request.get("preparing"):
        raise ProvisionError("Required endpoint environment variables are missing: " + ", ".join(missing))
    return result


def verify_resume(request):
    home = Path(request["home"])
    workspace = Path(request["workspace_path"]).expanduser()
    session = request["session"]
    if not session or session.startswith("-") or not re.fullmatch(r"[A-Za-z0-9_-]+", session):
        raise ProvisionError("Invalid native session ID")
    if not home.is_dir() or not workspace.is_dir():
        raise ProvisionError("Saved conversation home or workspace has not been restored")
    kind = request["kind"]
    candidates = []
    if kind == "codex":
        for directory in (home / "sessions", home / "archived_sessions"):
            if directory.is_dir():
                candidates.extend(directory.rglob("*" + session + ".jsonl"))
    elif kind == "claude":
        directory = home / "projects"
        if directory.is_dir():
            candidates.extend(directory.rglob(session + ".jsonl"))
    elif kind == "kimi":
        directory = home / "sessions"
        if directory.is_dir():
            candidates.extend(directory.glob("*/" + session + "/context.jsonl"))
            candidates.extend(directory.glob(session + "/context.jsonl"))
    elif kind == "opencode":
        # Session data is separate from OPENCODE_CONFIG_DIR. Preserve the
        # original XDG data home/OPENCODE_DB as part of the frozen environment.
        data = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share"))) / "opencode"
        directory = data / "storage/session"
        if directory.is_dir():
            candidates.extend(directory.glob("*/" + session + ".json"))
        database = Path(os.environ.get("OPENCODE_DB", str(data / "opencode.db")))
        if database.is_file():
            import sqlite3
            try:
                with sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True) as db:
                    if db.execute("SELECT 1 FROM session WHERE id = ? LIMIT 1", (session,)).fetchone():
                        return {"history_available": True}
            except sqlite3.Error:
                raise ProvisionError("Unsupported native session database; restore compatible history")
    else:
        raise ProvisionError("Native history verification is unsupported for this agent kind")
    if not any(path.is_file() and not path.is_symlink() and path.stat().st_size > 0 for path in candidates):
        raise ProvisionError("Native conversation history is missing; restore the saved home/data and workspace before resuming")
    return {"history_available": True}


def main():
    try:
        request = json.loads(sys.stdin.buffer.read(2 * MAX_BYTES + 1))
        operation = request["operation"]
        if operation == "discover":
            result = discover(request)
        elif operation == "export":
            result = export_request(request)
        elif operation == "prepare":
            result = prepare(request)
        elif operation == "verify":
            result = verify(request)
        elif operation == "verify_resume":
            result = verify_resume(request)
        else:
            raise ProvisionError("Unknown companion operation")
        print(json.dumps({"result": result}))
    except ProvisionError as error:
        print(json.dumps({"error": str(error)}))
        sys.exit(1)
    except Exception:
        # Do not echo source text, bundle data, native subprocess output, or
        # credentials through controller logs/errors.
        print(json.dumps({"error": "Companion request failed; check native format, permissions, and dependencies"}))
        sys.exit(1)


if __name__ == "__main__":
    main()
