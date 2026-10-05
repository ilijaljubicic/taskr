//! Behavioral regressions for the Herdr cutover decisions in DelegateToHerdrPlan.md.
//! Exercise real tool handlers with an isolated durable store and a fake CLI;
//! MCP protocol checks use an in-memory transport; never start a Herdr server,
//! model, SSH connection, or HTTP listener.
//! Run: `cargo test -p taskr-controller herdr_contract_tests --lib`.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

const EXECUTION_ID: &str = "exec-contract-original";
const AGENT_NAME: &str = "taskr-contract-codex-a1";
const PANE_ID: &str = "w1:p1";

#[derive(Clone, Copy)]
enum Observation {
    IdleAgent,
    DoneAgent,
    AvailableShell,
    LookupUnavailable,
    ReplacedOccupant,
    EndpointUnavailable,
    EmptyEndpoint,
    PaneListUnavailable,
    AgentPresentPaneListUnavailable,
    OrphanPane,
    OrphanShell,
    ExitIgnored,
    ShellCommand,
    UnknownShell,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "taskr-herdr-contract-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn environment_admin_tools_reject_workers_before_discovery_or_transport() {
    let (state, _, _) = seeded_state();
    let fixture = Fixture::new(state, Observation::EmptyEndpoint).await;
    for operation in [
        "admin_environment_discover",
        "admin_environment_sync",
        "admin_environment_sync_status",
        "admin_environment_sync_cancel",
    ] {
        let result = match operation {
            "admin_environment_discover" => {
                fixture.server.admin_environment_discover_tool(None).await
            }
            "admin_environment_sync" => fixture.server.admin_environment_sync_tool(None).await,
            "admin_environment_sync_cancel" => {
                fixture
                    .server
                    .admin_environment_sync_status_tool(None, true)
                    .await
            }
            _ => {
                fixture
                    .server
                    .admin_environment_sync_status_tool(None, false)
                    .await
            }
        };
        assert!(result.is_err(), "{operation} must be admin gated");
    }
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn launch_catalog_is_endpoint_scoped_and_requires_canonical_endpoint_argument() {
    let (state, _, _) = seeded_state();
    let fixture = Fixture::new(state, Observation::EmptyEndpoint).await;
    assert!(fixture
        .server
        .list_launch_profiles_tool(Some(&json!({})))
        .await
        .is_err());
    assert!(fixture
        .server
        .list_launch_profiles_tool(Some(&json!({"node":"local"})))
        .await
        .is_err());
    assert!(fixture
        .server
        .list_launch_profiles_tool(Some(&json!({"endpoint_id":""})))
        .await
        .is_err());
    let result = fixture
        .server
        .list_launch_profiles_tool(Some(&json!({"endpoint_id":"remote-a"})))
        .await
        .unwrap();
    let value = result.structured_content.unwrap();
    assert_eq!(value["endpoint_id"], "remote-a");
    assert!(value["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["endpoint_id"] == "remote-a"));
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn missing_prepared_environment_refuses_start_before_any_herdr_allocation() {
    let (state, _, task_id) = seeded_state();
    let mut fixture = Fixture::new(state, Observation::EmptyEndpoint).await;
    let home = fixture.dir.0.join("missing-home").display().to_string();
    let deployment = taskr_environment::PreparedEnvironment {
        deployment_id: "dep-fixture".into(),
        bundle_digest: "fixture-revision".into(),
        source_environment_id: "env-fixture".into(),
        source_revision: "revision".into(),
        kind: "codex".into(),
        display_name: "codex / fixture".into(),
        native_profiles: Vec::new(),
        home: home.clone(),
        cli_version: "0.160.0".into(),
        credential_policy: "endpoint".into(),
        authentication: "file_provisioned".into(),
    };
    fixture.server.launch_profiles = ResolvedLaunchProfiles::in_memory(
        vec![taskr_environment::LaunchChoice {
            endpoint_id: "local".into(),
            profile: LaunchProfile {
                id: "codex".into(),
                agent_kind: "codex".into(),
                args: Vec::new(),
                bypass_args: None,
                env: BTreeMap::from([("CODEX_HOME".into(), home)]),
                description: None,
            },
            deployment: Some(deployment),
            native_profile: None,
        }],
        CompanionConfig::default(),
    );
    assert_refused(
        fixture
            .server
            .start_coding_session_tool(Some(&start_args(&task_id)))
            .await,
    );
    fixture.assert_no_runtime_mutations();
    assert!(fixture.task(&task_id).execution.is_none());
}

#[tokio::test]
async fn environment_mcp_schema_exposes_explicit_policy_endpoint_and_cancellation() {
    let (state, _, _) = seeded_state();
    let fixture = Fixture::new_with_admin(state, Observation::EmptyEndpoint, true).await;
    let server = HerdrMcpServer {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        policy: fixture.server.policy.clone(),
        scheduler: fixture.server.scheduler.clone(),
        orchestration: fixture.server.orchestration.clone(),
        wait_jobs: fixture.server.wait_jobs.clone(),
    };
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let service = rmcp::service::serve_directly(server, server_transport, None);
    let mut stream = tokio::io::BufReader::new(client_transport);
    let catalog = file_tool_removal_rpc(&mut stream, 1, "tools/list", json!({})).await;
    let tools = catalog["result"]["tools"].as_array().unwrap();
    for name in [
        "admin_environment_discover",
        "admin_environment_sync",
        "admin_environment_sync_status",
        "admin_environment_sync_cancel",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
    let sync = tools
        .iter()
        .find(|tool| tool["name"] == "admin_environment_sync")
        .unwrap();
    let discovery = tools
        .iter()
        .find(|tool| tool["name"] == "admin_environment_discover")
        .unwrap();
    assert_eq!(
        discovery["inputSchema"]["properties"]["source_path"]["type"],
        "string"
    );
    for arguments in [
        json!({"homes": [], "source_path": "/package"}),
        json!({"source_path": " "}),
        json!({"source_path": "/package", "cache_root": "/caller-selected-cache"}),
    ] {
        let result = file_tool_removal_rpc(
            &mut stream,
            2,
            "tools/call",
            json!({"name":"admin_environment_discover", "arguments":arguments}),
        )
        .await;
        assert!(result.get("error").is_some() || result["result"]["isError"] == true);
    }
    for field in [
        "source_environment_id",
        "source_revision",
        "endpoint_id",
        "credential_policy",
    ] {
        assert!(sync["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!(field)));
    }
    let list = tools
        .iter()
        .find(|tool| tool["name"] == "list_launch_profiles")
        .unwrap();
    assert_eq!(list["inputSchema"]["required"], json!(["endpoint_id"]));
    let invalid = file_tool_removal_rpc(
        &mut stream,
        2,
        "tools/call",
        json!({"name":"admin_environment_sync", "arguments":{"endpoint_id":"local"}}),
    )
    .await;
    assert!(invalid.get("error").is_some());
    service.cancel().await.unwrap();
    fixture.assert_no_runtime_mutations();
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn respond(result: Value) -> String {
    format!(
        "printf '%s' {}",
        shell_quote(&json!({"id": "cli:contract", "result": result}).to_string())
    )
}

fn unavailable() -> String {
    "printf '%s' 'unable to connect to Herdr endpoint: connection refused' >&2; exit 1".into()
}

fn agent_row(observation: Observation) -> Value {
    let replaced = matches!(observation, Observation::ReplacedOccupant);
    let mut row = json!({
        "name": if replaced { "user-replacement" } else { AGENT_NAME },
        "agent": if replaced { "claude" } else { "codex" },
        "agent_status": "idle",
        "pane_id": PANE_ID,
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "terminal_id": "terminal-1",
        "focused": false,
        "revision": 7,
        "cwd": "/remote/repo",
        "foreground_cwd": "/remote/repo",
        "interactive_ready": true,
        "launch_pending": false,
        "agent_session": {
            "source": if replaced { "herdr:claude" } else { "herdr:codex" },
            "agent": if replaced { "claude" } else { "codex" },
            "kind": "id",
            "value": if replaced { "native-replacement" } else { "native-original" }
        }
    });
    if matches!(
        observation,
        Observation::AvailableShell
            | Observation::OrphanShell
            | Observation::ShellCommand
            | Observation::UnknownShell
    ) {
        row["agent"] = Value::Null;
        row["agent_status"] = json!("shelling");
        row["agent_session"] = Value::Null;
    }
    if matches!(observation, Observation::DoneAgent) {
        row["agent_status"] = json!("done");
    }
    if matches!(observation, Observation::UnknownShell) {
        row["agent_status"] = json!("unknown");
    }
    row
}

fn fake_cli(dir: &TempDir, observation: Observation) -> HerdrClient {
    let agent = agent_row(observation);
    let pane = json!({
        "pane_id": PANE_ID,
        "terminal_id": "terminal-1",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "focused": false,
        "revision": 7,
        "agent": agent["agent"],
        "agent_status": agent["agent_status"],
        "agent_session": agent["agent_session"],
        "cwd": "/remote/repo",
        "foreground_cwd": "/remote/repo",
        "label": agent["name"],
        "state_labels": {},
        "tokens": {}
    });
    let agents = if matches!(
        observation,
        Observation::EmptyEndpoint
            | Observation::PaneListUnavailable
            | Observation::OrphanPane
            | Observation::OrphanShell
            | Observation::ShellCommand
            | Observation::UnknownShell
    ) {
        json!([])
    } else {
        json!([agent.clone()])
    };
    let panes = if matches!(observation, Observation::EmptyEndpoint) {
        json!([])
    } else {
        json!([pane.clone()])
    };
    let get = if matches!(
        observation,
        Observation::LookupUnavailable | Observation::EndpointUnavailable
    ) {
        unavailable()
    } else if matches!(
        observation,
        Observation::OrphanPane | Observation::OrphanShell | Observation::UnknownShell
    ) {
        "printf '%s' '{\"error\":{\"code\":\"agent_not_found\",\"message\":\"agent target \
         taskr-contract-codex-a1 not found\"}}' >&2; exit 1"
            .into()
    } else {
        respond(json!({"type": "agent_info", "agent": agent}))
    };
    let list = if matches!(observation, Observation::EndpointUnavailable) {
        unavailable()
    } else {
        respond(json!({"type": "agent_list", "agents": agents}))
    };
    let pane_list = if matches!(
        observation,
        Observation::PaneListUnavailable
            | Observation::AgentPresentPaneListUnavailable
            | Observation::EndpointUnavailable
    ) {
        unavailable()
    } else {
        respond(json!({"type": "pane_list", "panes": panes}))
    };
    let pane_get = if matches!(
        observation,
        Observation::LookupUnavailable | Observation::EndpointUnavailable
    ) {
        unavailable()
    } else {
        respond(json!({"type": "pane_info", "pane": pane}))
    };
    let snapshot = if matches!(
        observation,
        Observation::LookupUnavailable | Observation::EndpointUnavailable
    ) {
        unavailable()
    } else {
        respond(json!({"agents": agents, "panes": panes, "workspaces": []}))
    };
    let bin = dir.0.join("herdr");
    let log = dir.0.join("calls.log");
    let exited = dir.0.join("exited");
    let closed = dir.0.join("closed");
    let mut shell_agent = agent_row(Observation::AvailableShell);
    shell_agent["name"] = json!(AGENT_NAME);
    let mut shell_pane = pane.clone();
    shell_pane["agent"] = Value::Null;
    shell_pane["agent_status"] = json!("shelling");
    shell_pane["agent_session"] = Value::Null;
    // A completed, unseen turn remains `done` until it is explicitly focused.
    // Waiting only for `idle` must time out without changing that state.
    let ready_check = format!(
        "matched=false; previous=''; for argument in \"$@\"; do \
         if [ \"$previous\" = '--until' ] && [ \"$argument\" = {status} ]; then matched=true; fi; \
         previous=$argument; done; \
         if [ \"$matched\" != true ]; then \
         printf '%s' '{{\"error\":{{\"code\":\"timeout\",\"message\":\"timed out waiting for agent status\"}}}}' >&2; exit 1; fi",
        status = shell_quote(agent["agent_status"].as_str().unwrap()),
    );
    let prompt = if matches!(observation, Observation::DoneAgent) {
        format!(
            "for argument in \"$@\"; do if [ \"$argument\" = '--wait' ]; then {ready_check}; break; fi; done; {}",
            respond(json!({"type": "agent_prompted", "agent": agent.clone()}))
        )
    } else {
        respond(json!({"type": "agent_prompted", "agent": agent.clone()}))
    };
    let body = format!(
        "#!/bin/sh\nset -eu\n\
         {{ printf '<call>\\n'; printf '%s\\n' \"$@\"; printf '</call>\\n'; }} >> {log}\n\
         while [ \"${{1-}}\" = '--machine' ] || [ \"${{1-}}\" = '--session' ]; do shift 2; done\n\
         case \"${{1-}} ${{2-}}\" in\n\
           'workspace list') {workspaces} ;;\n\
           'workspace create') {allocation} ;;\n\
           'tab list') {tabs} ;;\n\
           'tab rename'|'pane rename') {empty} ;;\n\
           'agent start') {started} ;;\n\
           'agent get') if [ -f {exited} ]; then {shell_agent}; else {get}; fi ;;\n\
           'agent list') if [ -f {exited} ]; then {no_agents}; else {list}; fi ;;\n\
           'agent prompt') for argument in \"$@\"; do if [ \"$argument\" = '/exit' ]; then {exit_marker}; fi; done; {prompt} ;;\n\
           'agent send-keys') {empty} ;;\n\
           'agent wait') {ready_check}; {waited} ;;\n\
           'pane get') if [ -f {exited} ]; then {shell_pane}; else {pane_get}; fi ;;\n\
           'pane process-info') if [ -f {exited} ]; then {shell_process_info}; else {process_info}; fi ;;\n\
           'pane list') if [ -f {closed} ]; then {no_panes}; elif [ -f {exited} ]; then {shell_panes}; else {pane_list}; fi ;;\n\
           'pane read'|'agent read') printf '%s' 'fixture output' ;;\n\
           'pane close') touch {closed}; {empty} ;;\n\
           'pane run') {empty} ;;\n\
           'api snapshot') {snapshot} ;;\n\
           'machine list') {machines} ;;\n\
           'machine status') {reachable} ;;\n\
           *) printf '%s' 'unexpected fake CLI operation' >&2; exit 2 ;;\n\
         esac\n",
        tabs = respond(json!({"tabs": []})),
        log = shell_quote(log.to_str().unwrap()),
        exited = shell_quote(exited.to_str().unwrap()),
        closed = shell_quote(closed.to_str().unwrap()),
        exit_marker = if matches!(observation, Observation::ExitIgnored) { ":".into() } else { format!("touch {}", shell_quote(exited.to_str().unwrap())) },
        shell_agent = respond(json!({"agent": shell_agent})),
        shell_pane = respond(json!({"pane": shell_pane})),
        shell_panes = respond(json!({"panes": [shell_pane]})),
        no_agents = respond(json!({"agents": []})),
        no_panes = respond(json!({"panes": []})),
        process_info = respond(json!({"process_info": {"pane_id": PANE_ID, "shell_pid": 500,
            "foreground_process_group_id": if matches!(observation, Observation::OrphanShell | Observation::AvailableShell | Observation::UnknownShell) { 500 } else { 501 },
            "foreground_processes": [{"pid": if matches!(observation, Observation::OrphanShell | Observation::AvailableShell | Observation::UnknownShell) { 500 } else { 501 }, "name": "process"}]}})),
        shell_process_info = respond(json!({"process_info": {"pane_id": PANE_ID, "shell_pid": 500, "foreground_process_group_id": 500, "foreground_processes": [{"pid": 500, "name": "bash"}]}})),
        workspaces = respond(json!({"type": "workspace_list", "workspaces": []})),
        allocation = respond(json!({
            "type": "workspace_created",
            "workspace": {
                "workspace_id": "w1", "number": 1, "label": "contract",
                "focused": false, "pane_count": 1, "tab_count": 1,
                "active_tab_id": "w1:t1", "agent_status": "shelling"
            },
            "tab": {
                "tab_id": "w1:t1", "workspace_id": "w1", "number": 1,
                "label": "contract", "focused": false, "pane_count": 1,
                "agent_status": "shelling"
            },
            "root_pane": {
                "pane_id": PANE_ID, "terminal_id": "terminal-1", "workspace_id": "w1",
                "tab_id": "w1:t1", "focused": false, "revision": 1,
                "agent": null, "agent_status": "shelling", "agent_session": null
            }
        })),
        started = respond(
            json!({"type": "agent_started", "agent": agent_row(Observation::IdleAgent), "argv": ["codex", "--fixture-argument"]})
        ),
        prompt = prompt,
        waited = respond(json!({"type": "agent_waited", "agent": agent})),
        empty = respond(json!({})),
        machines = respond(json!({"machines": [{"id": "remote-a"}]})),
        reachable = respond(json!({"reachable": true})),
    );
    std::fs::write(&bin, body).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    HerdrClient::new(HerdrClientConfig {
        bin,
        local_session: None,
    })
}

fn profiles() -> Arc<ResolvedLaunchProfiles> {
    let mut choices = Vec::new();
    for endpoint in ["local", "remote-a"] {
        for kind in ["codex", "claude", "opencode", "kimi"] {
            choices.push(taskr_environment::LaunchChoice {
                endpoint_id: endpoint.into(),
                deployment: None,
                native_profile: None,
                profile: LaunchProfile {
                    id: kind.into(),
                    agent_kind: kind.into(),
                    args: vec!["--fixture-argument".into()],
                    bypass_args: Some(vec!["--fixture-bypass".into()]),
                    env: BTreeMap::new(),
                    description: None,
                },
            });
        }
    }
    ResolvedLaunchProfiles::in_memory(choices, CompanionConfig::default())
}

fn task_input(plan_id: PlanId, title: &str) -> CreateTask {
    CreateTask {
        plan_id,
        title: title.into(),
        objective: format!("Perform {title}"),
        scope: TaskScope::default(),
        gates: vec![],
        slug: None,
        auto_schedule: false,
        run_spec: None,
    }
}

fn seeded_state() -> (OrchestrationState, PlanId, TaskId) {
    let mut state = OrchestrationState::new();
    let project = state
        .create_project(
            CreateProject {
                title: "Herdr contracts".into(),
                description: "Isolated regression fixture".into(),
                codex_home: Some("/remote/homes/codex".into()),
                claude_home: Some("/remote/homes/claude".into()),
                opencode_home: Some("/remote/homes/opencode".into()),
                kimi_home: Some("/remote/homes/kimi".into()),
                ..Default::default()
            },
            1,
        )
        .unwrap();
    let plan = state
        .create_plan(
            CreatePlan {
                project_id: project.id,
                title: "Herdr contract plan".into(),
                brief: "Verify the delegated runtime contracts".into(),
                instructions: None,
                slug: None,
            },
            2,
        )
        .unwrap();
    let task = state
        .create_task(task_input(plan.id.clone(), "Implementation"), 3)
        .unwrap();
    (state, plan.id, task.id)
}

fn execution(phase: ExecutionPhase) -> TaskExecution {
    TaskExecution {
        native_session_name: None,
        group: taskr_core::orchestration::ExecutionGroup::Work,
        inspection: false,
        resumed_from: None,
        pane_closed: false,
        report: None,
        execution_id: EXECUTION_ID.into(),
        endpoint_id: "local".into(),
        runtime_generation: None,
        launch_profile_id: "codex".into(),
        launch_args: vec![],
        launch_env: BTreeMap::new(),
        bypass_permissions: false,
        workspace_path: "/remote/repo".into(),
        role: "worker".into(),
        kind: "implementation".into(),
        skills: vec!["rust".into()],
        workspace_id: Some("w1".into()),
        tab_id: Some("w1:t1".into()),
        pane_id: Some(PANE_ID.into()),
        terminal_id: Some("terminal-1".into()),
        agent_name: Some(AGENT_NAME.into()),
        agent_kind: Some("codex".into()),
        agent_session: Some("native-original".into()),
        phase,
        recovery: ExecutionRecovery::Reconciled,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_seen_ms: 1,
    }
}

struct Fixture {
    server: HerdrMcpServer,
    dir: TempDir,
}

impl Fixture {
    async fn new(state: OrchestrationState, observation: Observation) -> Self {
        Self::new_with_admin(state, observation, false).await
    }

    async fn new_with_admin(
        state: OrchestrationState,
        observation: Observation,
        enable_admin_tools: bool,
    ) -> Self {
        let dir = TempDir::new();
        let store = store::SqliteOrchestrationStore::open(dir.0.join("store")).unwrap();
        store.save(&state, 10).unwrap();
        let orchestration = orchestration_actor::OrchestrationHandle::from_store(store).unwrap();
        let herdr = fake_cli(&dir, observation);
        let launch_profiles = profiles();
        let ctx = LaunchContext {
            herdr: herdr.clone(),
            launch_profiles: launch_profiles.clone(),
            orchestration: orchestration.clone(),
        };
        let (scheduler, _) = Actor::spawn(
            None,
            OrchestrationSchedulerActor,
            OrchestrationSchedulerState { ctx },
        )
        .await
        .unwrap();
        let server = HerdrMcpServer {
            herdr,
            launch_profiles,
            policy: Arc::new(ControllerPolicy {
                enable_admin_tools,
                max_timeout_seconds: 120.0,
                max_request_bytes: 2 * 1024 * 1024,
                max_capture_bytes: 2 * 1024 * 1024,
            }),
            scheduler,
            orchestration,
            wait_jobs: Arc::new(Mutex::new(HashMap::new())),
        };
        Self { server, dir }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.0.join("calls.log")).unwrap_or_default()
    }

    fn called(&self, group: &str, operation: &str) -> bool {
        self.calls().contains(&format!("\n{group}\n{operation}\n"))
    }

    fn assert_no_runtime_mutations(&self) {
        for (group, operation) in [
            ("workspace", "create"),
            ("tab", "create"),
            ("pane", "split"),
            ("agent", "start"),
            ("agent", "prompt"),
            ("agent", "send-keys"),
            ("pane", "run"),
            ("pane", "close"),
        ] {
            assert!(
                !self.called(group, operation),
                "unexpected {group} {operation}: {}",
                self.calls()
            );
        }
    }

    fn task(&self, task_id: &TaskId) -> Task {
        self.server.orchestration.snapshot().unwrap().tasks[task_id].clone()
    }
}

fn start_args(task_id: &TaskId) -> Value {
    json!({
        "task_id": task_id.0,
        "endpoint_id": "local",
        "launch_profile_id": "codex",
        "workspace_path": "/remote/repo",
        "bypass_permissions": false
    })
}

#[tokio::test]
async fn passed_task_keeps_worker_for_a_linked_audit_then_exits_after_audit_finishes() {
    for audit_status in [
        TaskStatus::Passed,
        TaskStatus::Delivered,
        TaskStatus::Canceled,
        TaskStatus::Failed,
    ] {
        let (mut state, plan_id, task_id) = seeded_state();
        state
            .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
            .unwrap();
        let audit = state.create_task(task_input(plan_id, "Audit"), 5).unwrap();
        state
            .update_task_status(&audit.id, TaskStatus::Running, 6)
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: audit.id.clone(),
                    to: task_id.clone(),
                    kind: TaskEdgeKind::Audits,
                    note: None,
                },
                7,
            )
            .unwrap();
        let fixture = Fixture::new(state, Observation::IdleAgent).await;
        let result = fixture
            .server
            .task_status_update_tool(Some(&json!({
                "task_id": task_id.0, "status": "Passed", "outcome": "Implementation passed"
            })))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["runtime_cleanup"]["state"],
            "held_for_audit"
        );
        assert_eq!(
            fixture.task(&task_id).execution.unwrap().phase,
            ExecutionPhase::Live
        );
        assert!(fixture.calls().is_empty());
        let ctx = LaunchContext {
            herdr: fixture.server.herdr.clone(),
            launch_profiles: fixture.server.launch_profiles.clone(),
            orchestration: fixture.server.orchestration.clone(),
        };
        assert!(execution_cleanup::sweep_finished_workers(&ctx)
            .await
            .is_empty());
        assert!(
            fixture.calls().is_empty(),
            "retry sweep must respect the audit hold"
        );
        let result = fixture
            .server
            .task_status_update_tool(Some(&json!({
                "task_id": audit.id.0, "status": audit_status, "outcome": "Audit ended"
            })))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(fixture.task(&task_id).status, TaskStatus::Passed);
        assert_eq!(
            fixture.task(&task_id).execution.unwrap().phase,
            ExecutionPhase::Stopped
        );
        assert_eq!(fixture.task(&audit.id).status, audit_status);
        assert!(fixture.called("pane", "close"));
    }
}

#[tokio::test]
async fn worker_is_held_until_all_linked_audits_finish() {
    let (mut state, plan_id, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let mut audits = Vec::new();
    for title in ["Audit one", "Audit two"] {
        let audit = state
            .create_task(task_input(plan_id.clone(), title), 5)
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: audit.id.clone(),
                    to: task_id.clone(),
                    kind: TaskEdgeKind::Audits,
                    note: None,
                },
                6,
            )
            .unwrap();
        audits.push(audit.id);
    }
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    for (id, status) in [
        (&task_id, TaskStatus::Passed),
        (&audits[0], TaskStatus::Passed),
    ] {
        let result = fixture
            .server
            .task_status_update_tool(Some(
                &json!({"task_id": id.0, "status": status, "outcome": "Passed"}),
            ))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
    }
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Live
    );
    assert!(fixture.calls().is_empty());
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": audits[1].0, "status": "Canceled"})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
}

#[tokio::test]
async fn cancellation_exits_immediately_even_when_an_audit_is_unfinished() {
    let (mut state, plan_id, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let audit = state.create_task(task_input(plan_id, "Audit"), 5).unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: audit.id,
                to: task_id.clone(),
                kind: TaskEdgeKind::Audits,
                note: None,
            },
            6,
        )
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task_id.0, "status": "Canceled"})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
}

#[tokio::test]
async fn final_task_cleanup_uses_the_recorded_remote_endpoint() {
    let (mut state, _, task_id) = seeded_state();
    let mut binding = execution(ExecutionPhase::Live);
    binding.endpoint_id = "remote-a".into();
    state.record_execution(&task_id, binding, 4).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({
            "task_id": task_id.0, "status": "Canceled"
        })))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(fixture.calls().contains("--machine\nremote-a\nagent\nget"));
    assert!(fixture
        .calls()
        .contains("--machine\nremote-a\nagent\nprompt"));
    assert!(fixture.calls().contains("/exit\n"));
    assert!(fixture.called("pane", "close"));
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().endpoint_id,
        "remote-a"
    );
}

#[tokio::test]
async fn late_cleanup_cannot_replace_a_newer_execution_binding() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Stopped), 4)
        .unwrap();
    let mut newer = execution(ExecutionPhase::Failed);
    newer.execution_id = "exec-newer".into();
    state.record_execution(&task_id, newer, 5).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let before = fixture.task(&task_id).execution.unwrap();
    assert!(fixture
        .server
        .orchestration
        .record_execution_if_current(task_id.clone(), execution(ExecutionPhase::Stopped))
        .is_err());
    assert_eq!(fixture.task(&task_id).execution.unwrap(), before);
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn final_task_status_closes_pane_and_preserves_conversation_and_report() {
    for status in [
        TaskStatus::Passed,
        TaskStatus::Delivered,
        TaskStatus::Canceled,
        TaskStatus::Failed,
    ] {
        let (mut state, _, task_id) = seeded_state();
        state
            .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
            .unwrap();
        state
            .update_task_status(&task_id, TaskStatus::Running, 5)
            .unwrap();
        let fixture = Fixture::new(state, Observation::IdleAgent).await;
        let args = json!({"task_id": task_id.0, "status": status,
            "outcome": "Operator's final report", "evidence": ["Preserved proof"]});
        let result = fixture
            .server
            .task_status_update_tool(Some(&args))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let task = fixture.task(&task_id);
        assert_eq!(task.status, status);
        assert_eq!(task.outcome.as_deref(), Some("Operator's final report"));
        assert_eq!(task.evidence, ["Preserved proof"]);
        assert_eq!(
            task.execution.as_ref().unwrap().phase,
            ExecutionPhase::Stopped
        );
        assert_eq!(
            task.execution.as_ref().unwrap().agent_session.as_deref(),
            Some("native-original")
        );
        assert!(fixture.called("pane", "close"));
        assert!(!fixture.called("workspace", "close"));
        assert!(!fixture.called("agent", "start"));
        let before = fixture.calls();
        let result = fixture
            .server
            .task_status_update_tool(Some(&args))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            fixture.calls(),
            before,
            "repeated final status must not close again"
        );
    }
}

#[tokio::test]
async fn validation_and_paused_task_states_do_not_close_workers() {
    for status in [
        TaskStatus::Running,
        TaskStatus::WaitingForValidation,
        TaskStatus::Blocked,
    ] {
        let (mut state, _, task_id) = seeded_state();
        state
            .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
            .unwrap();
        let fixture = Fixture::new(state, Observation::IdleAgent).await;
        let result = fixture
            .server
            .task_status_update_tool(Some(&json!({
                "task_id": task_id.0, "status": status, "outcome": "Validation report"
            })))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        assert_eq!(
            fixture.task(&task_id).execution.unwrap().phase,
            ExecutionPhase::Live
        );
        assert!(fixture.calls().is_empty());
    }
}

#[tokio::test]
async fn rejected_delivery_does_not_close_a_running_worker() {
    let (mut state, _, task_id) = seeded_state();
    state.tasks.get_mut(&task_id).unwrap().gates = vec!["Verified proof required".into()];
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Running, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({
            "task_id": task_id.0, "status": "Delivered"
        })))
        .await;
    assert_refused(result);
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Running);
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn final_status_never_closes_a_replacement_occupant() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::ReplacedOccupant).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({
            "task_id": task_id.0, "status": "Canceled", "outcome": "Canceled by operator"
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["runtime_cleanup"]["state"],
        "pending"
    );
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Canceled);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn missing_agent_name_does_not_authorize_closing_an_occupied_pane() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Exited), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanPane).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({
            "task_id": task_id.0, "status": "Canceled"
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn restart_cleanup_closes_the_verified_shell_of_a_finished_worker() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Exited), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Canceled, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanShell).await;
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    };
    assert!(execution_cleanup::sweep_finished_workers(&ctx)
        .await
        .is_empty());
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
    assert!(fixture.called("pane", "close"));
    assert!(!fixture.called("workspace", "close"));
}

#[tokio::test]
async fn failed_cleanup_keeps_final_state_and_retries_when_endpoint_recovers() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::EndpointUnavailable).await;
    let result = fixture.server.task_status_update_tool(Some(&json!({
        "task_id": task_id.0, "status": "Delivered", "outcome": "Verified delivery", "evidence": ["proof"]
    }))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Delivered);
    fixture.assert_no_runtime_mutations();
    fake_cli(&fixture.dir, Observation::IdleAgent);
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: OrchestrationHandle::from_store(
            store::SqliteOrchestrationStore::open(fixture.dir.0.join("store")).unwrap(),
        )
        .unwrap(),
    };
    assert!(execution_cleanup::sweep_finished_workers(&ctx)
        .await
        .is_empty());
    let task = ctx.orchestration.snapshot().unwrap().tasks[&task_id].clone();
    assert_eq!(task.status, TaskStatus::Delivered);
    assert_eq!(task.outcome.as_deref(), Some("Verified delivery"));
    assert_eq!(task.execution.unwrap().phase, ExecutionPhase::Stopped);
    assert!(fixture.called("pane", "close"));
}

#[tokio::test]
async fn canceled_task_cannot_be_revived_by_stale_launch_progress() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Starting), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Canceled, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    };
    assert!(ctx
        .persist(&task_id, execution(ExecutionPhase::Live))
        .is_err());
    assert!(fixture
        .server
        .orchestration
        .mark_execution_running(task_id.clone(), EXECUTION_ID, "Started".into())
        .is_err());
    assert!(!fixture
        .server
        .orchestration
        .mark_execution_blocked(
            task_id.clone(),
            EXECUTION_ID,
            "Late startup error".into(),
            None,
            false
        )
        .unwrap());
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Canceled);
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn cancel_during_allocation_retains_the_new_pane_without_starting_an_agent() {
    let (state, _, task_id) = seeded_state();
    let fixture = Fixture::new(state, Observation::OrphanPane).await;
    let bin = fixture.dir.0.join("herdr");
    let entered = fixture.dir.0.join("allocation-entered");
    let release = fixture.dir.0.join("release-allocation");
    let body = std::fs::read_to_string(&bin).unwrap();
    let barrier = format!(
        "'workspace create') touch {}; while [ ! -e {} ]; do sleep 0.01; done;",
        shell_quote(entered.to_str().unwrap()),
        shell_quote(release.to_str().unwrap())
    );
    std::fs::write(&bin, body.replacen("'workspace create')", &barrier, 1)).unwrap();
    let args = start_args(&task_id);
    let start = fixture.server.start_coding_session_tool(Some(&args));
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !entered.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let result = fixture
            .server
            .task_status_update_tool(Some(&json!({"task_id": task_id.0, "status": "Canceled"})))
            .await
            .unwrap();
        std::fs::write(&release, "continue").unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
    };
    let (result, _) = tokio::join!(start, cancel);
    assert_refused(result);
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Canceled);
    assert!(matches!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped | ExecutionPhase::Exited
    ));
    assert!(!fixture.called("pane", "close"));
    assert!(!fixture.called("agent", "start"));
    assert!(!fixture.called("agent", "prompt"));
}

#[tokio::test]
async fn coding_ready_wait_accepts_an_unseen_completed_turn_without_focusing_it() {
    for observation in [Observation::IdleAgent, Observation::DoneAgent] {
        let (mut state, _, task_id) = seeded_state();
        state
            .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
            .unwrap();
        let fixture = Fixture::new(state, observation).await;
        let snapshot = Arc::new(Mutex::new(RuntimeWaitSnapshot {
            wait_id: "wait-contract".into(),
            execution_id: EXECUTION_ID.into(),
            endpoint_id: "local".into(),
            kind: RuntimeWaitKind::CodingReady,
            state: "running".into(),
            result: None,
            error: None,
            started_at_ms: now_ms(),
            completed_at_ms: None,
        }));
        run_runtime_wait(
            fixture.server.herdr.clone(),
            EndpointTarget::local(None),
            AGENT_NAME.into(),
            RuntimeWaitKind::CodingReady,
            None,
            Duration::from_secs(1),
            Duration::from_millis(10),
            Duration::from_millis(10),
            snapshot.clone(),
        )
        .await;
        let snapshot = snapshot.lock().unwrap();
        assert_eq!(snapshot.state, "completed", "{snapshot:?}");
        let expected = if matches!(observation, Observation::DoneAgent) {
            "done"
        } else {
            "idle"
        };
        assert_eq!(snapshot.result.as_ref().unwrap()["agent_status"], expected);
        assert_eq!(snapshot.result.as_ref().unwrap()["matched"], expected);
        fixture.assert_no_runtime_mutations();
        assert!(!fixture.called("agent", "focus"));
    }
}

#[tokio::test]
async fn coding_send_wait_accepts_an_unseen_completed_turn() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::DoneAgent).await;
    let result = fixture.server.coding_send_tool(Some(&json!({
        "execution_id": EXECUTION_ID, "prompt": "Return the proof result", "wait_until_idle": true
    }))).await.unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(fixture.called("agent", "prompt"));
    assert!(!fixture.called("agent", "focus"));
}

fn assert_refused(result: Result<CallToolResult, McpError>) {
    match result {
        Err(_) => {}
        Ok(result) => assert_eq!(
            result.is_error,
            Some(true),
            "operation should have been refused: {result:?}"
        ),
    }
}

// Reconciliation may refresh phase/availability/timestamps, but it must keep
// the identity, placement, and frozen launch intent of an existing attempt.
fn assert_binding_retained(task: Task, before: &TaskExecution) {
    let after = task
        .execution
        .expect("the durable execution binding must be retained");
    assert_eq!(after.execution_id, before.execution_id);
    assert_eq!(after.endpoint_id, before.endpoint_id);
    assert_eq!(after.workspace_id, before.workspace_id);
    assert_eq!(after.tab_id, before.tab_id);
    assert_eq!(after.pane_id, before.pane_id);
    assert_eq!(after.agent_name, before.agent_name);
    assert_eq!(after.agent_kind, before.agent_kind);
    assert_eq!(after.agent_session, before.agent_session);
    assert_eq!(after.launch_profile_id, before.launch_profile_id);
    assert_eq!(after.launch_args, before.launch_args);
    assert_eq!(after.launch_env, before.launch_env);
    assert_eq!(after.workspace_path, before.workspace_path);
}

async fn file_tool_removal_rpc(
    stream: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stream
        .get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == id {
                return response;
            }
        }
    })
    .await
    .expect("MCP request must finish")
}

#[tokio::test]
async fn removed_file_tools_are_unknown_and_leave_files_and_herdr_untouched() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let server = HerdrMcpServer {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        policy: fixture.server.policy.clone(),
        scheduler: fixture.server.scheduler.clone(),
        orchestration: fixture.server.orchestration.clone(),
        wait_jobs: fixture.server.wait_jobs.clone(),
    };
    let dir = &fixture.dir;
    let existing = dir.0.join("existing.txt");
    let new_file = dir.0.join("new.txt");
    std::fs::write(&existing, "private file contents").unwrap();
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let service = rmcp::service::serve_directly(server, server_transport, None);
    let mut stream = tokio::io::BufReader::new(client_transport);

    let catalog = file_tool_removal_rpc(&mut stream, 1, "tools/list", json!({})).await;
    let tools = catalog["result"]["tools"].as_array().unwrap();
    for name in ["read_file", "save_file"] {
        assert!(tools.iter().all(|tool| tool["name"] != name));
    }
    for name in [
        "coding_read",
        "capture_output",
        "coding_send",
        "project_list",
    ] {
        assert!(tools.iter().any(|tool| tool["name"] == name));
    }

    for (index, (name, arguments)) in [
        ("read_file", json!({"path": existing})),
        ("save_file", json!({"path": existing, "content": "replace"})),
        (
            "save_file",
            json!({"path": existing, "content": "append", "append": true}),
        ),
        ("save_file", json!({"path": new_file, "content": "create"})),
    ]
    .into_iter()
    .enumerate()
    {
        let response = file_tool_removal_rpc(
            &mut stream,
            index as u64 + 2,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
        .await;
        assert_eq!(
            response["error"]["message"],
            format!("unknown tool '{name}'")
        );
        assert!(!response.to_string().contains("private file contents"));
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "private file contents"
        );
        assert!(!new_file.exists());
        assert!(
            !dir.0.join("calls.log").exists(),
            "file tools must not invoke Herdr"
        );
    }

    let read = file_tool_removal_rpc(
        &mut stream,
        6,
        "tools/call",
        json!({"name": "coding_read", "arguments": {"execution_id": EXECUTION_ID}}),
    )
    .await;
    assert!(read.get("error").is_none(), "{read}");
    assert!(
        read["result"].to_string().contains("fixture output"),
        "{read}"
    );
    service.cancel().await.unwrap();
}

#[tokio::test]
async fn descriptive_task_kind_and_skills_survive_each_agent_preset() {
    let (state, _, task_id) = seeded_state();
    let fixture = Fixture::new(state.clone(), Observation::IdleAgent).await;
    for agent_kind in ["codex", "claude", "opencode", "kimi"] {
        let request = ResolvedLaunchRequest {
            template: None,
            endpoint_id: "remote-a".into(),
            launch_profile_id: agent_kind.into(),
            workspace_path: "/remote/repo with spaces".into(),
            bypass_permissions: false,
            role: "reviewer".into(),
            kind: "review".into(),
            skills: vec!["rust-review".into()],
        };
        let (_, resolved, execution) = prepare_execution(
            &fixture.server.launch_context(),
            &state,
            &state.tasks[&task_id],
            None,
            Some(&request),
        )
        .expect("descriptive task kind is independent of executable agent kind");
        assert_eq!(execution.kind, "review");
        assert_eq!(execution.role, "reviewer");
        assert_eq!(execution.agent_kind.as_deref(), Some(agent_kind));
        assert_eq!(execution.skills, ["rust-review"]);
        assert_eq!(execution.endpoint_id, "remote-a");
        assert_eq!(execution.workspace_path, "/remote/repo with spaces");
        assert_eq!(
            resolved.env[home_env_var_for_kind(agent_kind).unwrap()],
            format!("/remote/homes/{agent_kind}")
        );
    }
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn run_spec_descriptive_kind_survives_frozen_launch_intent() {
    let (state, _, task_id) = seeded_state();
    let fixture = Fixture::new(state.clone(), Observation::IdleAgent).await;
    let run_spec = TaskRunSpec {
        endpoint_id: "remote-a".into(),
        launch_profile_id: "codex".into(),
        workspace_path: "/remote/repo".into(),
        bypass_permissions: false,
        role: "worker".into(),
        kind: "implementation".into(),
        skills: vec!["rust".into()],
        template: "task".into(),
        instruction: "Implement the contract".into(),
    };
    let (_, _, execution) = prepare_execution(
        &fixture.server.launch_context(),
        &state,
        &state.tasks[&task_id],
        Some(&run_spec),
        None,
    )
    .expect("run_spec kind is descriptive metadata");
    assert_eq!(execution.kind, "implementation");
    assert_eq!(execution.agent_kind.as_deref(), Some("codex"));
    assert_eq!(execution.skills, ["rust"]);
}

#[tokio::test]
async fn manual_start_rejects_unaccepted_dependency_before_allocation() {
    let (mut state, plan_id, task_id) = seeded_state();
    let dependency = state
        .create_task(task_input(plan_id, "Dependency"), 4)
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: task_id.clone(),
                to: dependency.id,
                kind: TaskEdgeKind::DependsOn,
                note: None,
            },
            5,
        )
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;

    let result = fixture
        .server
        .start_coding_session_tool(Some(&start_args(&task_id)))
        .await;

    fixture.assert_no_runtime_mutations();
    assert_refused(result);
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Backlog);
    assert!(fixture.task(&task_id).execution.is_none());
}

async fn assert_validator_blocks_manual_start(validator_status: TaskStatus) {
    let (mut state, plan_id, task_id) = seeded_state();
    let dependency = state
        .create_task(task_input(plan_id.clone(), "Accepted implementation"), 4)
        .unwrap();
    let validator = state
        .create_task(task_input(plan_id, "Validator"), 5)
        .unwrap();
    state
        .update_task_status(&dependency.id, TaskStatus::Passed, 6)
        .unwrap();
    state
        .update_task_status(&validator.id, validator_status, 7)
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: task_id.clone(),
                to: dependency.id.clone(),
                kind: TaskEdgeKind::DependsOn,
                note: None,
            },
            8,
        )
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: validator.id,
                to: dependency.id,
                kind: TaskEdgeKind::Validates,
                note: None,
            },
            9,
        )
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;

    let result = fixture
        .server
        .start_coding_session_tool(Some(&start_args(&task_id)))
        .await;

    fixture.assert_no_runtime_mutations();
    assert_refused(result);
    assert!(fixture.task(&task_id).execution.is_none());
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Backlog);
}

#[tokio::test]
async fn failed_validator_blocks_manual_start() {
    assert_validator_blocks_manual_start(TaskStatus::Failed).await;
}

#[tokio::test]
async fn blocked_validator_blocks_manual_start() {
    assert_validator_blocks_manual_start(TaskStatus::Blocked).await;
}

#[tokio::test]
async fn accepted_dependency_and_validator_permit_manual_start() {
    let (mut state, plan_id, task_id) = seeded_state();
    let dependency = state
        .create_task(task_input(plan_id.clone(), "Accepted implementation"), 4)
        .unwrap();
    let validator = state
        .create_task(task_input(plan_id, "Accepted validator"), 5)
        .unwrap();
    state
        .update_task_status(&dependency.id, TaskStatus::Passed, 6)
        .unwrap();
    state
        .update_task_status(&validator.id, TaskStatus::Passed, 7)
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: task_id.clone(),
                to: dependency.id.clone(),
                kind: TaskEdgeKind::DependsOn,
                note: None,
            },
            8,
        )
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: validator.id,
                to: dependency.id,
                kind: TaskEdgeKind::Validates,
                note: None,
            },
            9,
        )
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;

    let result = fixture
        .server
        .start_coding_session_tool(Some(&start_args(&task_id)))
        .await
        .unwrap();

    assert_ne!(
        result.is_error,
        Some(true),
        "accepted dependencies and validators must permit launch: {result:?}"
    );
    assert!(fixture.called("agent", "start"));
    assert!(fixture.called("agent", "prompt"));
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Running);
}

#[tokio::test]
async fn eligible_manual_start_reaches_herdr_and_records_running_task() {
    let (state, _, task_id) = seeded_state();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .start_coding_session_tool(Some(&start_args(&task_id)))
        .await
        .unwrap();

    assert_ne!(
        result.is_error,
        Some(true),
        "eligible launch should succeed: {result:?}"
    );
    assert!(fixture.called("workspace", "create"));
    assert!(fixture.called("agent", "start"));
    assert!(fixture.called("agent", "prompt"));
    let task = fixture.task(&task_id);
    assert_eq!(task.status, TaskStatus::Running);
    assert_eq!(task.execution.unwrap().pane_id.as_deref(), Some(PANE_ID));
}

#[tokio::test]
async fn stale_launch_snapshot_cannot_replace_an_existing_reservation() {
    let (state, _, task_id) = seeded_state();
    let stale_task = state.tasks[&task_id].clone();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let reserved = fixture
        .server
        .orchestration
        .record_execution(task_id.clone(), execution(ExecutionPhase::Pending))
        .unwrap();
    let request = ResolvedLaunchRequest {
        template: None,
        endpoint_id: "local".into(),
        launch_profile_id: "codex".into(),
        workspace_path: "/remote/repo".into(),
        bypass_permissions: false,
        role: "worker".into(),
        kind: String::new(),
        skills: vec![],
    };

    let result = launch_task_execution(
        &fixture.server.launch_context(),
        &stale_task,
        None,
        Some(&request),
    )
    .await;

    fixture.assert_no_runtime_mutations();
    assert!(
        result.is_err(),
        "a stale snapshot must not launch a second attempt"
    );
    assert_eq!(fixture.task(&task_id).execution.unwrap(), reserved);
}

#[tokio::test]
async fn unavailable_worker_does_not_authorize_a_replacement_launch() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Unavailable), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::EndpointUnavailable).await;
    let before = fixture.task(&task_id).execution.unwrap();

    let result = fixture
        .server
        .start_coding_session_tool(Some(&start_args(&task_id)))
        .await;

    fixture.assert_no_runtime_mutations();
    assert_refused(result);
    assert_binding_retained(fixture.task(&task_id), &before);
}

async fn assert_exec_refuses(observation: Observation) {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, observation).await;
    let result = fixture
        .server
        .exec_tool(Some(&json!({
            "execution_id": EXECUTION_ID, "command": ["echo", "contract"], "timeout_seconds": 0.01
        })))
        .await;

    fixture.assert_no_runtime_mutations();
    assert_refused(result);
}

#[tokio::test]
async fn exec_never_sends_shell_text_to_an_idle_coding_agent() {
    assert_exec_refuses(Observation::IdleAgent).await;
}

#[tokio::test]
async fn exec_requires_positive_shell_observation_when_lookup_fails() {
    assert_exec_refuses(Observation::LookupUnavailable).await;
}

#[tokio::test]
async fn exec_can_use_an_observed_available_shell_without_allocating_another_pane() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::AvailableShell).await;
    let result = fixture
        .server
        .exec_tool(Some(&json!({
            "execution_id": EXECUTION_ID, "command": ["echo", "contract"], "timeout_seconds": 0.01
        })))
        .await
        .unwrap();

    assert_ne!(
        result.is_error,
        Some(true),
        "available task-owned shell should accept exec: {result:?}"
    );
    assert!(fixture.called("pane", "run"));
    assert!(!fixture.called("workspace", "create"));
    assert!(!fixture.called("agent", "start"));
    assert!(!fixture.called("agent", "prompt"));
    assert!(!fixture.called("pane", "close"));
}

#[tokio::test]
async fn stop_never_closes_a_replacement_occupant_using_a_stale_pane_id() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::ReplacedOccupant).await;
    let result = fixture
        .server
        .execution_stop_tool(Some(
            &json!({"execution_id": EXECUTION_ID, "dry_run": false}),
        ))
        .await;

    fixture.assert_no_runtime_mutations();
    assert_refused(result);
    assert_ne!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
}

#[tokio::test]
async fn stop_dry_run_keeps_runtime_and_durable_binding_intact() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let before = fixture.task(&task_id).execution.unwrap();
    let result = fixture
        .server
        .execution_stop_tool(Some(
            &json!({"execution_id": EXECUTION_ID, "dry_run": true}),
        ))
        .await
        .unwrap();

    assert_ne!(
        result.is_error,
        Some(true),
        "dry run should succeed: {result:?}"
    );
    fixture.assert_no_runtime_mutations();
    assert_eq!(fixture.task(&task_id).execution.unwrap(), before);
}

#[tokio::test]
async fn stop_exits_the_verified_worker_and_closes_its_pane() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Running, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;

    let result = fixture
        .server
        .execution_stop_tool(Some(&json!({
            "execution_id": EXECUTION_ID, "dry_run": false
        })))
        .await
        .unwrap();

    assert_ne!(
        result.is_error,
        Some(true),
        "verified owned worker should be stoppable: {result:?}"
    );
    let closes = fixture
        .calls()
        .split("<call>\n")
        .filter(|call| call.starts_with("pane\nclose\n"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(closes.len(), 1);
    assert!(fixture.called("agent", "prompt"));
    assert!(fixture.calls().contains("/exit\n"));
    assert!(!fixture.called("workspace", "close"));
    assert!(!fixture.called("tab", "close"));
    assert!(!fixture.called("agent", "start"));
    let task = fixture.task(&task_id);
    assert_eq!(task.execution.unwrap().phase, ExecutionPhase::Stopped);
    assert_ne!(task.status, TaskStatus::Running);
}

#[tokio::test]
async fn restart_reconcile_marks_surviving_pane_without_agent_proven_exited() {
    let (mut state, _, task_id) = seeded_state();
    let before = execution(ExecutionPhase::Starting);
    state.record_execution(&task_id, before.clone(), 4).unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Running, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanPane).await;

    let summary = reconcile_executions_after_restart(&LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    })
    .await;

    assert!(
        summary.contains("1 proven exited"),
        "unexpected reconciliation summary: {summary}"
    );
    let task = fixture.task(&task_id);
    let after = task
        .execution
        .clone()
        .expect("the durable execution binding must be retained");
    assert_eq!(after.phase, ExecutionPhase::Exited);
    assert_eq!(after.recovery, ExecutionRecovery::Reconciled);
    assert_binding_retained(task, &before);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn restart_reconcile_confirms_live_worker_and_backfills_session() {
    let (mut state, _, task_id) = seeded_state();
    let mut before = execution(ExecutionPhase::Live);
    before.agent_session = None;
    state.record_execution(&task_id, before.clone(), 4).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;

    let summary = reconcile_executions_after_restart(&LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    })
    .await;

    assert!(
        summary.contains("1 execution(s) confirmed"),
        "unexpected reconciliation summary: {summary}"
    );
    let task = fixture.task(&task_id);
    let after = task
        .execution
        .clone()
        .expect("the durable execution binding must be retained");
    assert_eq!(after.phase, ExecutionPhase::Live);
    assert_eq!(after.recovery, ExecutionRecovery::Reconciled);
    // The native conversation identity is backfilled when the binding never
    // recorded one; every frozen launch field is preserved from the store.
    assert_eq!(
        after.agent_session.as_deref(),
        Some("native-original"),
        "agent_session must be backfilled from the live occupant"
    );
    let mut expected = before.clone();
    expected.agent_session = Some("native-original".into());
    assert_binding_retained(task, &expected);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn restart_reconcile_flags_a_replaced_occupant_for_adoption() {
    let (mut state, _, task_id) = seeded_state();
    let before = execution(ExecutionPhase::Live);
    state.record_execution(&task_id, before.clone(), 4).unwrap();
    let fixture = Fixture::new(state, Observation::ReplacedOccupant).await;

    let summary = reconcile_executions_after_restart(&LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    })
    .await;

    assert!(
        summary.contains("1 occupant mismatch(es)"),
        "unexpected reconciliation summary: {summary}"
    );
    let task = fixture.task(&task_id);
    let after = task
        .execution
        .clone()
        .expect("the durable execution binding must be retained");
    assert_eq!(after.phase, ExecutionPhase::Live);
    assert_eq!(after.recovery, ExecutionRecovery::OccupantMismatch);
    assert_binding_retained(task, &before);
    fixture.assert_no_runtime_mutations();
}

fn endpoint_migration_state(node_id: &str) -> (OrchestrationState, PlanId, TaskId) {
    let (mut state, plan_id, task_id) = seeded_state();
    let endpoint_id = endpoint_migration::unresolved_endpoint_id(node_id);
    state.tasks.get_mut(&task_id).unwrap().run_spec = Some(TaskRunSpec {
        endpoint_id: endpoint_id.clone(),
        launch_profile_id: "codex".into(),
        workspace_path: "/remote/repo".into(),
        bypass_permissions: false,
        role: "worker".into(),
        kind: "implementation".into(),
        skills: vec!["rust".into()],
        template: "task".into(),
        instruction: "Migrate placement only".into(),
    });
    let mut previous = execution(ExecutionPhase::Unavailable);
    previous.execution_id = format!("migrated-{node_id}-worker");
    previous.endpoint_id = endpoint_id;
    previous.recovery = ExecutionRecovery::NeedsReconciliation;
    previous.workspace_id = None;
    previous.tab_id = None;
    previous.pane_id = None;
    previous.agent_name = None;
    previous.agent_session = None;
    state.record_execution(&task_id, previous, 4).unwrap();
    (state, plan_id, task_id)
}

#[tokio::test]
async fn endpoint_migrate_is_admin_gated_without_any_herdr_call() {
    let (state, _, _) = endpoint_migration_state("build-farm-1");
    let before = serde_json::to_value(&state).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    assert_refused(
        fixture
            .server
            .endpoint_migrate_tool(Some(&json!({
                "legacy_node_id": "build-farm-1", "endpoint_id": "remote-a"
            })))
            .await,
    );
    assert_eq!(
        serde_json::to_value(fixture.server.orchestration.snapshot().unwrap()).unwrap(),
        before
    );
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn endpoint_migrate_rewrites_and_persists_records_once_without_aliases() {
    let (state, _, task_id) = endpoint_migration_state("build-farm-1");
    let before = state.tasks[&task_id].clone();
    let fixture = Fixture::new_with_admin(state, Observation::IdleAgent, true).await;
    let refused = fixture
        .server
        .endpoint_migrate_tool(Some(&json!({
            "legacy_node_id": "build-farm-1", "endpoint_id": "no-such-machine"
        })))
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(fixture.task(&task_id).run_spec, before.run_spec);
    assert_eq!(fixture.task(&task_id).execution, before.execution);

    let migrated = fixture
        .server
        .endpoint_migrate_tool(Some(&json!({
            "legacy_node_id": "build-farm-1", "endpoint_id": "remote-a"
        })))
        .await
        .unwrap();
    assert_ne!(migrated.is_error, Some(true));
    let report = migrated.structured_content.unwrap();
    assert_eq!(report["run_specs_updated"], 1);
    assert_eq!(report["executions_updated"], 1);
    assert_eq!(report["task_ids"], json!([task_id.0]));
    let mut expected_spec = before.run_spec.unwrap();
    expected_spec.endpoint_id = "remote-a".into();
    assert_eq!(fixture.task(&task_id).run_spec, Some(expected_spec));
    let mut expected_execution = before.execution.unwrap();
    expected_execution.endpoint_id = "remote-a".into();
    let actual = fixture.task(&task_id).execution.unwrap();
    expected_execution.updated_at_ms = actual.updated_at_ms;
    assert_eq!(actual, expected_execution);
    assert_eq!(actual.phase, ExecutionPhase::Unavailable);
    assert_eq!(actual.recovery, ExecutionRecovery::NeedsReconciliation);

    let after = serde_json::to_value(fixture.server.orchestration.snapshot().unwrap()).unwrap();
    assert!(after.get("endpoint_mappings").is_none());
    let reopened =
        orchestration_actor::OrchestrationHandle::open(Some(&fixture.dir.0.join("store"))).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.snapshot().unwrap()).unwrap(),
        after
    );
    let repeated = fixture
        .server
        .endpoint_migrate_tool(Some(&json!({
            "legacy_node_id": "build-farm-1", "endpoint_id": "local"
        })))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(repeated["run_specs_updated"], 0);
    assert_eq!(repeated["executions_updated"], 0);
    assert_eq!(
        serde_json::to_value(fixture.server.orchestration.snapshot().unwrap()).unwrap(),
        after
    );
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn endpoint_migration_does_not_retarget_modern_records_or_explicit_launches() {
    let (mut state, plan_id, _) = endpoint_migration_state("remote-a");
    let task = state
        .create_task(task_input(plan_id, "Modern worker"), 5)
        .unwrap();
    let mut modern = execution(ExecutionPhase::Live);
    modern.endpoint_id = "remote-a".into();
    state.record_execution(&task.id, modern.clone(), 6).unwrap();
    let modern = state.tasks[&task.id].execution.clone().unwrap();
    let fixture = Fixture::new_with_admin(state, Observation::IdleAgent, true).await;
    fixture
        .server
        .endpoint_migrate_tool(Some(&json!({
            "legacy_node_id": "remote-a", "endpoint_id": "local"
        })))
        .await
        .unwrap();
    assert_eq!(fixture.task(&task.id).execution, Some(modern));
    let state = fixture.server.orchestration.snapshot().unwrap();
    let request = ResolvedLaunchRequest {
        template: None,
        endpoint_id: "remote-a".into(),
        launch_profile_id: "codex".into(),
        workspace_path: "/remote/repo".into(),
        bypass_permissions: false,
        role: "worker".into(),
        kind: "implementation".into(),
        skills: vec![],
    };
    let (_, resolved, intent) = prepare_execution(
        &fixture.server.launch_context(),
        &state,
        &state.tasks[&task.id],
        None,
        Some(&request),
    )
    .unwrap();
    assert_eq!(resolved.endpoint_id, "remote-a");
    assert_eq!(intent.endpoint_id, "remote-a");
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn unresolved_endpoint_needs_migration_before_launch_or_observation() {
    let (mut state, _, task_id) = endpoint_migration_state("remote-a");
    state.tasks.get_mut(&task_id).unwrap().execution = None;
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let mut args = start_args(&task_id);
    args["endpoint_id"] = json!("legacy-node:remote-a");
    let result = fixture
        .server
        .start_coding_session_tool(Some(&args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(serde_json::to_string(&result)
        .unwrap()
        .contains("endpoint_migrate"));
    let endpoints = HashSet::from(["legacy-node:remote-a".into()]);
    let (live, warnings, observed, agents) =
        collect_live_runtime_keys(&fixture.server.herdr, &endpoints).await;
    assert!(live.is_empty() && observed.is_empty() && agents.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("endpoint_migrate"));
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn endpoint_migration_save_failure_keeps_all_in_memory_records_unchanged() {
    let (state, _, _) = endpoint_migration_state("build-farm-1");
    let before = serde_json::to_value(&state).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let database = fixture.dir.0.join("store/taskr.db");
    let saved = fixture.dir.0.join("original.db");
    std::fs::rename(&database, &saved).unwrap();
    std::fs::create_dir(&database).unwrap();
    assert!(fixture
        .server
        .orchestration
        .migrate_endpoint("build-farm-1", "remote-a")
        .is_err());
    assert_eq!(
        serde_json::to_value(fixture.server.orchestration.snapshot().unwrap()).unwrap(),
        before
    );
    std::fs::remove_dir(&database).unwrap();
    std::fs::rename(&saved, &database).unwrap();
}

#[tokio::test]
async fn admin_list_endpoint_agents_reports_the_live_inventory() {
    let (state, _, _) = seeded_state();
    let fixture = Fixture::new_with_admin(state, Observation::IdleAgent, true).await;

    let result = fixture
        .server
        .admin_list_endpoint_agents_tool(Some(&json!({"endpoint_id": "local"})))
        .await
        .unwrap();
    assert_ne!(
        result.is_error,
        Some(true),
        "admin inventory must succeed: {result:?}"
    );
    assert!(fixture.called("agent", "list"));
}

async fn prune_fixture(observation: Observation) -> (Fixture, TaskId) {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Delivered, 5)
        .unwrap();
    (Fixture::new(state, observation).await, task_id)
}

async fn prune(fixture: &Fixture) -> CallToolResult {
    fixture
        .server
        .orchestration_prune_tool(Some(&json!({
            "dry_run": false, "older_than_days": 1,
            "include_finished_plans": false, "include_stale_execution_records": true
        })))
        .await
        .unwrap()
}

#[tokio::test]
async fn prune_keeps_finished_task_binding_when_endpoint_is_unavailable() {
    let (fixture, task_id) = prune_fixture(Observation::EndpointUnavailable).await;
    let before = fixture.task(&task_id).execution.unwrap();

    let _ = prune(&fixture).await;

    assert_binding_retained(fixture.task(&task_id), &before);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_keeps_binding_when_pane_inventory_is_incomplete() {
    let (fixture, task_id) = prune_fixture(Observation::PaneListUnavailable).await;
    let before = fixture.task(&task_id).execution.unwrap();

    let _ = prune(&fixture).await;

    assert_binding_retained(fixture.task(&task_id), &before);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_keeps_binding_observed_in_real_schema_agent_list() {
    let (fixture, task_id) = prune_fixture(Observation::AgentPresentPaneListUnavailable).await;
    let before = fixture.task(&task_id).execution.unwrap();

    let _ = prune(&fixture).await;

    assert_binding_retained(fixture.task(&task_id), &before);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_can_remove_an_old_binding_proven_absent_on_a_reachable_endpoint() {
    let (fixture, task_id) = prune_fixture(Observation::EmptyEndpoint).await;
    let result = prune(&fixture).await;

    assert_ne!(
        result.is_error,
        Some(true),
        "prune should succeed: {result:?}"
    );
    assert!(fixture.task(&task_id).execution.is_none());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn unconfirmed_exit_keeps_the_final_report_and_never_deletes_the_layout() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::ExitIgnored).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({
            "task_id": task_id.0, "status": "Passed", "outcome": "Accepted report"
        })))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["runtime_cleanup"]["state"],
        "pending"
    );
    assert_eq!(
        fixture.task(&task_id).outcome.as_deref(),
        Some("Accepted report")
    );
    assert_ne!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
    assert!(!fixture.called("pane", "close"));
    assert!(!fixture.called("workspace", "close"));
}

async fn retained_fixture(observation: Observation, last_seen: u64) -> (Fixture, TaskId) {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Stopped), last_seen)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Delivered, 5)
        .unwrap();
    (Fixture::new(state, observation).await, task_id)
}

#[tokio::test]
async fn prune_dry_run_selects_old_retained_shells_without_removing_them() {
    let (fixture, task_id) = retained_fixture(Observation::OrphanShell, 4).await;
    let before = fixture.task(&task_id).execution;
    let result = fixture
        .server
        .orchestration_prune_tool(Some(&json!({
            "dry_run": true, "older_than_days": 1,
            "include_finished_plans": false, "include_stale_execution_records": true
        })))
        .await
        .unwrap();
    assert_eq!(
        result.structured_content.as_ref().unwrap()["report"]["pruned_execution_count"],
        1
    );
    assert_eq!(fixture.task(&task_id).execution, before);
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_removes_an_old_owned_retained_shell_and_its_binding() {
    let (fixture, task_id) = retained_fixture(Observation::OrphanShell, 4).await;
    let result = prune(&fixture).await;
    assert_ne!(result.is_error, Some(true));
    assert!(fixture.called("pane", "close"));
    assert!(!fixture.called("workspace", "close"));
    assert!(fixture.task(&task_id).execution.is_none());
    assert_eq!(fixture.task(&task_id).status, TaskStatus::Delivered);
}

#[tokio::test]
async fn prune_preserves_recent_retained_shells() {
    let (fixture, task_id) = retained_fixture(Observation::OrphanShell, now_ms()).await;
    let _ = prune(&fixture).await;
    assert!(fixture.task(&task_id).execution.is_some());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_preserves_retained_layouts_with_resumed_or_replaced_agents() {
    for observation in [
        Observation::IdleAgent,
        Observation::ReplacedOccupant,
        Observation::OrphanPane,
    ] {
        let (fixture, task_id) = retained_fixture(observation, 4).await;
        let _ = prune(&fixture).await;
        assert!(fixture.task(&task_id).execution.is_some());
        fixture.assert_no_runtime_mutations();
    }
}

#[tokio::test]
async fn prune_preserves_a_shell_whose_layout_identity_changed() {
    let (mut state, _, task_id) = seeded_state();
    let mut binding = execution(ExecutionPhase::Stopped);
    binding.tab_id = Some("w1:t9".into());
    state.record_execution(&task_id, binding, 4).unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Delivered, 5)
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanShell).await;
    let _ = prune(&fixture).await;
    assert!(fixture.task(&task_id).execution.is_some());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_archived_attempt_never_clears_the_tasks_new_execution() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Stopped), 4)
        .unwrap();
    let mut replacement = execution(ExecutionPhase::Live);
    replacement.execution_id = "exec-new-attempt".into();
    replacement.pane_id = Some("w1:p2".into());
    replacement.agent_name = Some("taskr-new-attempt".into());
    state
        .record_execution(&task_id, replacement.clone(), now_ms())
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanShell).await;
    let result = prune(&fixture).await;
    assert_ne!(result.is_error, Some(true));
    assert!(fixture.called("pane", "close"));
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().execution_id,
        replacement.execution_id
    );
    assert!(fixture
        .server
        .orchestration
        .snapshot()
        .unwrap()
        .retained_executions
        .is_empty());
}

#[tokio::test]
async fn finished_plan_prune_keeps_ownership_of_unobservable_workers() {
    let (mut state, plan_id, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Stopped), 4)
        .unwrap();
    state
        .update_task_status(&task_id, TaskStatus::Delivered, 5)
        .unwrap();
    state
        .update_plan_status(
            &plan_id,
            PlanStatus::Delivered,
            Some("Accepted plan".into()),
            6,
        )
        .unwrap();
    let fixture = Fixture::new(state, Observation::EndpointUnavailable).await;
    let result = fixture
        .server
        .orchestration_prune_tool(Some(&json!({
            "dry_run": false, "older_than_days": 1,
            "include_finished_plans": true, "include_stale_execution_records": true
        })))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert!(fixture
        .server
        .orchestration
        .snapshot()
        .unwrap()
        .plans
        .contains_key(&plan_id));
    assert!(fixture.task(&task_id).execution.is_some());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn prune_preserves_a_command_running_in_a_retained_shell() {
    let (fixture, task_id) = retained_fixture(Observation::ShellCommand, 4).await;
    let result = prune(&fixture).await;
    assert_ne!(result.is_error, Some(true));
    assert!(fixture.called("pane", "process-info"));
    assert!(fixture.task(&task_id).execution.is_some());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn cli_prune_uses_the_same_retained_layout_checks() {
    let (fixture, _) = retained_fixture(Observation::OrphanShell, 4).await;
    let store_path = fixture.dir.0.join("store");
    let preview = local_prune_store(
        Some(&store_path),
        &fixture.server.herdr,
        true,
        true,
        false,
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(preview.pruned_execution_count, 1);
    fixture.assert_no_runtime_mutations();
    let report = local_prune_store(
        Some(&store_path),
        &fixture.server.herdr,
        false,
        true,
        false,
        Some(1),
    )
    .await
    .unwrap();
    assert_eq!(report.pruned_execution_count, 1);
    assert!(fixture.called("pane", "close"));
}

#[tokio::test]
async fn deleting_a_project_preserves_stopped_layout_ownership_for_prune() {
    let (mut state, plan_id, task_id) = seeded_state();
    let project_id = state.plans[&plan_id].project_id.clone();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Stopped), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::OrphanShell).await;
    fixture
        .server
        .orchestration
        .delete_project(&project_id.0)
        .unwrap();
    let loaded = store::SqliteOrchestrationStore::open(fixture.dir.0.join("store"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(loaded.retained_executions[EXECUTION_ID].task_id, task_id);
    let result = prune(&fixture).await;
    assert_ne!(result.is_error, Some(true));
    assert!(fixture.called("pane", "close"));
    assert!(fixture
        .server
        .orchestration
        .snapshot()
        .unwrap()
        .retained_executions
        .is_empty());
}

#[tokio::test]
async fn cleanup_and_prune_accept_process_proven_shells_with_unknown_detection_state() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::UnknownShell).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task_id.0, "status": "Passed"})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
    assert!(fixture.called("pane", "close"));
    assert!(!fixture.called("agent", "start"));
    let (fixture, task_id) = retained_fixture(Observation::UnknownShell, 4).await;
    let _ = prune(&fixture).await;
    assert!(fixture.called("pane", "close"));
    assert!(fixture.task(&task_id).execution.is_none());
}

#[tokio::test]
async fn exit_persists_newly_observed_native_identity_before_it_disappears() {
    let (mut state, _, task_id) = seeded_state();
    let mut binding = execution(ExecutionPhase::Live);
    binding.agent_session = None;
    binding.terminal_id = None;
    state.record_execution(&task_id, binding, 4).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task_id.0, "status": "Passed"})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let completed = fixture.task(&task_id).execution.unwrap();
    assert_eq!(completed.agent_session.as_deref(), Some("native-original"));
    assert_eq!(completed.terminal_id.as_deref(), Some("terminal-1"));
    assert_eq!(completed.phase, ExecutionPhase::Stopped);
}

async fn layout_fixture() -> (Fixture, PlanId, TaskId) {
    let (state, plan, task) = seeded_state();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    std::fs::write(
        fixture.dir.0.join("herdr"),
        include_str!("fixtures/layout_herdr.py"),
    )
    .unwrap();
    (fixture, plan, task)
}

#[tokio::test]
async fn malformed_start_receipt_keeps_the_pane_and_blocks_duplicate_launch() {
    let (state, _, task) = seeded_state();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let path = fixture.dir.0.join("herdr");
    let body = std::fs::read_to_string(&path).unwrap();
    let replacement = "'agent start') printf '%s\\n' 'lost JSON receipt'; exit 0 ;; #";
    std::fs::write(&path, body.replacen("'agent start')", replacement, 1)).unwrap();
    assert_refused(
        fixture
            .server
            .start_coding_session_tool(Some(&start_args(&task)))
            .await,
    );
    let saved = fixture.task(&task).execution.unwrap();
    assert_eq!(saved.phase, ExecutionPhase::Starting);
    assert_eq!(saved.recovery, ExecutionRecovery::NeedsReconciliation);
    assert!(saved.pane_id.is_some());
    assert!(fixture.called("agent", "start"));
    assert!(!fixture.called("pane", "close"));
    assert!(!fixture.called("agent", "prompt"));
    let before = fixture.calls();
    assert_refused(
        fixture
            .server
            .start_coding_session_tool(Some(&start_args(&task)))
            .await,
    );
    assert_eq!(fixture.calls(), before);
}

#[tokio::test]
async fn allocation_timeout_blocks_replacement_cleanup_and_prune_after_restart() {
    let (state, _, task) = seeded_state();
    let fixture = Fixture::new(state, Observation::EmptyEndpoint).await;
    let path = fixture.dir.0.join("herdr");
    let body = std::fs::read_to_string(&path).unwrap();
    let replacement = "'workspace create') printf '%s\\n' '{\"error\":{\"code\":\"timeout\",\"message\":\"allocation receipt lost\"}}'; exit 1 ;; #";
    std::fs::write(&path, body.replacen("'workspace create')", replacement, 1)).unwrap();
    assert_refused(
        fixture
            .server
            .start_coding_session_tool(Some(&start_args(&task)))
            .await,
    );
    let saved = fixture.task(&task).execution.unwrap();
    assert_eq!(saved.phase, ExecutionPhase::Allocating);
    assert_eq!(saved.recovery, ExecutionRecovery::NeedsReconciliation);
    assert!(saved.pane_id.is_none());
    assert_refused(
        fixture
            .server
            .start_coding_session_tool(Some(&start_args(&task)))
            .await,
    );
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: OrchestrationHandle::open(Some(&fixture.dir.0.join("store"))).unwrap(),
    };
    reconcile_executions_after_restart(&ctx).await;
    assert_eq!(
        ctx.orchestration.snapshot().unwrap().tasks[&task]
            .execution
            .as_ref()
            .unwrap()
            .phase,
        ExecutionPhase::Allocating
    );
    ctx.orchestration
        .update_task_status(task.clone(), TaskStatus::Canceled)
        .unwrap();
    assert!(execution_cleanup::close_finished_worker(&ctx, &task)
        .await
        .is_err());
    let report = ctx
        .orchestration
        .prune_stale_execution_records(
            &HashSet::new(),
            &HashSet::from(["local".into()]),
            true,
            true,
            false,
            None,
        )
        .unwrap();
    assert!(report.candidates.is_empty());
    assert!(!fixture.called("agent", "start"));
    assert!(!fixture.called("pane", "close"));
}

#[tokio::test]
async fn generation_replacement_creates_a_new_plan_space_and_never_closes_reused_ids() {
    let (fixture, plan, first) = layout_fixture().await;
    let original = layout_start(&fixture, &first, "task").await;
    let path = fixture.dir.0.join("layout-state.json");
    let mut runtime: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    runtime["generation"] = json!("fixture-v2");
    // Simulate a replacement resource namespace with the same public IDs.
    for pane in runtime["panes"].as_object_mut().unwrap().values_mut() {
        pane["terminal_id"] = json!("replacement-terminal");
    }
    std::fs::write(&path, runtime.to_string()).unwrap();
    let next = fixture
        .server
        .orchestration
        .create_task(task_input(plan, "New incarnation worker"))
        .unwrap();
    let fresh = layout_start(&fixture, &next.id, "task").await;
    assert_ne!(fresh.workspace_id, original.workspace_id);
    assert_ne!(fresh.runtime_generation, original.runtime_generation);
    let before = fixture.calls();
    assert_refused(
        fixture
            .server
            .execution_stop_tool(Some(&json!({"execution_id":original.execution_id})))
            .await,
    );
    assert!(!fixture
        .calls()
        .strip_prefix(&before)
        .unwrap()
        .contains("pane\nclose\n"));
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    };
    reconcile_executions_after_restart(&ctx).await;
    let gone = fixture.task(&first).execution.unwrap();
    assert_eq!(gone.phase, ExecutionPhase::Exited);
    assert!(gone.pane_closed);
    assert_eq!(gone.agent_session, original.agent_session);
    assert_eq!(
        fixture.task(&next.id).execution.unwrap().phase,
        ExecutionPhase::Live
    );
}

async fn layout_start(fixture: &Fixture, task: &TaskId, template: &str) -> TaskExecution {
    let result = fixture
        .server
        .start_coding_session_tool(Some(&json!({
            "task_id": task.0, "endpoint_id": "local", "launch_profile_id": "codex",
            "workspace_path": "/remote/repo", "template": template,
        })))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    fixture.task(task).execution.unwrap()
}

#[tokio::test]
async fn plan_layout_groups_parallel_workers_and_overflow_without_label_ownership() {
    let (fixture, plan, first) = layout_fixture().await;
    let second = fixture
        .server
        .orchestration
        .create_task(task_input(plan.clone(), "Second worker"))
        .unwrap();
    let third = fixture
        .server
        .orchestration
        .create_task(task_input(plan.clone(), "Third worker"))
        .unwrap();
    let (one, two) = tokio::join!(
        layout_start(&fixture, &first, "task"),
        layout_start(&fixture, &second.id, "task")
    );
    assert_eq!(one.workspace_id, two.workspace_id);
    assert_eq!(one.tab_id, two.tab_id);
    assert_ne!(one.pane_id, two.pane_id);
    assert!(one
        .native_session_name
        .as_deref()
        .unwrap()
        .contains(&first.0));
    assert!(fixture.calls().contains("/rename task-"));
    // Display renaming must not break plan-space reuse.
    let target = fixture.server.herdr.target_for_endpoint("local");
    fixture
        .server
        .herdr
        .rename_layout(
            &target,
            "tab",
            one.tab_id.as_deref().unwrap(),
            "User renamed tab",
        )
        .await
        .unwrap();
    let three = layout_start(&fixture, &third.id, "task").await;
    assert_eq!(one.workspace_id, three.workspace_id);
    assert_ne!(one.tab_id, three.tab_id);
    for template in ["validate", "review", "quality-guard"] {
        let task = fixture
            .server
            .orchestration
            .create_task(task_input(plan.clone(), template))
            .unwrap();
        let execution = layout_start(&fixture, &task.id, template).await;
        assert_eq!(
            execution.group,
            taskr_core::orchestration::ExecutionGroup::from_template(template)
        );
        assert_eq!(one.workspace_id, execution.workspace_id);
        assert_ne!(one.tab_id, execution.tab_id);
    }
    let state = fixture.server.orchestration.snapshot().unwrap();
    let project_id = state.plans[&plan].project_id.clone();
    let other_plan = fixture
        .server
        .orchestration
        .create_plan(CreatePlan {
            project_id,
            title: "Another goal".into(),
            brief: "Separate layout".into(),
            instructions: None,
            slug: None,
        })
        .unwrap();
    let task = fixture
        .server
        .orchestration
        .create_task(task_input(other_plan.id.clone(), "Other plan worker"))
        .unwrap();
    let other = layout_start(&fixture, &task.id, "task").await;
    assert_ne!(one.workspace_id, other.workspace_id);
    let loaded = OrchestrationHandle::open(Some(&fixture.dir.0.join("store")))
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(loaded.plan_layouts.len(), 2);
    assert!(loaded
        .plan_layouts
        .values()
        .any(|layout| layout.plan_id == plan && layout.tabs.len() == 5));
}

#[tokio::test]
async fn completed_conversation_resumes_after_layout_cascades_without_replaying_task() {
    let (fixture, _, task) = layout_fixture().await;
    let original = layout_start(&fixture, &task, "task").await;
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task.0,
        "status": "Delivered", "outcome": "Accepted result", "evidence": ["Original proof"]})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let closed = fixture.task(&task).execution.unwrap();
    assert!(closed.pane_closed);
    assert_eq!(closed.agent_session, original.agent_session);
    let runtime: Value =
        serde_json::from_slice(&std::fs::read(fixture.dir.0.join("layout-state.json")).unwrap())
            .unwrap();
    assert_eq!(runtime["panes"], json!({}));
    assert_eq!(runtime["spaces"], json!({}));
    let before = fixture.calls();
    let result = fixture
        .server
        .execution_resume_tool(Some(&json!({"execution_id": original.execution_id})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let task_after = fixture.task(&task);
    let resumed = task_after.execution.as_ref().unwrap();
    assert!(resumed.inspection);
    assert_eq!(
        resumed.resumed_from.as_deref(),
        Some(original.execution_id.as_str())
    );
    assert_eq!(resumed.agent_session, original.agent_session);
    assert_eq!(resumed.launch_env, original.launch_env);
    assert_eq!(resumed.workspace_path, original.workspace_path);
    assert_ne!(resumed.pane_id, original.pane_id);
    assert_eq!(task_after.status, TaskStatus::Delivered);
    assert_eq!(task_after.outcome.as_deref(), Some("Accepted result"));
    assert_eq!(task_after.evidence, ["Original proof"]);
    let list = fixture.server.list_executions_tool(Some(&json!({
        "project_id": fixture.server.orchestration.snapshot().unwrap().plans[&task_after.plan_id].project_id.0
    }))).await.unwrap().structured_content.unwrap();
    assert!(list["executions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["execution_id"] == resumed.execution_id
            && row["inspection"] == true
            && row["runtime_generation"]
                == serde_json::to_value(&resumed.runtime_generation).unwrap()));
    let resumed_calls = fixture.calls();
    let new_calls = resumed_calls.strip_prefix(&before).unwrap();
    assert!(
        !new_calls.contains("agent\nprompt\n"),
        "resume must submit no prompt: {new_calls}"
    );
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: OrchestrationHandle::open(Some(&fixture.dir.0.join("store"))).unwrap(),
    };
    assert!(execution_cleanup::sweep_finished_workers(&ctx)
        .await
        .is_empty());
    assert_eq!(
        fixture.calls(),
        resumed_calls,
        "restart sweep must preserve explicit inspection"
    );
    let history = fixture
        .server
        .task_get_tool(Some(&json!({"task_id": task.0})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(
        history["execution_history"][0]["execution_id"],
        original.execution_id
    );
    assert!(history["execution_history"][0]["pane_closed"]
        .as_bool()
        .unwrap());
    let result = fixture
        .server
        .execution_stop_tool(Some(&json!({"execution_id": resumed.execution_id})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(fixture.task(&task).status, TaskStatus::Delivered);
    assert!(fixture.task(&task).execution.unwrap().pane_closed);
    // Older history remains a valid explicit source after another inspection.
    let result = fixture
        .server
        .execution_resume_tool(Some(&json!({"execution_id": original.execution_id})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
}

#[tokio::test]
async fn allocation_preserves_user_panes_and_does_not_claim_a_reused_terminal() {
    let (fixture, plan, first) = layout_fixture().await;
    let original = layout_start(&fixture, &first, "task").await;
    // A public pane ID still exists, but its terminal has been replaced by the
    // user. Neither splitting nor cleanup can treat it as the original worker.
    let path = fixture.dir.0.join("layout-state.json");
    let mut runtime: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    runtime["panes"][original.pane_id.as_deref().unwrap()]["terminal_id"] = json!("user-terminal");
    std::fs::write(&path, runtime.to_string()).unwrap();
    let next = fixture
        .server
        .orchestration
        .create_task(task_input(plan, "Next worker"))
        .unwrap();
    let execution = layout_start(&fixture, &next.id, "task").await;
    assert_eq!(execution.workspace_id, original.workspace_id);
    assert_ne!(execution.tab_id, original.tab_id);
    assert!(!fixture.called("pane", "split"));
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": first.0,
        "status": "Delivered", "outcome": "Saved report", "evidence": ["Saved evidence"]})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(!fixture.called("pane", "close"));
    let binding = fixture.task(&first).execution.unwrap();
    assert!(!binding.pane_closed);
    assert_eq!(
        binding.report.unwrap().outcome.as_deref(),
        Some("Saved report")
    );
    let loaded = OrchestrationHandle::open(Some(&fixture.dir.0.join("store")))
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(loaded.tasks[&first]
        .execution
        .as_ref()
        .unwrap()
        .report
        .is_some());
}

#[tokio::test]
async fn missing_native_id_keeps_worker_then_fresh_repeat_archives_original_report() {
    let (fixture, _, task) = layout_fixture().await;
    let mut original = layout_start(&fixture, &task, "task").await;
    let native = original.agent_session.clone();
    original.agent_session = None;
    fixture
        .server
        .orchestration
        .record_execution(task.clone(), original.clone())
        .unwrap();
    let path = fixture.dir.0.join("layout-state.json");
    let mut runtime: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    runtime["panes"][original.pane_id.as_deref().unwrap()]["agent_session"] = Value::Null;
    std::fs::write(&path, runtime.to_string()).unwrap();
    let before = fixture.calls();
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task.0,
        "status": "Delivered", "outcome": "First accepted result", "evidence": ["First proof"]})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(fixture.task(&task).status, TaskStatus::Delivered);
    assert!(!fixture.task(&task).execution.unwrap().pane_closed);
    let after = fixture.calls();
    assert!(!after.strip_prefix(&before).unwrap().contains("/exit"));
    assert!(!fixture.called("pane", "close"));
    runtime["panes"][original.pane_id.as_deref().unwrap()]["agent_session"] =
        json!({"value": native});
    std::fs::write(&path, runtime.to_string()).unwrap();
    let ctx = LaunchContext {
        herdr: fixture.server.herdr.clone(),
        launch_profiles: fixture.server.launch_profiles.clone(),
        orchestration: fixture.server.orchestration.clone(),
    };
    assert!(execution_cleanup::sweep_finished_workers(&ctx)
        .await
        .is_empty());
    let closed = fixture.task(&task).execution.unwrap();
    assert!(closed.pane_closed);
    assert_eq!(closed.agent_session, native);
    let result = fixture
        .server
        .task_status_update_tool(Some(&json!({"task_id": task.0,
        "status": "Planned", "outcome": "Repeat requested"})))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let repeat = layout_start(&fixture, &task, "task").await;
    assert_ne!(repeat.agent_session, closed.agent_session);
    assert_ne!(repeat.pane_id, closed.pane_id);
    assert!(!repeat.inspection);
    let history = fixture
        .server
        .task_get_tool(Some(&json!({"task_id": task.0})))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(
        history["execution_history"][0]["report"]["outcome"],
        "First accepted result"
    );
    assert_eq!(
        history["execution_history"][0]["report"]["evidence"],
        json!(["First proof"])
    );
}

#[tokio::test]
async fn resume_refuses_missing_ids_busy_tasks_and_disabled_profiles_before_allocation() {
    let (mut state, _, task) = seeded_state();
    let mut original = execution(ExecutionPhase::Stopped);
    original.agent_session = None;
    original.pane_closed = true;
    state.record_execution(&task, original, 4).unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let result = fixture
        .server
        .execution_resume_tool(Some(&json!({"execution_id": EXECUTION_ID})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(fixture.calls().is_empty());
    let mut original = fixture.task(&task).execution.unwrap();
    original.agent_session = Some("native-original".into());
    original.phase = ExecutionPhase::Live;
    fixture
        .server
        .orchestration
        .record_execution(task.clone(), original.clone())
        .unwrap();
    let result = fixture
        .server
        .execution_resume_tool(Some(&json!({"execution_id": EXECUTION_ID})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(fixture.calls().is_empty());
    original.phase = ExecutionPhase::Stopped;
    original.launch_profile_id = "disabled-profile".into();
    fixture
        .server
        .orchestration
        .record_execution(task, original)
        .unwrap();
    let result = fixture
        .server
        .execution_resume_tool(Some(&json!({"execution_id": EXECUTION_ID})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(fixture.calls().is_empty());
}

#[tokio::test]
async fn prune_refuses_terminal_identity_reuse_even_when_layout_ids_match() {
    let (mut state, _, task_id) = seeded_state();
    let mut binding = execution(ExecutionPhase::Stopped);
    binding.terminal_id = Some("old-terminal".into());
    state.record_execution(&task_id, binding, 4).unwrap();
    let fixture = Fixture::new(state, Observation::UnknownShell).await;
    let _ = prune(&fixture).await;
    assert!(fixture.task(&task_id).execution.is_some());
    fixture.assert_no_runtime_mutations();
}

#[tokio::test]
async fn exit_does_not_interrupt_an_already_settled_worker() {
    for observation in [Observation::IdleAgent, Observation::DoneAgent] {
        let (mut state, _, task_id) = seeded_state();
        state
            .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
            .unwrap();
        let fixture = Fixture::new(state, observation).await;
        let result = fixture
            .server
            .task_status_update_tool(Some(&json!({"task_id": task_id.0, "status": "Passed"})))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        assert!(!fixture.called("agent", "wait"));
        assert!(!fixture.calls().contains("\nesc\n"));
        assert!(fixture.calls().contains("/exit\n"));
    }
}

#[tokio::test]
async fn concurrent_final_updates_submit_only_one_native_exit() {
    let (mut state, _, task_id) = seeded_state();
    state
        .record_execution(&task_id, execution(ExecutionPhase::Live), 4)
        .unwrap();
    let fixture = Fixture::new(state, Observation::IdleAgent).await;
    let args = json!({"task_id": task_id.0, "status": "Passed"});
    let (first, second) = tokio::join!(
        fixture.server.task_status_update_tool(Some(&args)),
        fixture.server.task_status_update_tool(Some(&args)),
    );
    assert_ne!(first.unwrap().is_error, Some(true));
    assert_ne!(second.unwrap().is_error, Some(true));
    assert_eq!(fixture.calls().matches("\n/exit\n").count(), 1);
    assert_eq!(
        fixture.task(&task_id).execution.unwrap().phase,
        ExecutionPhase::Stopped
    );
}
