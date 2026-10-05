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


class CompanionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="taskr-bundle-test-")
        # macOS temporary directories can be reached through a symlinked /var.
        self.root = Path(self.temp.name).resolve(strict=True)
        self.source = self.root / "source/.codex"
        self.source.mkdir(parents=True)
        (self.source / "config.toml").write_text('model = "test-model"\n')
        self.home_patch = patch.object(Path, "home", return_value=self.root / "user")
        self.version_patch = patch.object(c, "native_version", return_value="0.160.0")
        self.home_patch.start()
        self.version_patch.start()
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(self.home_patch.stop)
        self.addCleanup(self.version_patch.stop)

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
        self.assertNotIn("review-model", json.dumps(result))
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

    def test_legacy_inline_profiles_and_unsupported_claude_plugins_report_issues(self):
        (self.source / "config.toml").write_text('[profiles.review]\nmodel="test"')
        result = c.discover({"homes": [str(self.source)]})
        self.assertFalse(result["environments"])
        self.assertIn("Legacy", result["issues"][0]["error"])
        claude = self.root / "source/.claude"
        claude.mkdir()
        (claude / "settings.json").write_text('{"enabledPlugins":{"test@market":true}}')
        result = c.discover({"homes": [str(claude)]})
        self.assertFalse(result["environments"])
        self.assertIn("unsupported", result["issues"][0]["error"])

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
