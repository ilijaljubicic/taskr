#!/usr/bin/env python3
"""Exercise environment MCP over HTTP with isolated homes and fake Herdr/SSH.

No coding agent, provider request, real SSH host, or user store is used.
Requires a debug TASKR build. Artifacts and a metadata-only report stay in /tmp.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import zipfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--taskr-bin", default=str(Path(__file__).resolve().parents[1] / "target/debug/taskr"))
parser.add_argument("--execution-ports", action="store_true", help="Also prove launch, pinned resume, namespace replacement and cleanup using fake Herdr resources")
args = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix="taskr-environment-mcp-proof-")).resolve(strict=True)
user = root / "user"
source = user / ".codex-work"
source.mkdir(parents=True)
(source / "config.toml").write_text('model="base-test"\n')
(source / "review.config.toml").write_text('model="review-test"\n[mcp_servers.profile_probe]\ncommand="/bin/echo"\n')
(source / "auth.json").write_text('{"OPENAI_API_KEY":"fixture-source-key"}')
key = source / "credentials/zai-key"
key.parent.mkdir()
key.write_text("fixture-source-helper-key")
(source / "glm-catalog.json").write_text(json.dumps({"models":[{"slug":"glm-fixture",
    "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"max"}]}]}))
(source / "glm.config.toml").write_text('model="glm-fixture"\nmodel_provider="ZAI"\nmodel_reasoning_effort="max"\n'
    'model_catalog_json="glm-catalog.json"\n'
    '[model_providers.ZAI.auth]\ncommand="cat"\nargs=[' + json.dumps(str(key)) + ']\n')
(source / "blocked.config.toml").write_text('model_provider="env-fixture"\n'
    '[model_providers.env-fixture]\nenv_key="TASKR_SMOKE_REQUIRED_PROVIDER_KEY"\n')
skill = source / "skills/proof/SKILL.md"
skill.parent.mkdir(parents=True)
skill.write_text("---\nname: taskr-fixture-proof\ndescription: A fixture skill.\n---\nFixture instructions.\n")
login = root / "endpoint-login"
login.mkdir()
(login / "auth.json").write_text('{"OPENAI_API_KEY":"fixture-endpoint-key"}')
(login / "credentials").mkdir()
(login / "credentials/zai-key").write_text("fixture-endpoint-helper-key")
bin_dir = root / "bin"
bin_dir.mkdir()
remote_id = "0123456789abcdef0123456789abcdef"


def executable(name, contents):
    path = bin_dir / name
    path.write_text(contents)
    path.chmod(0o700)
    return path


executable("codex", "#!/usr/bin/python3\nimport os,time,pathlib,sys\nif pathlib.Path(os.environ['PROOF_SLOW']).exists(): time.sleep(2)\nprint('--profile --no-daemon --no-alt-screen' if '--help' in sys.argv else 'codex-cli 0.160.0')\n")
executable("claude", "#!/usr/bin/python3\nprint('claude 2.1.0')\n")
herdr = executable("herdr", '''#!/usr/bin/python3
import json,os,pathlib,sys
args=sys.argv[1:]
with open(os.environ['PROOF_HERDR_LOG'],'a') as log: log.write(json.dumps(args)+'\\n')
if args[:2]==['machine','list']:
 result=[{'id':'0123456789abcdef0123456789abcdef','label':'Fixture remote','target':'fixture-host','session':'default','enabled':True,'selected':False}]
elif args[:2]==['api','snapshot']:
 result={'workspaces':[],'agents':[]}
else:
 print(json.dumps({'error':{'code':'forbidden_fixture_operation','message':'No terminals may be created'}})); sys.exit(1)
print(json.dumps({'result':result}))
''')
if args.execution_ports:
    herdr.write_text((Path(__file__).resolve().parents[1] / "crates/taskr-controller/src/fixtures/layout_herdr.py").read_text())
ssh = executable("ssh", '''#!/usr/bin/python3
import json,os,subprocess,sys
with open(os.environ['PROOF_SSH_LOG'],'a') as log: log.write(json.dumps(sys.argv[1:])+'\\n')
sys.exit(subprocess.call(['/bin/sh','-c',sys.argv[-1]]))
''')
environment = {**os.environ, "HOME": str(user), "PATH": str(bin_dir) + ":/usr/bin:/bin",
               "PROOF_SLOW": str(root / "slow"), "PROOF_HERDR_LOG": str(root / "herdr.jsonl"),
               "PROOF_SSH_LOG": str(root / "ssh.jsonl")}
if args.execution_ports:
    environment.update({"PROOF_WRITE_NATIVE_HISTORY": "1", "PROOF_MACHINE_CATALOG": json.dumps([
        {"id":remote_id, "target":"fixture-host", "enabled":True}])})
environment.pop("TASKR_MCP_TOKEN", None)
environment.pop("TASKR_SMOKE_REQUIRED_PROVIDER_KEY", None)
controller = None
session = None
base = None


def stop():
    global controller
    if controller is not None:
        controller.terminate()
        try:
            controller.wait(timeout=5)
        except subprocess.TimeoutExpired:
            controller.kill()
            controller.wait()
        controller = None


def start(admin=True):
    global controller, base, session
    session = None
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base = "http://127.0.0.1:" + str(port)
    command = [args.taskr_bin, "controller", "--port", str(port), "--store-path", str(root / "store"),
               "--herdr-bin", str(herdr), "--environment-ssh-bin", str(ssh), "--allow-remote-without-mcp-token"]
    if admin:
        command.append("--enable-admin-tools")
    with (root / "controller.log").open("ab") as log:
        controller = subprocess.Popen(command, env=environment, stdout=log, stderr=log)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if controller.poll() is not None:
            raise RuntimeError("Fixture controller failed to start; inspect controller.log")
        try:
            if urllib.request.urlopen(base + "/health", timeout=1).read() == b"ok":
                break
        except (OSError, urllib.error.URLError):
            time.sleep(0.05)
    else:
        raise RuntimeError("Fixture controller did not become healthy")
    initialized = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "taskr-environment-proof", "version": "1"}})
    assert initialized["result"]["serverInfo"]["name"] == "taskr-controller"
    rpc("notifications/initialized", {}, notification=True)


next_id = 0


def rpc(method, params, notification=False):
    global next_id, session
    next_id += 1
    message = {"jsonrpc": "2.0", "method": method, "params": params}
    if not notification:
        message["id"] = next_id
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if session:
        headers["Mcp-Session-Id"] = session
    request = urllib.request.Request(base + "/mcp", json.dumps(message).encode(), headers)
    with urllib.request.urlopen(request, timeout=10) as response:
        session = response.headers.get("Mcp-Session-Id", session)
        if notification:
            return None
        if response.headers.get_content_type() == "application/json":
            return json.loads(response.read())
        for line in response:
            if line.startswith(b"data:"):
                value = json.loads(line[5:])
                if value.get("id") == next_id:
                    return value
    raise RuntimeError("MCP returned no response")


def tool(name, arguments, refuse=False):
    response = rpc("tools/call", {"name": name, "arguments": arguments})
    failed = "error" in response or response.get("result", {}).get("isError", False)
    if refuse:
        assert failed, name + " should be rejected"
        return response
    assert not failed, name + " failed in isolated fixture"
    result = response["result"]
    return result.get("structuredContent") or json.loads(result["content"][0]["text"])


def wait(job):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        result = tool("admin_environment_sync_status", {"sync_job_id": job["sync_job_id"]})
        if result["state"] not in ("queued", "preparing"):
            assert result["state"] == "ready", "Fixture sync failed"
            return result
        time.sleep(0.02)
    raise RuntimeError("Fixture sync did not finish")


try:
    legacy_auth = {**environment, "MMUX_MCP_TOKEN": "fixture-old-token"}
    refused_auth = subprocess.run([args.taskr_bin, "controller", "--port", "0", "--store-path", str(root / "auth-refusal")],
                                  env=legacy_auth, capture_output=True, timeout=10)
    assert refused_auth.returncode != 0 and b"TASKR_MCP_TOKEN" in refused_auth.stderr
    assert b"fixture-old-token" not in refused_auth.stderr
    start()
    resources = rpc("resources/list", {})["result"]["resources"]
    assert any(resource["uri"] == "taskr://orchestration/status" for resource in resources)
    assert not any(resource["uri"].startswith("mmux://") for resource in resources)
    surface = rpc("tools/list", {})["result"]["tools"]
    names = {tool["name"] for tool in surface}
    assert {"admin_environment_discover", "admin_environment_sync", "admin_environment_sync_status", "admin_environment_sync_cancel"} <= names
    project = tool("project_create", {"title": "Taskr core MCP proof", "description": "Exercise the host-provided SQLite store", "slug": "taskr-core-proof"})
    plan = tool("plan_create", {"project_id": project["id"], "title": "Public orchestration contract", "brief": "Verify core mutations through the TASKR host"})
    task = tool("task_create", {"plan_id": plan["id"], "title": "Persistent fixture task", "objective": "Retain the task through restart without starting an agent"})
    tool("task_update", {"task_id": task["id"], "title": ""}, refuse=True)
    assert tool("task_get", {"task_id": task["id"]})["task"]["title"] == task["title"]
    tool("list_launch_profiles", {}, refuse=True)
    assert not tool("list_launch_profiles", {"endpoint_id": "local"})["profiles"]
    discovered = tool("admin_environment_discover", {"homes": [str(source)]})
    assert not discovered["issues"]
    selected = discovered["environments"][0]
    request = {"source_environment_id": selected["source_environment_id"], "source_revision": selected["source_revision"],
               "endpoint_id": "local", "credential_policy": "endpoint", "endpoint_auth_home": str(login),
               "deployment_root": str(root / "local-deployments")}
    dry = wait(tool("admin_environment_sync", {**request, "dry_run": True}))
    assert not dry["launch_profile_ids"] and not Path(dry["prepared"]["home"]).exists()
    local = wait(tool("admin_environment_sync", request))
    assert len(local["launch_profile_ids"]) == 3
    assert any(p["native_profile"] == "blocked" and p["missing_environment"] == ["TASKR_SMOKE_REQUIRED_PROVIDER_KEY"]
               for p in local["prepared"]["profile_readiness"])
    deployed = Path(local["prepared"]["home"])
    assert "fixture-endpoint-key" in (deployed / "auth.json").read_text()
    assert (deployed / "skills/proof/SKILL.md").read_text() == skill.read_text()
    helper = tomllib.loads((deployed / "glm.config.toml").read_text())["model_providers"]["ZAI"]["auth"]
    assert Path(helper["args"][0]).read_text() == "fixture-endpoint-helper-key"
    assert Path(helper["args"][0]).is_relative_to(deployed)
    assert not tool("list_launch_profiles", {"endpoint_id": remote_id})["profiles"]
    remote = wait(tool("admin_environment_sync", {**request, "endpoint_id": remote_id,
                  "deployment_root": str(root / "remote-deployments")}))
    assert len(tool("list_launch_profiles", {"endpoint_id": remote_id})["profiles"]) == 3
    (root / "slow").touch()
    canceled = tool("admin_environment_sync", {**request, "refresh": True})
    assert tool("admin_environment_sync_cancel", {"sync_job_id": canceled["sync_job_id"]})["state"] == "canceled"
    (root / "slow").unlink()
    tool("admin_environment_discover", {"homes": [], "source_path": str(source)}, refuse=True)
    tool("admin_environment_discover", {"source_path": " "}, refuse=True)
    collection = root / "collection"
    imported_home = collection / "codex/work"
    imported_home.mkdir(parents=True)
    (imported_home / "config.toml").write_text('model="import-base"\n')
    (imported_home / "review.config.toml").write_text('model="import-review"\n[mcp_servers.profile_probe]\ncommand="/bin/echo"\n')
    (imported_home / "auth.json").write_text('{"OPENAI_API_KEY":"fixture-import-key"}')
    imported_skill = imported_home / "skills/import-proof/SKILL.md"
    imported_skill.parent.mkdir(parents=True)
    imported_skill.write_text('---\nname: taskr-import-proof\ndescription: Imported fixture.\n---\nProvided instructions.\n')
    claude_home = collection / "claude/work"
    claude_home.mkdir(parents=True)
    (claude_home / "settings.json").write_text('{"model":"fixture-claude"}')
    ambient_skill = user / ".agents/skills/ambient/SKILL.md"
    ambient_skill.parent.mkdir(parents=True)
    ambient_skill.write_text("Never include this in imported environments")
    folder_discovery = tool("admin_environment_discover", {"source_path": str(collection)})
    assert not folder_discovery["issues"] and len(folder_discovery["environments"]) == 2
    folder_source = next(e for e in folder_discovery["environments"] if e["kind"] == "codex")
    folder_request = {**request, "source_environment_id": folder_source["source_environment_id"],
                      "source_revision": folder_source["source_revision"]}
    folder_ready = wait(tool("admin_environment_sync", folder_request))
    folder_home = Path(folder_ready["prepared"]["home"])
    assert (folder_home / "skills/import-proof/SKILL.md").is_file()
    assert not (folder_home / "skills/_user_agents/ambient").exists()
    archive_path = root / "environments.zip"
    with zipfile.ZipFile(archive_path, "w") as archive:
        for path in sorted(collection.rglob("*")):
            if path.is_file():
                archive.write(path, str(path.relative_to(collection)))
    zip_discovery = tool("admin_environment_discover", {"source_path": str(archive_path)})
    assert not zip_discovery["issues"] and len(zip_discovery["environments"]) == 2
    zip_source = next(e for e in zip_discovery["environments"] if e["kind"] == "codex")
    zip_request = {**request, "source_environment_id": zip_source["source_environment_id"],
                   "source_revision": zip_source["source_revision"], "credential_policy": "copy"}
    zip_ready = wait(tool("admin_environment_sync", zip_request))
    zip_home = Path(zip_ready["prepared"]["home"])
    assert "fixture-import-key" in (zip_home / "auth.json").read_text()
    assert not (zip_home / "skills/_user_agents/ambient").exists()
    remote_zip = wait(tool("admin_environment_sync", {**zip_request, "endpoint_id": remote_id,
                      "deployment_root": str(root / "remote-deployments")}))
    unsafe = root / "unsafe.zip"
    with zipfile.ZipFile(unsafe, "w") as archive:
        archive.writestr("../escape.txt", "must not escape")
    tool("admin_environment_discover", {"source_path": str(unsafe)}, refuse=True)
    assert not (root / "escape.txt").exists()
    stop()
    start()
    assert tool("task_get", {"task_id": task["id"]})["task"]["title"] == task["title"]
    reopened_zip = wait(tool("admin_environment_sync", {**zip_request, "refresh": True}))
    assert reopened_zip["launch_profile_ids"] == zip_ready["launch_profile_ids"]
    assert len(tool("list_launch_profiles", {"endpoint_id": "local", "include_retained": True})["profiles"]) == 7
    stop()
    start(admin=False)
    assert len(tool("list_launch_profiles", {"endpoint_id": "local", "include_retained": True})["profiles"]) == 7
    assert tool("admin_environment_sync_status", {"sync_job_id": local["sync_job_id"]}, refuse=True)
    tool("admin_environment_discover", {"homes": [str(source)]}, refuse=True)
    execution_checks = []
    if args.execution_ports:
        stop()
        start(admin=True)
        inventory = tool("list_environments", {})
        assert any(row["source_environment_id"] == selected["source_environment_id"] for row in inventory["environments"])
        assert tool("list_environments", {"project_id":project["id"]})["environments"] == []
        assert tool("list_launch_profiles", {"endpoint_id":"local", "project_id":project["id"]})["profiles"] == []
        tool("project_update", {"project_id":project["id"], "environment_ids":[selected["source_environment_id"]]})
        assigned = tool("list_launch_profiles", {"endpoint_id":"local", "project_id":project["id"]})["profiles"]
        assert assigned and all(p["source_environment_id"] == selected["source_environment_id"] for p in assigned)
        workspace = root / "workspace"
        workspace.mkdir()
        glm = next(p for p in tool("list_launch_profiles", {"endpoint_id":"local"})["profiles"] if p["native_profile"] == "glm")
        helper_path = Path(helper["args"][0])
        helper_contents = helper_path.read_bytes()
        helper_path.unlink()
        calls_before = (root / "herdr.jsonl").read_text()
        tool("start_coding_session", {"task_id":task["id"], "endpoint_id":"local", "launch_profile_id":glm["id"], "workspace_path":str(workspace)}, refuse=True)
        after = (root / "herdr.jsonl").read_text()[len(calls_before):]
        assert not any(json.loads(line)[:2] in (["workspace", "create"], ["tab", "create"], ["pane", "split"]) for line in after.splitlines())
        helper_path.write_bytes(helper_contents)
        helper_path.chmod(0o600)
        tool("start_coding_session", {"task_id":task["id"], "endpoint_id":"local", "launch_profile_id":"", "workspace_path":str(workspace)}, refuse=True)
        tool("start_coding_session", {"task_id":task["id"], "endpoint_id":"local", "launch_profile_id":glm["id"], "workspace_path":str(workspace), "launch_options":{"reasoning_effort":"xhigh"}}, refuse=True)
        tool("task_update", {"task_id":task["id"], "launch_hints":{"model":"Prefer GLM", "reasoning_effort":"Deep reasoning"}})
        launch = tool("start_coding_session", {"task_id":task["id"], "endpoint_id":"local", "launch_profile_id":glm["id"], "workspace_path":str(workspace), "launch_options":{"model":"glm-fixture", "reasoning_effort":"high"}})
        original = launch["execution"]
        assert original["runtime_generation"] and original["agent_session"]
        assert original["launch_options"] == {"model":"glm-fixture", "reasoning_effort":"high"}
        other_task = tool("task_create", {"plan_id":plan["id"], "title":"Different per-task effort", "objective":"Verify independent launch choices"})
        other = tool("start_coding_session", {"task_id":other_task["id"], "endpoint_id":"local", "launch_profile_id":glm["id"], "workspace_path":str(workspace), "template":"validate", "launch_options":{"reasoning_effort":"low"}})["execution"]
        assert other["launch_profile_id"] == original["launch_profile_id"]
        assert other["launch_options"]["reasoning_effort"] == "low"
        assert tomllib.loads((Path(local["prepared"]["home"]) / "glm.config.toml").read_text())["model_reasoning_effort"] == "max"
        tool("execution_stop", {"execution_id":other["execution_id"]})
        stopped = tool("execution_stop", {"execution_id":original["execution_id"]})["execution"]
        assert stopped["pane_closed"] and stopped["agent_session"] == original["agent_session"]
        tool("project_update", {"project_id":project["id"], "environment_ids":[]})
        tool("execution_resume", {"execution_id":original["execution_id"]}, refuse=True)
        tool("project_update", {"project_id":project["id"], "environment_ids":[selected["source_environment_id"]]})
        resumed = tool("execution_resume", {"execution_id":original["execution_id"]})["execution"]
        assert resumed["inspection"] and resumed["agent_session"] == original["agent_session"]
        assert resumed["launch_options"] == original["launch_options"]
        assert 'model_reasoning_effort="high"' in resumed["launch_args"]
        assert resumed["pane_id"] != original["pane_id"]
        runtime_path = bin_dir / "layout-state.json"
        runtime = json.loads(runtime_path.read_text())
        runtime["generation"] = "fixture-v2"
        runtime_path.write_text(json.dumps(runtime))
        calls_before = (root / "herdr.jsonl").read_text()
        tool("execution_stop", {"execution_id":resumed["execution_id"]}, refuse=True)
        assert not any(json.loads(line)[:2] == ["pane", "close"] for line in (root / "herdr.jsonl").read_text()[len(calls_before):].splitlines())
        stop()
        start(admin=False)
        lost = tool("task_get", {"task_id":task["id"]})["task"]["execution"]
        assert lost["phase"] == "exited" and lost["pane_closed"]
        assert lost["agent_session"] == original["agent_session"]
        reopened = tool("execution_resume", {"execution_id":original["execution_id"]})["execution"]
        assert reopened["runtime_generation"] != original["runtime_generation"]
        assert reopened["agent_session"] == original["agent_session"]
        tool("execution_stop", {"execution_id":reopened["execution_id"]})
        # Losing history must refuse before creating another pane.
        history = Path(local["prepared"]["home"]) / "sessions" / (original["agent_session"] + ".jsonl")
        history.unlink()
        calls_before = (root / "herdr.jsonl").read_text()
        tool("execution_resume", {"execution_id":original["execution_id"]}, refuse=True)
        after = (root / "herdr.jsonl").read_text()[len(calls_before):]
        assert not any(json.loads(line)[:2] in (["workspace", "create"], ["tab", "create"], ["pane", "split"]) for line in after.splitlines())
        execution_checks = ["missing helper credential refuses allocation", "pinned launch and generation", "verified exit/pane close", "restored native history resume", "stale namespace refusal", "restart namespace recovery", "new generation resume", "missing history refuses allocation"]
    stop()
    start(admin=True)
    claude_source = user / ".claude-work"
    plugin = claude_source / "plugins/cache/fixture/proof/1"
    (plugin / "skills/proof").mkdir(parents=True)
    (plugin / "skills/proof/SKILL.md").write_text("Claude plugin proof skill")
    (plugin / "probe.py").write_text("raise RuntimeError('Plugin must not execute during provisioning')")
    (plugin / ".mcp.json").write_text(json.dumps({"probe":{"command":"python3", "args":["${CLAUDE_PLUGIN_ROOT}/probe.py"]}}))
    marketplace = claude_source / "plugins/marketplaces/fixture"
    (marketplace / ".claude-plugin").mkdir(parents=True)
    (marketplace / ".claude-plugin/marketplace.json").write_text(json.dumps({"name":"fixture", "owner":{"name":"Fixture"}, "plugins":[{"name":"proof", "source":"./proof"}]}))
    (claude_source / "settings.json").write_text(json.dumps({"model":"sonnet", "enabledPlugins":{"proof@fixture":True}}))
    (claude_source / "plugins/installed_plugins.json").write_text(json.dumps({"version":2,"plugins":{"proof@fixture":[{"scope":"user","version":"1","installPath":str(plugin)}]}}))
    (claude_source / "plugins/known_marketplaces.json").write_text(json.dumps({"fixture":{"source":{"source":"github","repo":"fixture/plugins"},"installLocation":str(marketplace)}}))
    (login / ".credentials.json").write_text('{"accessToken":"fixture-claude-endpoint-key"}')
    broken = user / ".claude-broken"
    broken.mkdir()
    (broken / "settings.json").write_text('{"enabledPlugins":{"missing@fixture":true}}')
    discovery = tool("admin_environment_discover", {"homes":[str(claude_source),str(broken)]})
    assert len(discovery["environments"]) == 2 and len(discovery["issues"]) == 1
    ready_source = next(e for e in discovery["environments"] if e["source_home"] == str(claude_source))
    blocked_source = next(e for e in discovery["environments"] if e["source_home"] == str(broken))
    assert blocked_source["sync_blockers"]
    tool("project_update", {"project_id":project["id"], "environment_ids":[selected["source_environment_id"],ready_source["source_environment_id"],blocked_source["source_environment_id"]]})
    assigned = tool("list_environments", {"project_id":project["id"]})["environments"]
    assert any(e["source_environment_id"] == blocked_source["source_environment_id"] and e["sync_blockers"] for e in assigned)
    claude_request = {"source_environment_id":ready_source["source_environment_id"],"source_revision":ready_source["source_revision"],
                      "endpoint_id":"local","credential_policy":"endpoint","endpoint_auth_home":str(login),"deployment_root":str(root / "claude-deployments")}
    tool("admin_environment_sync", {**claude_request,"source_environment_id":blocked_source["source_environment_id"],"source_revision":blocked_source["source_revision"]}, refuse=True)
    claude_local = wait(tool("admin_environment_sync", claude_request))
    claude_home = Path(claude_local["prepared"]["home"])
    choice = next(p for p in tool("list_launch_profiles", {"endpoint_id":"local","project_id":project["id"]})["profiles"] if p["id"] in claude_local["launch_profile_ids"])
    assert "CLAUDE_CODE_PLUGIN_CACHE_DIR" in choice["env_keys"]
    record = json.loads((claude_home / "plugins/installed_plugins.json").read_text())["plugins"]["proof@fixture"][0]
    assert Path(record["installPath"]).is_relative_to(claude_home)
    assert (Path(record["installPath"]) / "skills/proof/SKILL.md").read_text() == "Claude plugin proof skill"
    assert (claude_home / ".credentials.json").read_text() == (login / ".credentials.json").read_text()
    claude_remote = wait(tool("admin_environment_sync", {**claude_request,"endpoint_id":remote_id,"deployment_root":str(root / "claude-remote-deployments")}))
    assert len(claude_remote["launch_profile_ids"]) == 1
    stop()
    start()
    assert any(p["id"] in claude_local["launch_profile_ids"] for p in tool("list_launch_profiles", {"endpoint_id":"local","project_id":project["id"]})["profiles"])
    assert any(e["source_environment_id"] == blocked_source["source_environment_id"] and e["sync_blockers"] for e in tool("list_environments", {"project_id":project["id"]})["environments"])
    calls = (root / "herdr.jsonl").read_text()
    if not args.execution_ports:
        assert not any(word in calls for word in ['"workspace"', '"agent"', '"pane"'])
    db = (root / "store/taskr.db").read_bytes()
    assert all(secret not in db for secret in [b"fixture-endpoint-key", b"fixture-source-key", b"fixture-import-key", b"fixture-endpoint-helper-key", b"fixture-source-helper-key", b"fixture-claude-endpoint-key"])
    report = {"passed": True, "local_choices": local["launch_profile_ids"], "remote_choices": remote["launch_profile_ids"],
              "imported_folder_home": str(folder_home), "imported_zip_home": str(zip_home), "remote_zip_choices": remote_zip["launch_profile_ids"],
              "checks": ["HTTP MCP schemas", "explicit endpoint", "native base/profile discovery", "dry-run", "destination login policy",
                         "skill cloning", "saved-target remote transport", "cancel", "folder and ZIP collection discovery", "imported native profiles",
                         "ambient skill isolation", "ZIP traversal refusal", "ZIP cache restart and remote sync", "restart persistence", "worker admin denial", "metadata-only SQLite",
                         "taskr-core host store mutations", "rejected mutation rollback", "project/plan/task restart persistence",
                         "provider helper credential policy and rebasing", "blocked profile prerequisite reporting",
                         "Claude plugin payload and registry rebasing", "Claude plugin remote sync", "blocked Claude homes remain assignable",
                         "Claude setup blockers and assignments survive restart"] + execution_checks,
              "real_agents_started": 0, "real_ssh_connections": 0, "user_controller_restarted": False}
    (root / "report.json").write_text(json.dumps(report, indent=2))
    print("Environment MCP smoke passed; evidence:", root / "report.json")
finally:
    stop()
