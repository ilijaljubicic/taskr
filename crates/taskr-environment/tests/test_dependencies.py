"""Profile dependency regressions. All credentials and homes are isolated fixtures."""
import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from test_companion import c


class DependencyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="taskr-dependencies-")
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "user/.codex"
        self.home.mkdir(parents=True)
        (self.home / "config.toml").write_text('model="fixture"\n')
        self.addCleanup(self.temp.cleanup)
        for context in (patch.object(Path, "home", return_value=self.root / "user"),
                        patch.dict(os.environ, {"HOME": str(self.root / "user")}),
                        patch.object(c, "native_version", return_value="0.160.0")):
            context.start()
            self.addCleanup(context.stop)

    def bundle(self, policy="copy", kind="codex"):
        source = c.export(self.home, kind)
        return c.export_request({"source_home": str(self.home), "kind": kind,
                                 "source_revision": source["source_revision"], "credential_policy": policy})

    def prepare(self, policy="copy", kind="codex", **options):
        return c.prepare({"bundle": self.bundle(policy, kind),
                          "deployment_root": str(self.root / "deployments"), **options})

    def helper_profile(self):
        key = self.home / "credentials/key"
        key.parent.mkdir()
        key.write_text("dummy-private-key")
        (self.home / "config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI]\nwire_api="responses"\n'
            '[model_providers.ZAI.auth]\ncommand="cat"\nargs=[' + json.dumps(str(key)) + ']\n')
        return key

    def test_helper_credentials_copy_rebase_rotate_and_never_execute(self):
        key = self.helper_profile()
        with patch.object(c.subprocess, "run", side_effect=AssertionError("helpers must not execute")):
            source = c.export(self.home, "codex")
            first = self.prepare()
        config = tomllib.loads((Path(first["home"]) / "config.toml").read_text())
        copied = Path(config["model_providers"]["ZAI"]["auth"]["args"][0])
        self.assertEqual(copied.read_text(), "dummy-private-key")
        self.assertEqual(copied.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("dummy-private-key", json.dumps(first))
        key.write_text("rotated-dummy-key")
        self.assertEqual(source["source_revision"], c.export(self.home, "codex")["source_revision"])
        second = self.prepare()
        self.assertNotEqual(first["deployment_id"], second["deployment_id"])
        self.assertEqual(copied.read_text(), "dummy-private-key")

    def test_endpoint_helper_policy_uses_endpoint_key_without_native_login(self):
        self.helper_profile()
        bundle = self.bundle("endpoint")
        self.assertNotIn("home/credentials/key", [f["path"] for f in bundle["files"]])
        endpoint = self.root / "endpoint"
        (endpoint / "credentials").mkdir(parents=True)
        (endpoint / "credentials/key").write_text("endpoint-private-key")
        result = self.prepare("endpoint", endpoint_auth_home=str(endpoint))
        self.assertEqual((Path(result["home"]) / "credentials/key").read_text(), "endpoint-private-key")
        self.assertFalse((Path(result["home"]) / "auth.json").exists())
        self.assertNotIn("private-key", json.dumps(result))

    def test_endpoint_policy_can_provision_helper_credentials_omitted_from_source(self):
        key = self.helper_profile()
        key.unlink()
        with self.assertRaisesRegex(c.ProvisionError, "Source helper credential file is missing"):
            self.bundle("copy")
        endpoint = self.root / "endpoint"
        (endpoint / "credentials").mkdir(parents=True)
        (endpoint / "credentials/key").write_text("endpoint-only-key")
        result = self.prepare("endpoint", endpoint_auth_home=str(endpoint))
        self.assertEqual((Path(result["home"]) / "credentials/key").read_text(), "endpoint-only-key")

    def test_required_environment_is_per_effective_profile_and_rechecked_at_launch(self):
        (self.home / "zai.config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI]\nenv_key="TASKR_TEST_ZAI_KEY"\n'
            'env_http_headers={"Optional"="TASKR_TEST_OPTIONAL_HEADER"}\n')
        with patch.dict(os.environ, {}, clear=True):
            result = self.prepare()
            statuses = {p["native_profile"]: p for p in result["profile_readiness"]}
            self.assertEqual(statuses[None]["state"], "ready")
            self.assertEqual(statuses["zai"]["missing_environment"], ["TASKR_TEST_ZAI_KEY"])
            with self.assertRaisesRegex(c.ProvisionError, "environment"):
                c.verify({**result, "native_profile": "zai"})
        with patch.dict(os.environ, {"TASKR_TEST_ZAI_KEY": "never-return-this"}):
            self.assertEqual(c.verify({**result, "native_profile": "zai"})["deployment_id"], result["deployment_id"])
        self.assertNotIn("never-return-this", json.dumps(result))

    def test_missing_helper_program_is_not_ready(self):
        (self.home / "config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI.auth]\ncommand="taskr-missing-auth-helper"\n')
        (self.home / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "commands": [
            {"config": "config.toml", "pointer": "/model_providers/ZAI/auth", "files": [], "environment": []}]}))
        with self.assertRaisesRegex(c.ProvisionError, "executable"):
            self.prepare()

    def test_mcp_script_arguments_cwd_notify_and_launch_prerequisites(self):
        server = self.home / "server"
        server.mkdir()
        (server / "main.py").write_text("raise RuntimeError('must not run')")
        notify = self.home / "notify.py"
        notify.write_text("raise RuntimeError('must not run')")
        (self.home / "config.toml").write_text('notify=["python3",' + json.dumps(str(notify)) + ']\n'
            '[mcp_servers.fixture]\ncommand="python3"\nargs=["main.py"]\ncwd=' + json.dumps(str(server)) + '\n')
        result = self.prepare()
        config = tomllib.loads((Path(result["home"]) / "config.toml").read_text())
        self.assertEqual(Path(config["notify"][1]).read_text(), notify.read_text())
        mcp = config["mcp_servers"]["fixture"]
        self.assertTrue(Path(mcp["cwd"]).is_relative_to(result["home"]))
        self.assertTrue(Path(mcp["args"][0]).is_file())
        with patch.object(c.shutil, "which", return_value=None):
            with self.assertRaisesRegex(c.ProvisionError, "executable"):
                c.verify(result)

    def test_nested_role_config_paths_are_relative_to_declaring_config(self):
        role = self.root / "external/roles/worker.toml"
        role.parent.mkdir(parents=True)
        (role.parent / "instructions.md").write_text("nested instructions")
        role.write_text('model_instructions_file="instructions.md"\n')
        (self.home / "config.toml").write_text('[agents.worker]\nconfig_file=' + json.dumps(str(role)) + '\n')
        result = self.prepare()
        config = tomllib.loads((Path(result["home"]) / "config.toml").read_text())
        nested = Path(config["agents"]["worker"]["config_file"])
        instructions = Path(tomllib.loads(nested.read_text())["model_instructions_file"])
        self.assertEqual(instructions.read_text(), "nested instructions")
        self.assertTrue(instructions.is_relative_to(result["home"]))

    def test_tilde_catalog_rebased_and_config_cycles_rejected(self):
        (self.home / "catalog.json").write_text('{"models":[]}')
        (self.home / "config.toml").write_text('model_catalog_json="~/.codex/catalog.json"\n')
        result = self.prepare()
        catalog = tomllib.loads((Path(result["home"]) / "config.toml").read_text())["model_catalog_json"]
        self.assertTrue(Path(catalog).is_relative_to(result["home"]))
        (self.home / "config.toml").write_text('[agents.cycle]\nconfig_file="config.toml"\n')
        with self.assertRaisesRegex(c.ProvisionError, "cycle"):
            self.bundle()

    def test_opaque_commands_need_explicit_dependency_declarations(self):
        (self.home / "config.toml").write_text('[mcp_servers.inline]\ncommand="python3"\nargs=["-c","pass"]\n')
        with self.assertRaisesRegex(c.ProvisionError, "taskr-dependencies.json"):
            self.bundle()
        (self.home / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "commands": [
            {"config": "config.toml", "pointer": "/mcp_servers/inline", "files": [], "environment": []}]}))
        self.prepare()

    def test_claude_registry_extracts_configuration_not_trust_and_keeps_project_scope(self):
        (self.home / "settings.json").write_text('{}')
        helper = self.home / "helper.sh"
        helper.write_text("#!/bin/sh\nexit 1\n")
        helper.chmod(0o700)
        (self.home / "settings.json").write_text(json.dumps({"apiKeyHelper": str(helper)}))
        declaration = {"config": "settings.json", "pointer": "/apiKeyHelper", "files": [], "environment": []}
        (self.home / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "commands": [declaration]}))
        registry = {"mcpServers": {"user": {"command": "python3", "args": ["--version"]}},
                    "projects": {"/old/project": {"mcpServers": {"local": {"command": "python3", "args": ["--version"]}},
                                                       "hasTrustDialogAccepted": True}},
                    "oauthAccount": {"secret": "must-not-copy"}}
        (self.home / ".claude.json").write_text(json.dumps(registry))
        with self.assertRaisesRegex(c.ProvisionError, "project.*mapping"):
            self.bundle(kind="claude")
        (self.home / "taskr-dependencies.json").write_text(json.dumps({"version": 1,
            "commands": [declaration],
            "claude_projects": {"/old/project": "/endpoint/project"}}))
        result = self.prepare(kind="claude")
        deployed = Path(result["home"])
        registry = json.loads((deployed / ".claude.json").read_text())
        self.assertEqual(set(registry), {"mcpServers", "projects"})
        self.assertIn("/endpoint/project", registry["projects"])
        self.assertNotIn("local", registry["mcpServers"])
        self.assertNotIn("must-not-copy", json.dumps(registry))
        self.assertTrue(Path(json.loads((deployed / "settings.json").read_text())["apiKeyHelper"]).is_file())
        registry["oauthAccount"] = {"private": "runtime-login"}
        registry["projects"]["/endpoint/project"]["hasTrustDialogAccepted"] = True
        (deployed / ".claude.json").write_text(json.dumps(registry))
        c.verify(result)
        registry["projects"]["/endpoint/project"]["mcpServers"] = {}
        (deployed / ".claude.json").write_text(json.dumps(registry))
        with self.assertRaisesRegex(c.ProvisionError, "changed"):
            c.verify(result)

    def test_effective_profile_overrides_do_not_keep_base_environment_requirements(self):
        (self.home / "config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI]\nenv_key="TASKR_TEST_BASE_KEY"\n')
        (self.home / "alternate.config.toml").write_text('[model_providers.ZAI]\nenv_key="TASKR_TEST_ALTERNATE_KEY"\n')
        with patch.dict(os.environ, {"TASKR_TEST_ALTERNATE_KEY": "dummy"}, clear=True):
            result = self.prepare()
            statuses = {p["native_profile"]: p for p in result["profile_readiness"]}
            self.assertEqual(statuses[None]["state"], "blocked")
            self.assertEqual(statuses["alternate"]["state"], "ready")
            c.verify({**result, "native_profile": "alternate"})

    def test_refresh_rechecks_environment_without_rewriting_immutable_configuration(self):
        (self.home / "config.toml").write_text('model_provider="ZAI"\n[model_providers.ZAI]\nenv_key="TASKR_TEST_KEY"\n')
        with patch.dict(os.environ, {}, clear=True):
            first = self.prepare()
            self.assertEqual(first["profile_readiness"][0]["state"], "blocked")
            repeated = self.prepare()
            self.assertEqual(first["home"], repeated["home"])
        config_before = (Path(first["home"]) / "config.toml").read_bytes()
        with patch.dict(os.environ, {"TASKR_TEST_KEY": "dummy"}):
            ready = self.prepare()
            self.assertEqual(ready["profile_readiness"][0]["state"], "ready")
            self.assertEqual(first["home"], ready["home"])
        self.assertEqual(config_before, (Path(first["home"]) / "config.toml").read_bytes())

    def test_declared_helper_files_and_environment_obey_policy_and_integrity(self):
        helper = self.home / "helper.sh"
        helper.write_text("#!/bin/sh\nexit 99\n")
        helper.chmod(0o700)
        key = self.home / "key.pem"
        key.write_text("dummy-key-material")
        (self.home / "config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI.auth]\ncommand=' + json.dumps(str(helper)) + '\n')
        with self.assertRaisesRegex(c.ProvisionError, "taskr-dependencies.json"):
            self.bundle()
        (self.home / "taskr-dependencies.json").write_text(json.dumps({"version": 1, "commands": [
            {"config": "config.toml", "pointer": "/model_providers/ZAI/auth",
             "files": [{"path": "key.pem", "credential": True}], "environment": ["TASKR_TEST_HELPER_ACCOUNT"]}]}))
        with patch.dict(os.environ, {"TASKR_TEST_HELPER_ACCOUNT": "dummy"}):
            result = self.prepare()
            self.assertEqual((Path(result["home"]) / "key.pem").read_text(), "dummy-key-material")
            (Path(result["home"]) / "key.pem").write_text("tampered")
            with self.assertRaisesRegex(c.ProvisionError, "changed"):
                c.verify(result)

    def test_imported_helper_dependencies_cannot_escape_collection(self):
        self.helper_profile()
        key = self.root / "outside-key"
        key.write_text("dummy-outside-key")
        (self.home / "credentials/key").unlink()
        (self.home / "credentials/key").symlink_to(key)
        discovered = c.discover({"source_path": str(self.home.parent)})
        self.assertFalse(discovered["environments"])
        self.assertTrue(any("escapes" in i["error"] for i in discovered["issues"]))

    def test_endpoint_environment_provider_needs_no_native_login(self):
        (self.home / "config.toml").write_text('model_provider="ZAI"\n[model_providers.ZAI]\nenv_key="TASKR_TEST_KEY"\n')
        endpoint = self.root / "empty-endpoint"
        endpoint.mkdir()
        with patch.dict(os.environ, {"TASKR_TEST_KEY": "dummy"}):
            result = self.prepare("endpoint", endpoint_auth_home=str(endpoint))
            self.assertEqual(result["profile_readiness"][0]["state"], "ready")
            self.assertFalse((Path(result["home"]) / "auth.json").exists())

    def test_external_command_cwd_retains_declared_sibling_files(self):
        directory = self.root / "external/server"
        directory.mkdir(parents=True)
        (directory / "main.py").write_text("pass")
        (directory / "settings.json").write_text('{"fixture":true}')
        (self.home / "config.toml").write_text('[mcp_servers.fixture]\ncommand="python3"\n'
            'args=["main.py","--config=settings.json"]\ncwd=' + json.dumps(str(directory)) + '\n')
        result = self.prepare()
        mcp = tomllib.loads((Path(result["home"]) / "config.toml").read_text())["mcp_servers"]["fixture"]
        cwd = Path(mcp["cwd"])
        self.assertEqual(Path(mcp["args"][0]).parent, cwd)
        self.assertEqual(Path(mcp["args"][1].split("=", 1)[1]).parent, cwd)
        self.assertEqual(json.loads((cwd / "settings.json").read_text()), {"fixture": True})

    def test_relative_command_file_does_not_guess_task_working_directory(self):
        (self.home / "main.py").write_text("pass")
        (self.home / "config.toml").write_text('[mcp_servers.fixture]\ncommand="python3"\nargs=["main.py"]\n')
        with self.assertRaisesRegex(c.ProvisionError, "explicit cwd"):
            self.bundle()

    def test_endpoint_policy_rejects_recognized_inline_command_credentials(self):
        (self.home / "config.toml").write_text('[mcp_servers.fixture]\ncommand="echo"\nargs=["--api-key","dummy-inline-key"]\n')
        with self.assertRaisesRegex(c.ProvisionError, "embeds credentials"):
            self.bundle("endpoint")
        self.prepare("copy")

    def test_configuration_cannot_be_replaced_as_credential_data(self):
        (self.home / "config.toml").write_text('model_provider="ZAI"\n'
            '[model_providers.ZAI.auth]\ncommand="cat"\nargs=[' + json.dumps(str(self.home / "config.toml")) + ']\n')
        with self.assertRaisesRegex(c.ProvisionError, "configuration cannot be classified"):
            self.bundle()

    def test_absolute_native_binary_is_endpoint_prerequisite_not_bundle_content(self):
        binary = self.home / "bin/native-mcp"
        binary.parent.mkdir()
        with binary.open("wb") as stream:
            stream.write(b"\x7fELF")
            stream.truncate(c.MAX_BYTES + 1)
        binary.chmod(0o700)
        (self.home / "config.toml").write_text('[mcp_servers.native]\ncommand=' + json.dumps(str(binary)) + '\n')
        bundle = self.bundle()
        self.assertFalse(any(f["path"].endswith("native-mcp") for f in bundle["files"]))
        result = self.prepare()
        command = tomllib.loads((Path(result["home"]) / "config.toml").read_text())["mcp_servers"]["native"]["command"]
        self.assertEqual(command, str(binary))
        binary.unlink()
        with self.assertRaisesRegex(c.ProvisionError, "executable is missing"):
            c.verify(result)

    def test_destination_must_provision_the_configured_native_binary_path(self):
        binary = self.root / "installed/native-mcp"
        binary.parent.mkdir()
        binary.write_bytes(b"\x7fELFfixture")
        binary.chmod(0o700)
        (self.home / "config.toml").write_text('[mcp_servers.native]\ncommand=' + json.dumps(str(binary)) + '\n')
        bundle = self.bundle()
        binary.unlink()
        with self.assertRaisesRegex(c.ProvisionError, "executable is missing"):
            c.prepare({"bundle": bundle, "deployment_root": str(self.root / "deployments"), "remote": True})
        self.assertFalse((self.root / "deployments").exists())

    def test_explicit_interpreter_runtime_is_preserved_when_not_on_path(self):
        binary = self.home / "provided-runtime/bin/node"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"\x7fELFfixture")
        binary.chmod(0o700)
        (self.home / "config.toml").write_text('[mcp_servers.native]\ncommand=' + json.dumps(str(binary)) + '\nargs=["--version"]\n')
        with patch.dict(os.environ, {"PATH": "/nonexistent"}):
            result = self.prepare()
            command = tomllib.loads((Path(result["home"]) / "config.toml").read_text())["mcp_servers"]["native"]["command"]
            self.assertEqual(command, str(binary))
            c.verify(result)
            binary.unlink()
            with self.assertRaisesRegex(c.ProvisionError, "executable is missing"):
                c.verify(result)

    def test_tilde_interpreter_uses_endpoint_home_without_shortening_to_path_name(self):
        binary = self.root / "user/runtime/bin/python3"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"\x7fELFfixture")
        binary.chmod(0o700)
        (self.home / "config.toml").write_text('[mcp_servers.native]\ncommand="~/runtime/bin/python3"\nargs=["--version"]\n')
        with patch.dict(os.environ, {"PATH": "/nonexistent"}):
            result = self.prepare()
            command = tomllib.loads((Path(result["home"]) / "config.toml").read_text())["mcp_servers"]["native"]["command"]
            self.assertEqual(command, str(binary))
            c.verify(result)

    def test_claude_default_user_registry_is_collected_without_ambient_import_fallback(self):
        claude = self.root / "user/.claude"
        claude.mkdir()
        (claude / "settings.json").write_text('{}')
        (self.root / "user/.claude.json").write_text(json.dumps({
            "mcpServers": {"user": {"command": "python3", "args": ["--version"]}}, "trusted": True}))
        self.home = claude
        result = self.prepare(kind="claude")
        self.assertEqual(set(json.loads((Path(result["home"]) / ".claude.json").read_text())), {"mcpServers"})
        imported = c.discover({"source_path": str(claude)})["environments"][0]
        bundle = c.export_request({**imported, "credential_policy": "copy"})
        self.assertNotIn("home/.claude.json", [f["path"] for f in bundle["files"]])


if __name__ == "__main__":
    unittest.main()
