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


class ProvisionError(Exception):
    pass


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
                if child.name in {".git", "__pycache__", ".DS_Store", "sessions", "archived_sessions", "logs", *AUTH_FILES.values()}:
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
    def dependency_path(value):
        if source:
            path = Path(value)
            if value.startswith("~/"):
                path = boundary / value[2:]
            elif original_home and path.is_relative_to(original_home):
                path = home / path.relative_to(original_home)
            elif original_user and path.is_relative_to(original_user):
                path = boundary / path.relative_to(original_user)
            elif not path.is_absolute():
                path = home / path
        else:
            path = Path(value).expanduser()
            path = home / path if not path.is_absolute() else path
        if not path.exists():
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
    path_mappings = {original_home: "."} if original_home else {}
    profiles = []
    if kind == "codex":
        for profile in sorted(home.glob("*.config.toml")):
            name = profile.name[:-len(".config.toml")]
            if not re.fullmatch(r"[A-Za-z0-9_-]+", name):
                raise ProvisionError("Invalid native profile filename")
            configs.append(read_config(checked(profile)))
            profiles.append(name)
            files.append(file_entry(checked(profile), "home/" + profile.name))
    for name in ("AGENTS.md", "CLAUDE.md", "hooks.json"):
        if (home / name).is_file():
            files.append(file_entry(checked(home / name), "home/" + name))
            if name == "hooks.json":
                configs.append(read_config(checked(home / name)))
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
    elif any(c.get("enabledPlugins") or c.get("extraKnownMarketplaces") for c in configs):
        # Claude installation records can mix project and user scopes. Refuse
        # incomplete clones rather than silently losing enabled plugins.
        raise ProvisionError("Claude plugin cloning is unsupported; provision a plugin-free source or its dependencies first")
    def dependencies(value, key=""):
        if isinstance(value, dict):
            for k, v in value.items():
                dependencies(v, k)
        elif isinstance(value, list):
            for v in value:
                dependencies(v, key)
        elif isinstance(value, str) and key in PATH_KEYS:
            path = dependency_path(value)
            destination = str(path.relative_to(home)) if path.is_relative_to(home) else (
                "dependencies/files/" + digest(str(path))[:20] + "/" + path.name)
            if not path.is_relative_to(home) or source:
                path_mappings[str(path)] = destination
                path_mappings[value] = destination
            files.extend(tree_files(path, "home/" + destination, boundary))
    for settings in configs:
        dependencies(settings)
        for command in hook_commands(settings):
            program, script = hook_command_parts(command)
            for reference in ([script] if script else []) + (
                    [program] if (program.startswith("/") or program.startswith("~/"))
                    and Path(program).name not in HOOK_INTERPRETERS else []):
                path = dependency_path(reference)
                destination = str(path.relative_to(home)) if path.is_relative_to(home) else (
                    "dependencies/files/" + digest(str(path))[:20] + "/" + path.name)
                path_mappings[str(path)] = destination
                path_mappings[reference] = destination
                files.extend(tree_files(path, "home/" + destination, boundary))
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
                     "files": [{k: f[k] for k in ("path", "sha256", "executable")} for f in files]}
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
            "path_mappings": path_mappings}


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
                "source_revision", "kind", "cli_version", "native_profiles")}
            if source:
                environment["source_location"] = source
            environments.append(environment)
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
    elif (home / AUTH_FILES[bundle["kind"]]).is_file():
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


def adapt_text(raw, source, final_home, suffix):
    pairs = {source["source_home"]: str(final_home),
             source["source_user_home"] + "/.agents/skills": str(final_home / "skills/_user_agents")}
    pairs.update({old: str(final_home / relative) for old, relative in source.get("path_mappings", {}).items()})
    ordered = sorted(pairs, key=len, reverse=True)
    absolute = [p for p in ordered if p.startswith("/")]
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
    auth_name = AUTH_FILES[bundle["kind"]]
    if bundle["credential_policy"] == "endpoint":
        auth_home = request.get("endpoint_auth_home")
        if not auth_home:
            raise ProvisionError("endpoint credential policy requires endpoint_auth_home")
        auth_file = Path(auth_home).expanduser() / auth_name
        if not auth_file.is_file():
            raise ProvisionError("Selected endpoint login file is missing; authenticate that home first")
        auth = auth_file.read_bytes()
        if len(auth) > MAX_BYTES:
            raise ProvisionError("Native login file exceeds the bundle size limit")
    deployment_id = "dep-" + digest([expected, str(root), request.get("endpoint_auth_home"),
                                    hashlib.sha256(auth).hexdigest() if auth is not None else None])[:32]
    final = root / deployment_id
    manifest_path = final / ".taskr-environment.json"
    # Dry-run validates without creating the managed root or staging files.
    total = 0
    seen = set()
    contents = []
    configs = []
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
        if destination.suffix in {".toml", ".json", ".md", ".sh", ".py"}:
            raw = adapt_text(raw.decode(), bundle, final, destination.suffix).encode()
        if member.name == "config.toml" or member.name.endswith(".config.toml"):
            configs.append(tomllib.loads(raw.decode()))
        elif member.name in {"settings.json", "hooks.json"}:
            configs.append(json.loads(raw.decode()))
        contents.append((member, raw, entry["executable"]))
    included = {str(member.relative_to("home")) for member, _, _ in contents}
    for settings in configs:
        validate_dependencies(settings, final, included, request.get("remote", False))
    policy = bundle["credential_policy"]
    # Copy policy may use provider credentials embedded in config rather than a
    # login file. Readiness does not claim provider connectivity or authorization.
    result = {"deployment_id": deployment_id, "bundle_digest": expected,
              "source_environment_id": bundle["source_environment_id"],
              "source_revision": bundle["source_revision"], "kind": bundle["kind"],
              "display_name": bundle["display_name"],
              "native_profiles": bundle["native_profiles"], "home": str(final),
              "cli_version": version, "credential_policy": policy,
              "authentication": "file_provisioned" if auth is not None or "home/" + auth_name in seen else "native_provider_config"}
    if final.exists():
        previous = verify({"home": str(final), "deployment_id": deployment_id})
        if previous.get("bundle_digest") != expected:
            raise ProvisionError("Owned environment identity mismatch")
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
    native_version(result["kind"])
    for relative, expected in result.get("integrity", {}).items():
        member = safe_member("home/" + relative)
        file = path.joinpath(*member.parts[1:])
        if file.is_symlink() or not file.is_file():
            raise ProvisionError("Prepared configuration or skills changed; sync a new revision")
        if hashlib.sha256(file.read_bytes()).hexdigest() != expected:
            semantic = result.get("config_integrity", {}).get(relative)
            if (result["kind"] != "codex" or semantic is None or file.suffix != ".toml"
                    or digest(pinned_config(read_config(file))) != semantic):
                raise ProvisionError("Prepared configuration or skills changed; sync a new revision")
    if result["authentication"] == "file_provisioned" and not (path / AUTH_FILES[result["kind"]]).is_file():
        raise ProvisionError("Prepared native login file is missing")
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
