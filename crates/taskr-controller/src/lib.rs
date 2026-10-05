pub use taskr_core::orchestration::CreateProject;

use clap::Parser;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use taskr_core::orchestration::{
    CreatePlan, CreateTask, CreateTaskEdge, ExecutionPhase, ExecutionRecovery, OrchestrationCounts,
    OrchestrationState, OrchestrationStatus, PlanId, PlanStatus, Project, ProjectId, ProjectStatus,
    ProjectSummary, Task, TaskDependencyBlocker, TaskEdgeKind, TaskExecution, TaskId, TaskRunSpec,
    TaskScope, TaskStatus, UpdatePlan, UpdateProject, UpdateTask,
};
use taskr_environment::{
    CompanionConfig, EnvironmentCatalog as ResolvedLaunchProfiles, SyncRequest,
};
use taskr_herdr::{
    generate_agent_name, home_env_var_for_kind, validate_agent_name, AllocatePaneRequest,
    EndpointTarget, HerdrClientConfig, LaunchProfile, NativeHerdrClient as HerdrClient,
    PanePlacement, PromptOutcome, PromptRequest, ReadSource, ResolvedLaunch, LOCAL_ENDPOINT_ID,
};

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use ractor::{rpc::CallResult, Actor, ActorProcessingErr, ActorRef, RpcReplyPort};
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};

use crate::orchestration_actor::OrchestrationHandle;
use crate::runtime::{LocalRuntime, LocalRuntimeConfig};

mod endpoint_migration;
mod execution_cleanup;
mod execution_resume;
mod orchestration_actor;
mod plan_layout;
mod runtime;
mod store;
mod store_paths;

#[cfg(all(test, unix))]
mod herdr_contract_tests;

const DEFAULT_CODING_READY_TIMEOUT_SECONDS: u64 = 120;
const DEFAULT_PRUNE_OLDER_THAN_DAYS: u64 = 14;

/// Cap on agent startup waits running concurrently: a burst of launches
/// queues for a permit instead of monopolizing Herdr or starving quick MCP
/// paths. Each individual wait stays bounded by its own startup timeout.
const MAX_CONCURRENT_STARTUP_JOBS: usize = 4;
static STARTUP_JOBS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_STARTUP_JOBS));

/// Registry of startup jobs observed by any inspection surface while the
/// launch itself is still running: the launch persists each phase, but this
/// registry answers "what is starting right now" without touching Herdr.
static STARTUP_JOB_REGISTRY: LazyLock<Mutex<BTreeMap<String, StartupJobEntry>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[derive(Clone, Debug, Serialize)]
struct StartupJobEntry {
    execution_id: String,
    task_id: String,
    task_slug: String,
    endpoint_id: String,
    launch_profile_id: String,
    started_at_ms: u64,
    state: &'static str,
    finished_at_ms: Option<u64>,
    error: Option<String>,
}

fn startup_registry_record_start(execution: &TaskExecution, task: &Task) {
    let entry = StartupJobEntry {
        execution_id: execution.execution_id.clone(),
        task_id: task.id.0.clone(),
        task_slug: task.slug.clone(),
        endpoint_id: execution.endpoint_id.clone(),
        launch_profile_id: execution.launch_profile_id.clone(),
        started_at_ms: now_ms(),
        state: "starting",
        finished_at_ms: None,
        error: None,
    };
    if let Ok(mut registry) = STARTUP_JOB_REGISTRY.lock() {
        registry.insert(execution.execution_id.clone(), entry);
    }
}

fn startup_registry_finish(execution_id: &str, state: &'static str, error: Option<String>) {
    if let Ok(mut registry) = STARTUP_JOB_REGISTRY.lock() {
        if let Some(entry) = registry.get_mut(execution_id) {
            entry.state = state;
            entry.finished_at_ms = Some(now_ms());
            entry.error = error;
        }
    }
}

fn startup_registry_snapshot() -> Vec<StartupJobEntry> {
    match STARTUP_JOB_REGISTRY.lock() {
        Ok(registry) => registry.values().cloned().collect(),
        Err(_) => Vec::new(),
    }
}
const CODING_TASK_SEND_PROMPT: &str = include_str!("prompts/coding_task_send.md");
const CODING_VALIDATE_SEND_PROMPT: &str = include_str!("prompts/coding_validate_send.md");
const CODING_REVIEW_SEND_PROMPT: &str = include_str!("prompts/coding_review_send.md");
const CODING_QUALITY_GUARD_SEND_PROMPT: &str = include_str!("prompts/coding_quality_guard_send.md");

/// Launch profile id recorded for adopted executions whose launch arguments
/// were observed, never chosen from a preset.
const ADOPTED_LAUNCH_PROFILE_ID: &str = "adopted";

// ═══════════════════════════════════════════════════════════════════════════════
//  rmcp imports (MCP HTTP server)
// ═══════════════════════════════════════════════════════════════════════════════

use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, GetPromptRequestParams, GetPromptResult,
        Implementation, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
        PromptMessageRole, RawResource, RawResourceTemplate, ReadResourceRequestParams,
        ReadResourceResult, Resource, ResourceContents, ResourceTemplate, ServerCapabilities,
        ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError,
};

// ═══════════════════════════════════════════════════════════════════════════════
//  Config
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Parser, Debug)]
#[command(name = "taskr")]
#[command(
    about = "Durable orchestration control plane over MCP — Herdr drives the terminals and coding agents"
)]
struct Cli {
    #[arg(
        long,
        default_value = "127.0.0.1",
        help = "Host to bind the MCP HTTP server"
    )]
    host: String,
    #[arg(
        long,
        default_value_t = 3000,
        help = "Port to bind the MCP HTTP server"
    )]
    port: u16,
    #[arg(long, help = "Bearer token protecting the public MCP endpoint.")]
    mcp_token: Option<String>,
    #[arg(
        long,
        help = "Path to a file containing the MCP bearer token. Prefer /run/secrets paths in containers."
    )]
    mcp_token_file: Option<String>,
    #[arg(
        long,
        default_value = "TASKR_MCP_TOKEN",
        help = "Environment variable to read the MCP bearer token from when --mcp-token/--mcp-token-file are not set."
    )]
    mcp_token_env: String,
    #[arg(
        long,
        help = "Directory for durable taskr state (orchestration store)."
    )]
    store_path: Option<PathBuf>,
    #[arg(
        long,
        help = "Permit MCP without bearer auth and ignore TASKR_MCP_TOKEN. Intended only behind localhost-only port forwarding."
    )]
    allow_remote_without_mcp_token: bool,
    #[arg(
        long,
        help = "Enable admin-only MCP tools that create or change project boundaries."
    )]
    enable_admin_tools: bool,
    #[arg(
        long,
        default_value_t = 120.0,
        help = "Maximum wait timeout accepted by wait tools."
    )]
    max_timeout_seconds: f64,
    #[arg(long, default_value_t = 2 * 1024 * 1024, help = "Maximum MCP HTTP request body size.")]
    max_request_bytes: usize,
    #[arg(long, default_value_t = 2 * 1024 * 1024, help = "Maximum bytes returned by terminal capture tools.")]
    max_capture_bytes: usize,
    #[arg(
        long,
        default_value = "herdr",
        help = "Herdr executable used for every terminal operation."
    )]
    herdr_bin: PathBuf,
    #[arg(
        long,
        help = "Explicit local Herdr session selection. Never affects saved-machine endpoints."
    )]
    herdr_session: Option<String>,
    #[arg(
        long,
        default_value = "python3",
        help = "Python 3.11+ executable for the environment companion."
    )]
    environment_python_bin: PathBuf,
    #[arg(
        long,
        default_value = "ssh",
        help = "OpenSSH executable for environment sync using Herdr's saved targets."
    )]
    environment_ssh_bin: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct ControllerPolicy {
    enable_admin_tools: bool,
    max_timeout_seconds: f64,
    max_request_bytes: usize,
    max_capture_bytes: usize,
}

impl ControllerPolicy {
    fn new(cli: &Cli) -> Result<Self, String> {
        Ok(Self {
            enable_admin_tools: cli.enable_admin_tools,
            max_timeout_seconds: cli.max_timeout_seconds,
            max_request_bytes: cli.max_request_bytes,
            max_capture_bytes: cli.max_capture_bytes,
        })
    }

    fn clamp_timeout(&self, requested: f64) -> Result<f64, String> {
        if !requested.is_finite() || requested <= 0.0 {
            return Err("timeout_seconds must be a positive finite number".into());
        }
        Ok(requested.min(self.max_timeout_seconds))
    }

    fn limit_capture_output(&self, mut output: String) -> String {
        if output.len() <= self.max_capture_bytes {
            return output;
        }
        let mut keep_from = output.len().saturating_sub(self.max_capture_bytes);
        while keep_from < output.len() && !output.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        let suffix = output.split_off(keep_from);
        format!(
            "[taskr truncated capture to last {} bytes]\n{}",
            self.max_capture_bytes, suffix
        )
    }

    fn ensure_admin_tools_enabled(&self, tool_name: &str) -> Result<(), McpError> {
        if self.enable_admin_tools {
            return Ok(());
        }
        Err(McpError::invalid_request(
            format!("{tool_name} requires controller flag --enable-admin-tools"),
            None,
        ))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Small shared helpers
// ═══════════════════════════════════════════════════════════════════════════════

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

pub(crate) fn task_status_allows_runtime_cleanup(status: TaskStatus) -> bool {
    taskr_core::coordination::final_for_cleanup(status)
}

/// Per-project configuration-home override for one agent kind.
fn project_home_for_kind(project: Option<&Project>, agent_kind: &str) -> Option<String> {
    let project = project?;
    let home = match home_env_var_for_kind(agent_kind)? {
        "CODEX_HOME" => project.codex_home.as_ref()?,
        "CLAUDE_CONFIG_DIR" => project.claude_home.as_ref()?,
        "OPENCODE_CONFIG_DIR" => project.opencode_home.as_ref()?,
        "KIMI_CODE_HOME" => project.kimi_home.as_ref()?,
        _ => return None,
    };
    let trimmed = home.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn task_project_id(state: &OrchestrationState, task: &Task) -> Option<ProjectId> {
    state
        .plans
        .get(&task.plan_id)
        .map(|plan| plan.project_id.clone())
}

fn resolve_project_id_or_slug(
    state: &OrchestrationState,
    project_id_or_slug: &str,
) -> Result<ProjectId, String> {
    let selector = project_id_or_slug.trim();
    if selector.is_empty() {
        return Err("project_id must not be empty".into());
    }
    if let Some(project) = state.projects.get(&ProjectId(selector.to_owned())) {
        return Ok(project.id.clone());
    }
    let mut matches = state
        .projects
        .values()
        .filter(|project| project.slug == selector)
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    match matches.as_slice() {
        [project] => Ok(project.id.clone()),
        [] => Err(format!("project '{selector}' not found")),
        projects => {
            let matches = projects
                .iter()
                .map(|project| project.id.0.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "project slug '{selector}' is ambiguous; matches: {matches}"
            ))
        }
    }
}

fn resolve_plan_id_or_slug(
    state: &OrchestrationState,
    plan_id_or_slug: &str,
) -> Result<PlanId, String> {
    let selector = plan_id_or_slug.trim();
    if selector.is_empty() {
        return Err("plan_id must not be empty".into());
    }
    if let Some(plan) = state.plans.get(&PlanId(selector.to_owned())) {
        return Ok(plan.id.clone());
    }
    let mut matches = state
        .plans
        .values()
        .filter(|plan| plan.slug == selector)
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    match matches.as_slice() {
        [plan] => Ok(plan.id.clone()),
        [] => Err(format!("plan '{selector}' not found")),
        plans => {
            let matches = plans
                .iter()
                .map(|plan| format!("{} in project {}", plan.id.0, plan.project_id.0))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "plan slug '{selector}' is ambiguous; matches: {matches}"
            ))
        }
    }
}

/// Find the task that owns a durable execution binding.
fn resolve_execution_owner(
    state: &OrchestrationState,
    execution_id: &str,
) -> Result<(TaskId, TaskExecution), String> {
    let selector = execution_id.trim();
    if selector.is_empty() {
        return Err("execution_id must not be empty".into());
    }
    let mut matches = state
        .tasks
        .values()
        .filter_map(|task| {
            task.execution
                .as_ref()
                .filter(|execution| execution.execution_id == selector)
                .map(|execution| (task.id.clone(), execution.clone()))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0 .0.cmp(&right.0 .0));
    match matches.as_slice() {
        [(task_id, execution)] => Ok((task_id.clone(), execution.clone())),
        [] => Err(format!("execution '{selector}' not found")),
        found => {
            let matches = found
                .iter()
                .map(|(task_id, _)| task_id.0.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "execution id '{selector}' is attached to multiple tasks; matches: {matches}"
            ))
        }
    }
}

/// One-shot attempt suffix for generated agent names. Herdr enforces live
/// name uniqueness; the suffix keeps replacement attempts distinct.
fn short_attempt_suffix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0)
        .wrapping_mul(1_000_003)
        % 1_000_000
        + 1
}

fn new_execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("exec-{nanos:x}")
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Launch transaction
// ═══════════════════════════════════════════════════════════════════════════════

/// Everything needed to run the Herdr launch transaction against the durable
/// store. Shared by the scheduler actor and the start_coding_session tool.
#[derive(Clone)]
struct LaunchContext {
    herdr: HerdrClient,
    launch_profiles: Arc<ResolvedLaunchProfiles>,
    orchestration: orchestration_actor::OrchestrationHandle,
}

impl LaunchContext {
    fn resolve_profile(
        &self,
        endpoint: &str,
        requested: Option<&str>,
    ) -> Result<LaunchProfile, String> {
        self.launch_profiles.resolve(endpoint, requested)
    }

    fn persist(&self, task_id: &TaskId, execution: TaskExecution) -> Result<TaskExecution, String> {
        self.orchestration
            .record_launch_progress(task_id.clone(), execution)
    }
}

/// Build the frozen launch intent for one task attempt. Does not touch Herdr.
fn prepare_execution(
    ctx: &LaunchContext,
    state: &OrchestrationState,
    task: &Task,
    run_spec: Option<&TaskRunSpec>,
    requested: Option<&ResolvedLaunchRequest>,
) -> Result<(LaunchProfile, ResolvedLaunch, TaskExecution), String> {
    // An explicit request overrides the stored run_spec per field; unset
    // request fields fall back to the task's frozen defaults.
    let (endpoint_id, launch_profile_id, workspace_path, bypass_permissions, role, kind, skills) = {
        if run_spec.is_none() && requested.is_none() {
            return Err("task has no launch request".into());
        }
        let field = |request_value: Option<String>, run_spec_value: Option<String>| {
            request_value
                .filter(|value| !value.trim().is_empty())
                .or(run_spec_value)
                .unwrap_or_default()
        };
        let endpoint_id = match requested {
            Some(request) => request.endpoint_id.clone(),
            None => run_spec.expect("checked above").endpoint_id.clone(),
        };
        let launch_profile_id = match requested {
            Some(request) => request.launch_profile_id.clone(),
            None => run_spec.expect("checked above").launch_profile_id.clone(),
        };
        let workspace_path = match requested {
            Some(request) => request.workspace_path.clone(),
            None => run_spec.expect("checked above").workspace_path.clone(),
        };
        let bypass_permissions = match requested {
            Some(request) => request.bypass_permissions,
            None => run_spec.expect("checked above").bypass_permissions,
        };
        let role = field(
            requested.map(|request| request.role.clone()),
            run_spec.map(|run_spec| run_spec.role.clone()),
        );
        let kind = field(
            requested.map(|request| request.kind.clone()),
            run_spec.map(|run_spec| run_spec.kind.clone()),
        );
        let skills = requested
            .map(|request| request.skills.clone())
            .filter(|skills| !skills.is_empty())
            .or_else(|| run_spec.map(|run_spec| run_spec.skills.clone()))
            .unwrap_or_default();
        (
            endpoint_id,
            launch_profile_id,
            workspace_path,
            bypass_permissions,
            role,
            kind,
            skills,
        )
    };
    if endpoint_id.trim().is_empty() {
        return Err("endpoint_id must not be empty".into());
    }
    endpoint_migration::require_resolved_endpoint(&endpoint_id)?;
    if workspace_path.trim().is_empty() {
        return Err("workspace_path must not be empty".into());
    }
    let profile = ctx.resolve_profile(
        &endpoint_id,
        if launch_profile_id.trim().is_empty() {
            None
        } else {
            Some(launch_profile_id.as_str())
        },
    )?;
    // A descriptive task kind (implementation, review, ...) is metadata
    // about the work; the executable agent kind comes from the profile.
    let agent_kind = profile.agent_kind.clone();
    let args = profile
        .effective_args(bypass_permissions)
        .map_err(|error| error.to_string())?
        .clone();
    let project =
        task_project_id(state, task).and_then(|project_id| state.projects.get(&project_id));
    let project_home = project_home_for_kind(project, &agent_kind);
    let choice = ctx
        .launch_profiles
        .choice(&endpoint_id, Some(&profile.id))?;
    if let Some(deployment) = &choice.deployment {
        if project_home
            .as_deref()
            .is_some_and(|home| home != deployment.home)
        {
            return Err("project configuration home conflicts with the prepared environment; select its deployment or clear the old override".into());
        }
    }
    let env = profile.effective_env(project_home.as_deref());
    let agent_name = generate_agent_name(&task.slug, &agent_kind, short_attempt_suffix());
    let execution_id = new_execution_id();

    let resolved = ResolvedLaunch {
        endpoint_id: endpoint_id.clone(),
        launch_profile_id: profile.id.clone(),
        agent_kind,
        args,
        env,
        bypass_permissions,
        workspace_path,
        skills,
        agent_name,
    };
    let execution = TaskExecution {
        native_session_name: None,
        group: taskr_core::orchestration::ExecutionGroup::from_template(
            requested
                .and_then(|request| request.template.map(|template| template.as_str()))
                .or_else(|| run_spec.map(|spec| spec.template.as_str()))
                .unwrap_or("task"),
        ),
        inspection: false,
        resumed_from: None,
        pane_closed: false,
        report: None,
        execution_id,
        endpoint_id: resolved.endpoint_id.clone(),
        runtime_generation: None,
        launch_profile_id: resolved.launch_profile_id.clone(),
        launch_args: resolved.args.clone(),
        launch_env: resolved.env.clone(),
        bypass_permissions: resolved.bypass_permissions,
        workspace_path: resolved.workspace_path.clone(),
        role: role.trim().to_owned(),
        kind: kind.trim().to_owned(),
        skills: resolved.skills.clone(),
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        terminal_id: None,
        agent_name: Some(resolved.agent_name.clone()),
        agent_kind: Some(resolved.agent_kind.clone()),
        agent_session: None,
        phase: ExecutionPhase::Pending,
        recovery: ExecutionRecovery::NeedsReconciliation,
        created_at_ms: 0,
        updated_at_ms: 0,
        last_seen_ms: 0,
    };
    Ok((profile, resolved, execution))
}

/// Explicit launch request for tools that start an execution without a
/// task run_spec.
struct ResolvedLaunchRequest {
    endpoint_id: String,
    launch_profile_id: String,
    workspace_path: String,
    bypass_permissions: bool,
    role: String,
    kind: String,
    skills: Vec<String>,
    template: Option<CodingTaskSendTemplate>,
}

/// Outcome of the launch transaction. `rolled_back` lists the Herdr resources
/// this attempt created and then closed again after a failure.
struct LaunchOutcome {
    execution: TaskExecution,
    warnings: Vec<String>,
}

/// Run the Herdr launch transaction: validate → persist pending → allocate
/// pane and persist ids → start agent → reconcile → submit prompt. On
/// failure the durable binding is kept with a truthful phase and only
/// newly created Herdr resources are rolled back.
async fn launch_task_execution(
    ctx: &LaunchContext,
    task: &Task,
    run_spec: Option<&TaskRunSpec>,
    requested: Option<&ResolvedLaunchRequest>,
) -> Result<LaunchOutcome, String> {
    let state = ctx.orchestration.snapshot()?;
    let (_profile, resolved, execution) =
        prepare_execution(ctx, &state, task, run_spec, requested)?;
    // The endpoint must exist in the live Herdr catalog: local, or a saved
    // machine profile. Legacy references have already been migrated in the
    // store; normal launches use the selected profile ID directly.
    if resolved.endpoint_id != LOCAL_ENDPOINT_ID {
        let catalog = ctx.herdr.list_endpoints().await.map_err(|error| {
            format!(
                "cannot verify endpoint '{}' against the Herdr machine catalog: {error}",
                resolved.endpoint_id
            )
        })?;
        if !catalog
            .iter()
            .any(|endpoint| endpoint.endpoint_id == resolved.endpoint_id)
        {
            return Err(format!(
                "endpoint '{}' is not a saved Herdr machine profile; select an ID from list_endpoints",
                resolved.endpoint_id
            ));
        }
    }
    // Judge replacement against the store's current binding, never a caller's
    // possibly stale snapshot; core's record_execution guard closes the rest
    // of the race. Refusals exit before any startup job is registered.
    if let Some(existing) = state
        .tasks
        .get(&task.id)
        .and_then(|task| task.execution.as_ref())
    {
        if !existing.phase.allows_replacement() {
            return Err(format!(
                "task '{}' already has execution '{}' in phase {}; only an exited, stopped, or failed execution can be replaced; stop the current one first with execution_stop",
                task.id.0,
                existing.execution_id,
                existing.phase.as_str()
            ));
        }
    }
    startup_registry_record_start(&execution, task);
    let execution_id = execution.execution_id.clone();
    let result = launch_task_execution_started(ctx, task, state, resolved, execution).await;
    match &result {
        Ok(outcome) => startup_registry_finish(&outcome.execution.execution_id, "completed", None),
        Err(error) => startup_registry_finish(&execution_id, "failed", Some(error.clone())),
    }
    if ctx
        .orchestration
        .snapshot()?
        .tasks
        .get(&task.id)
        .is_some_and(|task| task_status_allows_runtime_cleanup(task.status))
    {
        if let Err(error) = execution_cleanup::close_finished_worker(ctx, &task.id).await {
            eprintln!("finished task launch cleanup pending: {error}");
        }
        return Err(format!(
            "task '{}' finished while launching; no task prompt submitted",
            task.id.0
        ));
    }
    result
}

/// Launch continuation after the intent is frozen and the startup job is
/// registered. Owns every Herdr-touching step.
async fn launch_task_execution_started(
    ctx: &LaunchContext,
    task: &Task,
    _state: OrchestrationState,
    resolved: ResolvedLaunch,
    mut execution: TaskExecution,
) -> Result<LaunchOutcome, String> {
    // 1. Persist launch intent before touching Herdr.
    let mut persisted = ctx.persist(&task.id, execution.clone())?;
    let mut facts = taskr_core::coordination::LaunchFacts::default();
    if taskr_core::coordination::launch_step(&execution, facts)
        != taskr_core::coordination::LaunchStep::PrepareEndpoint
    {
        return Err("launch continuation requires a fresh pending intent; reconcile persisted attempts instead".into());
    }
    let target = ctx.herdr.target_for_endpoint(&resolved.endpoint_id);
    let prepared = match ctx
        .herdr
        .endpoint(
            target,
            taskr_herdr::EndpointAction::Prepare {
                lease_id: execution.execution_id.clone(),
            },
        )
        .await
    {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let phase = if error.delivery_certainty == taskr_herdr::DeliveryCertainty::NotDelivered
            {
                ExecutionPhase::Failed
            } else {
                ExecutionPhase::Pending
            };
            return Err(launch_failure(
                ctx,
                &task.id,
                &mut persisted,
                phase,
                format!("endpoint preparation failed: {error}"),
            ));
        }
    };
    execution.runtime_generation = prepared.generation;
    let target = prepared.target;
    facts.endpoint_prepared = true;
    let verification = if execution.inspection {
        ctx.launch_profiles
            .verify_resume_at(&ctx.herdr, &target, &execution)
            .await
    } else {
        ctx.launch_profiles
            .verify_at(&ctx.herdr, &target, &resolved.launch_profile_id)
            .await
    };
    if let Err(error) = verification {
        let _ = ctx
            .herdr
            .endpoint(
                target.clone(),
                taskr_herdr::EndpointAction::Release {
                    lease_id: execution.execution_id.clone(),
                },
            )
            .await;
        return Err(launch_failure(
            ctx,
            &task.id,
            &mut persisted,
            ExecutionPhase::Failed,
            error,
        ));
    }
    facts.environment_verified = true;
    if taskr_core::coordination::launch_step(&execution, facts)
        != taskr_core::coordination::LaunchStep::Allocate
    {
        return Err("launch policy did not authorize allocation".into());
    }
    let mut warnings = Vec::new();

    // 2. Allocate a fresh worker pane in the plan's endpoint-local space and
    // frozen execution group. Concurrent workers share a bounded group tab.
    execution.phase = ExecutionPhase::Allocating;
    persisted = ctx
        .persist(&task.id, execution.clone())
        .map_err(|error| format!("failed to persist allocation intent: {error}"))?;
    let allocated = match plan_layout::allocate(ctx, task, &execution, &resolved).await {
        Ok(allocated) => allocated,
        Err(error) => {
            return Err(launch_failure(
                ctx,
                &task.id,
                &mut persisted,
                taskr_core::coordination::allocation_failure_phase(match error.delivery_certainty {
                    taskr_herdr::DeliveryCertainty::NotDelivered => taskr_core::coordination::EffectOutcome::NotApplied,
                    taskr_herdr::DeliveryCertainty::Delivered => taskr_core::coordination::EffectOutcome::Applied,
                    taskr_herdr::DeliveryCertainty::Unknown => taskr_core::coordination::EffectOutcome::Unknown,
                }),
                format!("pane allocation failed: {error}; unknown/applied outcomes require reconciliation before another launch"),
            ))
        }
    };

    // 3. Persist the allocated runtime ids before starting the agent.
    execution.workspace_id = allocated.workspace_id.clone();
    execution.tab_id = allocated.tab_id.clone();
    execution.pane_id = Some(allocated.pane_id.clone());
    execution.terminal_id = allocated.terminal_id.clone();
    execution.phase = ExecutionPhase::Starting;
    persisted = match ctx.persist(&task.id, execution.clone()) {
        Ok(persisted) => persisted,
        Err(error) => {
            // No agent was started. Retain the allocated shell and ownership
            // rather than deleting the task's layout on cancellation.
            execution.phase = ExecutionPhase::Stopped;
            let _ = ctx
                .orchestration
                .record_execution_if_current(task.id.clone(), execution.clone());
            return Err(format!("failed to persist allocated pane: {error}"));
        }
    };

    facts.agent_start_dispatched = false;
    if taskr_core::coordination::launch_step(&execution, facts)
        != taskr_core::coordination::LaunchStep::StartAgent
    {
        return Err("launch policy did not authorize agent startup".into());
    }
    // 4. Start the agent inside the pane. The bounded startup wait holds
    // one of a few process-wide permits so concurrent launches queue
    // instead of saturating Herdr.
    let started = {
        let _permit = STARTUP_JOBS
            .acquire()
            .await
            .map_err(|_| "startup job queue is closed; cannot wait for agent startup".to_owned())?;
        if !execution.inspection
            && task_status_allows_runtime_cleanup(
                ctx.orchestration.snapshot()?.tasks[&task.id].status,
            )
        {
            return Err("task finished before agent startup".into());
        }
        ctx.herdr
            .start_agent(
                &target,
                &resolved.agent_name,
                &resolved.agent_kind,
                &allocated.pane_id,
                &resolved.args,
                Some(Duration::from_secs(60)),
            )
            .await
    };
    let agent_info = match started {
        Ok(info) => info,
        Err(error) => {
            let failure_phase =
                taskr_core::coordination::startup_failure_phase(match error.delivery_certainty {
                    taskr_herdr::DeliveryCertainty::NotDelivered => {
                        taskr_core::coordination::EffectOutcome::NotApplied
                    }
                    taskr_herdr::DeliveryCertainty::Delivered => {
                        taskr_core::coordination::EffectOutcome::Applied
                    }
                    taskr_herdr::DeliveryCertainty::Unknown => {
                        taskr_core::coordination::EffectOutcome::Unknown
                    }
                });
            if failure_phase == ExecutionPhase::Starting {
                // The start command may still have taken effect: a deadline
                // is an unknown outcome, not a proven failure. Keep the pane
                // and flag the binding for reconciliation instead of
                // rolling back a possibly live worker.
                persisted.phase = ExecutionPhase::Starting;
                persisted.recovery = ExecutionRecovery::NeedsReconciliation;
                let _ = ctx.persist(&task.id, persisted.clone());
                return Err(format!(
                    "agent start for execution '{}' has unknown outcome ({error}); the pane '{}' is kept and the binding needs reconciliation",
                    execution.execution_id, allocated.pane_id
                ));
            }
            // Roll back only resources this attempt created.
            {
                if let Err(close_error) = ctx.herdr.pane_close(&target, &allocated.pane_id).await {
                    warnings.push(format!(
                        "rollback could not close pane '{}': {close_error}",
                        allocated.pane_id
                    ));
                }
            }
            return Err(launch_failure(
                ctx,
                &task.id,
                &mut persisted,
                ExecutionPhase::Failed,
                format!("agent start failed: {error}"),
            ));
        }
    };

    // 5. Reconcile the observed agent placement into the durable binding.
    if let Some(name) = agent_info.name.as_deref() {
        if validate_agent_name(name).is_err() {
            warnings.push(format!(
                "herdr reported unexpected agent name '{name}' for execution '{}'",
                execution.execution_id
            ));
        }
    }
    execution.agent_name = agent_info.name.or(Some(resolved.agent_name.clone()));
    if execution.inspection
        && agent_info.agent_session.is_some()
        && agent_info.agent_session != execution.agent_session
    {
        return Err(launch_failure(
            ctx,
            &task.id,
            &mut persisted,
            ExecutionPhase::Failed,
            "resumed agent reported a different native conversation ID".into(),
        ));
    }
    execution.agent_session = agent_info
        .agent_session
        .clone()
        .or(execution.agent_session.clone());
    if execution.workspace_id.is_none() {
        execution.workspace_id = agent_info.workspace_id.clone();
    }
    if execution.tab_id.is_none() {
        execution.tab_id = agent_info.tab_id.clone();
    }
    execution.recovery = ExecutionRecovery::Reconciled;
    execution.phase = ExecutionPhase::Live;
    persisted = match ctx.persist(&task.id, execution.clone()) {
        Ok(persisted) => persisted,
        Err(error) => return Err(format!("failed to persist live execution: {error}")),
    };

    if !execution.inspection && resolved.agent_kind == "codex" {
        let name = format!(
            "{} · {} · {}",
            task.id.0,
            plan_layout::display_title(&task.title),
            execution.execution_id
        );
        ctx.herdr
            .agent_prompt(
                &target,
                &PromptRequest {
                    target: execution.agent_name.clone().unwrap(),
                    text: format!("/rename {name}"),
                    wait: false,
                    until: Vec::new(),
                    timeout: Some(Duration::from_secs(5)),
                },
            )
            .await
            .map_err(|e| format!("could not name native Codex conversation: {e}"))?;
        execution.native_session_name = Some(name);
        persisted = ctx.persist(&task.id, execution.clone())?;
    }
    Ok(LaunchOutcome {
        execution: persisted,
        warnings,
    })
}

/// Record a truthful failure phase on the durable binding and return the
/// launch error message.
fn launch_failure(
    ctx: &LaunchContext,
    task_id: &TaskId,
    execution: &mut TaskExecution,
    phase: ExecutionPhase,
    message: String,
) -> String {
    // Allocation may have already saved terminal ownership before a later
    // operation failed. Never replace it with the pre-allocation snapshot.
    if let Ok(state) = ctx.orchestration.snapshot() {
        if let Some(current) = state
            .tasks
            .get(task_id)
            .and_then(|task| task.execution.as_ref())
        {
            if current.execution_id == execution.execution_id {
                *execution = current.clone();
            }
        }
    }
    execution.phase = phase;
    let _ = ctx.persist(task_id, execution.clone());
    message
}

/// Submit the rendered task prompt to a live execution. A not-ready worker
/// gets one bounded wait and a single retry before the launch is declared
/// undelivered.
async fn deliver_task_prompt(
    ctx: &LaunchContext,
    execution: &TaskExecution,
    prompt: &str,
) -> Result<PromptOutcome, String> {
    let state = ctx.orchestration.snapshot()?;
    let (task_id, _) = resolve_execution_owner(&state, &execution.execution_id)?;
    if task_status_allows_runtime_cleanup(state.tasks[&task_id].status) {
        return Err("task finished before prompt delivery".into());
    }
    let target = ctx
        .herdr
        .target_for_endpoint(&execution.endpoint_id)
        .fenced(execution.runtime_generation.clone());
    let agent_ref = execution
        .agent_name
        .clone()
        .or_else(|| execution.pane_id.clone())
        .ok_or_else(|| "execution has no agent or pane binding".to_owned())?;
    let request = PromptRequest {
        target: agent_ref.clone(),
        text: prompt.to_owned(),
        wait: false,
        until: Vec::new(),
        timeout: None,
    };
    match ctx.herdr.agent_prompt(&target, &request).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.category == taskr_herdr::RuntimeErrorCategory::AgentNotReady => {
            ctx.herdr
                .agent_wait(
                    &target,
                    &agent_ref,
                    &coding_ready_states(),
                    Duration::from_secs(DEFAULT_CODING_READY_TIMEOUT_SECONDS),
                )
                .await
                .map_err(|wait_error| {
                    format!(
                        "prompt rejected (agent not ready) and readiness wait failed: {wait_error}"
                    )
                })?;
            ctx.herdr
                .agent_prompt(&target, &request)
                .await
                .map_err(|retry_error| format!("prompt retry failed: {retry_error}"))
        }
        Err(error) => Err(format!("prompt delivery failed: {error}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Orchestration scheduler actor
// ═══════════════════════════════════════════════════════════════════════════════

struct OrchestrationSchedulerActor;

enum OrchestrationSchedulerMessage {
    Report {
        args: OrchestrationReportArgs,
        reply: RpcReplyPort<Result<OrchestrationReport, String>>,
    },
    Next {
        args: OrchestrationSchedulerRunArgs,
        reply: RpcReplyPort<Result<OrchestrationSchedulerRunReport, String>>,
    },
    StartTask {
        args: TaskStartArgs,
        reply: RpcReplyPort<Result<TaskStartReport, String>>,
    },
}

#[derive(Clone)]
struct OrchestrationSchedulerState {
    ctx: LaunchContext,
}

impl Actor for OrchestrationSchedulerActor {
    type Msg = OrchestrationSchedulerMessage;
    type State = OrchestrationSchedulerState;
    type Arguments = OrchestrationSchedulerState;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(state)
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            OrchestrationSchedulerMessage::Report { args, reply } => {
                let result = run_orchestration_report(state, args).await;
                let _ = reply.send(result);
            }
            OrchestrationSchedulerMessage::Next { args, reply } => {
                let result = run_orchestration_next(state, args).await;
                let _ = reply.send(result);
            }
            OrchestrationSchedulerMessage::StartTask { args, reply } => {
                let result = run_task_start(state, args).await;
                let _ = reply.send(result);
            }
        }
        Ok(())
    }
}

async fn run_orchestration_report(
    scheduler: &OrchestrationSchedulerState,
    args: OrchestrationReportArgs,
) -> Result<OrchestrationReport, String> {
    let state = scheduler.ctx.orchestration.snapshot()?;
    let project_id = args
        .project_id
        .as_deref()
        .map(|selector| resolve_project_id_or_slug(&state, selector))
        .transpose()?;
    let plan_id = args
        .plan_id
        .as_deref()
        .map(|selector| resolve_plan_id_or_slug(&state, selector))
        .transpose()?;
    let run_report = run_orchestration_next(
        scheduler,
        OrchestrationSchedulerRunArgs {
            project_id: args.project_id,
            plan_id: args.plan_id,
            dry_run: true,
            max_tasks: state.tasks.len().max(1),
        },
    )
    .await?;
    Ok(OrchestrationReport {
        project_id,
        plan_id,
        ready: run_report.would_start,
        skipped: run_report.skipped,
        errors: run_report.errors,
    })
}

async fn run_orchestration_next(
    scheduler: &OrchestrationSchedulerState,
    args: OrchestrationSchedulerRunArgs,
) -> Result<OrchestrationSchedulerRunReport, String> {
    if args.max_tasks == 0 {
        return Err("max_tasks must be greater than zero".into());
    }
    let state = scheduler.ctx.orchestration.snapshot()?;
    let project_id = args
        .project_id
        .as_deref()
        .map(|selector| resolve_project_id_or_slug(&state, selector))
        .transpose()?;
    let plan_id = args
        .plan_id
        .as_deref()
        .map(|selector| resolve_plan_id_or_slug(&state, selector))
        .transpose()?;

    let mut report = OrchestrationSchedulerRunReport {
        dry_run: args.dry_run,
        project_id: project_id.clone(),
        plan_id: plan_id.clone(),
        max_tasks: args.max_tasks,
        would_start: Vec::new(),
        started: Vec::new(),
        skipped: Vec::new(),
        errors: Vec::new(),
    };

    let mut tasks = state.tasks.values().collect::<Vec<_>>();
    tasks.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    let mut launchables: Vec<(Task, TaskRunSpec)> = Vec::new();
    for task in tasks {
        if report.would_start.len() + launchables.len() >= args.max_tasks {
            break;
        }
        if !scheduler_task_matches_filters(&state, task, project_id.as_ref(), plan_id.as_ref()) {
            continue;
        }
        if let Some(reason) = scheduler_superseded_reason(&state, task) {
            report.skipped.push(schedule_skip(task, &reason));
            continue;
        }
        if !task.auto_schedule {
            report
                .skipped
                .push(schedule_skip(task, "auto_schedule is false"));
            continue;
        }
        let Some(run_spec) = task.run_spec.as_ref() else {
            report.skipped.push(schedule_skip(task, "missing run_spec"));
            continue;
        };
        if !matches!(task.status, TaskStatus::Backlog | TaskStatus::Planned) {
            report.skipped.push(schedule_skip(
                task,
                &format!("status {:?} is not auto-startable", task.status),
            ));
            continue;
        }
        let blocked_by = scheduler_dependency_blockers(&state, task);
        if !blocked_by.is_empty() {
            report.skipped.push(schedule_skip(
                task,
                &format!(
                    "dependencies not ready: {}",
                    format_blocking_dependencies(&blocked_by)
                ),
            ));
            continue;
        }
        if let Err(error) = scheduler
            .ctx
            .resolve_profile(&run_spec.endpoint_id, Some(&run_spec.launch_profile_id))
        {
            report.errors.push(schedule_error(task, &error));
            continue;
        }
        if let Some(execution) = task.execution.as_ref() {
            if execution.phase.is_active() {
                report.skipped.push(schedule_skip(
                    task,
                    &format!("execution '{}' is already active", execution.execution_id),
                ));
                continue;
            }
            if !execution.phase.allows_replacement() {
                report.skipped.push(schedule_skip(
                    task,
                    &format!(
                        "execution '{}' is {} and cannot be replaced automatically; stop it or prune the record first",
                        execution.execution_id,
                        execution.phase.as_str()
                    ),
                ));
                continue;
            }
            if args.dry_run {
                report.skipped.push(schedule_skip(
                    task,
                    &format!(
                        "previous execution '{}' is {}; a live run would replace it",
                        execution.execution_id,
                        execution.phase.as_str()
                    ),
                ));
                continue;
            }
        }

        let scheduled = OrchestrationScheduledTask {
            task_id: task.id.clone(),
            slug: task.slug.clone(),
            endpoint_id: run_spec.endpoint_id.clone(),
            execution_id: None,
            agent_name: None,
            pane_id: None,
            launch_profile_id: run_spec.launch_profile_id.clone(),
            template: run_spec.template.clone(),
        };
        if args.dry_run {
            report.would_start.push(scheduled);
            continue;
        }

        launchables.push((task.clone(), run_spec.clone()));
    }

    // Launch the batch concurrently: each job holds one bounded startup
    // permit, inspection stays responsive (registry-backed), and the report
    // is only returned once every dispatched job has reached a terminal
    // state.
    let mut jobs = tokio::task::JoinSet::new();
    for (task, run_spec) in launchables {
        let ctx = scheduler.ctx.clone();
        jobs.spawn(async move {
            scheduler_start_task_inner(&ctx, &task, &run_spec)
                .await
                .map_err(|error| (task.id.clone(), error))
        });
    }
    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok(Ok(started)) => report.started.push(started),
            Ok(Err((task_id, error))) => report.errors.push(OrchestrationScheduleError {
                task_id,
                reason: error,
            }),
            Err(join_error) => report.errors.push(OrchestrationScheduleError {
                task_id: TaskId("unknown".to_owned()),
                reason: format!("startup job join failure: {join_error}"),
            }),
        }
    }

    Ok(report)
}

async fn run_task_start(
    scheduler: &OrchestrationSchedulerState,
    args: TaskStartArgs,
) -> Result<TaskStartReport, String> {
    validate_prompt_text_value("task_id_or_slug", &args.task_id_or_slug)?;
    let state = scheduler.ctx.orchestration.snapshot()?;
    let task = resolve_task_by_id_or_slug(&state, &args.task_id_or_slug)?;
    if let Some(reason) = scheduler_superseded_reason(&state, &task) {
        return Ok(TaskStartReport {
            dry_run: args.dry_run,
            task_id: task.id.clone(),
            auto_schedule: task.auto_schedule,
            action: "skipped".into(),
            task: None,
            reason: Some(reason),
        });
    }
    let Some(run_spec) = task.run_spec.as_ref() else {
        return Err(format!("task '{}' has no run_spec", task.id.0));
    };
    let mut scheduled = OrchestrationScheduledTask {
        task_id: task.id.clone(),
        slug: task.slug.clone(),
        endpoint_id: run_spec.endpoint_id.clone(),
        execution_id: task.execution.as_ref().map(|e| e.execution_id.clone()),
        agent_name: task.execution.as_ref().and_then(|e| e.agent_name.clone()),
        pane_id: task.execution.as_ref().and_then(|e| e.pane_id.clone()),
        launch_profile_id: run_spec.launch_profile_id.clone(),
        template: run_spec.template.clone(),
    };
    if !matches!(task.status, TaskStatus::Backlog | TaskStatus::Planned) {
        return Ok(TaskStartReport {
            dry_run: args.dry_run,
            task_id: task.id.clone(),
            auto_schedule: task.auto_schedule,
            action: "skipped".into(),
            task: Some(scheduled),
            reason: Some(format!("status {:?} is not startable", task.status)),
        });
    }
    let blocked_by = scheduler_dependency_blockers(&state, &task);
    if !blocked_by.is_empty() {
        return Ok(TaskStartReport {
            dry_run: args.dry_run,
            task_id: task.id.clone(),
            auto_schedule: task.auto_schedule,
            action: "skipped".into(),
            task: Some(scheduled),
            reason: Some(format!(
                "dependencies not ready: {}",
                format_blocking_dependencies(&blocked_by)
            )),
        });
    }
    scheduler
        .ctx
        .resolve_profile(&run_spec.endpoint_id, Some(&run_spec.launch_profile_id))?;
    if let Some(execution) = task.execution.as_ref() {
        if execution.phase.is_active() {
            return Ok(TaskStartReport {
                dry_run: false,
                task_id: task.id.clone(),
                auto_schedule: task.auto_schedule,
                action: "already-running".into(),
                task: Some(scheduled),
                reason: Some(format!(
                    "execution '{}' is already active",
                    execution.execution_id
                )),
            });
        }
        if !execution.phase.allows_replacement() {
            return Ok(TaskStartReport {
                dry_run: args.dry_run,
                task_id: task.id.clone(),
                auto_schedule: task.auto_schedule,
                action: "skipped".into(),
                task: Some(scheduled),
                reason: Some(format!(
                    "execution '{}' is {} and cannot be replaced automatically; stop it or prune the record first",
                    execution.execution_id,
                    execution.phase.as_str()
                )),
            });
        }
    }
    if args.dry_run {
        return Ok(TaskStartReport {
            dry_run: true,
            task_id: task.id.clone(),
            auto_schedule: task.auto_schedule,
            action: "would-start".into(),
            task: Some(scheduled),
            reason: None,
        });
    }

    let started = scheduler_start_task_inner(&scheduler.ctx, &task, run_spec).await?;
    scheduled.execution_id = started.execution_id.clone();
    scheduled.agent_name = started.agent_name.clone();
    scheduled.pane_id = started.pane_id.clone();
    Ok(TaskStartReport {
        dry_run: false,
        task_id: task.id.clone(),
        auto_schedule: task.auto_schedule,
        action: "started".into(),
        task: Some(scheduled),
        reason: None,
    })
}

async fn scheduler_start_task_inner(
    ctx: &LaunchContext,
    task: &Task,
    run_spec: &TaskRunSpec,
) -> Result<OrchestrationScheduledTask, String> {
    let outcome = launch_task_execution(ctx, task, Some(run_spec), None).await?;
    for warning in &outcome.warnings {
        eprintln!("taskr scheduler warning: {warning}");
    }

    // Render and deliver the task prompt from durable state.
    let prompt_args = CodingTaskSendArgs {
        execution_id: outcome.execution.execution_id.clone(),
        task_id_or_slug: task.id.0.clone(),
        prompt: Some(run_spec.instruction.clone()),
        template: Some(scheduler_parse_template(&run_spec.template)?),
        include_dependencies: None,
        include_gates: None,
        include_scope: None,
        context_task_ids: None,
        extra_context: None,
    };
    let prompt = build_coding_task_prompt(&ctx.orchestration.snapshot()?, &prompt_args)?;
    if let Err(error) = deliver_task_prompt(ctx, &outcome.execution, &prompt).await {
        let _ = ctx.orchestration.mark_execution_blocked(
            task.id.clone(),
            &outcome.execution.execution_id,
            "scheduler could not deliver the task prompt".into(),
            Some(vec![error.clone()]),
            false,
        );
        return Err(error);
    }
    ctx.orchestration
        .mark_execution_running(
            task.id.clone(),
            &outcome.execution.execution_id,
            format!(
                "scheduler started execution '{}' on endpoint '{}'",
                outcome.execution.execution_id, outcome.execution.endpoint_id
            ),
        )
        .map_err(|error| format!("failed to mark task running: {error}"))?;
    Ok(OrchestrationScheduledTask {
        task_id: task.id.clone(),
        slug: task.slug.clone(),
        endpoint_id: outcome.execution.endpoint_id.clone(),
        execution_id: Some(outcome.execution.execution_id.clone()),
        agent_name: outcome.execution.agent_name.clone(),
        pane_id: outcome.execution.pane_id.clone(),
        launch_profile_id: outcome.execution.launch_profile_id.clone(),
        template: run_spec.template.clone(),
    })
}

fn scheduler_task_matches_filters(
    state: &OrchestrationState,
    task: &Task,
    project_id: Option<&ProjectId>,
    plan_id: Option<&PlanId>,
) -> bool {
    if plan_id.is_some_and(|plan_id| &task.plan_id != plan_id) {
        return false;
    }
    if let Some(project_id) = project_id {
        return task_project_id(state, task).as_ref() == Some(project_id);
    }
    true
}

fn scheduler_dependency_blockers(
    state: &OrchestrationState,
    task: &Task,
) -> Vec<TaskDependencyBlocker> {
    state
        .task_dependency_blockers(&task.id)
        .unwrap_or_else(|error| {
            vec![TaskDependencyBlocker {
                task_id: task.id.clone(),
                status: task.status,
                reason: error,
                validation_blocked_by: Vec::new(),
            }]
        })
}

fn scheduler_superseded_reason(state: &OrchestrationState, task: &Task) -> Option<String> {
    let superseded_by = state.task_superseded_by(&task.id).unwrap_or_default();
    if superseded_by.is_empty() {
        return None;
    }
    Some(format!(
        "superseded by {}",
        superseded_by
            .iter()
            .map(|task_id| task_id.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn format_blocking_dependencies(blocked_by: &[TaskDependencyBlocker]) -> String {
    blocked_by
        .iter()
        .map(|blocker| {
            if blocker.validation_blocked_by.is_empty() {
                format!(
                    "{} [{:?}: {}]",
                    blocker.task_id.0, blocker.status, blocker.reason
                )
            } else {
                format!(
                    "{} [{:?}: {}; validators: {}]",
                    blocker.task_id.0,
                    blocker.status,
                    blocker.reason,
                    blocker
                        .validation_blocked_by
                        .iter()
                        .map(|task_id| task_id.0.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn schedule_skip(task: &Task, reason: &str) -> OrchestrationScheduleSkip {
    OrchestrationScheduleSkip {
        task_id: task.id.clone(),
        reason: reason.to_owned(),
    }
}

fn schedule_error(task: &Task, reason: &str) -> OrchestrationScheduleError {
    OrchestrationScheduleError {
        task_id: task.id.clone(),
        reason: reason.to_owned(),
    }
}

fn scheduler_parse_template(template: &str) -> Result<CodingTaskSendTemplate, String> {
    match template.trim() {
        "task" => Ok(CodingTaskSendTemplate::Task),
        "validate" => Ok(CodingTaskSendTemplate::Validate),
        "review" => Ok(CodingTaskSendTemplate::Review),
        "quality-guard" => Ok(CodingTaskSendTemplate::QualityGuard),
        other => Err(format!("unsupported task run_spec template '{other}'")),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Tool result helpers + argument parsing
// ═══════════════════════════════════════════════════════════════════════════════

fn error_result(message: String) -> Result<CallToolResult, McpError> {
    let mut result = CallToolResult::success(vec![Content::text(format!("Error: {message}"))]);
    result.is_error = Some(true);
    Ok(result)
}

/// Surface a Herdr runtime failure with its full classification intact:
/// error category, endpoint, execution, underlying herdr code, and how
/// certain the (non-)delivery is. Clients parse this shape instead of
/// scraping prose.
fn structured_runtime_error(
    execution_id: Option<&str>,
    error: &taskr_herdr::RuntimeError,
) -> Result<CallToolResult, McpError> {
    let payload = json!({
        "category": error.category.as_str(),
        "operation": error.operation,
        "endpoint_id": error.endpoint,
        "execution_id": execution_id,
        "herdr_code": error.herdr_code,
        "delivery_certainty": error.delivery_certainty.as_str(),
        "message": error.detail,
    });
    let mut result = CallToolResult::success(vec![Content::json(payload.clone())
        .map_err(|json_error| McpError::internal_error(json_error.to_string(), None))?]);
    result.is_error = Some(true);
    result.structured_content = Some(payload);
    Ok(result)
}

fn json_result(value: Value) -> Result<CallToolResult, McpError> {
    let content = Content::json(value.clone())
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let mut result = CallToolResult::success(vec![content]);
    result.structured_content = Some(value);
    Ok(result)
}

fn store_result<T>(result: Result<T, String>) -> Result<T, McpError> {
    result.map_err(|error| McpError::internal_error(error, None))
}

fn mcp_invalid_request(message: String) -> McpError {
    McpError::invalid_request(message, None)
}

/// Parse tool arguments into `T`. Every argument struct in this file carries
/// `#[serde(deny_unknown_fields)]`, so deprecated node/session/profile
/// fields are rejected here at the boundary rather than ignored.
fn parse_tool_args<T: DeserializeOwned>(args: Option<&Value>) -> Result<T, McpError> {
    let args = args.cloned().unwrap_or_else(|| json!({}));
    serde_json::from_value::<T>(args)
        .map_err(|error| mcp_invalid_request(format!("invalid tool arguments: {error}")))
}

const MAX_PROMPT_TEXT_BYTES: usize = 1_000_000;

/// Validate a free-form text value arriving over the tool boundary: never
/// empty after trimming, never carrying control characters, never
/// unreasonably large.
fn validate_prompt_text_value(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > MAX_PROMPT_TEXT_BYTES {
        return Err(format!(
            "{field} is too large ({} bytes > {MAX_PROMPT_TEXT_BYTES})",
            value.len()
        ));
    }
    if value
        .chars()
        .any(|ch| ch != '\n' && ch != '\t' && ch.is_control())
    {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(())
}

fn validate_coding_prompt(prompt: &str) -> Result<(), String> {
    validate_prompt_text_value("prompt", prompt)
}

fn tool_schema(schema: Value, required: Option<Vec<&str>>) -> Arc<Map<String, Value>> {
    let mut map = Map::new();
    map.insert("type".into(), json!("object"));
    if let Value::Object(properties) = schema {
        map.insert("properties".into(), Value::Object(properties));
    } else {
        map.insert("properties".into(), json!({}));
    }
    map.insert("additionalProperties".into(), json!(false));
    if let Some(required) = required {
        map.insert("required".into(), json!(required));
    }
    Arc::new(map)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Scheduler report types
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrchestrationScheduledTask {
    pub(crate) task_id: TaskId,
    pub(crate) slug: String,
    pub(crate) endpoint_id: String,
    pub(crate) execution_id: Option<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) pane_id: Option<String>,
    pub(crate) launch_profile_id: String,
    pub(crate) template: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrchestrationScheduleSkip {
    pub(crate) task_id: TaskId,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrchestrationScheduleError {
    pub(crate) task_id: TaskId,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrchestrationReport {
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) plan_id: Option<PlanId>,
    pub(crate) ready: Vec<OrchestrationScheduledTask>,
    pub(crate) skipped: Vec<OrchestrationScheduleSkip>,
    pub(crate) errors: Vec<OrchestrationScheduleError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OrchestrationSchedulerRunReport {
    pub(crate) dry_run: bool,
    pub(crate) project_id: Option<ProjectId>,
    pub(crate) plan_id: Option<PlanId>,
    pub(crate) max_tasks: usize,
    pub(crate) would_start: Vec<OrchestrationScheduledTask>,
    pub(crate) started: Vec<OrchestrationScheduledTask>,
    pub(crate) skipped: Vec<OrchestrationScheduleSkip>,
    pub(crate) errors: Vec<OrchestrationScheduleError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TaskStartReport {
    pub(crate) dry_run: bool,
    pub(crate) task_id: TaskId,
    pub(crate) auto_schedule: bool,
    pub(crate) action: String,
    pub(crate) task: Option<OrchestrationScheduledTask>,
    pub(crate) reason: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Tool argument structs
// ═══════════════════════════════════════════════════════════════════════════════
//
// Core Create*/Update* input types (CreateProject, CreatePlan, CreateTask,
// CreateTaskEdge, UpdateProject, UpdatePlan, UpdateTask, TaskRunSpec) are
// reused directly as tool payloads; they already deny unknown fields.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectStatusUpdateArgs {
    project_id: String,
    status: ProjectStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanGetArgs {
    plan_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanStatusUpdateArgs {
    plan_id: String,
    status: PlanStatus,
    #[serde(default)]
    outcome: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskGetArgs {
    task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskEdgeRemoveArgs {
    from: String,
    to: String,
    kind: TaskEdgeKind,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStatusUpdateArgs {
    task_id: String,
    status: TaskStatus,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    blockers: Option<Vec<String>>,
    #[serde(default)]
    evidence: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationStatusArgs {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default)]
    include_tasks: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationReportArgs {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationSchedulerRunArgs {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
    #[serde(default = "default_max_tasks")]
    max_tasks: usize,
}

fn default_true() -> bool {
    true
}

fn default_max_tasks() -> usize {
    5
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationNextArgs {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    plan_id: Option<String>,
    #[serde(default = "default_true")]
    dry_run: bool,
    #[serde(default = "default_max_tasks")]
    max_tasks: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStartArgs {
    task_id_or_slug: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartCodingSessionArgs {
    task_id: String,
    endpoint_id: String,
    launch_profile_id: String,
    workspace_path: String,
    #[serde(default)]
    bypass_permissions: bool,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    skills: Option<Vec<String>>,
    #[serde(default)]
    template: Option<CodingTaskSendTemplate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionAdoptArgs {
    task_id: String,
    endpoint_id: String,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    agent_name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionStopArgs {
    execution_id: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListExecutionsArgs {
    project_id: String,
    #[serde(default)]
    include_completed: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminEndpointAgentsArgs {
    #[serde(default)]
    endpoint_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointMigrateArgs {
    legacy_node_id: String,
    endpoint_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingSendArgs {
    execution_id: String,
    prompt: String,
    #[serde(default)]
    wait_until_idle: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CodingTaskSendTemplate {
    Task,
    Validate,
    Review,
    QualityGuard,
}

impl CodingTaskSendTemplate {
    fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Validate => "validate",
            Self::Review => "review",
            Self::QualityGuard => "quality-guard",
        }
    }

    fn template_text(self) -> &'static str {
        match self {
            Self::Task => CODING_TASK_SEND_PROMPT,
            Self::Validate => CODING_VALIDATE_SEND_PROMPT,
            Self::Review => CODING_REVIEW_SEND_PROMPT,
            Self::QualityGuard => CODING_QUALITY_GUARD_SEND_PROMPT,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingTaskSendArgs {
    execution_id: String,
    task_id_or_slug: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    template: Option<CodingTaskSendTemplate>,
    #[serde(default)]
    include_dependencies: Option<bool>,
    #[serde(default)]
    include_gates: Option<bool>,
    #[serde(default)]
    include_scope: Option<bool>,
    #[serde(default)]
    context_task_ids: Option<Vec<String>>,
    #[serde(default)]
    extra_context: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingReadArgs {
    execution_id: String,
    #[serde(default)]
    source: ReadSourceSelector,
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadSourceSelector {
    #[default]
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

impl From<ReadSourceSelector> for ReadSource {
    fn from(selector: ReadSourceSelector) -> Self {
        match selector {
            ReadSourceSelector::Visible => ReadSource::Visible,
            ReadSourceSelector::Recent => ReadSource::Recent,
            ReadSourceSelector::RecentUnwrapped => ReadSource::RecentUnwrapped,
            ReadSourceSelector::Detection => ReadSource::Detection,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureOutputArgs {
    execution_id: String,
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckStateArgs {
    execution_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInputArgs {
    execution_id: String,
    text: String,
    #[serde(default = "default_true")]
    enter: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendKeyArgs {
    execution_id: String,
    key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingActionArgs {
    execution_id: String,
    action: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    execution_id: String,
    command: Vec<String>,
    #[serde(default)]
    timeout_seconds: Option<f64>,
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeWaitKind {
    Stable,
    Sentinel,
    CodingReady,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitStartArgs {
    execution_id: String,
    kind: RuntimeWaitKind,
    #[serde(default)]
    sentinel: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<f64>,
    #[serde(default)]
    poll_seconds: Option<f64>,
    #[serde(default)]
    stability_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitStatusArgs {
    wait_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitCancelArgs {
    wait_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestrationPruneArgs {
    #[serde(default = "default_true")]
    dry_run: bool,
    #[serde(default)]
    older_than_days: Option<u64>,
    #[serde(default = "default_true")]
    include_stale_execution_records: bool,
    #[serde(default = "default_true")]
    include_finished_plans: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Runtime wait registry
// ═══════════════════════════════════════════════════════════════════════════════

/// Herdr keeps unseen completions in `done`; both states accept new input.
fn coding_ready_states() -> Vec<String> {
    vec!["idle".into(), "done".into()]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RuntimeWaitSnapshot {
    wait_id: String,
    execution_id: String,
    endpoint_id: String,
    kind: RuntimeWaitKind,
    state: String,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    started_at_ms: u64,
    #[serde(default)]
    completed_at_ms: Option<u64>,
}

struct WaitJobEntry {
    status: Arc<Mutex<RuntimeWaitSnapshot>>,
    handle: JoinHandle<()>,
}

type WaitJobRegistry = Arc<Mutex<HashMap<String, WaitJobEntry>>>;

fn runtime_wait_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("wait-{nanos:x}")
}

fn wait_job_snapshot(
    registry: &WaitJobRegistry,
    wait_id: &str,
) -> Result<RuntimeWaitSnapshot, String> {
    let guard = registry
        .lock()
        .map_err(|_| "wait job registry lock poisoned".to_owned())?;
    let entry = guard
        .get(wait_id)
        .ok_or_else(|| format!("wait job '{wait_id}' not found (completed waits are pruned)"))?;
    let status = entry
        .status
        .lock()
        .map_err(|_| "wait job status lock poisoned".to_owned())?;
    Ok(status.clone())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  MCP server
// ═══════════════════════════════════════════════════════════════════════════════

struct HerdrMcpServer {
    herdr: HerdrClient,
    launch_profiles: Arc<ResolvedLaunchProfiles>,
    policy: Arc<ControllerPolicy>,
    scheduler: ActorRef<OrchestrationSchedulerMessage>,
    orchestration: orchestration_actor::OrchestrationHandle,
    wait_jobs: WaitJobRegistry,
}

const SCHEDULER_CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl HerdrMcpServer {
    async fn orchestration_report(
        &self,
        args: OrchestrationReportArgs,
    ) -> Result<OrchestrationReport, String> {
        match self
            .scheduler
            .call(
                |reply| OrchestrationSchedulerMessage::Report { args, reply },
                Some(SCHEDULER_CALL_TIMEOUT),
            )
            .await
        {
            Ok(CallResult::Success(result)) => result,
            Ok(CallResult::Timeout) => Err("orchestration scheduler timed out".into()),
            Ok(CallResult::SenderError) => {
                Err("orchestration scheduler dropped the reply channel".into())
            }
            Err(error) => Err(format!("orchestration scheduler unreachable: {error}")),
        }
    }

    async fn orchestration_next(
        &self,
        args: OrchestrationNextArgs,
    ) -> Result<OrchestrationSchedulerRunReport, String> {
        let args = OrchestrationSchedulerRunArgs {
            project_id: args.project_id,
            plan_id: args.plan_id,
            dry_run: args.dry_run,
            max_tasks: args.max_tasks,
        };
        match self
            .scheduler
            .call(
                |reply| OrchestrationSchedulerMessage::Next { args, reply },
                Some(SCHEDULER_CALL_TIMEOUT),
            )
            .await
        {
            Ok(CallResult::Success(result)) => result,
            Ok(CallResult::Timeout) => Err("orchestration scheduler timed out".into()),
            Ok(CallResult::SenderError) => {
                Err("orchestration scheduler dropped the reply channel".into())
            }
            Err(error) => Err(format!("orchestration scheduler unreachable: {error}")),
        }
    }

    async fn task_start(&self, args: TaskStartArgs) -> Result<TaskStartReport, String> {
        match self
            .scheduler
            .call(
                |reply| OrchestrationSchedulerMessage::StartTask { args, reply },
                Some(SCHEDULER_CALL_TIMEOUT),
            )
            .await
        {
            Ok(CallResult::Success(result)) => result,
            Ok(CallResult::Timeout) => Err("orchestration scheduler timed out".into()),
            Ok(CallResult::SenderError) => {
                Err("orchestration scheduler dropped the reply channel".into())
            }
            Err(error) => Err(format!("orchestration scheduler unreachable: {error}")),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Execution resolution + live runtime observation
// ═══════════════════════════════════════════════════════════════════════════════

struct ResolvedExecution {
    task_id: TaskId,
    task: Task,
    execution: TaskExecution,
}

impl HerdrMcpServer {
    fn resolve_execution(&self, execution_id: &str) -> Result<ResolvedExecution, String> {
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| error.to_string())?;
        let (task_id, execution) = resolve_execution_owner(&state, execution_id)?;
        endpoint_migration::require_resolved_endpoint(&execution.endpoint_id)?;
        let task =
            state.tasks.get(&task_id).cloned().ok_or_else(|| {
                format!("task '{}' vanished while resolving execution", task_id.0)
            })?;
        Ok(ResolvedExecution {
            task_id,
            task,
            execution,
        })
    }

    fn execution_agent_ref(execution: &TaskExecution) -> Option<String> {
        execution
            .agent_name
            .clone()
            .or_else(|| execution.pane_id.clone())
    }

    /// Whether an observed agent is still the worker this execution
    /// launched. Both the live name and the native session reference must
    /// agree with the durable binding when both sides know them.
    fn execution_owns_agent(execution: &TaskExecution, info: &taskr_herdr::AgentInfo) -> bool {
        taskr_core::coordination::owns_agent(
            execution,
            &taskr_core::coordination::AgentObservation {
                name: info.name.clone(),
                terminal_id: info.terminal_id.clone(),
                session: info.agent_session.clone(),
            },
        )
    }

    /// Confirm the live occupant of the execution's binding is still the
    /// agent this execution launched before any interaction is delivered or
    /// output is read. The stored name and pane ID alone are not proof: the
    /// pane may have been replaced, exited, or re-tasked since the binding
    /// was written, so every interaction path checks the endpoint first.
    async fn verify_execution_occupant(
        &self,
        execution: &TaskExecution,
    ) -> Result<taskr_herdr::AgentInfo, String> {
        let agent_ref = HerdrMcpServer::execution_agent_ref(execution).ok_or_else(|| {
            format!(
                "execution '{}' has no agent or pane binding",
                execution.execution_id
            )
        })?;
        let target = self.herdr_target(execution);
        let info = self
            .herdr
            .agent_get(&target, &agent_ref)
            .await
            .map_err(|error| {
                format!(
                    "cannot verify the live occupant of execution '{}' (binding '{}'): {error}; \
                     reconcile the binding (execution_adopt or restart) before interacting",
                    execution.execution_id, agent_ref
                )
            })?;
        if !HerdrMcpServer::execution_owns_agent(execution, &info) {
            let actual = info.name.as_deref().unwrap_or("<unknown>");
            let expected = execution.agent_name.as_deref().unwrap_or("<unnamed>");
            return Err(format!(
                "binding of execution '{}' is occupied by '{actual}' instead of the launched \
                 agent '{expected}'; adopt the replacement explicitly (execution_adopt) or stop \
                 the execution before interacting",
                execution.execution_id
            ));
        }
        Ok(info)
    }

    fn herdr_target(&self, execution: &TaskExecution) -> EndpointTarget {
        self.herdr
            .target_for_endpoint(&execution.endpoint_id)
            .fenced(execution.runtime_generation.clone())
    }

    fn require_live(execution: &TaskExecution) -> Result<(), String> {
        if execution.phase.is_active() {
            Ok(())
        } else {
            Err(format!(
                "execution '{}' phase is {} and cannot accept interaction",
                execution.execution_id,
                execution.phase.as_str()
            ))
        }
    }
}

/// Observe one endpoint's live runtime keys (`endpoint:pane` and
/// `endpoint:agent-name`). An unreachable endpoint contributes a warning and
/// no keys, so prune callers can skip it rather than destroy records.
/// Returns the observed agent rows too, so reconciliation can verify the
/// occupant's identity instead of trusting a matching name alone.
async fn collect_live_runtime_keys(
    herdr: &HerdrClient,
    endpoint_ids: &HashSet<String>,
) -> (
    HashSet<String>,
    Vec<String>,
    Vec<String>,
    Vec<(String, taskr_herdr::AgentInfo)>,
) {
    let mut live = HashSet::new();
    let mut warnings = Vec::new();
    // An endpoint counts as observed only when both inventories succeeded:
    // prune may destroy a binding on this endpoint solely because its
    // runtime key is absent, and absence is only provable with a complete
    // view of agents AND panes.
    let mut observed = Vec::new();
    let mut agents_by_key = Vec::new();
    for endpoint_id in endpoint_ids {
        if let Err(error) = endpoint_migration::require_resolved_endpoint(endpoint_id) {
            warnings.push(format!("endpoint '{endpoint_id}' unresolved: {error}"));
            continue;
        }
        let target = match herdr
            .endpoint(
                herdr.target_for_endpoint(endpoint_id),
                taskr_herdr::EndpointAction::Observe,
            )
            .await
        {
            Ok(endpoint) if endpoint.ready => endpoint.target,
            Ok(_) => {
                warnings.push(format!("endpoint '{endpoint_id}' is not ready"));
                continue;
            }
            Err(error) => {
                warnings.push(format!("endpoint '{endpoint_id}' unavailable: {error}"));
                continue;
            }
        };

        let agents = match herdr.agent_list(&target).await {
            Ok(agents) => agents,
            Err(error) => {
                warnings.push(format!("endpoint '{endpoint_id}' unavailable: {error}"));
                continue;
            }
        };
        for agent in &agents {
            for resource in [agent.name.as_deref(), agent.pane_id.as_deref()]
                .into_iter()
                .flatten()
            {
                for generation in [None, target.runtime_generation.as_deref()] {
                    let key = taskr_core::orchestration::runtime_key_for(
                        endpoint_id,
                        generation,
                        resource,
                    );
                    live.insert(key.clone());
                    agents_by_key.push((key, agent.clone()));
                }
            }
        }
        // Panes without a recognized agent still hold live runtime;
        // include them so prune never destroys a binding Herdr is
        // merely not attributing to an agent.
        match herdr.panes(&target).await {
            Ok(panes) => {
                observed.push(endpoint_id.clone());
                for pane in panes {
                    live.insert(taskr_core::orchestration::runtime_key_for(
                        endpoint_id,
                        None,
                        &pane.pane_id,
                    ));
                    live.insert(taskr_core::orchestration::runtime_key_for(
                        endpoint_id,
                        target.runtime_generation.as_deref(),
                        &pane.pane_id,
                    ));
                }
            }
            Err(error) => warnings.push(format!(
                "endpoint '{endpoint_id}' pane listing failed: {error}"
            )),
        }
    }
    (live, warnings, observed, agents_by_key)
}

fn to_json<T: Serialize>(value: &T) -> Result<Value, McpError> {
    serde_json::to_value(value)
        .map_err(|error| McpError::internal_error(format!("serialization failed: {error}"), None))
}

/// Split one named argument out of the raw argument object so the remainder
/// can be deserialized into a core `deny_unknown_fields` input type (serde
/// cannot combine `deny_unknown_fields` with `flatten`).
fn split_named_arg(args: Option<&Value>, key: &str) -> Result<(String, Value), McpError> {
    let mut map = match args {
        Some(Value::Object(map)) => map.clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => {
            return Err(mcp_invalid_request(
                "tool arguments must be a JSON object".into(),
            ))
        }
    };
    let value = map
        .remove(key)
        .ok_or_else(|| mcp_invalid_request(format!("missing required argument '{key}'")))?;
    let id: String = serde_json::from_value(value).map_err(|error| {
        mcp_invalid_request(format!("argument '{key}' must be a string: {error}"))
    })?;
    Ok((id, Value::Object(map)))
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Orchestration tools (projects / plans / tasks / edges / status)
// ═══════════════════════════════════════════════════════════════════════════════

impl HerdrMcpServer {
    async fn project_create_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        self.policy.ensure_admin_tools_enabled("project_create")?;
        let input: CreateProject = parse_tool_args(args)?;
        if input.title.trim().is_empty() {
            return Err(mcp_invalid_request("title must not be empty".into()));
        }
        match self.orchestration.create_project(input) {
            Ok(project) => json_result(to_json(&project)?),
            Err(error) => error_result(error),
        }
    }

    async fn project_update_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        self.policy.ensure_admin_tools_enabled("project_update")?;
        let (project_id, rest) = split_named_arg(args, "project_id")?;
        let update: UpdateProject = parse_tool_args(Some(&rest))?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved =
            resolve_project_id_or_slug(&state, &project_id).map_err(mcp_invalid_request)?;
        match self.orchestration.update_project(resolved, update) {
            Ok(project) => json_result(to_json(&project)?),
            Err(error) => error_result(error),
        }
    }

    async fn project_status_update_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        self.policy
            .ensure_admin_tools_enabled("project_status_update")?;
        let args: ProjectStatusUpdateArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved =
            resolve_project_id_or_slug(&state, &args.project_id).map_err(mcp_invalid_request)?;
        match self
            .orchestration
            .update_project_status(resolved, args.status)
        {
            Ok(project) => json_result(to_json(&project)?),
            Err(error) => error_result(error),
        }
    }

    async fn project_list_tool(&self, _args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let status = self
            .orchestration
            .status()
            .map_err(|error| McpError::internal_error(error, None))?;
        json_result(json!({
            "projects": status.projects,
            "counts": summarize_orchestration_counts(&status),
        }))
    }

    async fn plan_create_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let input: CreatePlan = parse_tool_args(args)?;
        if input.title.trim().is_empty() {
            return Err(mcp_invalid_request("title must not be empty".into()));
        }
        if input.brief.trim().is_empty() {
            return Err(mcp_invalid_request("brief must not be empty".into()));
        }
        let state = store_result(self.orchestration.snapshot())?;
        let project_id =
            resolve_project_id_or_slug(&state, &input.project_id.0).map_err(mcp_invalid_request)?;
        let input = CreatePlan {
            project_id,
            ..input
        };
        match self.orchestration.create_plan(input) {
            Ok(plan) => json_result(to_json(&plan)?),
            Err(error) => error_result(error),
        }
    }

    async fn plan_update_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let (plan_id, rest) = split_named_arg(args, "plan_id")?;
        let update: UpdatePlan = parse_tool_args(Some(&rest))?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved = resolve_plan_id_or_slug(&state, &plan_id).map_err(mcp_invalid_request)?;
        match self.orchestration.update_plan(resolved, update) {
            Ok(plan) => json_result(to_json(&plan)?),
            Err(error) => error_result(error),
        }
    }

    async fn plan_status_update_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: PlanStatusUpdateArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved =
            resolve_plan_id_or_slug(&state, &args.plan_id).map_err(mcp_invalid_request)?;
        match self
            .orchestration
            .update_plan_status(resolved, args.status, args.outcome)
        {
            Ok(plan) => json_result(to_json(&plan)?),
            Err(error) => error_result(error),
        }
    }

    async fn plan_get_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: PlanGetArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let plan_id =
            resolve_plan_id_or_slug(&state, &args.plan_id).map_err(mcp_invalid_request)?;
        let plan = state
            .plans
            .get(&plan_id)
            .cloned()
            .ok_or_else(|| mcp_invalid_request(format!("plan '{}' not found", args.plan_id)))?;
        let tasks = state
            .tasks
            .values()
            .filter(|task| task.plan_id == plan_id)
            .map(|task| json!({"id": task.id.0, "slug": task.slug, "title": task.title, "status": task.status}))
            .collect::<Vec<_>>();
        json_result(json!({"plan": plan, "tasks": tasks}))
    }

    async fn plan_list_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let selector: Option<String> = match args {
            None => None,
            Some(value) => {
                let args: Map<String, Value> = parse_tool_args(Some(value))?;
                args.get("project_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }
        };
        let status = self
            .orchestration
            .status()
            .map_err(|error| McpError::internal_error(error, None))?;
        let plans = match selector {
            Some(selector) => {
                let state = store_result(self.orchestration.snapshot())?;
                let project_id =
                    resolve_project_id_or_slug(&state, &selector).map_err(mcp_invalid_request)?;
                status
                    .plans
                    .into_iter()
                    .filter(|plan| plan.project_id == project_id)
                    .collect::<Vec<_>>()
            }
            None => status.plans,
        };
        json_result(json!({"plans": plans}))
    }

    async fn task_create_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let input: CreateTask = parse_tool_args(args)?;
        if input.title.trim().is_empty() {
            return Err(mcp_invalid_request("title must not be empty".into()));
        }
        if input.objective.trim().is_empty() {
            return Err(mcp_invalid_request("objective must not be empty".into()));
        }
        let state = store_result(self.orchestration.snapshot())?;
        let plan_id =
            resolve_plan_id_or_slug(&state, &input.plan_id.0).map_err(mcp_invalid_request)?;
        let input = CreateTask { plan_id, ..input };
        match self.orchestration.create_task(input) {
            Ok(task) => json_result(to_json(&task)?),
            Err(error) => error_result(error),
        }
    }

    async fn task_update_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let (task_id, rest) = split_named_arg(args, "task_id")?;
        let update: UpdateTask = parse_tool_args(Some(&rest))?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved = resolve_task_by_id_or_slug(&state, &task_id).map_err(mcp_invalid_request)?;
        match self.orchestration.update_task(resolved.id, update) {
            Ok(task) => json_result(to_json(&task)?),
            Err(error) => error_result(error),
        }
    }

    async fn task_get_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: TaskGetArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let task =
            resolve_task_by_id_or_slug(&state, &args.task_id).map_err(mcp_invalid_request)?;
        let dependency_blockers = state.task_dependency_blockers(&task.id).unwrap_or_default();
        let superseded_by = state.task_superseded_by(&task.id).unwrap_or_default();
        let mut execution_history: Vec<_> = state
            .retained_executions
            .values()
            .filter(|row| row.task_id == task.id)
            .map(|row| row.execution.clone())
            .collect();
        execution_history.sort_by(|left, right| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.execution_id.cmp(&left.execution_id))
        });
        json_result(json!({
            "task": task,
            "execution_history": execution_history,
            "dependency_blockers": dependency_blockers,
            "superseded_by": superseded_by,
        }))
    }

    async fn task_status_update_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: TaskStatusUpdateArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let resolved =
            resolve_task_by_id_or_slug(&state, &args.task_id).map_err(mcp_invalid_request)?;
        match self.orchestration.update_task_status_details(
            resolved.id,
            args.status,
            args.outcome,
            args.blockers,
            args.evidence,
        ) {
            Ok(task) if task_status_allows_runtime_cleanup(task.status) => {
                let ctx = LaunchContext {
                    herdr: self.herdr.clone(),
                    launch_profiles: self.launch_profiles.clone(),
                    orchestration: self.orchestration.clone(),
                };
                match execution_cleanup::cleanup_after_task_update(&ctx, &task.id).await {
                    Ok(task) => {
                        let mut value = to_json(&task)?;
                        let held = execution_cleanup::audit_holds(
                            &store_result(self.orchestration.snapshot())?,
                            &task.id,
                        );
                        if !held.is_empty() {
                            value["runtime_cleanup"] =
                                json!({"state": "held_for_audit", "audit_task_ids": held});
                        }
                        json_result(value)
                    }
                    Err(error) => {
                        let fresh = store_result(self.orchestration.snapshot())?;
                        let mut value = to_json(fresh.tasks.get(&task.id).unwrap_or(&task))?;
                        value["runtime_cleanup"] =
                            json!({"state": "pending", "error": error, "retry": "automatic"});
                        let mut result = json_result(value)?;
                        result.is_error = Some(true);
                        Ok(result)
                    }
                }
            }
            Ok(task) => json_result(to_json(&task)?),
            Err(error) => error_result(error),
        }
    }

    async fn task_edge_add_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: TaskEdgeRemoveArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let from = resolve_task_by_id_or_slug(&state, &args.from)
            .map_err(mcp_invalid_request)?
            .id;
        let to = resolve_task_by_id_or_slug(&state, &args.to)
            .map_err(mcp_invalid_request)?
            .id;
        let input = CreateTaskEdge {
            from,
            to,
            kind: args.kind,
            note: None,
        };
        match self.orchestration.add_task_edge(input) {
            Ok(edge) => json_result(to_json(&edge)?),
            Err(error) => error_result(error),
        }
    }

    async fn task_edge_remove_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: TaskEdgeRemoveArgs = parse_tool_args(args)?;
        let state = store_result(self.orchestration.snapshot())?;
        let from = resolve_task_by_id_or_slug(&state, &args.from)
            .map_err(mcp_invalid_request)?
            .id;
        let to = resolve_task_by_id_or_slug(&state, &args.to)
            .map_err(mcp_invalid_request)?
            .id;
        match self.orchestration.remove_task_edge(from, to, args.kind) {
            Ok(()) => json_result(
                json!({"removed": {"from": args.from, "to": args.to, "kind": args.kind}}),
            ),
            Err(error) => error_result(error),
        }
    }

    async fn task_start_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: TaskStartArgs = parse_tool_args(args)?;
        match self.task_start(args).await {
            Ok(report) => json_result(to_json(&report)?),
            Err(error) => error_result(error),
        }
    }

    async fn orchestration_status_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: OrchestrationStatusArgs = parse_tool_args(args)?;
        match self.orchestration_status_impl(&args).await {
            Ok(value) => json_result(value),
            Err(error) => error_result(error),
        }
    }

    async fn orchestration_status_impl(
        &self,
        args: &OrchestrationStatusArgs,
    ) -> Result<Value, String> {
        let mut status = self
            .orchestration
            .status()
            .map_err(|error| error.to_string())?;
        if args.project_id.is_some() || args.plan_id.is_some() {
            let state = self
                .orchestration
                .snapshot()
                .map_err(|error| error.to_string())?;
            let project_id = args
                .project_id
                .as_deref()
                .map(|selector| resolve_project_id_or_slug(&state, selector))
                .transpose()?;
            let plan_id = args
                .plan_id
                .as_deref()
                .map(|selector| resolve_plan_id_or_slug(&state, selector))
                .transpose()?;
            status = filter_orchestration_status(
                status,
                project_id.as_ref(),
                plan_id.as_ref(),
                args.include_tasks.unwrap_or(true),
            );
        } else if !args.include_tasks.unwrap_or(true) {
            status.tasks.clear();
        }

        // Decorate execution summaries with live runtime facts, one
        // agent_list per observed endpoint.
        let endpoints = status
            .executions
            .iter()
            .map(|execution| execution.endpoint_id.clone())
            .collect::<HashSet<_>>();
        let (live_runtime_keys, warnings, _observed, _agents) =
            collect_live_runtime_keys(&self.herdr, &endpoints).await;
        for execution in &mut status.executions {
            let runtime_key = format!(
                "{}:{}",
                execution.endpoint_id,
                execution
                    .pane_id
                    .clone()
                    .or_else(|| execution.agent_name.clone())
                    .unwrap_or_else(|| execution.execution_id.clone())
            );
            execution.runtime_state = Some(if live_runtime_keys.contains(&runtime_key) {
                "live".into()
            } else {
                "missing".into()
            });
        }
        if !warnings.is_empty() {
            status.warnings.extend(warnings);
        }

        let mut value = to_json(&status).map_err(|error| error.to_string())?;
        if let Some(object) = value.as_object_mut() {
            object.insert("counts".into(), summarize_orchestration_counts(&status));
        }
        Ok(value)
    }

    async fn orchestration_report_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: OrchestrationReportArgs = parse_tool_args(args)?;
        match self.orchestration_report(args).await {
            Ok(report) => json_result(to_json(&report)?),
            Err(error) => error_result(error),
        }
    }

    async fn orchestration_next_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: OrchestrationNextArgs = parse_tool_args(args)?;
        match self.orchestration_next(args).await {
            Ok(report) => json_result(to_json(&report)?),
            Err(error) => error_result(error),
        }
    }

    async fn orchestration_prune_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: OrchestrationPruneArgs = parse_tool_args(args)?;
        let older_than_days = args
            .older_than_days
            .unwrap_or(DEFAULT_PRUNE_OLDER_THAN_DAYS);
        if older_than_days == 0 {
            return Err(mcp_invalid_request(
                "older_than_days must be at least 1; use execution_stop for live cleanup".into(),
            ));
        }
        let state = store_result(self.orchestration.snapshot())?;
        let endpoints = state
            .tasks
            .values()
            .filter_map(|task| task.execution.as_ref().map(|e| e.endpoint_id.clone()))
            .chain(
                state
                    .retained_executions
                    .values()
                    .map(|retained| retained.execution.endpoint_id.clone()),
            )
            .collect::<HashSet<_>>();
        let (mut live_runtime_keys, mut warnings, observed, _agents) =
            collect_live_runtime_keys(&self.herdr, &endpoints).await;
        execution_cleanup::prepare_retained_layout_prune(
            &self.herdr,
            &state,
            args.dry_run,
            args.include_stale_execution_records,
            Some(older_than_days),
            &mut live_runtime_keys,
            &mut warnings,
        )
        .await;
        let observed_endpoints = observed.iter().cloned().collect::<HashSet<_>>();
        match self.orchestration.prune_stale_execution_records(
            &live_runtime_keys,
            &observed_endpoints,
            args.dry_run,
            args.include_stale_execution_records,
            args.include_finished_plans,
            Some(older_than_days),
        ) {
            Ok(report) => json_result(json!({
                "dry_run": args.dry_run,
                "older_than_days": older_than_days,
                "endpoints_observed": observed,
                "endpoint_warnings": warnings,
                "report": report,
            })),
            Err(error) => error_result(error),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Execution lifecycle tools
// ═══════════════════════════════════════════════════════════════════════════════

impl HerdrMcpServer {
    fn launch_context(&self) -> LaunchContext {
        LaunchContext {
            herdr: self.herdr.clone(),
            launch_profiles: self.launch_profiles.clone(),
            orchestration: self.orchestration.clone(),
        }
    }

    async fn start_coding_session_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: StartCodingSessionArgs = parse_tool_args(args)?;
        if let Err(error) = validate_prompt_text_value("task_id", &args.task_id) {
            return Err(mcp_invalid_request(error));
        }
        if let Err(error) = validate_prompt_text_value("endpoint_id", &args.endpoint_id) {
            return Err(mcp_invalid_request(error));
        }
        if let Err(error) = validate_prompt_text_value("workspace_path", &args.workspace_path) {
            return Err(mcp_invalid_request(error));
        }
        let ctx = self.launch_context();
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| McpError::internal_error(error, None))?;
        let task =
            resolve_task_by_id_or_slug(&state, &args.task_id).map_err(mcp_invalid_request)?;
        if let Some(existing) = task.execution.as_ref() {
            if !existing.phase.allows_replacement() {
                return error_result(format!(
                    "task '{}' already has execution '{}' (phase {}); only an exited, stopped, or failed execution can be replaced; stop the current one first with execution_stop",
                    task.id.0, existing.execution_id, existing.phase.as_str()
                ));
            }
        }
        // Manual launches pass the same dependency/validator gates as the
        // scheduler; readiness is judged before any runtime allocation.
        let blockers = scheduler_dependency_blockers(&state, &task);
        if !blockers.is_empty() {
            return error_result(format!(
                "task '{}' is not ready to launch; dependencies not ready: {}",
                task.id.0,
                format_blocking_dependencies(&blockers)
            ));
        }
        let request = ResolvedLaunchRequest {
            endpoint_id: args.endpoint_id.clone(),
            launch_profile_id: args.launch_profile_id.clone(),
            workspace_path: args.workspace_path.clone(),
            bypass_permissions: args.bypass_permissions,
            role: args.role.clone().unwrap_or_default(),
            kind: args.kind.clone().unwrap_or_default(),
            skills: args.skills.clone().unwrap_or_default(),
            template: args.template,
        };
        let outcome = match launch_task_execution(
            &ctx,
            &task,
            task.run_spec.as_ref(),
            Some(&request),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => return error_result(error),
        };

        // Render and deliver the task prompt from durable state.
        let template = args
            .template
            .or_else(|| {
                task.run_spec
                    .as_ref()
                    .and_then(|run_spec| scheduler_parse_template(&run_spec.template).ok())
            })
            .unwrap_or(CodingTaskSendTemplate::Task);
        let instruction = task
            .run_spec
            .as_ref()
            .map(|run_spec| run_spec.instruction.clone());
        let prompt_args = CodingTaskSendArgs {
            execution_id: outcome.execution.execution_id.clone(),
            task_id_or_slug: task.id.0.clone(),
            prompt: instruction,
            template: Some(template),
            include_dependencies: None,
            include_gates: None,
            include_scope: None,
            context_task_ids: None,
            extra_context: None,
        };
        let prompt_state = self
            .orchestration
            .snapshot()
            .map_err(|error| McpError::internal_error(error, None))?;
        let prompt = match build_coding_task_prompt(&prompt_state, &prompt_args) {
            Ok(prompt) => prompt,
            Err(error) => {
                return error_result(format!(
                    "execution '{}' launched but prompt rendering failed: {error}",
                    outcome.execution.execution_id
                ));
            }
        };
        let delivery = match deliver_task_prompt(&ctx, &outcome.execution, &prompt).await {
            Ok(delivery) => delivery,
            Err(error) => {
                let _ = self.orchestration.mark_execution_blocked(
                    task.id.clone(),
                    &outcome.execution.execution_id,
                    "started execution but could not deliver the task prompt".into(),
                    Some(vec![error.clone()]),
                    false,
                );
                return error_result(format!(
                    "execution '{}' launched but prompt delivery failed: {error}",
                    outcome.execution.execution_id
                ));
            }
        };
        if let Err(error) = self.orchestration.mark_execution_running(
            task.id.clone(),
            &outcome.execution.execution_id,
            format!(
                "started execution '{}' on endpoint '{}'",
                outcome.execution.execution_id, outcome.execution.endpoint_id
            ),
        ) {
            let _ = execution_cleanup::close_finished_worker(&ctx, &task.id).await;
            return error_result(error);
        }

        json_result(json!({
            "task_id": task.id.0,
            "execution": outcome.execution,
            "delivery": {
                "delivered": delivery.delivered,
                "status": delivery.status,
            },
            "warnings": outcome.warnings,
        }))
    }

    async fn execution_adopt_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: ExecutionAdoptArgs = parse_tool_args(args)?;
        endpoint_migration::require_resolved_endpoint(&args.endpoint_id)
            .map_err(mcp_invalid_request)?;
        if args.pane_id.is_none() && args.agent_name.is_none() {
            return Err(mcp_invalid_request(
                "execution_adopt requires pane_id or agent_name (at least one)".into(),
            ));
        }
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| McpError::internal_error(error, None))?;
        let task =
            resolve_task_by_id_or_slug(&state, &args.task_id).map_err(mcp_invalid_request)?;
        let endpoint = match self
            .herdr
            .endpoint(
                self.herdr.target_for_endpoint(&args.endpoint_id),
                taskr_herdr::EndpointAction::Observe,
            )
            .await
        {
            Ok(endpoint) if endpoint.ready => endpoint,
            Ok(_) => return error_result("endpoint is not ready; adoption never starts it".into()),
            Err(error) => return error_result(error.to_string()),
        };
        let target = endpoint.target;

        // Observe the candidate target before recording anything.
        let agent_info = if let Some(agent_name) = args.agent_name.as_deref() {
            match self.herdr.agent_get(&target, agent_name).await {
                Ok(info) => Some(info),
                Err(error) => {
                    return error_result(format!(
                        "cannot adopt agent '{agent_name}' on endpoint '{}': {error}",
                        args.endpoint_id
                    ));
                }
            }
        } else {
            None
        };
        let pane_value = if let Some(pane_id) = args.pane_id.as_deref() {
            match self.herdr.pane_get(&target, pane_id).await {
                Ok(value) => Some(value),
                Err(error) => {
                    return error_result(format!(
                        "cannot adopt pane '{pane_id}' on endpoint '{}': {error}",
                        args.endpoint_id
                    ));
                }
            }
        } else if let Some(pane_id) = agent_info.as_ref().and_then(|info| info.pane_id.clone()) {
            self.herdr.pane_get(&target, &pane_id).await.ok()
        } else {
            None
        };

        let observed_pane = agent_info
            .as_ref()
            .and_then(|info| info.pane_id.clone())
            .or_else(|| args.pane_id.clone())
            .or_else(|| {
                pane_value
                    .as_ref()
                    .and_then(|value| value.get("pane_id").and_then(Value::as_str))
                    .map(str::to_owned)
            });
        let observed_name = agent_info
            .as_ref()
            .and_then(|info| info.name.clone())
            .or_else(|| args.agent_name.clone());
        let observed_kind = args
            .kind
            .clone()
            .filter(|kind| !kind.trim().is_empty())
            .or_else(|| agent_info.as_ref().and_then(|info| info.kind.clone()))
            .filter(|kind| !kind.trim().is_empty())
            .ok_or_else(|| {
                mcp_invalid_request(
                    "execution_adopt could not determine the agent kind; pass kind explicitly"
                        .into(),
                )
            })?;
        let observed_session = agent_info
            .as_ref()
            .and_then(|info| info.agent_session.clone());
        let workspace_path = agent_info
            .as_ref()
            .and_then(|info| info.cwd.clone())
            .or_else(|| {
                pane_value.as_ref().and_then(|value| {
                    ["cwd", "pane_current_path", "current_path", "path"]
                        .iter()
                        .find_map(|key| value.get(*key).and_then(Value::as_str))
                        .map(str::to_owned)
                })
            })
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| {
                mcp_invalid_request(
                    "execution_adopt could not observe a workspace path on the target; adopt from a pane or agent with a working directory".into(),
                )
            })?;

        // Reject conflicting active ownership unless it is this same target.
        let resource = observed_pane
            .clone()
            .or(observed_name.clone())
            .ok_or_else(|| {
                mcp_invalid_request(
                    "execution_adopt could not determine a pane or agent binding".into(),
                )
            })?;
        let new_runtime_key = taskr_core::orchestration::runtime_key_for(
            &args.endpoint_id,
            endpoint.generation.as_deref(),
            &resource,
        );
        if let Some(existing) = task.execution.as_ref() {
            if existing.phase.is_active() && existing.runtime_key() != new_runtime_key {
                return error_result(format!(
                    "task '{}' already has active execution '{}' bound to '{}' (phase {}); conflicts with the adoption target '{}'",
                    task.id.0,
                    existing.execution_id,
                    existing.runtime_key(),
                    existing.phase.as_str(),
                    new_runtime_key
                ));
            }
        }

        let execution_id = match task.execution.as_ref() {
            // Re-adopting the same target keeps the existing identity.
            Some(existing)
                if existing.runtime_key() == new_runtime_key
                    && existing.launch_profile_id == ADOPTED_LAUNCH_PROFILE_ID =>
            {
                existing.execution_id.clone()
            }
            _ => new_execution_id(),
        };
        let execution = TaskExecution {
            native_session_name: None,
            group: taskr_core::orchestration::ExecutionGroup::Work,
            inspection: false,
            resumed_from: None,
            pane_closed: false,
            report: None,
            execution_id,
            endpoint_id: args.endpoint_id.clone(),
            runtime_generation: endpoint.generation,
            launch_profile_id: ADOPTED_LAUNCH_PROFILE_ID.into(),
            launch_args: Vec::new(),
            launch_env: BTreeMap::new(),
            bypass_permissions: false,
            workspace_path: workspace_path.clone(),
            role: args.role.clone().unwrap_or_default(),
            kind: observed_kind.clone(),
            skills: Vec::new(),
            workspace_id: agent_info
                .as_ref()
                .and_then(|info| info.workspace_id.clone()),
            tab_id: agent_info.as_ref().and_then(|info| info.tab_id.clone()),
            pane_id: observed_pane.clone(),
            terminal_id: agent_info.as_ref().and_then(|info| {
                info.raw
                    .get("terminal_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
            agent_name: observed_name.clone(),
            agent_kind: Some(observed_kind.clone()),
            agent_session: observed_session.clone(),
            phase: ExecutionPhase::Live,
            recovery: ExecutionRecovery::Reconciled,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_seen_ms: 0,
        };
        let recorded = match self
            .orchestration
            .record_execution(task.id.clone(), execution)
        {
            Ok(recorded) => recorded,
            Err(error) => return error_result(error),
        };
        if let Err(error) = self.orchestration.mark_execution_running(
            task.id.clone(),
            &recorded.execution_id,
            format!(
                "adopted live worker '{}' on endpoint '{}'",
                recorded
                    .agent_name
                    .clone()
                    .or(recorded.pane_id.clone())
                    .unwrap_or_else(|| recorded.runtime_key()),
                recorded.endpoint_id
            ),
        ) {
            return error_result(error);
        }

        json_result(json!({
            "task_id": task.id.0,
            "execution": recorded,
            "observed": {
                "agent": agent_info.map(|info| json!({
                    "name": info.name,
                    "kind": info.kind,
                    "status": info.status,
                    "cwd": info.cwd,
                    "agent_session": info.agent_session,
                })),
                "pane": pane_value,
            },
        }))
    }

    async fn execution_resume_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            execution_id: String,
        }
        let args: Args = parse_tool_args(args)?;
        let ctx = LaunchContext {
            herdr: self.herdr.clone(),
            launch_profiles: self.launch_profiles.clone(),
            orchestration: self.orchestration.clone(),
        };
        match execution_resume::resume(&ctx, &args.execution_id).await {
            Ok(outcome) => json_result(
                json!({"execution": outcome.execution, "warnings": outcome.warnings,
                "prompt_submitted": false, "task_status_changed": false}),
            ),
            Err(error) => error_result(error),
        }
    }

    async fn execution_stop_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: ExecutionStopArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if args.dry_run {
            return json_result(json!({
                "dry_run": true,
                "execution_id": resolved.execution.execution_id,
                "endpoint_id": resolved.execution.endpoint_id,
                "pane_id": resolved.execution.pane_id,
                "agent_name": resolved.execution.agent_name,
                "phase": resolved.execution.phase,
                "task_id": resolved.task_id.0,
                "task_status": resolved.task.status,
                "action": "would-exit-agent-and-close-pane",
            }));
        }
        let ctx = LaunchContext {
            herdr: self.herdr.clone(),
            launch_profiles: self.launch_profiles.clone(),
            orchestration: self.orchestration.clone(),
        };
        let closed =
            match execution_cleanup::close_worker(&ctx, &resolved.task_id, &resolved.execution)
                .await
            {
                Ok(closed) => closed,
                Err(error) => return error_result(error),
            };
        let blocked = self
            .orchestration
            .mark_execution_blocked(
                resolved.task_id,
                &closed.execution.execution_id,
                format!(
                    "operator stopped execution '{}': {}",
                    closed.execution.execution_id, closed.note
                ),
                None,
                true,
            )
            .unwrap_or(false);
        json_result(json!({
            "dry_run": false, "execution": closed.execution,
            "closed_pane": closed.pane_id, "note": closed.note,
            "task_note": blocked.then_some("task moved to Blocked"),
        }))
    }

    async fn list_executions_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: ListExecutionsArgs = parse_tool_args(args)?;
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| McpError::internal_error(error, None))?;
        let project_id =
            resolve_project_id_or_slug(&state, &args.project_id).map_err(mcp_invalid_request)?;

        let mut rows = Vec::new();
        for task in state.tasks.values() {
            if task_project_id(&state, task).as_ref() != Some(&project_id) {
                continue;
            }
            let Some(execution) = task.execution.as_ref() else {
                continue;
            };
            if !args.include_completed
                && task.status.is_finished()
                && !(execution.inspection && execution.phase.is_active())
            {
                continue;
            }
            rows.push((task, execution));
        }
        rows.sort_by(|left, right| left.1.execution_id.cmp(&right.1.execution_id));

        let endpoints = rows
            .iter()
            .map(|(_, execution)| execution.endpoint_id.clone())
            .collect::<HashSet<_>>();
        let (live_runtime_keys, warnings, _observed, _agents) =
            collect_live_runtime_keys(&self.herdr, &endpoints).await;
        let startup_jobs = startup_registry_snapshot();
        let mut executions = Vec::new();
        for (task, execution) in rows {
            let runtime_key = execution.runtime_key();
            let runtime_state = if live_runtime_keys.contains(&runtime_key) {
                "live"
            } else if warnings.iter().any(|warning| {
                warning.starts_with(&format!("endpoint '{}'", execution.endpoint_id))
            }) {
                "unknown"
            } else {
                "missing"
            };
            let mut row = json!({
                "task_id": task.id.0,
                "task_slug": task.slug,
                "task_status": task.status,
                "execution_id": execution.execution_id,
                "endpoint_id": execution.endpoint_id,
                "runtime_generation": execution.runtime_generation,
                "launch_profile_id": execution.launch_profile_id,
                "workspace_path": execution.workspace_path,
                "role": execution.role,
                "kind": execution.kind,
                "phase": execution.phase,
                "recovery": execution.recovery,
                "pane_id": execution.pane_id,
                "terminal_id": execution.terminal_id,
                "agent_name": execution.agent_name,
                "agent_kind": execution.agent_kind,
                "agent_session": execution.agent_session,
                "native_session_name": execution.native_session_name,
                "workspace_id": execution.workspace_id,
                "tab_id": execution.tab_id,
                "group": execution.group,
                "inspection": execution.inspection,
                "resumed_from": execution.resumed_from,
                "pane_closed": execution.pane_closed,
                "report": execution.report,
                "last_seen_ms": execution.last_seen_ms,
                "runtime_state": runtime_state,
                "runtime_key": runtime_key,
            });
            if let Some(job) = startup_jobs
                .iter()
                .find(|job| job.execution_id == execution.execution_id)
            {
                if let Some(object) = row.as_object_mut() {
                    object.insert(
                        "startup_job".into(),
                        serde_json::to_value(job).unwrap_or(Value::Null),
                    );
                }
            }
            executions.push(row);
        }
        json_result(json!({
            "project_id": project_id.0,
            "executions": executions,
            "endpoint_warnings": warnings,
        }))
    }

    async fn list_endpoints_tool(&self, _args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let endpoints = match self.herdr.list_endpoints().await {
            Ok(endpoints) => endpoints,
            Err(error) => return error_result(error.to_string()),
        };
        let pending = endpoint_migration::pending_endpoint_migrations(&store_result(
            self.orchestration.snapshot(),
        )?);
        let mut rows = Vec::new();
        for endpoint in endpoints {
            let target = self.herdr.target_for_endpoint(&endpoint.endpoint_id);
            let probed = self.herdr.check_endpoint(&target).await;
            // Ok(false) is the endpoint reporting itself down; Err means the
            // probe could not run at all, so availability stays unknown.
            let (available, error) = match probed {
                Ok(true) => (Some(true), None),
                Ok(false) => (
                    Some(false),
                    Some("endpoint reports itself unavailable".to_owned()),
                ),
                Err(error) => (None, Some(error.to_string())),
            };
            rows.push(json!({
                "endpoint_id": endpoint.endpoint_id,
                "label": endpoint.label,
                "mode": endpoint.mode,
                "available": available,
                "error": error,
            }));
        }
        json_result(json!({
            "endpoints": rows,
            "pending_endpoint_migrations": pending,
        }))
    }

    async fn list_launch_profiles_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            endpoint_id: String,
        }
        let args: Args = parse_tool_args(args)?;
        endpoint_migration::require_resolved_endpoint(&args.endpoint_id)
            .map_err(mcp_invalid_request)?;
        if args.endpoint_id.trim().is_empty() {
            return Err(mcp_invalid_request("endpoint_id must not be empty".into()));
        }
        let choices = self
            .launch_profiles
            .list(&args.endpoint_id)
            .map_err(mcp_invalid_request)?;
        let profiles = choices.iter().map(|choice| json!({
            "id": choice.profile.id, "endpoint_id": choice.endpoint_id,
            "agent_kind": choice.profile.agent_kind, "native_profile": choice.native_profile,
            "description": choice.profile.description, "args": choice.profile.args,
            "env_keys": choice.profile.env.keys().collect::<Vec<_>>(),
            "deployment": choice.deployment, "enabled": true,
            "readiness": "prepared", "availability": "not_probed"
        })).collect::<Vec<_>>();
        json_result(json!({"endpoint_id": args.endpoint_id, "profiles": profiles}))
    }

    async fn admin_environment_discover_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        self.policy
            .ensure_admin_tools_enabled("admin_environment_discover")?;
        let args: taskr_environment::DiscoveryRequest = parse_tool_args(args)?;
        match self.launch_profiles.discover(args).await {
            Ok(result) => json_result(to_json(&result)?),
            Err(error) => error_result(error),
        }
    }

    async fn admin_environment_sync_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        self.policy
            .ensure_admin_tools_enabled("admin_environment_sync")?;
        let args: SyncRequest = parse_tool_args(args)?;
        endpoint_migration::require_resolved_endpoint(&args.endpoint_id)
            .map_err(mcp_invalid_request)?;
        match self.launch_profiles.sync(self.herdr.clone(), args).await {
            Ok(job) => json_result(to_json(&job)?),
            Err(error) => error_result(error),
        }
    }

    async fn admin_environment_sync_status_tool(
        &self,
        args: Option<&Value>,
        cancel: bool,
    ) -> Result<CallToolResult, McpError> {
        self.policy.ensure_admin_tools_enabled(if cancel {
            "admin_environment_sync_cancel"
        } else {
            "admin_environment_sync_status"
        })?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            sync_job_id: String,
        }
        let args: Args = parse_tool_args(args)?;
        let result = if cancel {
            self.launch_profiles.cancel(&args.sync_job_id)
        } else {
            self.launch_profiles.status(&args.sync_job_id)
        };
        match result {
            Ok(job) => json_result(to_json(&job)?),
            Err(error) => error_result(error),
        }
    }

    async fn admin_list_endpoint_agents_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        self.policy
            .ensure_admin_tools_enabled("admin_list_endpoint_agents")?;
        let args: AdminEndpointAgentsArgs = parse_tool_args(args)?;
        let endpoint_id = args.endpoint_id.unwrap_or_else(|| "local".into());
        endpoint_migration::require_resolved_endpoint(&endpoint_id).map_err(mcp_invalid_request)?;
        let target = self.herdr.target_for_endpoint(&endpoint_id);
        match self.herdr.agent_list(&target).await {
            Ok(agents) => {
                let rows = agents
                    .iter()
                    .map(|agent| {
                        json!({
                            "name": agent.name,
                            "kind": agent.kind,
                            "status": agent.status,
                            "pane_id": agent.pane_id,
                            "workspace_id": agent.workspace_id,
                            "tab_id": agent.tab_id,
                            "agent_session": agent.agent_session,
                            "cwd": agent.cwd,
                            "foreground_cwd": agent.foreground_cwd,
                        })
                    })
                    .collect::<Vec<_>>();
                json_result(json!({"endpoint_id": endpoint_id, "agents": rows}))
            }
            Err(error) => structured_runtime_error(None, &error),
        }
    }

    /// Rewrite marked legacy references once. Resolved records and future
    /// explicit requests never pass through an alias table.
    async fn endpoint_migrate_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        self.policy.ensure_admin_tools_enabled("endpoint_migrate")?;
        let args: EndpointMigrateArgs = parse_tool_args(args)?;
        for (field, value) in [
            ("legacy_node_id", args.legacy_node_id.as_str()),
            ("endpoint_id", args.endpoint_id.as_str()),
        ] {
            validate_prompt_text_value(field, value).map_err(mcp_invalid_request)?;
        }
        if args.legacy_node_id.trim() == LOCAL_ENDPOINT_ID {
            return Err(mcp_invalid_request("'local' is already resolved".into()));
        }
        let endpoint_id = args.endpoint_id.trim();
        endpoint_migration::require_resolved_endpoint(endpoint_id).map_err(mcp_invalid_request)?;
        let catalog = self.herdr.list_endpoints().await.map_err(|error| {
            mcp_invalid_request(format!("cannot read the Herdr machine catalog: {error}"))
        })?;
        if !catalog
            .iter()
            .any(|endpoint| endpoint.endpoint_id == endpoint_id)
        {
            return error_result(format!(
                "endpoint '{endpoint_id}' is not in the Herdr catalog; select an ID from list_endpoints"
            ));
        }
        match self
            .orchestration
            .migrate_endpoint(&args.legacy_node_id, endpoint_id)
        {
            Ok(report) => json_result(to_json(&report)?),
            Err(error) => error_result(error),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Interaction tools
// ═══════════════════════════════════════════════════════════════════════════════

const MAX_READ_LINES: u32 = 10_000;
const DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 120.0;
const MIN_POLL_SECONDS: f64 = 0.25;
const MIN_STABILITY_SECONDS: f64 = 0.5;

impl HerdrMcpServer {
    fn clamp_read_lines(&self, lines: Option<u32>) -> Option<u32> {
        lines.map(|lines| lines.min(MAX_READ_LINES))
    }

    async fn coding_send_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: CodingSendArgs = parse_tool_args(args)?;
        if let Err(error) = validate_coding_prompt(&args.prompt) {
            return Err(mcp_invalid_request(error));
        }
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        let target = self.herdr_target(&resolved.execution);
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let timeout = if args.wait_until_idle {
            Some(Duration::from_secs_f64(
                self.policy
                    .clamp_timeout(DEFAULT_WAIT_TIMEOUT_SECONDS)
                    .unwrap_or(120.0),
            ))
        } else {
            None
        };
        let request = PromptRequest {
            target: agent_ref,
            text: args.prompt,
            wait: args.wait_until_idle,
            until: if args.wait_until_idle {
                coding_ready_states()
            } else {
                Vec::new()
            },
            timeout,
        };
        match self.herdr.agent_prompt(&target, &request).await {
            Ok(outcome) => {
                let _ = self.touch_execution_last_seen(&resolved.task_id);
                json_result(json!({
                    "execution_id": resolved.execution.execution_id,
                    "delivered": outcome.delivered,
                    "status": outcome.status,
                }))
            }
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    fn touch_execution_last_seen(&self, task_id: &TaskId) -> Result<(), String> {
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| error.to_string())?;
        let Some(task) = state.tasks.get(task_id) else {
            return Ok(());
        };
        let Some(execution) = task.execution.clone() else {
            return Ok(());
        };
        self.orchestration
            .record_execution(task_id.clone(), execution)
            .map(|_| ())
    }

    async fn coding_task_send_tool(
        &self,
        args: Option<&Value>,
    ) -> Result<CallToolResult, McpError> {
        let args: CodingTaskSendArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        if let Some(prompt) = args.prompt.as_deref() {
            if let Err(error) = validate_coding_prompt(prompt) {
                return Err(mcp_invalid_request(error));
            }
        }
        if let Some(extra) = args.extra_context.as_deref() {
            if let Err(error) = validate_prompt_text_value("extra_context", extra) {
                return Err(mcp_invalid_request(error));
            }
        }
        let state = self
            .orchestration
            .snapshot()
            .map_err(|error| McpError::internal_error(error, None))?;
        let prompt = build_coding_task_prompt(&state, &args)
            .map_err(|error| McpError::internal_error(error, None))?;
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        let target = self.herdr_target(&resolved.execution);
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let request = PromptRequest {
            target: agent_ref,
            text: prompt,
            wait: false,
            until: Vec::new(),
            timeout: None,
        };
        match self.herdr.agent_prompt(&target, &request).await {
            Ok(outcome) => {
                let _ = self.touch_execution_last_seen(&resolved.task_id);
                // If the prompted task is the execution's own task and it has
                // not started yet, move it to Running.
                if let Ok(content_task) = resolve_task_by_id_or_slug(&state, &args.task_id_or_slug)
                {
                    if content_task.id == resolved.task_id
                        && matches!(
                            content_task.status,
                            TaskStatus::Backlog | TaskStatus::Planned
                        )
                    {
                        let _ = self.orchestration.mark_execution_running(
                            content_task.id.clone(),
                            &resolved.execution.execution_id,
                            format!(
                                "prompt delivered to execution '{}'",
                                resolved.execution.execution_id
                            ),
                        );
                    }
                }
                json_result(json!({
                    "execution_id": resolved.execution.execution_id,
                    "task_id_or_slug": args.task_id_or_slug,
                    "template": args.template.map(|t| t.as_str()),
                    "delivered": outcome.delivered,
                    "status": outcome.status,
                }))
            }
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    async fn coding_read_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: CodingReadArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        let target = self.herdr_target(&resolved.execution);
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        match self
            .herdr
            .agent_read(
                &target,
                &agent_ref,
                args.source.into(),
                self.clamp_read_lines(args.lines),
            )
            .await
        {
            Ok(result) => {
                let text = self.policy.limit_capture_output(result.text);
                json_result(json!({
                    "execution_id": resolved.execution.execution_id,
                    "source": args.source,
                    "text": text,
                    "truncated": result.truncated,
                }))
            }
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    async fn capture_output_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: CaptureOutputArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        let Some(pane_id) = resolved.execution.pane_id.clone() else {
            return error_result(format!(
                "execution '{}' has no pane binding; use coding_read for agent output",
                resolved.execution.execution_id
            ));
        };
        let target = self.herdr_target(&resolved.execution);
        match self
            .herdr
            .pane_read(&target, &pane_id, self.clamp_read_lines(args.lines))
            .await
        {
            Ok(result) => {
                let text = self.policy.limit_capture_output(result.text);
                json_result(json!({
                    "execution_id": resolved.execution.execution_id,
                    "pane_id": pane_id,
                    "text": text,
                    "truncated": result.truncated,
                }))
            }
            Err(error) => error_result(error.to_string()),
        }
    }

    async fn check_state_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: CheckStateArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        let execution = &resolved.execution;
        let mut runtime_observation = json!({
            "runtime_state": if execution.phase.is_active() { "unknown" } else { "not-expected" },
        });
        if execution.phase.is_active() {
            if let Some(agent_ref) = HerdrMcpServer::execution_agent_ref(execution) {
                let target = self.herdr_target(execution);
                match self.herdr.agent_get(&target, &agent_ref).await {
                    Ok(info) => {
                        runtime_observation = json!({
                            "runtime_state": "live",
                            "agent_status": info.status,
                            "foreground_cwd": info.foreground_cwd,
                            "observed_pane_id": info.pane_id,
                            "observed_agent_session": info.agent_session,
                        });
                    }
                    Err(error)
                        if error.category == taskr_herdr::RuntimeErrorCategory::MissingTarget =>
                    {
                        runtime_observation = json!({
                            "runtime_state": "missing",
                            "note": "no agent or pane with this binding is live on the endpoint",
                        });
                    }
                    Err(error) => {
                        runtime_observation = json!({
                            "runtime_state": "unknown",
                            "note": error.to_string(),
                        });
                    }
                }
            }
        }

        let mut value = json!({
            "execution_id": execution.execution_id,
            "endpoint_id": execution.endpoint_id,
            "phase": execution.phase,
            "recovery": execution.recovery,
            "task_id": resolved.task_id.0,
            "task_status": resolved.task.status,
            "agent_name": execution.agent_name,
            "agent_kind": execution.agent_kind,
            "agent_session": execution.agent_session,
            "pane_id": execution.pane_id,
            "workspace_path": execution.workspace_path,
            "launch_profile_id": execution.launch_profile_id,
            "last_seen_ms": execution.last_seen_ms,
        });
        if let Some(object) = value.as_object_mut() {
            if let Some(extra) = runtime_observation.as_object() {
                for (key, field) in extra {
                    object.insert(key.clone(), field.clone());
                }
            }
            if let Some(job) = startup_registry_snapshot()
                .into_iter()
                .find(|job| job.execution_id == execution.execution_id)
            {
                object.insert(
                    "startup_job".into(),
                    serde_json::to_value(job).unwrap_or(Value::Null),
                );
            }
        }
        json_result(value)
    }

    async fn send_input_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: SendInputArgs = parse_tool_args(args)?;
        if let Err(error) = validate_prompt_text_value("text", &args.text) {
            return Err(mcp_invalid_request(error));
        }
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let target = self.herdr_target(&resolved.execution);
        let mut keys = vec![args.text.clone()];
        if args.enter {
            keys.push("Enter".into());
        }
        match self.herdr.agent_send_keys(&target, &agent_ref, &keys).await {
            Ok(()) => json_result(json!({
                "execution_id": resolved.execution.execution_id,
                "sent_text": args.text,
                "enter": args.enter,
            })),
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    async fn send_key_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: SendKeyArgs = parse_tool_args(args)?;
        if args.key.trim().is_empty() {
            return Err(mcp_invalid_request("key must not be empty".into()));
        }
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let target = self.herdr_target(&resolved.execution);
        match self
            .herdr
            .agent_send_keys(&target, &agent_ref, std::slice::from_ref(&args.key))
            .await
        {
            Ok(()) => json_result(json!({
                "execution_id": resolved.execution.execution_id,
                "sent_key": args.key,
            })),
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    async fn coding_action_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: CodingActionArgs = parse_tool_args(args)?;
        let keys: &[&str] = match args.action.trim().to_lowercase().as_str() {
            "approve" | "yes" => &["y"],
            "reject" | "no" => &["n"],
            "cancel" => &["C-c"],
            "escape" | "dismiss" => &["Escape"],
            "continue" | "enter" => &["Enter"],
            other => {
                return Err(mcp_invalid_request(format!(
                    "unsupported coding action '{other}'; supported: approve, reject, cancel, escape, dismiss, continue"
                )));
            }
        };
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let keys = keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
        if let Err(error) = self.verify_execution_occupant(&resolved.execution).await {
            return error_result(error);
        }
        let target = self.herdr_target(&resolved.execution);
        match self.herdr.agent_send_keys(&target, &agent_ref, &keys).await {
            Ok(()) => json_result(json!({
                "execution_id": resolved.execution.execution_id,
                "action": args.action,
                "sent_keys": keys,
            })),
            Err(error) => {
                structured_runtime_error(Some(resolved.execution.execution_id.as_str()), &error)
            }
        }
    }

    async fn exec_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: ExecArgs = parse_tool_args(args)?;
        if args.command.is_empty() {
            return Err(mcp_invalid_request("command must not be empty".into()));
        }
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        let Some(pane_id) = resolved.execution.pane_id.clone() else {
            return error_result(format!(
                "execution '{}' has no pane binding; exec runs only in task-owned panes",
                resolved.execution.execution_id
            ));
        };
        let target = self.herdr_target(&resolved.execution);

        // Exec requires a positive observation that the pane holds no coding
        // agent: an idle-status agent still owns the pane, and a failed
        // lookup proves nothing. Only a verified agent-free pane may
        // receive shell input.
        if let Some(agent_ref) = HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            let info = match self.herdr.agent_get(&target, &agent_ref).await {
                Ok(info) => info,
                Err(error) => {
                    return error_result(format!(
                        "execution '{}' agent lookup failed ({error}); exec requires a positive observation that the pane holds no coding agent",
                        resolved.execution.execution_id
                    ));
                }
            };
            if info.kind.is_some() || info.agent_session.is_some() {
                return error_result(format!(
                    "execution '{}' pane '{}' is occupied by agent '{}'; exec never sends shell text to a coding agent",
                    resolved.execution.execution_id,
                    pane_id,
                    info.kind.as_deref().unwrap_or("unknown")
                ));
            }
            // Positive shell evidence only: agents, editors, and any
            // unrecognized foreground program are all rejected.
            if info.status.as_deref() != Some("shelling") {
                return error_result(format!(
                    "execution '{}' pane '{}' foreground is {:?}, not a verified shell; exec refuses unknown foreground states",
                    resolved.execution.execution_id,
                    pane_id,
                    info.status.as_deref().unwrap_or("<unknown>")
                ));
            }
        }

        let timeout = self
            .policy
            .clamp_timeout(args.timeout_seconds.unwrap_or(30.0))
            .map_err(mcp_invalid_request)?;
        if let Err(error) = self.herdr.pane_run(&target, &pane_id, &args.command).await {
            return structured_runtime_error(
                Some(resolved.execution.execution_id.as_str()),
                &error,
            );
        }

        // Read the pane tail until output stabilizes or the timeout passes.
        let started = Instant::now();
        let poll = Duration::from_millis(400);
        let mut previous = String::new();
        let mut stable_polls = 0u32;
        let mut text = previous.clone();
        let deadline = Duration::from_secs_f64(timeout);
        while started.elapsed() < deadline {
            tokio::time::sleep(poll).await;
            match self
                .herdr
                .pane_read(&target, &pane_id, self.clamp_read_lines(args.lines))
                .await
            {
                Ok(result) => {
                    text = result.text;
                    if text == previous {
                        stable_polls += 1;
                        if stable_polls >= 3 {
                            break;
                        }
                    } else {
                        stable_polls = 0;
                        previous = text.clone();
                    }
                }
                Err(error) => {
                    return structured_runtime_error(
                        Some(resolved.execution.execution_id.as_str()),
                        &error,
                    );
                }
            }
        }
        let text = self.policy.limit_capture_output(text);
        json_result(json!({
            "execution_id": resolved.execution.execution_id,
            "pane_id": pane_id,
            "command": args.command,
            "timed_out": started.elapsed() >= deadline,
            "text": text,
        }))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Runtime wait tools
// ═══════════════════════════════════════════════════════════════════════════════

impl HerdrMcpServer {
    async fn wait_start_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: WaitStartArgs = parse_tool_args(args)?;
        let resolved = match self.resolve_execution(&args.execution_id) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
        if let Err(error) = HerdrMcpServer::require_live(&resolved.execution) {
            return error_result(error);
        }
        let agent_ref = match HerdrMcpServer::execution_agent_ref(&resolved.execution) {
            Some(agent_ref) => agent_ref,
            None => {
                return error_result(format!(
                    "execution '{}' has no agent or pane binding",
                    resolved.execution.execution_id
                ));
            }
        };
        let sentinel = match args.kind {
            RuntimeWaitKind::Sentinel => {
                let sentinel = args
                    .sentinel
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        mcp_invalid_request(
                            "sentinel is required when wait kind is 'sentinel'".into(),
                        )
                    })?;
                Some(sentinel.to_owned())
            }
            _ => None,
        };
        let timeout = self
            .policy
            .clamp_timeout(args.timeout_seconds.unwrap_or(DEFAULT_WAIT_TIMEOUT_SECONDS))
            .map_err(mcp_invalid_request)?;
        let poll = args.poll_seconds.unwrap_or(1.0).max(MIN_POLL_SECONDS);
        let stability = args
            .stability_seconds
            .unwrap_or(2.0)
            .max(MIN_STABILITY_SECONDS);

        // Drop finished jobs so the registry cannot grow without bound.
        {
            let mut guard = self
                .wait_jobs
                .lock()
                .map_err(|_| mcp_invalid_request("wait job registry lock poisoned".into()))?;
            guard.retain(|_, entry| {
                entry
                    .status
                    .lock()
                    .map(|status| status.state == "running")
                    .unwrap_or(false)
            });
        }

        let wait_id = runtime_wait_id();
        let snapshot = RuntimeWaitSnapshot {
            wait_id: wait_id.clone(),
            execution_id: resolved.execution.execution_id.clone(),
            endpoint_id: resolved.execution.endpoint_id.clone(),
            kind: args.kind,
            state: "running".into(),
            result: None,
            error: None,
            started_at_ms: now_ms(),
            completed_at_ms: None,
        };
        let status = Arc::new(Mutex::new(snapshot));
        let herdr = self.herdr.clone();
        let target = self.herdr_target(&resolved.execution);
        let handle = tokio::spawn(run_runtime_wait(
            herdr,
            target,
            agent_ref,
            args.kind,
            sentinel,
            Duration::from_secs_f64(timeout),
            Duration::from_secs_f64(poll),
            Duration::from_secs_f64(stability),
            status.clone(),
        ));
        {
            let mut guard = self
                .wait_jobs
                .lock()
                .map_err(|_| mcp_invalid_request("wait job registry lock poisoned".into()))?;
            guard.insert(
                wait_id.clone(),
                WaitJobEntry {
                    status: status.clone(),
                    handle,
                },
            );
        }
        let value = to_json(
            &*status
                .lock()
                .map_err(|_| mcp_invalid_request("wait job status lock poisoned".into()))?,
        )?;
        json_result(value)
    }

    async fn wait_status_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: WaitStatusArgs = parse_tool_args(args)?;
        match wait_job_snapshot(&self.wait_jobs, &args.wait_id) {
            Ok(snapshot) => json_result(to_json(&snapshot)?),
            Err(error) => error_result(error),
        }
    }

    async fn wait_cancel_tool(&self, args: Option<&Value>) -> Result<CallToolResult, McpError> {
        let args: WaitCancelArgs = parse_tool_args(args)?;
        let entry = {
            let mut guard = self
                .wait_jobs
                .lock()
                .map_err(|_| mcp_invalid_request("wait job registry lock poisoned".into()))?;
            guard.remove(&args.wait_id)
        };
        let Some(entry) = entry else {
            return error_result(format!("wait job '{}' not found", args.wait_id));
        };
        if let Ok(mut status) = entry.status.lock() {
            if status.state == "running" {
                status.state = "canceled".into();
                status.completed_at_ms = Some(now_ms());
            }
        }
        entry.handle.abort();
        let snapshot = entry
            .status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| mcp_invalid_request("wait job status lock poisoned".into()))?;
        json_result(to_json(&snapshot)?)
    }
}

#[allow(clippy::too_many_arguments)] // one flat job description for a spawned poll loop
async fn run_runtime_wait(
    herdr: HerdrClient,
    target: EndpointTarget,
    agent_ref: String,
    kind: RuntimeWaitKind,
    sentinel: Option<String>,
    timeout: Duration,
    poll: Duration,
    stability: Duration,
    status: Arc<Mutex<RuntimeWaitSnapshot>>,
) {
    let started = Instant::now();
    let outcome = match kind {
        RuntimeWaitKind::CodingReady => {
            match herdr
                .agent_wait(&target, &agent_ref, &coding_ready_states(), timeout)
                .await
            {
                Ok(info) => Ok(json!({
                    "matched": info.status,
                    "agent_status": info.status,
                    "agent_name": info.name,
                    "pane_id": info.pane_id,
                })),
                Err(error) => Err(format!(
                    "agent did not become ready (idle or done): {error}"
                )),
            }
        }
        RuntimeWaitKind::Stable | RuntimeWaitKind::Sentinel => {
            let mut previous: Option<String> = None;
            let mut stable_since: Option<Instant> = None;
            loop {
                if started.elapsed() >= timeout {
                    break Err(match (&kind, sentinel.as_deref()) {
                        (RuntimeWaitKind::Sentinel, Some(sentinel)) => {
                            format!("sentinel '{sentinel}' not observed within timeout")
                        }
                        _ => "terminal output never stabilized within timeout".into(),
                    });
                }
                match herdr
                    .agent_read(&target, &agent_ref, ReadSource::Recent, Some(400))
                    .await
                {
                    Ok(result) => {
                        if let Some(sentinel) = sentinel.as_deref() {
                            if result.text.contains(sentinel) {
                                break Ok(json!({
                                    "matched": sentinel,
                                    "text": tail_lines(&result.text, 40),
                                }));
                            }
                        } else if previous.as_deref() == Some(result.text.as_str()) {
                            if stable_since.is_none_or(|since| since.elapsed() >= stability) {
                                break Ok(json!({
                                    "stable": true,
                                    "text": tail_lines(&result.text, 40),
                                }));
                            }
                        } else {
                            stable_since = Some(Instant::now());
                        }
                        previous = Some(result.text);
                    }
                    Err(error)
                        if error.category == taskr_herdr::RuntimeErrorCategory::MissingTarget =>
                    {
                        break Err(format!("agent '{agent_ref}' is no longer present: {error}"));
                    }
                    Err(_) => {
                        // Transient read failures are tolerated until timeout.
                    }
                }
                tokio::time::sleep(poll).await;
            }
        }
    };
    if let Ok(mut guard) = status.lock() {
        guard.completed_at_ms = Some(now_ms());
        match outcome {
            Ok(value) => {
                guard.state = "completed".into();
                guard.result = Some(value);
            }
            Err(error) => {
                guard.state = "failed".into();
                guard.error = Some(error);
            }
        }
    }
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Task resolution + prompt builders
// ═══════════════════════════════════════════════════════════════════════════════

fn resolve_task_by_id_or_slug(
    state: &OrchestrationState,
    task_id_or_slug: &str,
) -> Result<Task, String> {
    let selector = task_id_or_slug.trim();
    if selector.is_empty() {
        return Err("task_id_or_slug must not be empty".into());
    }
    if let Some(task) = state.tasks.get(&TaskId(selector.to_owned())) {
        return Ok(task.clone());
    }
    let mut matches = state
        .tasks
        .values()
        .filter(|task| task.slug == selector)
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    match matches.as_slice() {
        [task] => Ok((*task).clone()),
        [] => Err(format!("task '{selector}' not found")),
        tasks => {
            let matches = tasks
                .iter()
                .map(|task| {
                    format!(
                        "{} in plan {}",
                        task.id.0,
                        state
                            .plans
                            .get(&task.plan_id)
                            .map(|plan| plan.id.0.as_str())
                            .unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "task slug '{selector}' is ambiguous; matches: {matches}"
            ))
        }
    }
}

fn task_label(id: &str, slug: &str) -> String {
    if id == slug {
        id.to_owned()
    } else {
        format!("{slug} ({id})")
    }
}

fn task_status_name(status: TaskStatus) -> String {
    format!("{status:?}").to_ascii_lowercase()
}

fn edge_kind_name(kind: TaskEdgeKind) -> String {
    format!("{kind:?}")
        .chars()
        .enumerate()
        .fold(String::new(), |mut acc, (index, ch)| {
            if ch.is_ascii_uppercase() && index > 0 {
                acc.push('-');
            }
            acc.extend(ch.to_lowercase());
            acc
        })
}

fn template_from_run_spec(run_spec: Option<&TaskRunSpec>) -> CodingTaskSendTemplate {
    run_spec
        .and_then(|run_spec| scheduler_parse_template(&run_spec.template).ok())
        .unwrap_or(CodingTaskSendTemplate::Task)
}

fn string_list(title: &str, items: &[String]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let lines = items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{title}:\n{lines}"))
}

fn format_inline_list(items: &[String]) -> String {
    items.join(", ")
}

/// One durable execution rendered into prompt form.
fn format_task_execution(execution: &TaskExecution) -> String {
    let mut lines = vec![format!(
        "- endpoint={} launch_profile={} role={} kind={} skills={} workspace_path(runtime start directory)={} bypass_permissions={}",
        execution.endpoint_id,
        execution.launch_profile_id,
        if execution.role.is_empty() { "-" } else { &execution.role },
        execution.kind,
        if execution.skills.is_empty() {
            "-".to_owned()
        } else {
            format_inline_list(&execution.skills)
        },
        execution.workspace_path,
        execution.bypass_permissions,
    )];
    if let Some(pane_id) = execution.pane_id.as_deref() {
        lines.push(format!("  pane={pane_id}"));
    }
    if let Some(agent_name) = execution.agent_name.as_deref() {
        lines.push(format!("  agent={agent_name}"));
    }
    lines.push(format!(
        "  phase={} execution={}",
        execution.phase.as_str(),
        execution.execution_id
    ));
    lines.join("\n")
}

fn build_scope_section(scope: &TaskScope) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(section) = string_list("Include paths", &scope.include_paths) {
        lines.push(section);
    }
    if let Some(section) = string_list("Exclude paths", &scope.exclude_paths) {
        lines.push(section);
    }
    if let Some(notes) = scope
        .notes
        .as_deref()
        .filter(|notes| !notes.trim().is_empty())
    {
        lines.push(format!("Notes: {notes}"));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn dependencies_section(state: &OrchestrationState, task: &Task) -> Option<String> {
    let mut lines = Vec::new();
    for edge in state.task_edges.iter().filter(|edge| edge.from == task.id) {
        let target = state.tasks.get(&edge.to);
        let status = target
            .map(|task| task_status_name(task.status))
            .unwrap_or_else(|| "unknown".into());
        lines.push(format!(
            "- {} {} [{}]",
            edge_kind_name(edge.kind),
            task_label(
                &edge.to.0,
                target.map(|task| task.slug.as_str()).unwrap_or("?")
            ),
            status
        ));
    }
    if lines.is_empty() {
        None
    } else {
        Some(format!("Dependencies:\n{}", lines.join("\n")))
    }
}

fn blockers_section(task: &Task) -> Option<String> {
    string_list("Known blockers", &task.blockers)
}

/// Section toggles for a rendered task card, so a prompt can be trimmed to
/// exactly what the worker needs.
#[derive(Clone, Copy)]
struct TaskCardSections {
    dependencies: bool,
    gates: bool,
    scope: bool,
}

impl TaskCardSections {
    fn all() -> Self {
        Self {
            dependencies: true,
            gates: true,
            scope: true,
        }
    }

    fn from_args(args: &CodingTaskSendArgs) -> Self {
        Self {
            dependencies: args.include_dependencies.unwrap_or(true),
            gates: args.include_gates.unwrap_or(true),
            scope: args.include_scope.unwrap_or(true),
        }
    }
}

fn build_task_card(
    state: &OrchestrationState,
    task: &Task,
    card_sections: TaskCardSections,
) -> String {
    let plan = state.plans.get(&task.plan_id);
    let mut sections = Vec::new();
    sections.push(format!(
        "### Task {} [{}]",
        task_label(&task.id.0, &task.slug),
        task_status_name(task.status)
    ));
    sections.push(format!("Objective:\n{}", task.objective));
    if let Some(outcome) = task.outcome.as_deref().filter(|o| !o.trim().is_empty()) {
        sections.push(format!("Recorded outcome: {outcome}"));
    }
    if let Some(evidence) = string_list("Evidence", &task.evidence) {
        sections.push(evidence);
    }
    if let Some(plan) = plan {
        let mut lines = vec![format!(
            "Plan: {} [{}]",
            task_label(&plan.id.0, &plan.slug),
            task_status_name_plan(plan.status)
        )];
        if !plan.brief.trim().is_empty() {
            lines.push(format!("Brief: {}", plan.brief));
        }
        if let Some(instructions) = plan
            .instructions
            .as_deref()
            .filter(|instructions| !instructions.trim().is_empty())
        {
            lines.push(format!("Instructions: {instructions}"));
        }
        sections.push(lines.join("\n"));
    }
    if card_sections.scope {
        if let Some(scope) = build_scope_section(&task.scope) {
            sections.push(format!("Scope:\n{scope}"));
        }
    }
    if card_sections.gates && !task.gates.is_empty() {
        sections.push(format!(
            "Gates (all must pass before delivery):\n{}",
            task.gates
                .iter()
                .map(|gate| format!("- {gate}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if card_sections.dependencies {
        if let Some(dependencies) = dependencies_section(state, task) {
            sections.push(dependencies);
        }
    }
    if let Some(blockers) = blockers_section(task) {
        sections.push(blockers);
    }
    if let Some(run_spec) = task.run_spec.as_ref() {
        sections.push(format!(
            "Run intent:\n- endpoint={} launch_profile={} role={} kind={} skills={} workspace_path(runtime start directory)={} bypass_permissions={} template={}",
            run_spec.endpoint_id,
            run_spec.launch_profile_id,
            if run_spec.role.is_empty() { "-" } else { &run_spec.role },
            run_spec.kind,
            if run_spec.skills.is_empty() {
                "-".to_owned()
            } else {
                format_inline_list(&run_spec.skills)
            },
            run_spec.workspace_path,
            run_spec.bypass_permissions,
            run_spec.template,
        ));
    }
    sections.join("\n\n")
}

fn task_status_name_plan(status: PlanStatus) -> String {
    format!("{status:?}").to_ascii_lowercase()
}

fn resolve_context_task_cards(
    state: &OrchestrationState,
    context_task_ids: Option<&Vec<String>>,
) -> Result<Vec<String>, String> {
    let Some(selectors) = context_task_ids else {
        return Ok(Vec::new());
    };
    let mut cards = Vec::new();
    for selector in selectors {
        if selector.trim().is_empty() {
            return Err("context_task_ids entries must not be empty".into());
        }
        let task = resolve_task_by_id_or_slug(state, selector)?;
        cards.push(build_task_card(state, &task, TaskCardSections::all()));
    }
    Ok(cards)
}

/// Render the full prompt sent to a coding worker for one task.
fn build_coding_task_prompt(
    state: &OrchestrationState,
    args: &CodingTaskSendArgs,
) -> Result<String, String> {
    let task = resolve_task_by_id_or_slug(state, &args.task_id_or_slug)?;
    let plan = state.plans.get(&task.plan_id);
    let project = plan.and_then(|plan| state.projects.get(&plan.project_id));
    let template = args
        .template
        .unwrap_or_else(|| template_from_run_spec(task.run_spec.as_ref()));
    let instruction = args
        .prompt
        .as_deref()
        .or_else(|| task.run_spec.as_ref().map(|spec| spec.instruction.as_str()))
        .unwrap_or(&task.objective);
    validate_prompt_text_value("instruction", instruction)?;

    let mut sections = Vec::new();
    sections.push(format!(
        "You are working on task {} [{}].",
        task_label(&task.id.0, &task.slug),
        task_status_name(task.status)
    ));

    // The worker's own execution context, when the target execution is bound
    // to this very task.
    if let Some(execution) = task.execution.as_ref() {
        if execution.execution_id == args.execution_id {
            sections.push(format!(
                "Your live execution (workspace binding):\n{}",
                format_task_execution(execution)
            ));
        }
    }

    sections.push(build_task_card(
        state,
        &task,
        TaskCardSections::from_args(args),
    ));
    for card in resolve_context_task_cards(state, args.context_task_ids.as_ref())? {
        sections.push(format!("Context:\n{card}"));
    }
    if let Some(extra) = args
        .extra_context
        .as_deref()
        .filter(|extra| !extra.trim().is_empty())
    {
        sections.push(format!("Additional context:\n{extra}"));
    }
    sections.push(
        "When the objective and all gates are satisfied, stop and report; the operator records \
         status transitions through taskr orchestration tools."
            .into(),
    );

    // Cards already carry section toggles, plan instructions, launch intent,
    // outcomes, and evidence. Render them at the template's card slot rather
    // than sending the raw template followed by unrelated context.
    render_coding_template(
        template.template_text().trim(),
        &[
            ("{{task_id}}", task.id.0.clone()),
            ("{{task_slug}}", task.slug.clone()),
            ("{{task_title}}", task.title.clone()),
            ("{{task_status}}", task_status_name(task.status)),
            (
                "{{project}}",
                project
                    .map(|project| task_label(&project.id.0, &project.slug))
                    .unwrap_or_else(|| "<missing>".into()),
            ),
            (
                "{{plan}}",
                plan.map(|plan| task_label(&plan.id.0, &plan.slug))
                    .unwrap_or_else(|| task.plan_id.0.clone()),
            ),
            ("{{objective}}", task.objective.clone()),
            ("{{plan_brief_section}}", String::new()),
            ("{{plan_instructions_section}}", String::new()),
            ("{{scheduler_section}}", String::new()),
            ("{{scope_section}}", String::new()),
            ("{{gates_section}}", String::new()),
            ("{{dependencies_section}}", String::new()),
            ("{{blockers_section}}", String::new()),
            ("{{task_card_context_section}}", sections.join("\n\n")),
            ("{{extra_context_section}}", String::new()),
            ("{{instruction}}", instruction.into()),
        ],
    )
}

/// Substitute only tokens in the bundled template. Inserted user text is
/// opaque, even when it contains braces that resemble another placeholder.
fn render_coding_template(template: &str, fields: &[(&str, String)]) -> Result<String, String> {
    let mut remaining = template;
    let mut rendered = String::new();
    while let Some(start) = remaining.find("{{") {
        rendered.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        let end = remaining
            .find("}}")
            .ok_or("unterminated coding prompt placeholder")?
            + 2;
        let placeholder = &remaining[..end];
        let (_, value) = fields
            .iter()
            .find(|(key, _)| *key == placeholder)
            .ok_or_else(|| format!("unknown coding prompt placeholder '{placeholder}'"))?;
        rendered.push_str(value);
        remaining = &remaining[end..];
    }
    rendered.push_str(remaining);
    Ok(rendered)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Orchestration status filtering + summaries
// ═══════════════════════════════════════════════════════════════════════════════

fn filter_orchestration_status(
    mut status: OrchestrationStatus,
    project_id: Option<&ProjectId>,
    plan_id: Option<&PlanId>,
    include_tasks: bool,
) -> OrchestrationStatus {
    let plan_ids = status
        .plans
        .iter()
        .filter(|plan| plan_id.is_none_or(|id| plan.id == *id))
        .filter(|plan| project_id.is_none_or(|id| plan.project_id == *id))
        .map(|plan| plan.id.clone())
        .collect::<HashSet<_>>();
    status
        .projects
        .retain(|project| project_id.is_none_or(|id| &project.id == id));
    status.plans.retain(|plan| plan_ids.contains(&plan.id));
    status.tasks.retain(|task| plan_ids.contains(&task.plan_id));
    if !include_tasks {
        status.tasks.clear();
    }
    let task_ids = status
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect::<HashSet<_>>();
    status
        .task_edges
        .retain(|edge| task_ids.contains(&edge.from) && task_ids.contains(&edge.to));
    status
        .executions
        .retain(|execution| task_ids.contains(&execution.task_id));
    status.counts = OrchestrationCounts {
        total_projects: status.projects.len(),
        total_plans: status.plans.len(),
        total_tasks: status.tasks.len(),
        durable_execution_records: status.executions.len(),
        cleanup_candidates: status.cleanup_candidates.len(),
        active_projects: status
            .projects
            .iter()
            .filter(|project| project.status == ProjectStatus::Active)
            .count(),
        archived_projects: status
            .projects
            .iter()
            .filter(|project| project.status == ProjectStatus::Archived)
            .count(),
        active_plans: status
            .plans
            .iter()
            .filter(|plan| !plan.status.is_finished())
            .count(),
        blocked_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Blocked)
            .count(),
        waiting_for_validation_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::WaitingForValidation)
            .count(),
        passed_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Passed)
            .count(),
        delivered_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Delivered)
            .count(),
        failed_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Failed)
            .count(),
        canceled_plans: status
            .plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Canceled)
            .count(),
        active_tasks: status
            .tasks
            .iter()
            .filter(|task| !task.status.is_finished())
            .count(),
        blocked_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Blocked)
            .count(),
        waiting_for_validation_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::WaitingForValidation)
            .count(),
        passed_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Passed)
            .count(),
        delivered_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Delivered)
            .count(),
        failed_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Failed)
            .count(),
        canceled_tasks: status
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Canceled)
            .count(),
    };
    status
}

fn summarize_orchestration_counts(status: &OrchestrationStatus) -> Value {
    json!({
        "total_projects": status.counts.total_projects,
        "active_projects": status.counts.active_projects,
        "archived_projects": status.counts.archived_projects,
        "total_plans": status.counts.total_plans,
        "active_plans": status.counts.active_plans,
        "blocked_plans": status.counts.blocked_plans,
        "waiting_for_validation_plans": status.counts.waiting_for_validation_plans,
        "passed_plans": status.counts.passed_plans,
        "delivered_plans": status.counts.delivered_plans,
        "failed_plans": status.counts.failed_plans,
        "canceled_plans": status.counts.canceled_plans,
        "total_tasks": status.counts.total_tasks,
        "active_tasks": status.counts.active_tasks,
        "blocked_tasks": status.counts.blocked_tasks,
        "waiting_for_validation_tasks": status.counts.waiting_for_validation_tasks,
        "passed_tasks": status.counts.passed_tasks,
        "delivered_tasks": status.counts.delivered_tasks,
        "failed_tasks": status.counts.failed_tasks,
        "canceled_tasks": status.counts.canceled_tasks,
        "durable_execution_records": status.counts.durable_execution_records,
        "cleanup_candidates": status.counts.cleanup_candidates,
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
//  MCP ServerHandler
// ═══════════════════════════════════════════════════════════════════════════════

impl ServerHandler for HerdrMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .build();
        info.server_info = Implementation::new("taskr-controller", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "TASKR durable orchestration over Herdr-managed terminals. Durable state lives in \
             projects/plans/tasks; task executions bind a task to one Herdr endpoint + pane + \
             coding agent. Target runtimes only by execution_id; deprecated node/session \
             fields are rejected."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "project_create",
                "Create a durable project (admin; requires --enable-admin-tools).",
                tool_schema(
                    json!({
                        "title": {"type": "string"},
                        "description": {"type": "string"},
                        "slug": {"type": "string"},
                        "codex_home": {"type": ["string", "null"]},
                        "claude_home": {"type": ["string", "null"]},
                        "opencode_home": {"type": ["string", "null"]},
                        "kimi_home": {"type": ["string", "null"]},
                    }),
                    Some(vec!["title", "description"]),
                ),
            ),
            Tool::new(
                "project_update",
                "Update project configuration homes (admin). Omitted homes keep their value; explicit null clears.",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "codex_home": {"type": ["string", "null"]},
                        "claude_home": {"type": ["string", "null"]},
                        "opencode_home": {"type": ["string", "null"]},
                        "kimi_home": {"type": ["string", "null"]},
                    }),
                    Some(vec!["project_id"]),
                ),
            ),
            Tool::new(
                "project_status_update",
                "Set project status to Active or Archived (admin).",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "status": {"type": "string", "enum": ["Active", "Archived"]},
                    }),
                    Some(vec!["project_id", "status"]),
                ),
            ),
            Tool::new(
                "project_list",
                "List projects with plan/task counts.",
                tool_schema(json!({}), None),
            ),
            Tool::new(
                "plan_create",
                "Create a plan inside a project.",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "title": {"type": "string"},
                        "brief": {"type": "string"},
                        "instructions": {"type": "string"},
                        "slug": {"type": "string"},
                    }),
                    Some(vec!["project_id", "title", "brief"]),
                ),
            ),
            Tool::new(
                "plan_update",
                "Update plan title/brief/instructions.",
                tool_schema(
                    json!({
                        "plan_id": {"type": "string"},
                        "title": {"type": "string"},
                        "brief": {"type": "string"},
                        "instructions": {"type": "string"},
                    }),
                    Some(vec!["plan_id"]),
                ),
            ),
            Tool::new(
                "plan_status_update",
                "Set plan status; outcome is required for Delivered.",
                tool_schema(
                    json!({
                        "plan_id": {"type": "string"},
                        "status": {"type": "string", "enum": ["Backlog","Planned","Running","WaitingForValidation","Blocked","Failed","Passed","Delivered","Canceled"]},
                        "outcome": {"type": "string"},
                    }),
                    Some(vec!["plan_id", "status"]),
                ),
            ),
            Tool::new(
                "plan_get",
                "Get one plan with its tasks.",
                tool_schema(
                    json!({"plan_id": {"type": "string"}}),
                    Some(vec!["plan_id"]),
                ),
            ),
            Tool::new(
                "plan_list",
                "List plans, optionally filtered by project.",
                tool_schema(json!({"project_id": {"type": "string"}}), None),
            ),
            Tool::new(
                "task_create",
                "Create a task inside a plan; optional run_spec pins the launch intent.",
                tool_schema(
                    json!({
                        "plan_id": {"type": "string"},
                        "title": {"type": "string"},
                        "objective": {"type": "string"},
                        "scope": {
                            "type": "object",
                            "properties": {
                                "include_paths": {"type": "array", "items": {"type": "string"}},
                                "exclude_paths": {"type": "array", "items": {"type": "string"}},
                                "notes": {"type": "string"},
                            },
                        },
                        "gates": {"type": "array", "items": {"type": "string"}},
                        "slug": {"type": "string"},
                        "auto_schedule": {"type": "boolean"},
                        "run_spec": {
                            "type": "object",
                            "properties": {
                                "endpoint_id": {"type": "string"},
                                "launch_profile_id": {"type": "string"},
                                "workspace_path": {"type": "string"},
                                "bypass_permissions": {"type": "boolean"},
                                "role": {"type": "string"},
                                "kind": {"type": "string"},
                                "skills": {"type": "array", "items": {"type": "string"}},
                                "template": {"type": "string", "enum": ["task","validate","review","quality-guard"]},
                                "instruction": {"type": "string"},
                            },
                            "required": ["endpoint_id","launch_profile_id","workspace_path","bypass_permissions","role","kind","template","instruction"],
                        },
                    }),
                    Some(vec!["plan_id", "title", "objective"]),
                ),
            ),
            Tool::new(
                "task_update",
                "Update task fields; run_spec may be replaced or cleared with null.",
                tool_schema(
                    json!({
                        "task_id": {"type": "string"},
                        "title": {"type": "string"},
                        "objective": {"type": "string"},
                        "scope": {
                            "type": "object",
                            "properties": {
                                "include_paths": {"type": "array", "items": {"type": "string"}},
                                "exclude_paths": {"type": "array", "items": {"type": "string"}},
                                "notes": {"type": ["string", "null"]},
                            },
                        },
                        "gates": {"type": "array", "items": {"type": "string"}},
                        "auto_schedule": {"type": "boolean"},
                        "run_spec": {"type": ["object", "null"]},
                    }),
                    Some(vec!["task_id"]),
                ),
            ),
            Tool::new(
                "task_get",
                "Get one task with dependency blockers and superseding tasks.",
                tool_schema(
                    json!({"task_id": {"type": "string"}}),
                    Some(vec!["task_id"]),
                ),
            ),
            Tool::new(
                "task_status_update",
                "Set task status; outcome required before Passed/Delivered on gated tasks. Final states save conversation/report references, exit the worker, and close its verified pane; unfinished Audits tasks hold successful workers. Inspection resumes are stopped explicitly. Cleanup failures return the committed task with pending retry information.",
                tool_schema(
                    json!({
                        "task_id": {"type": "string"},
                        "status": {"type": "string", "enum": ["Backlog","Planned","Running","WaitingForValidation","Blocked","Failed","Passed","Delivered","Canceled"]},
                        "outcome": {"type": "string"},
                        "blockers": {"type": "array", "items": {"type": "string"}},
                        "evidence": {"type": "array", "items": {"type": "string"}},
                    }),
                    Some(vec!["task_id", "status"]),
                ),
            ),
            Tool::new(
                "task_edge_add",
                "Add a task edge (ParentOf, DependsOn, Validates, Audits, Supersedes, Related).",
                tool_schema(
                    json!({
                        "from": {"type": "string"},
                        "to": {"type": "string"},
                        "kind": {"type": "string", "enum": ["ParentOf","DependsOn","Validates","Audits","Supersedes","Related"]},
                    }),
                    Some(vec!["from", "to", "kind"]),
                ),
            ),
            Tool::new(
                "task_edge_remove",
                "Remove a task edge.",
                tool_schema(
                    json!({
                        "from": {"type": "string"},
                        "to": {"type": "string"},
                        "kind": {"type": "string", "enum": ["ParentOf","DependsOn","Validates","Audits","Supersedes","Related"]},
                    }),
                    Some(vec!["from", "to", "kind"]),
                ),
            ),
            Tool::new(
                "task_start",
                "Start one task now (or preview with dry_run); replaces non-active previous executions.",
                tool_schema(
                    json!({
                        "task_id_or_slug": {"type": "string"},
                        "dry_run": {"type": "boolean"},
                    }),
                    Some(vec!["task_id_or_slug"]),
                ),
            ),
            Tool::new(
                "orchestration_status",
                "Orchestration overview with live execution phases; optionally filtered.",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "plan_id": {"type": "string"},
                        "include_tasks": {"type": "boolean"},
                    }),
                    None,
                ),
            ),
            Tool::new(
                "orchestration_report",
                "Report which tasks would start right now, without launching.",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "plan_id": {"type": "string"},
                    }),
                    None,
                ),
            ),
            Tool::new(
                "orchestration_next",
                "Run one scheduler pass (dry_run previews, otherwise launches tasks).",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "plan_id": {"type": "string"},
                        "dry_run": {"type": "boolean"},
                        "max_tasks": {"type": "integer", "minimum": 1},
                    }),
                    None,
                ),
            ),
            Tool::new(
                "orchestration_prune",
                "Remove aged retained shell terminals, stale execution records, and finished plans after verifying Herdr ownership and foreground state.",
                tool_schema(
                    json!({
                        "dry_run": {"type": "boolean"},
                        "older_than_days": {"type": "integer", "minimum": 1},
                        "include_stale_execution_records": {"type": "boolean"},
                        "include_finished_plans": {"type": "boolean"},
                    }),
                    None,
                ),
            ),
            Tool::new(
                "start_coding_session",
                "Launch a coding agent for one task on an explicit endpoint + launch profile + workspace, then deliver the task prompt.",
                tool_schema(
                    json!({
                        "task_id": {"type": "string"},
                        "endpoint_id": {"type": "string"},
                        "launch_profile_id": {"type": "string"},
                        "workspace_path": {"type": "string"},
                        "bypass_permissions": {"type": "boolean"},
                        "role": {"type": "string"},
                        "kind": {"type": "string"},
                        "skills": {"type": "array", "items": {"type": "string"}},
                        "template": {"type": "string", "enum": ["task", "validate", "review", "quality-guard"]},
                    }),
                    Some(vec!["task_id", "endpoint_id", "launch_profile_id", "workspace_path"]),
                ),
            ),
            Tool::new(
                "execution_adopt",
                "Adopt an already-running pane/agent as a task execution with observed provenance.",
                tool_schema(
                    json!({
                        "task_id": {"type": "string"},
                        "endpoint_id": {"type": "string"},
                        "pane_id": {"type": "string"},
                        "agent_name": {"type": "string"},
                        "role": {"type": "string"},
                        "kind": {"type": "string"},
                    }),
                    Some(vec!["task_id", "endpoint_id"]),
                ),
            ),
            Tool::new(
                "execution_resume",
                "Reopen a saved native conversation in a fresh plan/group pane. Does not resend a prompt or change task status. Stop inspection explicitly with execution_stop.",
                tool_schema(json!({"execution_id": {"type": "string"}}), Some(vec!["execution_id"])),
            ),
            Tool::new(
                "execution_stop",
                "Save the native conversation reference, exit one task's coding agent, and close its verified pane. Also stops inspection resumes without changing task status.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "dry_run": {"type": "boolean"},
                    }),
                    Some(vec!["execution_id"]),
                ),
            ),
            Tool::new(
                "list_executions",
                "List durable executions for a project with live runtime facts.",
                tool_schema(
                    json!({
                        "project_id": {"type": "string"},
                        "include_completed": {"type": "boolean"},
                    }),
                    Some(vec!["project_id"]),
                ),
            ),
            Tool::new(
                "list_endpoints",
                "List Herdr endpoints (local + saved machine profiles) with availability.",
                tool_schema(json!({}), None),
            ),
            Tool::new(
                "list_launch_profiles",
                "List prepared environment/native-profile launch choices on one endpoint. Sync is explicit admin setup.",
                tool_schema(json!({"endpoint_id":{"type":"string"}}), Some(vec!["endpoint_id"])),
            ),
            Tool::new(
                "coding_send",
                "Send a raw prompt to a task execution's coding agent.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "prompt": {"type": "string"},
                        "wait_until_idle": {"type": "boolean"},
                    }),
                    Some(vec!["execution_id", "prompt"]),
                ),
            ),
            Tool::new(
                "coding_task_send",
                "Render a task-aware prompt (template, gates, scope, context cards) and deliver it to a task execution.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "task_id_or_slug": {"type": "string"},
                        "prompt": {"type": "string"},
                        "template": {"type": "string", "enum": ["task","validate","review","quality-guard"]},
                        "include_dependencies": {"type": "boolean"},
                        "include_gates": {"type": "boolean"},
                        "include_scope": {"type": "boolean"},
                        "context_task_ids": {"type": "array", "items": {"type": "string"}},
                        "extra_context": {"type": "string"},
                    }),
                    Some(vec!["execution_id", "task_id_or_slug"]),
                ),
            ),
            Tool::new(
                "coding_read",
                "Read a task execution's coding-agent terminal output.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "source": {"type": "string", "enum": ["visible","recent","recent_unwrapped","detection"]},
                        "lines": {"type": "integer", "minimum": 1},
                    }),
                    Some(vec!["execution_id"]),
                ),
            ),
            Tool::new(
                "capture_output",
                "Capture a task execution's raw pane output.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "lines": {"type": "integer", "minimum": 1},
                    }),
                    Some(vec!["execution_id"]),
                ),
            ),
            Tool::new(
                "check_state",
                "Truthful lifecycle and occupant facts for one task execution.",
                tool_schema(
                    json!({"execution_id": {"type": "string"}}),
                    Some(vec!["execution_id"]),
                ),
            ),
            Tool::new(
                "send_input",
                "Send literal text (optionally followed by Enter) to a task execution's agent.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "text": {"type": "string"},
                        "enter": {"type": "boolean"},
                    }),
                    Some(vec!["execution_id", "text"]),
                ),
            ),
            Tool::new(
                "send_key",
                "Send one key press to a task execution's agent.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "key": {"type": "string"},
                    }),
                    Some(vec!["execution_id", "key"]),
                ),
            ),
            Tool::new(
                "coding_action",
                "Send a semantic action (approve, reject, cancel, escape, dismiss, continue) to a task execution's agent.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "action": {"type": "string"},
                    }),
                    Some(vec!["execution_id", "action"]),
                ),
            ),
            Tool::new(
                "exec",
                "Run one shell command inside a task execution's pane while its agent is idle.",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "command": {"type": "array", "items": {"type": "string"}},
                        "timeout_seconds": {"type": "number"},
                        "lines": {"type": "integer", "minimum": 1},
                    }),
                    Some(vec!["execution_id", "command"]),
                ),
            ),
            Tool::new(
                "wait_start",
                "Start an async wait on a task execution (stable output, sentinel text, or coding-ready).",
                tool_schema(
                    json!({
                        "execution_id": {"type": "string"},
                        "kind": {"type": "string", "enum": ["stable", "sentinel", "coding_ready"]},
                        "sentinel": {"type": "string"},
                        "timeout_seconds": {"type": "number"},
                        "poll_seconds": {"type": "number"},
                        "stability_seconds": {"type": "number"},
                    }),
                    Some(vec!["execution_id", "kind"]),
                ),
            ),
            Tool::new(
                "wait_status",
                "Poll one async wait job.",
                tool_schema(
                    json!({"wait_id": {"type": "string"}}),
                    Some(vec!["wait_id"]),
                ),
            ),
            Tool::new(
                "wait_cancel",
                "Cancel one async wait job.",
                tool_schema(
                    json!({"wait_id": {"type": "string"}}),
                    Some(vec!["wait_id"]),
                ),
            ),
            Tool::new("admin_environment_discover", "Admin: discover installed Codex/Claude homes or import a collection folder/ZIP via source_path. homes and source_path are mutually exclusive. No agents are launched.",
                tool_schema(json!({"homes":{"type":"array","items":{"type":"string"}},
                    "source_path":{"type":"string","minLength":1,"description":"Controller-readable native home, collection folder, or ZIP archive; optional taskr-environments.json manifest."}}), None)),
            Tool::new("admin_environment_sync", "Admin: explicitly clone an environment revision to a Herdr endpoint through the companion; returns a durable sync job.",
                tool_schema(json!({
                    "source_environment_id":{"type":"string"}, "source_revision":{"type":"string"},
                    "endpoint_id":{"type":"string"}, "credential_policy":{"type":"string","enum":["endpoint","copy"]},
                    "endpoint_auth_home":{"type":"string"}, "deployment_root":{"type":"string"}, "dry_run":{"type":"boolean"}, "refresh":{"type":"boolean"}
                }), Some(vec!["source_environment_id","source_revision","endpoint_id","credential_policy"]))),
            Tool::new("admin_environment_sync_status", "Admin: inspect progress and ready launch choices for a durable sync job.",
                tool_schema(json!({"sync_job_id":{"type":"string"}}), Some(vec!["sync_job_id"]))),
            Tool::new("admin_environment_sync_cancel", "Admin: cancel an unfinished sync without selecting its deployment or stopping agents.",
                tool_schema(json!({"sync_job_id":{"type":"string"}}), Some(vec!["sync_job_id"]))),
            Tool::new(
                "admin_list_endpoint_agents",
                "Admin: list every recognized live coding agent on one endpoint.",
                tool_schema(
                    json!({"endpoint_id": {"type": "string"}}),
                    None,
                ),
            ),
            Tool::new(
                "endpoint_migrate",
                "Admin: permanently rewrite unresolved legacy node references to a selected Herdr endpoint. Updates stored run specs and execution endpoints once; creates no routing alias and does not launch or adopt workers.",
                tool_schema(
                    json!({
                        "legacy_node_id": {"type": "string"},
                        "endpoint_id": {"type": "string"},
                    }),
                    Some(vec!["legacy_node_id", "endpoint_id"]),
                ),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args_value = request
            .arguments
            .as_ref()
            .map(|object| Value::Object(object.clone()));
        let args = args_value.as_ref();
        match request.name.as_ref() {
            "project_create" => self.project_create_tool(args).await,
            "project_update" => self.project_update_tool(args).await,
            "project_status_update" => self.project_status_update_tool(args).await,
            "project_list" => self.project_list_tool(args).await,
            "plan_create" => self.plan_create_tool(args).await,
            "plan_update" => self.plan_update_tool(args).await,
            "plan_status_update" => self.plan_status_update_tool(args).await,
            "plan_get" => self.plan_get_tool(args).await,
            "plan_list" => self.plan_list_tool(args).await,
            "task_create" => self.task_create_tool(args).await,
            "task_update" => self.task_update_tool(args).await,
            "task_get" => self.task_get_tool(args).await,
            "task_status_update" => self.task_status_update_tool(args).await,
            "task_edge_add" => self.task_edge_add_tool(args).await,
            "task_edge_remove" => self.task_edge_remove_tool(args).await,
            "task_start" => self.task_start_tool(args).await,
            "orchestration_status" => self.orchestration_status_tool(args).await,
            "orchestration_report" => self.orchestration_report_tool(args).await,
            "orchestration_next" => self.orchestration_next_tool(args).await,
            "orchestration_prune" => self.orchestration_prune_tool(args).await,
            "start_coding_session" => self.start_coding_session_tool(args).await,
            "execution_adopt" => self.execution_adopt_tool(args).await,
            "execution_stop" => self.execution_stop_tool(args).await,
            "execution_resume" => self.execution_resume_tool(args).await,
            "list_executions" => self.list_executions_tool(args).await,
            "list_endpoints" => self.list_endpoints_tool(args).await,
            "list_launch_profiles" => self.list_launch_profiles_tool(args).await,
            "coding_send" => self.coding_send_tool(args).await,
            "coding_task_send" => self.coding_task_send_tool(args).await,
            "coding_read" => self.coding_read_tool(args).await,
            "capture_output" => self.capture_output_tool(args).await,
            "check_state" => self.check_state_tool(args).await,
            "send_input" => self.send_input_tool(args).await,
            "send_key" => self.send_key_tool(args).await,
            "coding_action" => self.coding_action_tool(args).await,
            "exec" => self.exec_tool(args).await,
            "wait_start" => self.wait_start_tool(args).await,
            "wait_status" => self.wait_status_tool(args).await,
            "wait_cancel" => self.wait_cancel_tool(args).await,
            "admin_environment_discover" => self.admin_environment_discover_tool(args).await,
            "admin_environment_sync" => self.admin_environment_sync_tool(args).await,
            "admin_environment_sync_status" => {
                self.admin_environment_sync_status_tool(args, false).await
            }
            "admin_environment_sync_cancel" => {
                self.admin_environment_sync_status_tool(args, true).await
            }
            "admin_list_endpoint_agents" => self.admin_list_endpoint_agents_tool(args).await,
            "endpoint_migrate" => self.endpoint_migrate_tool(args).await,
            other => Err(mcp_invalid_request(format!("unknown tool '{other}'"))),
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(vec![
            coding_prompt_definition("coding-task-send", "Task prompt for a coding worker"),
            coding_prompt_definition(
                "coding-validate-send",
                "Validation prompt for a coding worker",
            ),
            coding_prompt_definition("coding-review-send", "Review prompt for a coding worker"),
            coding_prompt_definition(
                "coding-quality-guard-send",
                "Quality-guard prompt for a coding worker",
            ),
        ]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let text = match request.name.as_str() {
            "coding-task-send" => CODING_TASK_SEND_PROMPT,
            "coding-validate-send" => CODING_VALIDATE_SEND_PROMPT,
            "coding-review-send" => CODING_REVIEW_SEND_PROMPT,
            "coding-quality-guard-send" => CODING_QUALITY_GUARD_SEND_PROMPT,
            other => {
                return Err(mcp_invalid_request(format!("unknown prompt '{other}'")));
            }
        };
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            text,
        )]))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![Resource::new(
            RawResource::new("taskr://orchestration/status", "orchestration-status")
                .with_description("Current orchestration status snapshot")
                .with_mime_type("application/json"),
            None,
        )]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(
                RawResourceTemplate::new("taskr://project/{project_id}/status", "project-status")
                    .with_description("Per-project orchestration status snapshot"),
                None,
            ),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.as_str();
        if uri == "taskr://orchestration/status" {
            let status = self.orchestration.status().map_err(mcp_invalid_request)?;
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                serde_json::to_string(&status)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?,
                uri.to_owned(),
            )
            .with_mime_type("application/json")]));
        }
        if let Some(project_selector) = uri
            .strip_prefix("taskr://project/")
            .and_then(|rest| rest.strip_suffix("/status"))
        {
            let state = self.orchestration.snapshot().map_err(mcp_invalid_request)?;
            let project_id = resolve_project_id_or_slug(&state, project_selector)
                .map_err(mcp_invalid_request)?;
            let status = self.orchestration.status().map_err(mcp_invalid_request)?;
            let status = filter_orchestration_status(status, Some(&project_id), None, true);
            return Ok(ReadResourceResult::new(vec![ResourceContents::text(
                serde_json::to_string(&status)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?,
                uri.to_owned(),
            )
            .with_mime_type("application/json")]));
        }
        Err(mcp_invalid_request(format!("unknown resource '{uri}'")))
    }
}

fn coding_prompt_definition(name: &'static str, description: &'static str) -> Prompt {
    Prompt::new(
        name,
        Some(description),
        Some(vec![PromptArgument::new("task_id_or_slug")
            .with_description("Task the prompt is about")
            .with_required(true)]),
    )
}

// ═══════════════════════════════════════════════════════════════════════════════
//  HTTP server wiring
// ═══════════════════════════════════════════════════════════════════════════════

fn loopback_allowed_origins() -> Vec<String> {
    vec![
        "http://localhost".into(),
        "http://127.0.0.1".into(),
        "http://localhost:3000".into(),
        "http://127.0.0.1:3000".into(),
        "http://localhost:5173".into(),
        "http://127.0.0.1:5173".into(),
        "http://localhost:6274".into(),
        "http://127.0.0.1:6274".into(),
    ]
}

async fn security_middleware(req: Request<Body>, next: Next) -> Result<Response, StatusCode> {
    // Reject cross-site browser requests (DNS rebinding / drive-by POST).
    if let Some(site) = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if !matches!(site, "same-origin" | "same-site" | "none") {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(next.run(req).await)
}

#[derive(Clone)]
struct AuthState {
    token: Option<Arc<String>>,
}

async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(token) = state.token.as_deref() else {
        return Ok(next.run(req).await);
    };
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| {
            req.headers()
                .get("x-mcp-token")
                .and_then(|value| value.to_str().ok())
        });
    let authorized =
        provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()));
    if authorized {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn warn_if_secret_file_permissions_are_loose(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                eprintln!(
                    "warning: token file '{path}' is readable by group/other (mode {mode:o}); tighten with chmod 600"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn resolve_token_value(cli: &Cli) -> Result<Option<String>, String> {
    if let Some(token) = cli.mcp_token.as_deref() {
        return Ok(Some(token.to_owned()));
    }
    if let Some(path) = cli.mcp_token_file.as_deref() {
        warn_if_secret_file_permissions_are_loose(path);
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read --mcp-token-file '{path}': {error}"))?;
        let token = text.trim();
        if token.is_empty() {
            return Err(format!("--mcp-token-file '{path}' is empty"));
        }
        return Ok(Some(token.to_owned()));
    }
    if let Ok(token) = std::env::var(&cli.mcp_token_env) {
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_owned()));
        }
    }
    if cli.mcp_token_env == "TASKR_MCP_TOKEN"
        && std::env::var("MMUX_MCP_TOKEN").is_ok_and(|token| !token.trim().is_empty())
    {
        return Err("MMUX_MCP_TOKEN has been renamed to TASKR_MCP_TOKEN; update the environment or explicitly select --mcp-token-env before starting Taskr.".into());
    }
    Ok(None)
}

fn validate_remote_mcp_bind_auth(
    host: &str,
    token: Option<&str>,
    allow_remote_without_token: bool,
) -> Result<(), String> {
    let loopback = matches!(host, "127.0.0.1" | "::1" | "localhost" | "::");
    if loopback {
        return Ok(());
    }
    if token.is_some() {
        return Ok(());
    }
    if allow_remote_without_token {
        eprintln!(
            "warning: binding non-loopback host '{host}' WITHOUT bearer auth (--allow-remote-without-mcp-token); anyone who can reach this port can drive every task execution"
        );
        return Ok(());
    }
    Err(format!(
        "binding non-loopback host '{host}' requires an MCP bearer token (--mcp-token, --mcp-token-file, or {0}) or --allow-remote-without-mcp-token",
        "TASKR_MCP_TOKEN"
    ))
}

/// Restart reconciliation: bindings claiming an active phase must be
/// re-verified after the controller or Herdr restarted, because their live
/// facts predate the restart. Each active binding is flagged
/// NeedsReconciliation, its endpoint is observed once with bounded reads,
/// and the binding is then either confirmed (Reconciled), recorded as
/// exited when a fully-observed endpoint proves the pane and agent are
/// gone, or left flagged when the endpoint could not be observed.
async fn reconcile_executions_after_restart(ctx: &LaunchContext) -> String {
    let state = match ctx.orchestration.snapshot() {
        Ok(state) => state,
        Err(error) => return format!("restart reconciliation skipped: {error}"),
    };
    for task in state.tasks.values() {
        let Some(execution) = task.execution.as_ref() else {
            continue;
        };
        if execution.phase.is_active()
            && execution.recovery != ExecutionRecovery::NeedsReconciliation
        {
            let mut updated = execution.clone();
            updated.recovery = ExecutionRecovery::NeedsReconciliation;
            let _ = ctx.orchestration.record_execution(task.id.clone(), updated);
        }
    }

    let state = match ctx.orchestration.snapshot() {
        Ok(state) => state,
        Err(error) => return format!("restart reconciliation incomplete: {error}"),
    };
    let endpoints = state
        .tasks
        .values()
        .filter_map(|task| task.execution.as_ref())
        .filter(|execution| execution.phase.is_active())
        .map(|execution| execution.endpoint_id.clone())
        .collect::<HashSet<_>>();
    if endpoints.is_empty() {
        return "restart reconciliation: no active executions".into();
    }
    let (live_keys, warnings, observed, live_agents) =
        collect_live_runtime_keys(&ctx.herdr, &endpoints).await;
    for warning in &warnings {
        eprintln!("taskr reconcile: {warning}");
    }
    let mut confirmed = 0usize;
    let mut marked_exited = 0usize;
    let mut mismatched = 0usize;
    let mut pending = 0usize;
    for task in state.tasks.values() {
        let Some(execution) = task.execution.as_ref() else {
            continue;
        };
        if !execution.phase.is_active() {
            continue;
        }
        if execution.runtime_generation.is_some() {
            let target = ctx
                .herdr
                .target_for_endpoint(&execution.endpoint_id)
                .fenced(execution.runtime_generation.clone());
            match ctx
                .herdr
                .endpoint(target.clone(), taskr_herdr::EndpointAction::Observe)
                .await
            {
                Err(error)
                    if error.category == taskr_herdr::RuntimeErrorCategory::GenerationMismatch =>
                {
                    let Some(observed) = error.observed_generation.as_deref() else {
                        pending += 1;
                        continue;
                    };
                    if ctx
                        .orchestration
                        .namespace_lost(&execution.execution_id, observed)
                        .is_err()
                    {
                        pending += 1;
                        continue;
                    }
                    let _ = ctx
                        .herdr
                        .endpoint(
                            target,
                            taskr_herdr::EndpointAction::Release {
                                lease_id: execution.execution_id.clone(),
                            },
                        )
                        .await;
                    marked_exited += 1;
                    continue;
                }
                Err(_) => {
                    pending += 1;
                    continue;
                }
                Ok(_) => {}
            }
        }
        if taskr_core::coordination::unresolved_allocation(execution) {
            pending += 1;
            continue;
        }
        let endpoint_observed = observed.iter().any(|id| id == &execution.endpoint_id);
        let runtime_key = execution.runtime_key();
        let present = live_keys.contains(&runtime_key);
        let mut updated = execution.clone();
        if endpoint_observed && !present {
            updated.phase = ExecutionPhase::Exited;
            updated.recovery = ExecutionRecovery::Reconciled;
            marked_exited += 1;
        } else if present {
            // A live key alone is not proof of ownership: the pane may have
            // been re-tasked with a different agent under the same pane id.
            // Verify the observed occupant against the frozen binding; the
            // stored endpoint, profile, homes, arguments, directory, and
            // native session identity are never overwritten by observation.
            match live_agents
                .iter()
                .find(|(key, _)| *key == runtime_key)
                .map(|(_, agent)| agent)
            {
                Some(agent) if !HerdrMcpServer::execution_owns_agent(execution, agent) => {
                    let actual = agent.name.as_deref().unwrap_or("<unknown>");
                    let expected = execution.agent_name.as_deref().unwrap_or("<unnamed>");
                    eprintln!(
                        "taskr reconcile: execution '{}' binding '{}' is occupied by '{actual}' \
                         instead of '{expected}'; flagging for adoption",
                        execution.execution_id, runtime_key
                    );
                    updated.recovery = ExecutionRecovery::OccupantMismatch;
                    mismatched += 1;
                }
                Some(_) => {
                    // Recovered worker: keep every frozen launch field and
                    // backfill the native conversation identity only when
                    // the binding never recorded one.
                    if execution.agent_session.is_none() {
                        if let Some(agent) = live_agents.iter().find(|(key, _)| *key == runtime_key)
                        {
                            updated.agent_session = agent.1.agent_session.clone();
                        }
                    }
                    updated.recovery = ExecutionRecovery::Reconciled;
                    confirmed += 1;
                }
                None => {
                    // The endpoint observation is complete (both inventories
                    // succeeded) and the bound runtime is live, but no
                    // registered agent occupies it under the expected
                    // identity: the launched worker is proven gone, even
                    // though its pane may survive as an orphan.
                    eprintln!(
                        "taskr reconcile: execution '{}' binding '{}' has no registered agent; \
                         marking exited",
                        execution.execution_id, runtime_key
                    );
                    updated.phase = ExecutionPhase::Exited;
                    updated.recovery = ExecutionRecovery::Reconciled;
                    marked_exited += 1;
                }
            }
        } else {
            pending += 1;
            continue;
        }
        let _ = ctx.orchestration.record_execution(task.id.clone(), updated);
    }
    format!(
        "restart reconciliation: {confirmed} execution(s) confirmed, {mismatched} occupant mismatch(es), {marked_exited} proven exited, {pending} pending observation"
    )
}

pub(crate) async fn run_mcp_http_server(config: LocalRuntimeConfig) -> Result<(), String> {
    let LocalRuntimeConfig {
        bind,
        herdr,
        launch_profiles,
        policy,
        mcp_token,
        orchestration,
    } = config;

    let ctx = LaunchContext {
        herdr: herdr.clone(),
        launch_profiles: launch_profiles.clone(),
        orchestration: orchestration.clone(),
    };
    // Capability check: a Herdr server is not assumed from the binary's
    // existence; the API-backed surface is probed once, with a bounded
    // timeout, before the controller accepts traffic.
    match herdr
        .check_endpoint(&herdr.target_for_endpoint(LOCAL_ENDPOINT_ID))
        .await
    {
        Ok(true) => println!(
            "herdr capability probe: local endpoint '{LOCAL_ENDPOINT_ID}' verified via api snapshot"
        ),
        Ok(false) => eprintln!(
            "warning: herdr capability probe reports local endpoint '{LOCAL_ENDPOINT_ID}' unavailable; launches and reads fail until Herdr is reachable"
        ),
        Err(error) => eprintln!(
            "warning: herdr capability probe failed: {error}; launches and reads fail until Herdr is reachable"
        ),
    }
    launch_profiles.resume_pending(herdr.clone())?;
    let reconcile_summary = reconcile_executions_after_restart(&ctx).await;
    println!("{reconcile_summary}");
    let cleanup_ctx = ctx.clone();
    let (scheduler, _mailbox) = Actor::spawn(
        None,
        OrchestrationSchedulerActor,
        OrchestrationSchedulerState { ctx },
    )
    .await
    .map_err(|error| format!("failed to start orchestration scheduler: {error}"))?;

    let wait_jobs: WaitJobRegistry = Arc::new(Mutex::new(HashMap::new()));
    let server = Arc::new(HerdrMcpServer {
        herdr,
        launch_profiles,
        policy: policy.clone(),
        scheduler,
        orchestration,
        wait_jobs,
    });

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| format!("failed to bind {bind}: {error}"))?;
    println!("taskr controller listening on http://{bind}/mcp");
    if mcp_token.is_none() {
        println!("MCP endpoint has NO bearer auth (loopback mode); do not expose beyond localhost");
    }

    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_allowed_origins(loopback_allowed_origins())
            .with_stateful_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts(),
    );
    let mcp_router = axum::Router::new().nest_service("/mcp", service);
    let api_router = mcp_router
        .layer(DefaultBodyLimit::max(policy.max_request_bytes))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers(Any)
                .expose_headers([
                    axum::http::header::HeaderName::from_static("mcp-session-id"),
                    axum::http::header::HeaderName::from_static("mcp-protocol-version"),
                ]),
        )
        .layer(middleware::from_fn(security_middleware))
        .route_layer(middleware::from_fn_with_state(
            AuthState { token: mcp_token },
            auth_middleware,
        ));
    let health_router = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    let app = api_router.merge(health_router);

    let cleanup = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            for error in execution_cleanup::renew_endpoint_leases(&cleanup_ctx).await {
                eprintln!("endpoint lease renewal pending: {error}");
            }
            for error in execution_cleanup::sweep_finished_workers(&cleanup_ctx).await {
                eprintln!("finished task worker cleanup pending: {error}");
            }
        }
    });
    let result = axum::serve(listener, app)
        .await
        .map_err(|error| format!("server error: {error}"));
    cleanup.abort();
    result
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Process entry + CLI helpers usable from the binary target
// ═══════════════════════════════════════════════════════════════════════════════

pub fn main_entry() {
    main_entry_from(std::env::args_os().collect());
}

pub fn main_entry_from(args: Vec<OsString>) {
    let cli = Cli::parse_from(args);
    if let Err(error) = run_cli(&cli) {
        eprintln!("taskr: {error}");
        std::process::exit(1);
    }
}

pub fn print_help() -> Result<(), String> {
    use clap::CommandFactory;
    Cli::command()
        .print_help()
        .map_err(|error| format!("failed to print help: {error}"))
}

fn run_cli(cli: &Cli) -> Result<(), String> {
    let policy = Arc::new(ControllerPolicy::new(cli)?);
    let token = resolve_token_value(cli)?;
    validate_remote_mcp_bind_auth(
        &cli.host,
        token.as_deref(),
        cli.allow_remote_without_mcp_token,
    )?;

    let bind: SocketAddr = format!("{}:{}", cli.host, cli.port)
        .parse()
        .map_err(|error| format!("invalid --host/--port bind address: {error}"))?;
    let orchestration = OrchestrationHandle::open(cli.store_path.as_deref())?;
    let launch_profiles = ResolvedLaunchProfiles::open(
        &store_paths::resolve_store_path(cli.store_path.as_deref())?,
        CompanionConfig {
            python_bin: cli.environment_python_bin.clone(),
            ssh_bin: cli.environment_ssh_bin.clone(),
            ..CompanionConfig::default()
        },
    )?;
    let herdr = HerdrClient::new(HerdrClientConfig {
        bin: cli.herdr_bin.clone(),
        local_session: cli.herdr_session.clone(),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start tokio runtime: {error}"))?;
    let local = LocalRuntime::new(LocalRuntimeConfig {
        bind,
        herdr,
        launch_profiles,
        policy,
        mcp_token: token.map(Arc::new),
        orchestration,
    });
    runtime.block_on(local.run())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Local store helpers for the CLI subcommands
// ═══════════════════════════════════════════════════════════════════════════════

pub fn resolve_store_path(path: Option<&Path>) -> Result<PathBuf, String> {
    store_paths::resolve_store_path(path)
}

pub fn local_projects(store_path: Option<&Path>) -> Result<Vec<ProjectSummary>, String> {
    let handle = OrchestrationHandle::open(store_path)?;
    Ok(handle.status()?.projects)
}

pub fn local_create_project(
    store_path: Option<&Path>,
    input: CreateProject,
) -> Result<ProjectSummary, String> {
    let handle = OrchestrationHandle::open(store_path)?;
    let project = handle.create_project(input)?;
    handle
        .status()?
        .projects
        .into_iter()
        .find(|summary| summary.id == project.id)
        .ok_or_else(|| format!("project '{}' vanished right after creation", project.id.0))
}

pub use crate::orchestration_actor::DeletedProjectReport;

pub fn local_delete_project(
    store_path: Option<&Path>,
    project_id_or_slug: &str,
) -> Result<DeletedProjectReport, String> {
    OrchestrationHandle::open(store_path)?.delete_project(project_id_or_slug)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  CLI helpers for project administration and pruning
// ═══════════════════════════════════════════════════════════════════════════════

/// Public prune outcome for the `taskr prune` CLI subcommand.
pub struct PruneStoreOutcome {
    pub dry_run: bool,
    pub pruned_execution_count: usize,
    pub pruned_plan_count: usize,
    pub candidates: Vec<PruneExecutionCandidate>,
    pub endpoints_observed: Vec<String>,
    pub endpoint_warnings: Vec<String>,
}

pub struct PruneExecutionCandidate {
    pub runtime_key: String,
    pub endpoint_id: String,
    pub execution_id: String,
    pub pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub task_id: String,
    pub last_seen_ms: u64,
    pub reason: String,
}

/// Observe live endpoints through Herdr, then prune stale durable execution
/// records and finished plans in the local store.
pub async fn local_prune_store(
    store_path: Option<&Path>,
    herdr: &HerdrClient,
    dry_run: bool,
    include_stale_execution_records: bool,
    include_finished_plans: bool,
    older_than_days: Option<u64>,
) -> Result<PruneStoreOutcome, String> {
    let orchestration = OrchestrationHandle::open(store_path)?;
    let state = orchestration.snapshot()?;
    let endpoints = state
        .tasks
        .values()
        .filter_map(|task| task.execution.as_ref().map(|e| e.endpoint_id.clone()))
        .chain(
            state
                .retained_executions
                .values()
                .map(|retained| retained.execution.endpoint_id.clone()),
        )
        .collect::<HashSet<_>>();
    let (mut live_runtime_keys, mut warnings, observed, _agents) =
        collect_live_runtime_keys(herdr, &endpoints).await;
    execution_cleanup::prepare_retained_layout_prune(
        herdr,
        &state,
        dry_run,
        include_stale_execution_records,
        older_than_days,
        &mut live_runtime_keys,
        &mut warnings,
    )
    .await;
    let observed_endpoints = observed.iter().cloned().collect::<HashSet<_>>();
    let report = orchestration.prune_stale_execution_records(
        &live_runtime_keys,
        &observed_endpoints,
        dry_run,
        include_stale_execution_records,
        include_finished_plans,
        older_than_days,
    )?;
    Ok(PruneStoreOutcome {
        dry_run,
        pruned_execution_count: report.pruned_execution_count,
        pruned_plan_count: report.pruned_plan_count,
        candidates: report
            .candidates
            .into_iter()
            .map(|candidate| PruneExecutionCandidate {
                runtime_key: candidate.key,
                endpoint_id: candidate.endpoint_id,
                execution_id: candidate.execution_id,
                pane_id: candidate.pane_id,
                agent_name: candidate.agent_name,
                task_id: candidate.task_id,
                last_seen_ms: candidate.last_seen_ms,
                reason: candidate.reason,
            })
            .collect(),
        endpoints_observed: observed,
        endpoint_warnings: warnings,
    })
}

/// Blocking variant of [`local_prune_store`] for the CLI: builds a
/// current-thread runtime and a Herdr client from the given binary path.
pub fn local_prune_store_blocking(
    store_path: Option<&Path>,
    herdr_bin: &str,
    herdr_session: Option<&str>,
    dry_run: bool,
    include_stale_execution_records: bool,
    include_finished_plans: bool,
    older_than_days: Option<u64>,
) -> Result<PruneStoreOutcome, String> {
    let herdr = HerdrClient::new(HerdrClientConfig {
        bin: PathBuf::from(herdr_bin),
        local_session: herdr_session.map(str::to_owned),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start tokio runtime: {error}"))?;
    runtime.block_on(local_prune_store(
        store_path,
        &herdr,
        dry_run,
        include_stale_execution_records,
        include_finished_plans,
        older_than_days,
    ))
}

/// Prune finished plans (and their contained tasks) from the state.
/// Imported by the orchestration actor's prune path.
pub(crate) fn prune_finished_plans(
    state: &mut OrchestrationState,
    include_finished_plans: bool,
    cutoff_ms: Option<u64>,
) -> usize {
    if !include_finished_plans {
        return 0;
    }
    let finished = state
        .plans
        .values()
        .filter(|plan| plan.status.is_finished())
        .filter(|plan| {
            plan.completed_at_ms
                .is_some_and(|completed| cutoff_ms.is_none_or(|cutoff| completed <= cutoff))
        })
        // Do not discard ownership while a worker or retained layout still
        // exists (or its endpoint could not be observed). Prune executions first.
        .filter(|plan| {
            !state
                .tasks
                .values()
                .any(|task| task.plan_id == plan.id && task.execution.is_some())
                && !state.retained_executions.values().any(|retained| {
                    state
                        .tasks
                        .get(&retained.task_id)
                        .is_some_and(|task| task.plan_id == plan.id)
                })
        })
        .map(|plan| plan.id.clone())
        .collect::<Vec<_>>();
    let mut pruned = 0usize;
    for plan_id in finished {
        let task_ids = state
            .tasks
            .values()
            .filter(|task| task.plan_id == plan_id)
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        state.tasks.retain(|_, task| task.plan_id != plan_id);
        state
            .task_edges
            .retain(|edge| !task_ids.contains(&edge.from) && !task_ids.contains(&edge.to));
        state.plans.remove(&plan_id);
        state
            .plan_layouts
            .retain(|_, layout| layout.plan_id != plan_id);
        pruned += 1;
    }
    pruned
}

// ═══════════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cli(args: &[&str]) -> Cli {
        let mut argv = vec![OsString::from("taskr")];
        argv.extend(args.iter().map(OsString::from));
        Cli::parse_from(argv)
    }

    fn seeded_state() -> (OrchestrationState, ProjectId, PlanId, TaskId) {
        let mut state = OrchestrationState::new();
        let project = state
            .create_project(
                CreateProject {
                    title: "Seed project".into(),
                    description: "Seeded for tests".into(),
                    slug: Some("seed".into()),
                    codex_home: Some("/codex-home".into()),
                    claude_home: Some("/claude-home".into()),
                    opencode_home: None,
                    kimi_home: None,
                },
                1_000,
            )
            .expect("create project");
        let plan = state
            .create_plan(
                CreatePlan {
                    project_id: project.id.clone(),
                    title: "Seed plan".into(),
                    brief: "Plan brief text.".into(),
                    instructions: Some("Plan instructions.".into()),
                    slug: None,
                },
                1_100,
            )
            .expect("create plan");
        let task = state
            .create_task(
                CreateTask {
                    plan_id: plan.id.clone(),
                    title: "Seed task".into(),
                    objective: "Do the seeded work".into(),
                    scope: TaskScope::default(),
                    gates: vec!["tests pass".into()],
                    slug: Some("seed-task".into()),
                    auto_schedule: false,
                    run_spec: None,
                },
                1_200,
            )
            .expect("create task");
        (state, project.id, plan.id, task.id)
    }

    fn bound_execution(execution_id: &str) -> TaskExecution {
        TaskExecution {
            native_session_name: None,
            group: taskr_core::orchestration::ExecutionGroup::Work,
            inspection: false,
            resumed_from: None,
            pane_closed: false,
            report: None,
            execution_id: execution_id.into(),
            endpoint_id: "local".into(),
            runtime_generation: None,
            launch_profile_id: "codex".into(),
            launch_args: vec!["exec".into()],
            launch_env: BTreeMap::new(),
            bypass_permissions: false,
            workspace_path: "/workspace/seed".into(),
            role: "implementation-worker".into(),
            kind: "codex".into(),
            skills: vec!["rust".into()],
            workspace_id: Some("ws-1".into()),
            tab_id: Some("tab-1".into()),
            pane_id: Some("%3".into()),
            terminal_id: None,
            agent_name: Some("seed-agent".into()),
            agent_kind: Some("codex".into()),
            agent_session: Some("sess-1".into()),
            phase: ExecutionPhase::Live,
            recovery: ExecutionRecovery::Reconciled,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            last_seen_ms: 1_000,
        }
    }

    #[test]
    fn controller_policy_clamps_and_limits() {
        let policy = ControllerPolicy::new(&test_cli(&[])).expect("default policy");

        let error = policy.clamp_timeout(-1.0).expect_err("negative timeout");
        assert!(error.contains("positive"), "{error}");

        let clamped = policy.clamp_timeout(10_000.0).expect("clamp large");
        assert!((clamped - policy.max_timeout_seconds).abs() < f64::EPSILON);

        let unchanged = policy.clamp_timeout(2.5).expect("clamp small");
        assert!((unchanged - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn controller_policy_capture_limit_keeps_tail_and_marks_truncation() {
        let policy = ControllerPolicy {
            enable_admin_tools: false,
            max_timeout_seconds: 120.0,
            max_request_bytes: 2 * 1024 * 1024,
            max_capture_bytes: 1_000,
        };
        let small = policy.limit_capture_output("short".into());
        assert_eq!(small, "short");

        let long = "x".repeat(2_000) + "TAIL";
        let limited = policy.limit_capture_output(long.clone());
        assert!(limited.starts_with("[taskr truncated capture"));
        assert!(limited.ends_with("TAIL"));
        assert!(limited.len() < long.len());
    }

    #[test]
    fn prompt_text_validation_rejects_empty_control_and_oversize() {
        assert!(validate_prompt_text_value("prompt", "hello").is_ok());
        assert!(validate_prompt_text_value("prompt", "   ").is_err());
        assert!(validate_prompt_text_value("prompt", "bad\u{0001}char").is_err());
        let oversize = "a".repeat(MAX_PROMPT_TEXT_BYTES + 1);
        assert!(validate_prompt_text_value("prompt", &oversize).is_err());
    }

    #[test]
    fn scheduler_template_names_parse_kebab_only() {
        for name in ["task", "validate", "review", "quality-guard"] {
            assert!(scheduler_parse_template(name).is_ok(), "{name}");
        }
        assert!(scheduler_parse_template("nope").is_err());
    }

    #[test]
    fn tool_schema_shapes_object_with_required() {
        let schema = tool_schema(
            json!({"name": {"type": "string"}, "size": {"type": "integer"}}),
            Some(vec!["name"]),
        );
        assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
        assert_eq!(
            schema.get("additionalProperties").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            schema
                .get("required")
                .and_then(Value::as_array)
                .map(|required| required.len()),
            Some(1)
        );
    }

    #[test]
    fn tool_args_reject_deprecated_node_session_and_profile_fields() {
        let error = parse_tool_args::<OrchestrationStatusArgs>(Some(&json!({
            "node_id": "legacy-node"
        })))
        .expect_err("node_id must be rejected");
        assert!(error.to_string().contains("unknown"), "{error}");

        let error = parse_tool_args::<StartCodingSessionArgs>(Some(&json!({
            "task_id": "t1",
            "session": "legacy-session",
            "workspace_path": "/tmp"
        })))
        .expect_err("session must be rejected");
        assert!(error.to_string().contains("unknown"), "{error}");

        let error = parse_tool_args::<StartCodingSessionArgs>(Some(&json!({
            "task_id": "t1",
            "coder_profile": "legacy-profile",
            "workspace_path": "/tmp"
        })))
        .expect_err("coder_profile must be rejected");
        assert!(error.to_string().contains("unknown"), "{error}");
    }

    #[test]
    fn file_limit_flags_are_rejected() {
        for flag in ["--max-read-bytes", "--max-write-bytes"] {
            for args in [
                vec!["taskr".to_owned(), flag.to_owned(), "4096".to_owned()],
                vec!["taskr".to_owned(), format!("{flag}=4096")],
            ] {
                let error = Cli::try_parse_from(args).expect_err("removed file limit flag");
                assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
            }
        }
    }

    #[test]
    fn cli_rejects_removed_flags_and_parses_herdr_flags() {
        for removed in [
            "--enable-local-node",
            "--enable-microsandbox-node",
            "--sandbox-name",
            "--tmux-config",
            "--enabled-coder-profiles",
            "--default-coder-profile",
        ] {
            assert!(
                Cli::try_parse_from(["taskr", removed, "x"]).is_err(),
                "{removed} should no longer parse"
            );
        }

        let cli = test_cli(&[
            "--herdr-bin",
            "/usr/local/bin/herdr",
            "--herdr-session",
            "main",
            "--environment-python-bin",
            "/usr/bin/python3",
        ]);
        assert_eq!(cli.herdr_bin, PathBuf::from("/usr/local/bin/herdr"));
        assert_eq!(cli.herdr_session.as_deref(), Some("main"));
        assert_eq!(
            cli.environment_python_bin,
            PathBuf::from("/usr/bin/python3")
        );
        for removed in [
            "--launch-profiles-file",
            "--enabled-launch-profiles",
            "--default-launch-profile",
        ] {
            assert!(Cli::try_parse_from(["taskr", removed, "x"]).is_err());
        }
    }

    #[test]
    fn coding_task_prompt_renders_card_execution_and_section_toggles() {
        let (mut state, _project_id, _plan_id, task_id) = seeded_state();
        state
            .tasks
            .get_mut(&task_id)
            .expect("task exists")
            .execution = Some(bound_execution("exec-1"));

        let args = CodingTaskSendArgs {
            execution_id: "exec-1".into(),
            task_id_or_slug: task_id.0.clone(),
            prompt: None,
            template: None,
            include_dependencies: None,
            include_gates: None,
            include_scope: None,
            context_task_ids: None,
            extra_context: Some("Watch the flaky test.".into()),
        };
        let prompt = build_coding_task_prompt(&state, &args).expect("prompt");
        assert!(prompt.contains("Your live execution"), "{prompt}");
        assert!(prompt.contains("Objective:"), "{prompt}");
        assert!(prompt.contains("tests pass"), "{prompt}");
        assert!(prompt.contains("Plan brief text."), "{prompt}");
        assert!(prompt.contains("endpoint=local"), "{prompt}");
        assert!(prompt.contains("Watch the flaky test."), "{prompt}");

        let trimmed = CodingTaskSendArgs {
            include_gates: Some(false),
            ..args
        };
        let prompt = build_coding_task_prompt(&state, &trimmed).expect("prompt");
        assert!(!prompt.contains("Gates"), "{prompt}");
        assert!(!prompt.contains("tests pass"), "{prompt}");
    }

    #[test]
    fn coding_templates_render_instructions_once_and_include_context_evidence() {
        let (mut state, project_id, plan_id, task_id) = seeded_state();
        let task = state.tasks.get_mut(&task_id).unwrap();
        task.outcome = Some("Native file proof passed".into());
        task.evidence = vec!["proof-input.txt was read on the endpoint".into()];
        let instruction = "Preserve literal {{task_id}} and {{instruction}}; reply 'quoted' `value`.\n```text\nproof\n```";
        for template in [
            CodingTaskSendTemplate::Task,
            CodingTaskSendTemplate::Validate,
            CodingTaskSendTemplate::Review,
            CodingTaskSendTemplate::QualityGuard,
        ] {
            let args = CodingTaskSendArgs {
                execution_id: "exec-proof".into(),
                task_id_or_slug: task_id.0.clone(),
                prompt: Some(instruction.into()),
                template: Some(template),
                include_dependencies: None,
                include_gates: None,
                include_scope: None,
                context_task_ids: Some(vec![task_id.0.clone()]),
                extra_context: Some("Extra context stays literal: {{task_title}}".into()),
            };
            let prompt = build_coding_task_prompt(&state, &args).unwrap();
            assert!(prompt.contains(&format!("Task: {}", task_id.0)), "{prompt}");
            assert!(prompt.contains(&project_id.0), "{prompt}");
            assert!(prompt.contains(&plan_id.0), "{prompt}");
            assert_eq!(prompt.matches(instruction).count(), 1, "{prompt}");
            assert!(prompt.contains("Plan instructions."), "{prompt}");
            assert!(
                prompt.contains("proof-input.txt was read on the endpoint"),
                "{prompt}"
            );
            assert!(prompt.contains("Native file proof passed"), "{prompt}");
            assert!(
                prompt.contains("Extra context stays literal: {{task_title}}"),
                "{prompt}"
            );
            for key in [
                "{{objective}}",
                "{{scope_section}}",
                "{{plan_instructions_section}}",
                "{{task_card_context_section}}",
            ] {
                assert!(!prompt.contains(key), "unrendered {key}: {prompt}");
            }
        }
    }

    #[test]
    fn coding_template_uses_run_instruction_when_no_explicit_instruction_is_supplied() {
        let (mut state, _, _, task_id) = seeded_state();
        state.tasks.get_mut(&task_id).unwrap().run_spec = Some(TaskRunSpec {
            endpoint_id: "local".into(),
            launch_profile_id: "codex".into(),
            workspace_path: "/workspace".into(),
            bypass_permissions: false,
            role: "worker".into(),
            kind: "validation".into(),
            skills: vec![],
            template: "task".into(),
            instruction: "Scheduled native proof instruction".into(),
        });
        let args = CodingTaskSendArgs {
            execution_id: "exec-proof".into(),
            task_id_or_slug: task_id.0.clone(),
            prompt: None,
            template: None,
            include_dependencies: None,
            include_gates: None,
            include_scope: None,
            context_task_ids: None,
            extra_context: None,
        };
        let prompt = build_coding_task_prompt(&state, &args).unwrap();
        assert!(
            prompt.contains("Instruction:\nScheduled native proof instruction"),
            "{prompt}"
        );
    }

    #[test]
    fn filter_status_scopes_to_project_and_recounts() {
        let mut state = OrchestrationState::new();
        let first = state
            .create_project(
                CreateProject {
                    title: "First".into(),
                    description: "First project".into(),
                    slug: Some("first".into()),
                    ..Default::default()
                },
                1_000,
            )
            .expect("project");
        let second = state
            .create_project(
                CreateProject {
                    title: "Second".into(),
                    description: "Second project".into(),
                    slug: Some("second".into()),
                    ..Default::default()
                },
                1_000,
            )
            .expect("project");
        for project in [&first, &second] {
            let plan = state
                .create_plan(
                    CreatePlan {
                        project_id: project.id.clone(),
                        title: format!("{} plan", project.title),
                        brief: "Brief.".into(),
                        instructions: None,
                        slug: None,
                    },
                    1_100,
                )
                .expect("plan");
            state
                .create_task(
                    CreateTask {
                        plan_id: plan.id,
                        title: "Task".into(),
                        objective: "Objective".into(),
                        scope: TaskScope::default(),
                        gates: Vec::new(),
                        slug: None,
                        auto_schedule: false,
                        run_spec: None,
                    },
                    1_200,
                )
                .expect("task");
        }

        let status = state.orchestration_status(2_000);
        assert_eq!(status.projects.len(), 2);
        assert_eq!(status.tasks.len(), 2);

        let filtered = filter_orchestration_status(status, Some(&first.id), None, true);
        assert_eq!(filtered.projects.len(), 1);
        assert_eq!(filtered.plans.len(), 1);
        assert_eq!(filtered.tasks.len(), 1);
        assert_eq!(filtered.counts.total_tasks, 1);
        assert_eq!(filtered.counts.active_projects, 1);
    }

    #[test]
    fn resolve_task_accepts_id_and_slug_but_not_unknown() {
        let (state, _project_id, _plan_id, task_id) = seeded_state();

        let by_id = resolve_task_by_id_or_slug(&state, &task_id.0).expect("by id");
        assert_eq!(by_id.id, task_id);

        let by_slug = resolve_task_by_id_or_slug(&state, "seed-task").expect("by slug");
        assert_eq!(by_slug.id, task_id);

        let error = resolve_task_by_id_or_slug(&state, "nope").expect_err("unknown task");
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn format_task_execution_lists_binding_details() {
        let execution = bound_execution("exec-9");
        let text = format_task_execution(&execution);
        assert!(text.contains("endpoint=local"), "{text}");
        assert!(text.contains("launch_profile=codex"), "{text}");
        assert!(text.contains("/workspace/seed"), "{text}");
        assert!(text.contains("phase=live"), "{text}");
        assert!(text.contains("pane=%3"), "{text}");
    }

    #[test]
    fn project_home_lookup_maps_kinds_and_skips_blank() {
        let (state, project_id, _plan_id, _task_id) = seeded_state();
        let project = state.projects.get(&project_id);

        assert_eq!(
            project_home_for_kind(project, "codex").as_deref(),
            Some("/codex-home")
        );
        assert_eq!(
            project_home_for_kind(project, "claude").as_deref(),
            Some("/claude-home")
        );
        assert_eq!(project_home_for_kind(project, "shell"), None);
        assert_eq!(project_home_for_kind(None, "codex"), None);
    }

    #[test]
    fn status_counts_include_durable_execution_records() {
        let (mut state, _project_id, _plan_id, task_id) = seeded_state();
        state
            .tasks
            .get_mut(&task_id)
            .expect("task exists")
            .execution = Some(bound_execution("exec-1"));

        let status = state.orchestration_status(2_000);
        assert_eq!(status.counts.durable_execution_records, 1);
        let summary = summarize_orchestration_counts(&status);
        assert_eq!(
            summary
                .get("durable_execution_records")
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn prune_finished_plans_removes_only_aged_finished_plans() {
        let (mut state, _project_id, plan_id, task_id) = seeded_state();
        state.plans.get_mut(&plan_id).expect("plan exists").status = PlanStatus::Delivered;
        state
            .plans
            .get_mut(&plan_id)
            .expect("plan exists")
            .completed_at_ms = Some(5_000);

        let pruned = prune_finished_plans(&mut state, false, None);
        assert_eq!(pruned, 0);
        assert!(state.plans.contains_key(&plan_id));

        let pruned = prune_finished_plans(&mut state, true, Some(10_000));
        assert_eq!(pruned, 1);
        assert!(!state.plans.contains_key(&plan_id));
        assert!(!state.tasks.contains_key(&task_id));
        assert!(state.task_edges.is_empty());

        let pruned = prune_finished_plans(&mut state, true, Some(1_000));
        assert_eq!(pruned, 0);
    }
}
