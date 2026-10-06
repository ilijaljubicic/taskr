"""Contract tests for portable bundles; never read real agent homes/login files."""
import base64
import importlib.util
import json
from pathlib import Path
import tempfile
import stat
import tomllib
import unittest
import zipfile
import warnings
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("companion", Path(__file__).parents[1] / "companion.py")
c = importlib.util.module_from_spec(spec)
spec.loader.exec_module(c)
NATIVE_VERSION = c.native_version
NATIVE_EFFORTS = c.native_reasoning_efforts


class CompanionTests(unittest.TestCase):
    def test_profile_model_options_follow_its_provider_catalog(self):
        cache = {"models": [{"slug":"test-model", "supported_reasoning_levels":[{"effort":"high"}]},
                            {"slug":"fast-model", "supported_reasoning_levels":[{"effort":"low"}]}],
                 "account_metadata":"must-not-be-published"}
        (self.source / "models_cache.json").write_text(json.dumps(cache))
        catalog = self.source / "glm-catalog.json"
        catalog.write_text(json.dumps({"models":[{"slug":"glm-5.3", "supported_reasoning_levels":[
            {"effort":"low"},{"effort":"high"},{"effort":"max"}]}]}))
        (self.source / "glm.config.toml").write_text(
            'model="glm-5.3"\nmodel_provider="ZAI"\nmodel_reasoning_effort="max"\nmodel_catalog_json="glm-catalog.json"\n')
        bundle = c.export(self.source, "codex")
        base, glm = bundle["profile_settings"]
        self.assertEqual({row["model"] for row in base["model_options"]}, {"test-model","fast-model"})
        self.assertEqual(glm["native_profile"], "glm")
        self.assertEqual(glm["model_options"], [{"model":"glm-5.3","reasoning_efforts":["low","high","max"]}])
        self.assertNotIn("must-not-be-published", json.dumps(bundle["profile_settings"]))
        self.assertFalse(any(entry["path"].endswith("models_cache.json") for entry in bundle["files"]))

    def test_imported_profile_without_catalog_advertises_only_its_configured_model(self):
        bundle = c.export(self.source, "codex")
        self.assertEqual(bundle["profile_settings"][0]["model_options"],
                         [{"model":"test-model","reasoning_efforts":[]}])

    def test_imported_model_cache_cannot_escape_the_collection(self):
        external = self.root / "controller-models.json"
        external.write_text(json.dumps({"models": [{"slug": "controller-only"}]}))
        (self.source / "models_cache.json").symlink_to(external)
        discovered = self.imported()
        self.assertFalse(discovered["environments"])
        self.assertIn("escapes", discovered["issues"][0]["error"])

    def test_claude_effort_choices_come_from_native_help_and_preserve_api_routing(self):
        home = self.root / "claude"
        home.mkdir()
        (home / "settings.json").write_text(json.dumps({"model":"sonnet", "env":{
            "ANTHROPIC_BASE_URL":"https://fixture.invalid", "ANTHROPIC_DEFAULT_SONNET_MODEL":"glm-5.3"}}))
        with patch.object(c.subprocess, "run", return_value=SimpleNamespace(
                stdout=b"--effort <level> Effort level\n  (low, medium, high, xhigh, max)", returncode=0)):
            supported = NATIVE_EFFORTS("claude")
        with patch.object(c, "native_reasoning_efforts", return_value=supported):
            settings = c.export(home, "claude")["profile_settings"][0]
        self.assertEqual(settings["model_options"], [{"model":"sonnet","reasoning_efforts":["low","medium","high","xhigh","max"]}])

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="taskr-bundle-test-")
        # macOS temporary directories can be reached through a symlinked /var.
        self.root = Path(self.temp.name).resolve(strict=True)
        self.source = self.root / "source/.codex"
        self.source.mkdir(parents=True)
        (self.source / "config.toml").write_text('model = "test-model"\n')
        self.home_patch = patch.object(Path, "home", return_value=self.root / "user")
        self.version_patch = patch.object(c, "native_version", return_value="0.160.0")
        self.efforts_patch = patch.object(c, "native_reasoning_efforts", return_value=[])
        self.home_patch.start()
        self.version_patch.start()
        self.efforts_patch.start()
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(self.home_patch.stop)
        self.addCleanup(self.version_patch.stop)
        self.addCleanup(self.efforts_patch.stop)

    def bundle(self, policy="copy"):
        source = c.export(self.source, "codex")
        return c.export_request({"source_home": str(self.source), "kind": "codex",
                                 "source_revision": source["source_revision"], "credential_policy": policy})

    def prepare(self, bundle=None, **options):
        return c.prepare({"bundle": bundle or self.bundle(), "deployment_root": str(self.root / "deployed"), **options})

    def test_resume_requires_restored_home_workspace_and_exact_codex_history(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        request = {"home": str(self.source), "workspace_path": str(workspace),
                   "kind": "codex", "session": "native-123"}
        with self.assertRaisesRegex(c.ProvisionError, "history is missing"):
            c.verify_resume(request)
        history = self.source / "sessions/2026/10/05/rollout-test-native-123.jsonl"
        history.parent.mkdir(parents=True)
        history.write_text('{"session":"native-123"}\n')
        self.assertTrue(c.verify_resume(request)["history_available"])
        history.unlink()
        archived = self.source / "archived_sessions/rollout-test-native-123.jsonl"
        archived.parent.mkdir()
        archived.write_text("saved history")
        self.assertTrue(c.verify_resume(request)["history_available"])
        with self.assertRaisesRegex(c.ProvisionError, "not been restored"):
            c.verify_resume({**request, "workspace_path": str(workspace / "missing")})
        with self.assertRaisesRegex(c.ProvisionError, "Invalid native session"):
            c.verify_resume({**request, "session": "*"})

    def test_resume_verifies_claude_kimi_and_opencode_history_without_starting_agents(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        request = {"home": str(self.source), "workspace_path": str(workspace), "session": "native-123"}
        claude = self.source / "projects/project-key/native-123.jsonl"
        claude.parent.mkdir(parents=True)
        claude.write_text("saved conversation")
        self.assertTrue(c.verify_resume({**request, "kind": "claude"})["history_available"])
        kimi = self.source / "sessions/workspace-key/native-123/context.jsonl"
        kimi.parent.mkdir(parents=True)
        kimi.write_text("saved context")
        self.assertTrue(c.verify_resume({**request, "kind": "kimi"})["history_available"])
        data = self.root / "native-data"
        opencode = data / "opencode/storage/session/project-key/native-123.json"
        opencode.parent.mkdir(parents=True)
        opencode.write_text("{}")
        with patch.dict(c.os.environ, {"XDG_DATA_HOME": str(data)}):
            self.assertTrue(c.verify_resume({**request, "kind": "opencode"})["history_available"])
            opencode.unlink()
            with self.assertRaisesRegex(c.ProvisionError, "history is missing"):
                c.verify_resume({**request, "kind": "opencode"})

    def test_discovery_base_and_native_profiles_without_configuration_contents(self):
        (self.source / "review.config.toml").write_text('model = "review-model"\n')
        (self.source / "auth.json").write_text('{"token":"never-return-this"}')
        result = c.discover({"homes": [str(self.source)]})
        env = result["environments"][0]
        self.assertEqual(env["native_profiles"], ["review"])
        self.assertEqual(env["kind"], "codex")
        self.assertNotIn("never-return-this", json.dumps(result))
        self.assertEqual(env["profile_settings"][1]["model"], "review-model")
        self.assertNotIn("files", env)
        self.assertFalse(result["issues"])

    def test_retained_mmux_deployment_keeps_frozen_home_and_integrity(self):
        deployed = self.prepare()
        home = Path(deployed["home"])
        config_before = (home / "config.toml").read_bytes()
        (home / ".taskr-environment.json").rename(home / ".mmux-environment.json")
        verified = c.verify({"home": str(home), "deployment_id": deployed["deployment_id"]})
        self.assertEqual(verified["home"], str(home))
        self.assertEqual((home / "config.toml").read_bytes(), config_before)
        (home / "config.toml").write_text('model="changed"')
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify({"home": str(home), "deployment_id": deployed["deployment_id"]})

    def test_retained_zip_cache_accepts_original_ownership_marker(self):
        archive = self.root / "environments.zip"
        with zipfile.ZipFile(archive, "w") as zipped:
            zipped.writestr(".codex/config.toml", 'model="fixture"\n')
        request = {"source_path": str(archive), "cache_root": str(self.root / "cache")}
        original = c.discover(request)
        self.assertFalse(original["issues"])
        marker = next((self.root / "cache").rglob(".taskr-source.json"))
        marker.rename(marker.with_name(".mmux-source.json"))
        retained = c.discover(request)
        self.assertEqual(original, retained)

    def test_skill_change_changes_revision_but_history_and_login_do_not(self):
        before = c.export(self.source, "codex")["source_revision"]
        (self.source / "sessions").mkdir()
        (self.source / "sessions/session.jsonl").write_text("native history")
        (self.source / "auth.json").write_text("native login")
        self.assertEqual(before, c.export(self.source, "codex")["source_revision"])
        skill = self.source / "skills/test/SKILL.md"
        skill.parent.mkdir(parents=True)
        skill.write_text("test skill")
        self.assertNotEqual(before, c.export(self.source, "codex")["source_revision"])

    def test_source_drift_refuses_export(self):
        source = c.export(self.source, "codex")
        (self.source / "config.toml").write_text('model="changed"')
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.export_request({**source, "credential_policy": "copy"})

    def test_endpoint_login_never_copies_source_login_or_embedded_credentials(self):
        (self.source / "auth.json").write_text('{"token":"source-secret"}')
        bundle = self.bundle("endpoint")
        self.assertNotIn("home/auth.json", [f["path"] for f in bundle["files"]])
        target_login = self.root / "target-login"
        target_login.mkdir()
        (target_login / "auth.json").write_text('{"token":"target-secret"}')
        result = self.prepare(bundle, endpoint_auth_home=str(target_login))
        self.assertIn("target-secret", (Path(result["home"]) / "auth.json").read_text())
        self.assertNotIn("secret", json.dumps(result))
        (self.source / "config.toml").write_text('[model_providers.test]\nhttp_headers={Authorization="embedded-secret"}')
        with self.assertRaisesRegex(c.ProvisionError, "embeds credentials"):
            self.bundle("endpoint")

    def test_explicit_copy_policy_and_private_permissions(self):
        (self.source / "auth.json").write_text('{"token":"source-secret"}')
        result = self.prepare()
        auth = Path(result["home"]) / "auth.json"
        self.assertIn("source-secret", auth.read_text())
        self.assertEqual(auth.stat().st_mode & 0o777, 0o600)
        self.assertEqual(auth.parent.stat().st_mode & 0o777, 0o700)

    def test_dry_run_and_failure_never_publish_a_home(self):
        result = self.prepare(dry_run=True)
        self.assertTrue(result["dry_run"])
        self.assertFalse(Path(result["home"]).exists())
        with self.assertRaisesRegex(c.ProvisionError, "login file is missing"):
            self.prepare(self.bundle("endpoint"), endpoint_auth_home=str(self.root / "missing"))
        self.assertFalse((self.root / "deployed").exists())

    def test_idempotent_sync_and_new_revision_leave_existing_conversation(self):
        first = self.prepare()
        history = Path(first["home"]) / "sessions/task.jsonl"
        history.parent.mkdir()
        history.write_text("saved conversation")
        again = self.prepare()
        self.assertEqual(first["deployment_id"], again["deployment_id"])
        (self.source / "config.toml").write_text('model="new"')
        second = self.prepare()
        self.assertNotEqual(first["home"], second["home"])
        self.assertEqual(history.read_text(), "saved conversation")

    def test_path_traversal_and_corrupted_contents_rejected(self):
        for change in [{"path": "home/../../outside"}, {"data": base64.b64encode(b"wrong").decode()}]:
            bundle = self.bundle()
            bundle["files"][0].update(change)
            bundle["bundle_digest"] = c.digest({k: v for k, v in bundle.items() if k != "bundle_digest"})
            with self.assertRaises(c.ProvisionError):
                self.prepare(bundle)
            self.assertFalse((self.root / "deployed").exists())

    def test_symlink_cycle_rejected_and_file_symlink_materialized(self):
        skill = self.source / "skills/test"
        skill.mkdir(parents=True)
        external = self.root / "instructions.md"
        external.write_text("portable instruction")
        (skill / "SKILL.md").symlink_to(external)
        result = self.prepare()
        copied = Path(result["home"]) / "skills/test/SKILL.md"
        self.assertFalse(copied.is_symlink())
        self.assertEqual(copied.read_text(), external.read_text())
        (skill / "cycle").symlink_to(skill)
        with self.assertRaisesRegex(c.ProvisionError, "cycle"):
            self.bundle()

    def test_external_user_skills_and_declared_file_dependency_are_adapted(self):
        extra = Path.home() / ".agents/skills/user-skill"
        extra.mkdir(parents=True)
        (extra / "SKILL.md").write_text("user skill")
        instructions = self.root / "external/instructions.md"
        instructions.parent.mkdir()
        instructions.write_text("model instructions")
        (self.source / "config.toml").write_text("model_instructions_file = " + json.dumps(str(instructions)))
        result = self.prepare()
        home = Path(result["home"])
        self.assertTrue((home / "skills/_user_agents/user-skill/SKILL.md").is_file())
        config = tomllib.loads((home / "config.toml").read_text())
        self.assertTrue(Path(config["model_instructions_file"]).is_relative_to(home))
        self.assertEqual(Path(config["model_instructions_file"]).read_text(), "model instructions")

    def test_configured_codex_plugins_are_included_with_marketplace_path_adaptation(self):
        cache = self.source / "plugins/cache/test/plugin/1.0.0"
        cache.mkdir(parents=True)
        (cache / "plugin.txt").write_text("plugin content")
        marketplace = self.root / "marketplace/.agents/plugins"
        marketplace.mkdir(parents=True)
        (marketplace / "marketplace.json").write_text('{"name":"test","plugins":[]}')
        (self.source / "config.toml").write_text('[plugins."plugin@test"]\nenabled=true\n[marketplaces.test]\nsource_type="local"\nsource=' + json.dumps(str(marketplace.parents[1])))
        result = self.prepare()
        home = Path(result["home"])
        self.assertEqual((home / "plugins/cache/test/plugin/1.0.0/plugin.txt").read_text(), "plugin content")
        config = tomllib.loads((home / "config.toml").read_text())
        self.assertTrue((Path(config["marketplaces"]["test"]["source"]) / ".agents/plugins/marketplace.json").is_file())

    def test_quoted_destination_preserves_json_and_toml_paths(self):
        source = {"source_home": "/original/home", "source_user_home": "/original", "path_mappings": {}}
        destination = Path('/destination/with"quote\'and\\slash')
        raw = "path = '/original/home/skills/file'"
        adapted = c.adapt_text(raw, source, destination, ".toml")
        self.assertEqual(tomllib.loads(adapted)["path"], str(destination / "skills/file"))
        raw = json.dumps({"path": "/original/home/skills/file"})
        self.assertEqual(json.loads(c.adapt_text(raw, source, destination, ".json"))["path"], str(destination / "skills/file"))

    def test_remote_loopback_and_missing_executable_dependencies_fail_before_publication(self):
        (self.source / "config.toml").write_text('[mcp_servers.test]\nurl="http://localhost:1234/mcp"')
        with self.assertRaisesRegex(c.ProvisionError, "localhost"):
            self.prepare(remote=True)
        (self.source / "config.toml").write_text('[mcp_servers.test]\ncommand="taskr-test-no-such-binary"')
        with self.assertRaisesRegex(c.ProvisionError, "MCP executable"):
            self.prepare()
        self.assertFalse((self.root / "deployed").exists())

    def test_native_hooks_json_bundles_rebased_script_and_tracks_integrity(self):
        script = self.source / "hook script.sh"
        script.write_text("#!/bin/sh\nexit 0\n")
        script.chmod(0o700)
        hooks = {"hooks":{"SessionStart":[{"hooks":[{
            "type":"command", "command":"bash " + c.shlex.quote(str(script)) + " session"
        }]}]}}
        (self.source / "hooks.json").write_text(json.dumps(hooks))
        bundle = self.bundle()
        self.assertIn("home/hook script.sh", [entry["path"] for entry in bundle["files"]])
        result = self.prepare(bundle)
        home = Path(result["home"])
        deployed = home / script.name
        self.assertEqual(deployed.read_bytes(), script.read_bytes())
        self.assertEqual(deployed.stat().st_mode & 0o777, 0o700)
        command = next(c.hook_commands(json.loads((home / "hooks.json").read_text())))
        self.assertEqual(c.shlex.split(command), ["bash", str(deployed), "session"])
        deployed.unlink()
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify(result)

    def test_missing_hook_script_refused_at_source_and_endpoint_before_publication(self):
        script = self.source / "hook.sh"
        (self.source / "hooks.json").write_text(json.dumps({"hooks":{
            "SessionStart":[{"command":"bash " + c.shlex.quote(str(script))}]
        }}))
        with self.assertRaisesRegex(c.ProvisionError, "missing at the source"):
            self.bundle()
        script.write_text("exit 0")
        bundle = self.bundle()
        bundle["files"] = [entry for entry in bundle["files"] if entry["path"] != "home/hook.sh"]
        bundle["bundle_digest"] = c.digest({key:value for key,value in bundle.items() if key != "bundle_digest"})
        with self.assertRaisesRegex(c.ProvisionError, "bundled hook dependency is missing"):
            self.prepare(bundle)
        self.assertFalse((self.root / "deployed").exists())

    def test_codex_native_hook_trust_writes_allow_resume_but_launch_settings_stay_pinned(self):
        result = self.prepare()
        config = Path(result["home"]) / "config.toml"
        original = config.read_text()
        config.write_text(original + '\n[hooks.state."native-hook-id"]\ntrusted_hash="sha256:reviewed"\n')
        self.assertEqual(c.verify(result)["deployment_id"], result["deployment_id"])
        config.write_text(config.read_text().replace('model = "test-model"', 'model = "changed-model"'))
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify(result)
        config.write_text(original + '\n[hooks]\ncommand="different-hook"\n')
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify(result)

    def test_verify_refuses_deleted_or_changed_configuration(self):
        result = self.prepare()
        self.assertEqual(c.verify(result)["deployment_id"], result["deployment_id"])
        (Path(result["home"]) / "config.toml").write_text('model="tampered"')
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify(result)

    def test_claude_native_settings_and_skills_use_separate_home(self):
        claude = self.root / "source/.claude"
        claude.mkdir()
        (claude / "settings.json").write_text('{"model":"claude-test"}')
        skills = claude / "skills/review"
        skills.mkdir(parents=True)
        (skills / "SKILL.md").write_text("review")
        source = c.export(claude, "claude")
        bundle = c.export_request({**source, "credential_policy": "copy"})
        result = self.prepare(bundle)
        self.assertEqual(result["kind"], "claude")
        self.assertTrue((Path(result["home"]) / "settings.json").is_file())
        self.assertTrue((Path(result["home"]) / "skills/review/SKILL.md").is_file())

    def test_different_roots_and_destination_logins_get_distinct_deployment_ids(self):
        first = self.prepare()
        second = self.prepare(deployment_root=str(self.root / "other-root"))
        self.assertNotEqual(first["deployment_id"], second["deployment_id"])
        login = self.root / "endpoint-auth"
        login.mkdir()
        (login / "auth.json").write_text('{"token":"first-login"}')
        one = self.prepare(self.bundle("endpoint"), endpoint_auth_home=str(login))
        (login / "auth.json").write_text('{"token":"second-login"}')
        two = self.prepare(self.bundle("endpoint"), endpoint_auth_home=str(login))
        self.assertNotEqual(one["deployment_id"], two["deployment_id"])
        self.assertIn("first-login", (Path(one["home"]) / "auth.json").read_text())

    def test_cli_downgrade_and_unowned_destination_are_rejected(self):
        bundle = self.bundle()
        with patch.object(c, "native_version", return_value="0.159.0"):
            with self.assertRaisesRegex(c.ProvisionError, "older"):
                self.prepare(bundle)
        preview = self.prepare(bundle, dry_run=True)
        Path(preview["home"]).mkdir(parents=True)
        with self.assertRaisesRegex(c.ProvisionError, "owned"):
            self.prepare(bundle)

    def test_legacy_inline_profiles_and_missing_claude_plugins_report_issues(self):
        (self.source / "config.toml").write_text('[profiles.review]\nmodel="test"')
        result = c.discover({"homes": [str(self.source)]})
        self.assertFalse(result["environments"])
        self.assertIn("Legacy", result["issues"][0]["error"])
        claude = self.root / "source/.claude"
        claude.mkdir()
        (claude / "settings.json").write_text('{"enabledPlugins":{"test@market":true}}')
        result = c.discover({"homes": [str(claude)]})
        self.assertEqual(len(result["environments"]), 1)
        self.assertIn("installed_plugins.json", result["environments"][0]["sync_blockers"][0])
        self.assertIn("installed_plugins.json", result["issues"][0]["error"])

    def claude_plugin_home(self):
        home = self.root / "source/.claude"
        root = self.root / "installed-plugin"
        root.mkdir()
        (root / ".claude-plugin").mkdir()
        (root / "scripts").mkdir()
        (root / "scripts/probe.py").write_text("raise RuntimeError('Plugin commands must never execute during sync')")
        (root / "skills/proof").mkdir(parents=True)
        (root / "skills/proof/SKILL.md").write_text("Plugin skill")
        (root / ".in_use").mkdir()
        (root / ".in_use/123").write_text("runtime lock")
        (root / ".claude-plugin/plugin.json").write_text(json.dumps({
            "name": "probe", "version": "1.0.0", "skills": ["./skills/proof"],
            "mcpServers": {"probe": {"command": "python3", "args": ["${CLAUDE_PLUGIN_ROOT}/scripts/probe.py"]}},
            "lspServers": {"probe": {"command": "python3", "extensionToLanguage": {".py": "python"}}}}))
        (root / "hooks").mkdir()
        (root / "hooks/hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [
            {"hooks": [{"type": "command", "command": 'python3 "${CLAUDE_PLUGIN_ROOT}/scripts/probe.py"'}]}]}}))
        marketplace = self.root / "marketplace"
        (marketplace / ".claude-plugin").mkdir(parents=True)
        (marketplace / ".claude-plugin/marketplace.json").write_text(json.dumps({
            "name": "fixture", "owner": {"name": "Fixture"}, "plugins": [{"name": "probe", "source": "./probe"}]}))
        (home / "plugins").mkdir(parents=True)
        (home / "settings.json").write_text(json.dumps({"model": "sonnet", "enabledPlugins": {"probe@fixture": True}}))
        (home / ".credentials.json").write_text('{"accessToken":"fixture-login"}')
        (home / "plugins/installed_plugins.json").write_text(json.dumps({"version": 2, "plugins": {"probe@fixture": [
            {"scope": "user", "installPath": str(root), "version": "1.0.0"}]}}))
        (home / "plugins/known_marketplaces.json").write_text(json.dumps({"fixture": {
            "source": {"source": "github", "repo": "fixture/plugins"}, "installLocation": str(marketplace), "autoUpdate": True}}))
        return home, root, marketplace

    def claude_bundle(self, home, policy="copy"):
        source = c.export(home, "claude")
        return c.export_request({"source_home": str(home), "kind": "claude", "source_revision": source["source_revision"], "credential_policy": policy})

    def test_claude_plugins_clone_payload_registry_catalog_and_runtime_root_dependencies(self):
        home, root, marketplace = self.claude_plugin_home()
        settings_path = home / "settings.json"
        settings = json.loads(settings_path.read_text())
        settings["extraKnownMarketplaces"] = {"fixture": {"source": {"source": "github", "repo": "fixture/plugins"}, "autoUpdate": True}}
        settings_path.write_text(json.dumps(settings))
        catalog_path = marketplace / ".claude-plugin/marketplace.json"
        catalog = json.loads(catalog_path.read_text())
        catalog.update({"renames": {"old-unselected": "new-unselected"}, "forceRemoveDeletedPlugins": True,
                        "metadata": {"pluginRoot": "./old-plugin-root", "description": "Preserve display metadata"}})
        catalog_path.write_text(json.dumps(catalog))
        original_registry = (home / "plugins/installed_plugins.json").read_bytes()
        bundle = self.claude_bundle(home)
        self.assertNotIn("CLAUDE_PLUGIN_ROOT", {d["reference"] for d in bundle["dependencies"] if d["kind"] == "environment"})
        result = self.prepare(bundle)
        deployed = Path(result["home"])
        record = json.loads((deployed / "plugins/installed_plugins.json").read_text())["plugins"]["probe@fixture"][0]
        plugin = Path(record["installPath"])
        self.assertTrue(plugin.is_relative_to(deployed))
        self.assertEqual((plugin / "skills/proof/SKILL.md").read_text(), "Plugin skill")
        self.assertFalse((plugin / ".in_use").exists())
        self.assertEqual((home / "plugins/installed_plugins.json").read_bytes(), original_registry)
        known = json.loads((deployed / "plugins/known_marketplaces.json").read_text())["fixture"]
        self.assertFalse(known["autoUpdate"])
        self.assertFalse(json.loads((deployed / "settings.json").read_text())["extraKnownMarketplaces"]["fixture"]["autoUpdate"])
        self.assertTrue(json.loads(settings_path.read_text())["extraKnownMarketplaces"]["fixture"]["autoUpdate"])
        catalog = json.loads((Path(known["installLocation"]) / ".claude-plugin/marketplace.json").read_text())
        self.assertNotIn("renames", catalog)
        self.assertNotIn("forceRemoveDeletedPlugins", catalog)
        self.assertEqual(catalog["metadata"], {"description": "Preserve display metadata"})
        self.assertEqual(Path(known["installLocation"]) / catalog["plugins"][0]["source"], plugin)
        self.assertTrue(c.verify({"home": str(deployed), "deployment_id": result["deployment_id"]}))
        (plugin / "hooks/hooks.json").write_text("{}")
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify({"home": str(deployed), "deployment_id": result["deployment_id"]})

    def test_claude_project_plugin_scope_requires_mapping_and_is_not_promoted(self):
        home, root, _ = self.claude_plugin_home()
        registry = {"version": 2, "plugins": {"probe@fixture": [{"scope": "project", "projectPath": "/original/repo", "installPath": str(root), "version": "1"}]}}
        (home / "plugins/installed_plugins.json").write_text(json.dumps(registry))
        blocked = c.discover({"homes": [str(home)]})["environments"][0]
        self.assertIn("mapped project", blocked["sync_blockers"][0])
        (home / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "claude_projects": {"/original/repo": "/endpoint/repo"}}))
        result = self.prepare(self.claude_bundle(home))
        record = json.loads((Path(result["home"]) / "plugins/installed_plugins.json").read_text())["plugins"]["probe@fixture"][0]
        self.assertEqual(record["scope"], "project")
        self.assertEqual(record["projectPath"], "/endpoint/repo")

    def test_disabled_claude_plugins_and_uninstalled_marketplace_declarations_do_not_block(self):
        home = self.root / "claude-disabled"
        home.mkdir()
        (home / "settings.json").write_text(json.dumps({"enabledPlugins": {"missing@fixture": False},
            "extraKnownMarketplaces": {"fixture": {"source": {"source": "github", "repo": "fixture/plugins"}}}}))
        result = c.discover({"homes": [str(home)]})
        self.assertFalse(result["issues"])
        self.assertEqual(len(result["environments"]), 1)

    def test_claude_marketplace_inline_lsp_is_included_and_checked(self):
        home, root, marketplace = self.claude_plugin_home()
        (root / ".claude-plugin/plugin.json").unlink()
        manifest = marketplace / ".claude-plugin/marketplace.json"
        value = json.loads(manifest.read_text())
        value["plugins"][0]["lspServers"] = {"probe": {"command": "taskr-nonexistent-fixture-lsp", "extensionToLanguage": {".py": "python"}}}
        manifest.write_text(json.dumps(value))
        bundle = self.claude_bundle(home)
        with self.assertRaisesRegex(c.ProvisionError, "executable is missing"):
            self.prepare(bundle)

    def test_imported_claude_plugin_payload_cannot_escape_collection(self):
        home, root, _ = self.claude_plugin_home()
        result = c.discover({"source_path": str(home), "cache_root": str(self.root / "cache")})
        self.assertFalse(result["environments"])
        self.assertIn("escapes", result["issues"][0]["error"])

    def test_claude_plugin_metadata_obeys_credential_policy(self):
        home, root, _ = self.claude_plugin_home()
        manifest = root / ".claude-plugin/plugin.json"
        value = json.loads(manifest.read_text())
        value["mcpServers"]["probe"]["env"] = {"SERVICE_API_KEY": "fixture-private-key"}
        manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(c.ProvisionError, "embeds credentials"):
            self.claude_bundle(home, "endpoint")
        result = self.prepare(self.claude_bundle(home, "copy"))
        self.assertTrue(result["home"])

    def test_missing_claude_plugin_payload_is_visible_with_a_setup_blocker(self):
        home, root, _ = self.claude_plugin_home()
        root.rename(root.with_name("removed-plugin"))
        discovered = c.discover({"homes": [str(home)]})
        self.assertEqual(len(discovered["environments"]), 1)
        self.assertIn("payload is missing", discovered["environments"][0]["sync_blockers"][0])

    def test_claude_plugin_dependency_payload_is_cloned_without_changing_enablement(self):
        home, root, marketplace = self.claude_plugin_home()
        dependency = self.root / "dependency-plugin"
        (dependency / "skills/proof").mkdir(parents=True)
        (dependency / "skills/proof/SKILL.md").write_text("Dependency skill")
        manifest_path = root / ".claude-plugin/plugin.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["dependencies"] = ["dependency"]
        manifest_path.write_text(json.dumps(manifest))
        installed_path = home / "plugins/installed_plugins.json"
        installed = json.loads(installed_path.read_text())
        installed["plugins"]["dependency@fixture"] = [{"scope": "user", "installPath": str(dependency), "version": "1"}]
        installed_path.write_text(json.dumps(installed))
        catalog_path = marketplace / ".claude-plugin/marketplace.json"
        catalog = json.loads(catalog_path.read_text())
        catalog["plugins"].append({"name": "dependency", "source": "./dependency"})
        catalog_path.write_text(json.dumps(catalog))
        result = self.prepare(self.claude_bundle(home))
        deployed = Path(result["home"])
        registrations = json.loads((deployed / "plugins/installed_plugins.json").read_text())["plugins"]
        self.assertEqual(set(registrations), {"probe@fixture", "dependency@fixture"})
        self.assertEqual(json.loads((deployed / "settings.json").read_text())["enabledPlugins"], {"probe@fixture": True})

    def test_claude_exec_hook_arguments_and_native_data_variables_are_preserved(self):
        home, root, _ = self.claude_plugin_home()
        (root / "hooks/hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [{"hooks": [
            {"type": "command", "command": "python3", "args": ["${CLAUDE_PLUGIN_ROOT}/scripts/probe.py", "${CLAUDE_PLUGIN_DATA}/state.json"]}]}]}}))
        bundle = self.claude_bundle(home)
        self.assertNotIn("CLAUDE_PLUGIN_DATA", {d["reference"] for d in bundle["dependencies"]})
        result = self.prepare(bundle)
        deployed = Path(result["home"])
        plugin = Path(json.loads((deployed / "plugins/installed_plugins.json").read_text())["plugins"]["probe@fixture"][0]["installPath"])
        self.assertIn("${CLAUDE_PLUGIN_DATA}", (plugin / "hooks/hooks.json").read_text())

    def test_claude_opaque_plugin_commands_use_plugin_local_dependency_declarations(self):
        home, root, _ = self.claude_plugin_home()
        manifest_path = root / ".claude-plugin/plugin.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["mcpServers"]["probe"]["args"] = ["-c", "raise RuntimeError('Must not execute')"]
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(c.ProvisionError, "Opaque"):
            self.claude_bundle(home)
        (root / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "commands": [{
            "config": ".claude-plugin/plugin.json", "pointer": "/mcpServers/probe",
            "files": [{"path": "../scripts/probe.py"}], "environment": ["TASKR_PLUGIN_FIXTURE_REQUIRED"]}]}))
        bundle = self.claude_bundle(home)
        result = self.prepare(bundle)
        self.assertEqual(result["profile_readiness"][0]["missing_environment"], ["TASKR_PLUGIN_FIXTURE_REQUIRED"])

    def test_claude_plugin_collection_and_zip_import_rebase_original_machine_paths(self):
        home, root, marketplace = self.claude_plugin_home()
        collection = self.root / "plugin-collection"
        collection.mkdir()
        new_home, new_root, new_market = collection / "claude", collection / "payload", collection / "marketplace"
        home.rename(new_home)
        root.rename(new_root)
        marketplace.rename(new_market)
        installed_path = new_home / "plugins/installed_plugins.json"
        installed = json.loads(installed_path.read_text())
        installed["plugins"]["probe@fixture"][0]["installPath"] = str(new_root)
        installed_path.write_text(json.dumps(installed))
        known_path = new_home / "plugins/known_marketplaces.json"
        known = json.loads(known_path.read_text())
        known["fixture"]["installLocation"] = str(new_market)
        known_path.write_text(json.dumps(known))
        (collection / "taskr-environments.json").write_text(json.dumps({"version": 1, "environments": [{
            "home": "claude", "kind": "claude", "original_home": str(new_home), "original_user_home": str(collection)}]}))
        archive = self.root / "plugin-collection.zip"
        with zipfile.ZipFile(archive, "w") as output:
            for file in collection.rglob("*"):
                if file.is_file():
                    output.write(file, str(file.relative_to(collection)))
        for location in (collection, archive):
            discovery = c.discover({"source_path": str(location), "cache_root": str(self.root / "plugin-cache")})
            self.assertFalse(discovery["issues"])
            source = discovery["environments"][0]
            bundle = self.imported_bundle(source)
            result = self.prepare(bundle)
            deployed = Path(result["home"])
            record = json.loads((deployed / "plugins/installed_plugins.json").read_text())["plugins"]["probe@fixture"][0]
            plugin = Path(record["installPath"])
            self.assertTrue(plugin.is_relative_to(deployed))
            self.assertEqual((plugin / "skills/proof/SKILL.md").read_text(), "Plugin skill")

    def test_claude_configured_plugin_registry_root_is_cloned_and_rebased(self):
        home, _, _ = self.claude_plugin_home()
        custom = self.root / "custom-plugin-registry"
        (home / "plugins").rename(custom)
        settings_path = home / "settings.json"
        settings = json.loads(settings_path.read_text())
        settings["env"] = {"CLAUDE_CODE_PLUGIN_CACHE_DIR": str(custom)}
        settings_path.write_text(json.dumps(settings))
        result = self.prepare(self.claude_bundle(home))
        deployed = Path(result["home"])
        self.assertEqual(json.loads((deployed / "settings.json").read_text())["env"]["CLAUDE_CODE_PLUGIN_CACHE_DIR"], str(deployed / "plugins"))
        self.assertTrue((deployed / "plugins/installed_plugins.json").is_file())

    def test_packaging_size_limit_is_enforced_before_collecting_a_whole_tree(self):
        skills = self.source / "skills"
        skills.mkdir()
        (skills / "one.md").write_text("x" * 30)
        (skills / "two.md").write_text("x" * 30)
        with patch.object(c, "MAX_BYTES", 50):
            with self.assertRaisesRegex(c.ProvisionError, "limit"):
                self.bundle()

    def test_invalid_json_home_does_not_hide_another_healthy_source(self):
        claude = self.root / "source/.claude"
        claude.mkdir()
        (claude / "settings.json").write_text("[]")
        result = c.discover({"homes": [str(claude), str(self.source)]})
        self.assertEqual(len(result["environments"]), 1)
        self.assertEqual(len(result["issues"]), 1)

    def test_version_number_alone_does_not_authorize_missing_native_launch_flags(self):
        def output(args, **kwargs):
            return SimpleNamespace(returncode=0, stdout=b"codex-cli 0.160.0" if args[-1] == "--version" else b"--profile --no-alt-screen")
        with patch.object(c.shutil, "which", return_value="/fixture/codex"), patch.object(c.subprocess, "run", side_effect=output):
            with self.assertRaisesRegex(c.ProvisionError, "no-daemon"):
                NATIVE_VERSION("codex")

    def test_credential_references_are_distinct_from_literal_credentials(self):
        self.assertFalse(c.contains_credentials({"bearer_token_env_var": "MCP_TOKEN", "env_http_headers": {"Authorization": "MCP_TOKEN"},
            "headers": {"Authorization": "Bearer ${MCP_TOKEN}"}, "apiKeyHelper": "/bin/helper", "tokenizer": "test"}))
        self.assertTrue(c.contains_credentials({"headers": {"Authorization": "Bearer literal-credential"}}))
        self.assertTrue(c.contains_credentials({"accessToken": "literal-credential"}))
        self.assertTrue(c.contains_credentials({"api-key": "literal-credential"}))

    def imported(self, source=None):
        return c.discover({"source_path": str(source or self.source.parent), "cache_root": str(self.root / "cache")})

    def imported_bundle(self, source, policy="copy"):
        return c.export_request({**source, "credential_policy": policy})

    def archive(self, name="collection.zip"):
        archive = self.root / name
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for path in sorted(self.source.parent.rglob("*")):
                if path.is_file():
                    output.write(path, str(path.relative_to(self.source.parent)))
        return archive

    def test_collection_folder_discovers_all_homes_and_profiles_without_ambient_skills(self):
        (self.source / "review.config.toml").write_text('model="review"')
        claude = self.source.parent / "custom/claude-home"
        claude.mkdir(parents=True)
        (claude / "settings.json").write_text('{"model":"claude-test"}')
        ambient = Path.home() / ".agents/skills/ambient/SKILL.md"
        ambient.parent.mkdir(parents=True)
        ambient.write_text("must not import this")
        shared = self.source.parent / ".agents/skills/shared/SKILL.md"
        shared.parent.mkdir(parents=True)
        shared.write_text("provided shared skill")
        result = self.imported()
        self.assertFalse(result["issues"])
        self.assertEqual({e["kind"] for e in result["environments"]}, {"codex", "claude"})
        codex = next(e for e in result["environments"] if e["kind"] == "codex")
        self.assertEqual(codex["native_profiles"], ["review"])
        bundle = self.imported_bundle(codex)
        paths = {e["path"] for e in bundle["files"]}
        self.assertIn("home/skills/_user_agents/shared/SKILL.md", paths)
        self.assertFalse(any("ambient" in p for p in paths))
        self.assertNotIn("review", json.dumps(result["environments"][1].get("files", [])))

    def test_single_native_home_import_is_supported(self):
        source = self.imported(self.source)["environments"][0]
        self.assertEqual(source["source_location"]["relative_home"], ".")
        self.assertIn(".codex", source["display_name"])
        self.assertEqual(self.imported_bundle(source)["source_environment_id"], source["source_environment_id"])

    def test_manifest_rebases_original_homes_shared_dependencies_and_relative_paths(self):
        root = self.source.parent
        (root / "instructions.md").write_text("shared instructions")
        (self.source / "local.md").write_text("local instructions")
        (self.source / "config.toml").write_text('model_instructions_file="/home/other/instructions.md"')
        (self.source / "review.config.toml").write_text('model_instructions_file="local.md"')
        manifest = {"version": 1, "environments": [{"home": ".codex", "kind": "codex", "name": "Imported work", "original_home": "/home/other/.codex", "original_user_home": "/home/other", "cli_version": "0.160.0"}]}
        (root / "taskr-environments.json").write_text(json.dumps(manifest))
        source = self.imported()["environments"][0]
        self.assertEqual(source["display_name"], "codex / Imported work")
        result = self.prepare(self.imported_bundle(source))
        home = Path(result["home"])
        base = tomllib.loads((home / "config.toml").read_text())
        review = tomllib.loads((home / "review.config.toml").read_text())
        self.assertEqual(Path(base["model_instructions_file"]).read_text(), "shared instructions")
        self.assertEqual(Path(review["model_instructions_file"]).read_text(), "local instructions")
        self.assertTrue(Path(base["model_instructions_file"]).is_relative_to(home))

    def test_imported_external_absolute_path_and_symlinks_cannot_read_controller_files(self):
        external = self.root / "controller-only.md"
        external.write_text("private controller content")
        (self.source / "config.toml").write_text('model_instructions_file=' + json.dumps(str(external)))
        result = self.imported()
        self.assertFalse(result["environments"])
        self.assertIn("escapes", result["issues"][0]["error"])
        (self.source / "config.toml").write_text('model="test"')
        skills = self.source / "skills"
        skills.mkdir()
        (skills / "escape.md").symlink_to(external)
        result = self.imported()
        self.assertFalse(result["environments"])
        self.assertIn("escapes", result["issues"][0]["error"])

    def test_folder_configuration_and_manifest_drift_require_rediscovery(self):
        source = self.imported()["environments"][0]
        (self.source / "config.toml").write_text('model="changed"')
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            self.imported_bundle(source)
        source = self.imported()["environments"][0]
        (self.source.parent / "taskr-environments.json").write_text(json.dumps({"version": 1, "environments": [{"home": ".codex", "kind": "codex", "name": "renamed"}]}))
        with self.assertRaisesRegex(c.ProvisionError, "manifest changed"):
            self.imported_bundle(source)

    def test_zip_import_survives_cache_reuse_and_keeps_credentials_explicit(self):
        (self.source / "auth.json").write_text('{"token":"import-login"}')
        (self.source / "review.config.toml").write_text('model="review"')
        archive = self.archive()
        first = self.imported(archive)["environments"][0]
        self.assertEqual(first, self.imported(archive)["environments"][0])
        source_home = Path(first["source_home"])
        self.assertEqual(source_home.stat().st_mode & 0o777, 0o700)
        self.assertEqual((source_home / "auth.json").stat().st_mode & 0o777, 0o600)
        self.assertNotIn("import-login", json.dumps(first))
        copied = self.imported_bundle(first)
        endpoint = self.imported_bundle(first, "endpoint")
        self.assertIn("home/auth.json", {e["path"] for e in copied["files"]})
        self.assertNotIn("home/auth.json", {e["path"] for e in endpoint["files"]})
        result = self.prepare(copied)
        self.assertTrue((Path(result["home"]) / "review.config.toml").is_file())

    def test_zip_replacement_changes_revision_but_keeps_source_identity(self):
        archive = self.archive()
        first = self.imported(archive)["environments"][0]
        (self.source / "config.toml").write_text('model="new"')
        self.archive()
        with self.assertRaisesRegex(c.ProvisionError, "archive changed"):
            self.imported_bundle(first)
        second = self.imported(archive)["environments"][0]
        self.assertEqual(first["source_environment_id"], second["source_environment_id"])
        self.assertNotEqual(first["source_revision"], second["source_revision"])

    def test_zip_cache_tampering_is_rejected_including_new_symlinks(self):
        source = self.imported(self.archive())["environments"][0]
        root = Path(source["source_location"]["cache_root"])
        (root / "link").symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(c.ProvisionError, "cache changed"):
            self.imported_bundle(source)
        (root / "link").unlink()
        (Path(source["source_home"]) / "config.toml").write_text('model="tampered"')
        with self.assertRaisesRegex(c.ProvisionError, "cache changed"):
            self.imported_bundle(source)

    def test_unsafe_zip_members_never_publish_a_cache(self):
        for name in ("../escape", "/absolute", "C:/drive", "nested\\escape", ".taskr-source.json"):
            with self.subTest(name=name):
                archive = self.root / "unsafe.zip"
                with zipfile.ZipFile(archive, "w") as output:
                    output.writestr(name, "content")
                with self.assertRaises(c.ProvisionError):
                    self.imported(archive)
                self.assertFalse(list((self.root / "cache").iterdir()))
        self.assertFalse((self.root / "escape").exists())

    def test_zip_symlinks_and_file_directory_collisions_are_rejected(self):
        archive = self.root / "unsafe.zip"
        with zipfile.ZipFile(archive, "w") as output:
            link = zipfile.ZipInfo("link")
            link.create_system = 3
            link.external_attr = (stat.S_IFLNK | 0o777) << 16
            output.writestr(link, "/etc/passwd")
        with self.assertRaises(c.ProvisionError):
            self.imported(archive)
        with zipfile.ZipFile(archive, "w") as output:
            output.writestr("file", "content")
            output.writestr("file/child", "content")
        with self.assertRaises(c.ProvisionError):
            self.imported(archive)
        self.assertFalse(list((self.root / "cache").iterdir()))

    def test_archive_expansion_and_file_count_limits_are_enforced_before_publication(self):
        archive = self.archive()
        with patch.object(c, "MAX_COLLECTION_BYTES", 1024):
            (self.source / "large.txt").write_text("x" * 2000)
            archive = self.archive()
            with self.assertRaisesRegex(c.ProvisionError, "limit"):
                self.imported(archive)
        with patch.object(c, "MAX_COLLECTION_FILES", 1):
            with self.assertRaisesRegex(c.ProvisionError, "limit"):
                self.imported(archive)
        self.assertFalse(list((self.root / "cache").iterdir()))

    def test_manifest_traversal_unknown_fields_and_duplicate_homes_are_rejected(self):
        manifest = self.source.parent / "taskr-environments.json"
        entries = [{"home": "../", "kind": "codex"}, {"home": ".codex", "kind": "codex", "unexpected": "x"}]
        for entry in entries:
            manifest.write_text(json.dumps({"version": 1, "environments": [entry]}))
            with self.assertRaises(c.ProvisionError):
                self.imported()
        manifest.write_text(json.dumps({"version": 1, "environments": [{"home": ".codex", "kind": "codex"}] * 2}))
        with self.assertRaises(c.ProvisionError):
            self.imported()

    def test_discovery_inputs_are_mutually_exclusive(self):
        with self.assertRaisesRegex(c.ProvisionError, "mutually exclusive"):
            c.discover({"source_path": str(self.source), "homes": [str(self.source)]})

    def test_zip_wrapping_folder_loads_manifest_and_shared_skills(self):
        root = self.source.parent
        shared = root / ".agents/skills/shared/SKILL.md"
        shared.parent.mkdir(parents=True)
        shared.write_text("wrapped shared skill")
        (root / "taskr-environments.json").write_text(json.dumps({"version": 1, "environments": [{"home": ".codex", "kind": "codex", "name": "Wrapped work"}]}))
        archive = self.root / "wrapped.zip"
        with zipfile.ZipFile(archive, "w") as output:
            for path in root.rglob("*"):
                if path.is_file():
                    output.write(path, "backup/" + str(path.relative_to(root)))
        source = self.imported(archive)["environments"][0]
        self.assertEqual(source["display_name"], "codex / Wrapped work")
        result = self.prepare(self.imported_bundle(source))
        self.assertEqual((Path(result["home"]) / "skills/_user_agents/shared/SKILL.md").read_text(), "wrapped shared skill")

    def test_manifest_declared_version_does_not_execute_source_cli(self):
        (self.source.parent / "taskr-environments.json").write_text(json.dumps({"version": 1, "environments": [{"home": ".codex", "kind": "codex", "cli_version": "0.160.0"}]}))
        with patch.object(c, "native_version", side_effect=AssertionError("must not execute a source CLI")):
            source = self.imported()["environments"][0]
            self.assertEqual(self.imported_bundle(source)["cli_version"], "0.160.0")

    def test_duplicate_and_encrypted_zip_entries_are_rejected(self):
        archive = self.root / "unsafe.zip"
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("config.toml", 'model="one"')
                output.writestr("config.toml", 'model="two"')
        with self.assertRaisesRegex(c.ProvisionError, "duplicate"):
            self.imported(archive)
        with zipfile.ZipFile(archive, "w") as output:
            output.writestr("config.toml", 'model="one"')
        raw = bytearray(archive.read_bytes())
        for signature, offset in ((b"PK\x03\x04", 6), (b"PK\x01\x02", 8)):
            index = raw.index(signature) + offset
            raw[index] |= 1
        archive.write_bytes(raw)
        with self.assertRaisesRegex(c.ProvisionError, "encrypted"):
            self.imported(archive)
        self.assertFalse(list((self.root / "cache").iterdir()))

    def test_directory_dependency_cannot_bypass_login_policy_or_copy_history(self):
        (self.source / "auth.json").write_text('{"token":"source-login"}')
        (self.source / "sessions").mkdir()
        (self.source / "sessions/history.jsonl").write_text("native history")
        (self.source / "config.toml").write_text('path="."')
        source = self.imported()["environments"][0]
        bundle = self.imported_bundle(source, "endpoint")
        self.assertFalse(any(e["path"] == "home/auth.json" or "sessions/" in e["path"] for e in bundle["files"]))
        (self.source / "config.toml").write_text('path="auth.json"')
        result = self.imported()
        self.assertFalse(result["environments"])
        self.assertIn("Native login files", result["issues"][0]["error"])


if __name__ == "__main__":
    unittest.main()
