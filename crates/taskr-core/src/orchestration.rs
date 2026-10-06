use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PlanId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub slug: String,
    pub title: String,
    pub description: String,
    /// Stable source environments whose prepared profiles this project may use.
    #[serde(default)]
    pub environment_ids: Vec<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub claude_home: Option<String>,
    #[serde(default)]
    pub opencode_home: Option<String>,
    #[serde(default)]
    pub kimi_home: Option<String>,
    pub status: ProjectStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ProjectStatus {
    Active,
    Archived,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub project_id: ProjectId,
    pub slug: String,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub instructions: Option<String>,
    pub status: PlanStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub outcome: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum PlanStatus {
    Backlog,
    Planned,
    Running,
    WaitingForValidation,
    Blocked,
    Failed,
    Passed,
    Delivered,
    Canceled,
}

impl PlanStatus {
    pub const ALL: [Self; 9] = [
        Self::Backlog,
        Self::Planned,
        Self::Running,
        Self::WaitingForValidation,
        Self::Blocked,
        Self::Failed,
        Self::Passed,
        Self::Delivered,
        Self::Canceled,
    ];

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Delivered | Self::Canceled | Self::Failed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub plan_id: PlanId,
    pub slug: String,
    pub title: String,
    pub objective: String,
    pub scope: TaskScope,
    pub status: TaskStatus,
    pub execution: Option<TaskExecution>,
    pub gates: Vec<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub completed_at_ms: Option<u64>,
    pub outcome: Option<String>,
    pub blockers: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub auto_schedule: bool,
    #[serde(default)]
    pub run_spec: Option<TaskRunSpec>,
    /// Advisory preferences for the orchestrator, distinct from launch options.
    #[serde(default)]
    pub launch_hints: AgentLaunchHints,
}

/// Lifecycle phase of a task's execution, as observed through Herdr.
///
/// Runtime observations never imply task acceptance: a `Live` worker can
/// still fail its gates, and an `Exited` worker's results are committed
/// through the ordinary task reporting path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    /// Launch intent persisted; no runtime resources allocated yet.
    #[default]
    Pending,
    /// Workspace/tab/pane allocation submitted but not yet confirmed.
    Allocating,
    /// Agent start submitted but not yet observed by Herdr.
    Starting,
    /// Agent is recognized live on the endpoint.
    Live,
    /// Endpoint unreachable; the durable binding is kept, not deleted.
    Unavailable,
    /// The agent or its pane is gone on the endpoint.
    Exited,
    /// Stopped deliberately through TASKR-owned cleanup.
    Stopped,
    /// The launch failed with a truthful error observation.
    Failed,
}

impl ExecutionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Allocating => "allocating",
            Self::Starting => "starting",
            Self::Live => "live",
            Self::Unavailable => "unavailable",
            Self::Exited => "exited",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Allocating | Self::Starting | Self::Live
        )
    }

    /// Whether a new execution attempt may replace a binding in this phase.
    /// Only a terminal phase proves the previous attempt released its
    /// runtime resources; an unavailable binding may have a live worker.
    pub fn allows_replacement(self) -> bool {
        matches!(self, Self::Exited | Self::Stopped | Self::Failed)
    }
}

/// Recovery disposition recorded for an execution binding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRecovery {
    /// The binding has not yet been confirmed against live runtime state.
    /// Migrated pre-Herdr bindings start here.
    #[default]
    NeedsReconciliation,
    /// The binding was confirmed against live runtime state.
    Reconciled,
    /// The bound pane is live but occupied by a different agent; an operator
    /// must adopt or stop it before the execution may be interacted with.
    OccupantMismatch,
    /// The binding was explicitly abandoned (replaced or given up).
    Abandoned,
}

/// Fixed visual groups, chosen from the launch prompt mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionGroup {
    #[default]
    Work,
    Validation,
    Review,
    Quality,
}

impl ExecutionGroup {
    pub fn from_template(template: &str) -> Self {
        match template.trim() {
            "validate" => Self::Validation,
            "review" => Self::Review,
            "quality-guard" => Self::Quality,
            _ => Self::Work,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Validation => "Validation",
            Self::Review => "Review",
            Self::Quality => "Quality",
        }
    }
}

/// Durable layout identity. Labels are display text, never ownership keys.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanLayout {
    pub plan_id: PlanId,
    pub endpoint_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub runtime_generation: Option<String>,
    pub tabs: Vec<PlanTab>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanTab {
    pub tab_id: String,
    pub group: ExecutionGroup,
    pub ordinal: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub task_status: TaskStatus,
    pub outcome: Option<String>,
    pub evidence: Vec<String>,
}

/// One TASKR-owned execution attempt of a task, bound to an endpoint and its
/// Herdr resources. Replaces the tmux-shaped task session record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExecution {
    /// Stable TASKR execution/attempt identity.
    pub execution_id: String,
    /// Selected endpoint: `local` or a saved Herdr machine profile ID.
    pub endpoint_id: String,
    /// Incarnation of the endpoint resource namespace, never a protocol version.
    #[serde(default)]
    pub runtime_generation: Option<String>,
    pub launch_profile_id: String,
    /// Effective model/effort chosen for this attempt, when known.
    #[serde(default)]
    pub launch_options: AgentLaunchOptions,
    /// Resolved native agent arguments frozen for this attempt.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Resolved non-secret configuration environment frozen for this attempt.
    #[serde(default)]
    pub launch_env: BTreeMap<String, String>,
    pub bypass_permissions: bool,
    pub workspace_path: String,
    pub role: String,
    pub kind: String,
    pub skills: Vec<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    /// Immutable Herdr terminal identity; protects pane closure against ID reuse.
    #[serde(default)]
    pub terminal_id: Option<String>,
    pub agent_name: Option<String>,
    pub agent_kind: Option<String>,
    /// Native coding-CLI conversation reference, when Herdr reports one.
    pub agent_session: Option<String>,
    #[serde(default)]
    pub native_session_name: Option<String>,
    #[serde(default)]
    pub group: ExecutionGroup,
    /// Explicit session inspection must survive a completed task's cleanup sweep.
    #[serde(default)]
    pub inspection: bool,
    #[serde(default)]
    pub resumed_from: Option<String>,
    /// Historical placement IDs remain for provenance after the terminal closes.
    #[serde(default)]
    pub pane_closed: bool,
    #[serde(default)]
    pub report: Option<ExecutionReport>,
    pub phase: ExecutionPhase,
    pub recovery: ExecutionRecovery,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub last_seen_ms: u64,
}

impl TaskExecution {
    /// Stable runtime key used to correlate the binding with live resources.
    /// Herdr IDs are server-scoped, so the endpoint is part of the key.
    pub fn runtime_key(&self) -> String {
        let live = self
            .pane_id
            .as_deref()
            .or(self.agent_name.as_deref())
            .unwrap_or(&self.execution_id);
        runtime_key_for(&self.endpoint_id, self.runtime_generation.as_deref(), live)
    }
}

/// Namespace-qualified runtime identity. Length framing prevents ambiguous IDs.
pub fn runtime_key_for(endpoint: &str, generation: Option<&str>, resource: &str) -> String {
    match generation {
        Some(generation) => format!(
            "{}:{}:{}:{}:{}",
            endpoint.len(),
            endpoint,
            generation.len(),
            generation,
            resource
        ),
        None => format!("{endpoint}:{resource}"),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskScope {
    pub include_paths: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunSpec {
    /// Selected endpoint: `local` or a saved Herdr machine profile ID.
    pub endpoint_id: String,
    pub launch_profile_id: String,
    pub workspace_path: String,
    pub bypass_permissions: bool,
    pub role: String,
    pub kind: String,
    #[serde(default)]
    pub skills: Vec<String>,
    pub template: String,
    pub instruction: String,
    #[serde(default)]
    pub launch_options: AgentLaunchOptions,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLaunchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl AgentLaunchOptions {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("model", &self.model),
            ("reasoning_effort", &self.reasoning_effort),
        ] {
            if let Some(value) = value {
                if value.is_empty()
                    || value.starts_with('-')
                    || value.chars().any(char::is_whitespace)
                    || value.chars().any(char::is_control)
                {
                    return Err(format!("{name} must be a non-empty native option without whitespace or leading '-'") );
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLaunchHints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    Backlog,
    Planned,
    Running,
    WaitingForValidation,
    Blocked,
    Failed,
    Passed,
    Delivered,
    Canceled,
}

impl TaskStatus {
    pub const ALL: [Self; 9] = [
        Self::Backlog,
        Self::Planned,
        Self::Running,
        Self::WaitingForValidation,
        Self::Blocked,
        Self::Failed,
        Self::Passed,
        Self::Delivered,
        Self::Canceled,
    ];

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Delivered | Self::Canceled | Self::Failed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskEdge {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: TaskEdgeKind,
    pub created_at_ms: u64,
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum TaskEdgeKind {
    ParentOf,
    DependsOn,
    Validates,
    Audits,
    Supersedes,
    Related,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationStatus {
    pub projects: Vec<ProjectSummary>,
    pub plans: Vec<PlanSummary>,
    pub tasks: Vec<TaskSummary>,
    pub task_edges: Vec<TaskEdgeSummary>,
    pub executions: Vec<ExecutionSummary>,
    pub counts: OrchestrationCounts,
    pub cleanup_candidates: Vec<ExecutionCleanupCandidate>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSummary {
    pub id: ProjectId,
    pub slug: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub environment_ids: Vec<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub claude_home: Option<String>,
    #[serde(default)]
    pub opencode_home: Option<String>,
    #[serde(default)]
    pub kimi_home: Option<String>,
    pub status: ProjectStatus,
    pub plan_count: usize,
    pub active_plan_count: usize,
    pub plan_status_counts: BTreeMap<PlanStatus, usize>,
    pub task_count: usize,
    pub active_task_count: usize,
    pub task_status_counts: BTreeMap<TaskStatus, usize>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSummary {
    pub id: PlanId,
    pub project_id: ProjectId,
    pub slug: String,
    pub title: String,
    pub status: PlanStatus,
    pub outcome: Option<String>,
    pub task_count: usize,
    pub active_task_count: usize,
    pub task_status_counts: BTreeMap<TaskStatus, usize>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSummary {
    pub id: TaskId,
    pub plan_id: PlanId,
    pub slug: String,
    pub title: String,
    pub status: TaskStatus,
    pub outcome: Option<String>,
    pub evidence: Vec<String>,
    pub auto_schedule: bool,
    pub run_spec: Option<TaskRunSpec>,
    pub execution: Option<TaskExecution>,
    pub parent: Option<TaskId>,
    pub child_count: usize,
    pub dependency_count: usize,
    pub blocked_by: Vec<TaskId>,
    pub validator_count: usize,
    pub validation_blocked_by: Vec<TaskId>,
    pub unapproved_validator_count: usize,
    pub failed_validator_count: usize,
    pub open_gate_count: usize,
    pub failed_gate_count: usize,
    pub blocker_count: usize,
    pub blockers: Vec<String>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDependencyBlocker {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub reason: String,
    pub validation_blocked_by: Vec<TaskId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEdgeSummary {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: TaskEdgeKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSummary {
    pub endpoint_id: String,
    pub execution_id: String,
    pub launch_profile_id: String,
    pub workspace_path: String,
    pub bypass_permissions: bool,
    pub task_id: TaskId,
    pub role: String,
    pub kind: String,
    pub phase: ExecutionPhase,
    pub recovery: ExecutionRecovery,
    pub pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub agent_kind: Option<String>,
    pub agent_session: Option<String>,
    pub last_seen_ms: Option<u64>,
    pub runtime_state: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationCounts {
    pub total_projects: usize,
    pub active_projects: usize,
    pub archived_projects: usize,
    pub total_plans: usize,
    pub active_plans: usize,
    pub blocked_plans: usize,
    pub waiting_for_validation_plans: usize,
    pub passed_plans: usize,
    pub delivered_plans: usize,
    pub failed_plans: usize,
    pub canceled_plans: usize,
    pub total_tasks: usize,
    pub active_tasks: usize,
    pub blocked_tasks: usize,
    pub waiting_for_validation_tasks: usize,
    pub passed_tasks: usize,
    pub delivered_tasks: usize,
    pub failed_tasks: usize,
    pub canceled_tasks: usize,
    pub durable_execution_records: usize,
    pub cleanup_candidates: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCleanupCandidate {
    pub endpoint_id: String,
    pub execution_id: String,
    pub pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub reason: String,
    pub created_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrchestrationState {
    pub projects: HashMap<ProjectId, Project>,
    pub plans: HashMap<PlanId, Plan>,
    pub tasks: HashMap<TaskId, Task>,
    pub task_edges: Vec<TaskEdge>,
    pub next_plan_id: u64,
    pub next_task_id: u64,
    /// Previous attempts, including resumable conversations whose panes closed.
    #[serde(default)]
    pub retained_executions: HashMap<String, RetainedExecution>,
    #[serde(default)]
    pub plan_layouts: HashMap<String, PlanLayout>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedExecution {
    pub task_id: TaskId,
    pub execution: TaskExecution,
}

impl Default for OrchestrationState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    pub plan_id: PlanId,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub scope: TaskScope,
    #[serde(default)]
    pub gates: Vec<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub auto_schedule: bool,
    #[serde(default)]
    pub run_spec: Option<TaskRunSpec>,
    #[serde(default)]
    pub launch_hints: AgentLaunchHints,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub environment_ids: Vec<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub claude_home: Option<String>,
    #[serde(default)]
    pub opencode_home: Option<String>,
    #[serde(default)]
    pub kimi_home: Option<String>,
}

/// An omitted home keeps its current value; an explicit null clears it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProject {
    /// Omitted assignments are preserved; an empty list removes all assignments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ids: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_home_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub codex_home: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_home_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub claude_home: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_home_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub opencode_home: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_home_update",
        skip_serializing_if = "Option::is_none"
    )]
    pub kimi_home: Option<Option<String>>,
}

fn deserialize_home_update<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

fn validate_environment_ids(ids: &[String]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for id in ids {
        if id.trim().is_empty() || id.trim() != id || id.chars().any(char::is_control) {
            return Err("environment_ids must contain non-empty IDs without whitespace or control characters".into());
        }
        if !seen.insert(id) {
            return Err(format!("duplicate environment ID '{id}'"));
        }
    }
    Ok(())
}

fn validate_coder_home(field: &str, home: Option<&str>) -> Result<(), String> {
    if let Some(home) = home {
        if home.trim().is_empty() || home.chars().any(char::is_control) {
            return Err(format!(
                "{field} must be a non-empty path without control characters"
            ));
        }
        if !home.starts_with('/') && home != "~" && !home.starts_with("~/") {
            return Err(format!(
                "{field} must be an absolute path or start with ~/ on the execution node"
            ));
        }
    }
    Ok(())
}

impl Project {
    pub fn require_environment(&self, environment_id: &str) -> Result<(), String> {
        if !environment_id.trim().is_empty()
            && self.environment_ids.iter().any(|id| id == environment_id)
        {
            Ok(())
        } else {
            Err(format!(
                "environment '{environment_id}' is not assigned to project '{}'; assign it with project_update before launching",
                self.id.0
            ))
        }
    }

    pub fn coder_home(&self, profile: &str) -> Option<&str> {
        match profile {
            "codex" => self.codex_home.as_deref(),
            "claude" => self.claude_home.as_deref(),
            "opencode" => self.opencode_home.as_deref(),
            "kimi" => self.kimi_home.as_deref(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePlan {
    pub project_id: ProjectId,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProjectStatus {
    pub project_id: ProjectId,
    pub status: ProjectStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlan {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanStatus {
    pub plan_id: PlanId,
    pub status: PlanStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTaskScope {
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,
    #[serde(default)]
    pub notes: Option<Option<String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTask {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub scope: UpdateTaskScope,
    #[serde(default)]
    pub gates: Option<Vec<String>>,
    #[serde(default)]
    pub auto_schedule: Option<bool>,
    #[serde(default)]
    pub run_spec: Option<Option<TaskRunSpec>>,
    #[serde(default)]
    pub launch_hints: Option<AgentLaunchHints>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTaskEdge {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: TaskEdgeKind,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateTaskStatus {
    pub task_id: TaskId,
    pub status: TaskStatus,
}

impl OrchestrationState {
    pub fn new() -> Self {
        Self {
            projects: HashMap::new(),
            plans: HashMap::new(),
            tasks: HashMap::new(),
            task_edges: Vec::new(),
            next_plan_id: 1,
            next_task_id: 1,
            retained_executions: HashMap::new(),
            plan_layouts: HashMap::new(),
        }
    }

    pub fn orchestration_status(&self, now_ms: u64) -> OrchestrationStatus {
        let mut warnings = Vec::new();
        let mut parent_by_child: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        let mut child_counts: HashMap<TaskId, usize> = HashMap::new();
        let mut dependencies_by_task: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        let mut seen_edges = HashSet::new();

        for edge in &self.task_edges {
            if !seen_edges.insert((edge.from.clone(), edge.to.clone(), edge.kind)) {
                warnings.push(format!(
                    "duplicate {:?} edge from '{}' to '{}'",
                    edge.kind, edge.from.0, edge.to.0
                ));
            }
            if edge.from == edge.to {
                warnings.push(format!(
                    "task edge cannot point to itself for '{}'",
                    edge.from.0
                ));
            }
            if !self.tasks.contains_key(&edge.from) {
                warnings.push(format!(
                    "task edge references missing from task '{}'",
                    edge.from.0
                ));
            }
            if !self.tasks.contains_key(&edge.to) {
                warnings.push(format!(
                    "task edge references missing to task '{}'",
                    edge.to.0
                ));
            }

            match edge.kind {
                TaskEdgeKind::ParentOf => {
                    *child_counts.entry(edge.from.clone()).or_default() += 1;
                    parent_by_child
                        .entry(edge.to.clone())
                        .or_default()
                        .push(edge.from.clone());
                }
                TaskEdgeKind::DependsOn => {
                    dependencies_by_task
                        .entry(edge.from.clone())
                        .or_default()
                        .push(edge.to.clone());
                }
                _ => {}
            }
        }

        for parents in parent_by_child.values_mut() {
            parents.sort_by(|left, right| left.0.cmp(&right.0));
            parents.dedup();
        }
        for (child, parents) in &parent_by_child {
            if parents.len() > 1 {
                warnings.push(format!(
                    "task '{}' has multiple ParentOf parents: {}",
                    child.0,
                    parents
                        .iter()
                        .map(|parent| parent.0.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        for dependencies in dependencies_by_task.values_mut() {
            dependencies.sort_by(|left, right| left.0.cmp(&right.0));
            dependencies.dedup();
        }

        let mut plan_count_by_project: HashMap<ProjectId, usize> = HashMap::new();
        let mut active_plan_count_by_project: HashMap<ProjectId, usize> = HashMap::new();
        let mut plan_status_counts_by_project: HashMap<ProjectId, BTreeMap<PlanStatus, usize>> =
            HashMap::new();
        for plan in self.plans.values() {
            *plan_count_by_project
                .entry(plan.project_id.clone())
                .or_default() += 1;
            *plan_status_counts_by_project
                .entry(plan.project_id.clone())
                .or_insert_with(empty_plan_status_counts)
                .entry(plan.status)
                .or_default() += 1;
            if !plan.status.is_finished() {
                *active_plan_count_by_project
                    .entry(plan.project_id.clone())
                    .or_default() += 1;
            }
            if !self.projects.contains_key(&plan.project_id) {
                warnings.push(format!(
                    "plan '{}' references missing project '{}'",
                    plan.id.0, plan.project_id.0
                ));
            }
        }

        let mut task_count_by_plan: HashMap<PlanId, usize> = HashMap::new();
        let mut active_task_count_by_plan: HashMap<PlanId, usize> = HashMap::new();
        let mut status_counts_by_plan: HashMap<PlanId, BTreeMap<TaskStatus, usize>> =
            HashMap::new();
        for task in self.tasks.values() {
            *task_count_by_plan.entry(task.plan_id.clone()).or_default() += 1;
            *status_counts_by_plan
                .entry(task.plan_id.clone())
                .or_insert_with(empty_task_status_counts)
                .entry(task.status)
                .or_default() += 1;
            if !task.status.is_finished() {
                *active_task_count_by_plan
                    .entry(task.plan_id.clone())
                    .or_default() += 1;
            }
            if !self.plans.contains_key(&task.plan_id) {
                warnings.push(format!(
                    "task '{}' references missing plan '{}'",
                    task.id.0, task.plan_id.0
                ));
            }
        }

        let mut counts = OrchestrationCounts {
            total_projects: self.projects.len(),
            total_plans: self.plans.len(),
            total_tasks: self.tasks.len(),
            durable_execution_records: self
                .tasks
                .values()
                .filter(|task| task.execution.is_some())
                .count(),
            ..OrchestrationCounts::default()
        };
        let mut projects = self
            .projects
            .values()
            .map(|project| {
                if project.created_at_ms > now_ms {
                    warnings.push(format!(
                        "project '{}' has a future created_at_ms",
                        project.id.0
                    ));
                }
                if project.updated_at_ms > now_ms {
                    warnings.push(format!(
                        "project '{}' has a future updated_at_ms",
                        project.id.0
                    ));
                }
                match project.status {
                    ProjectStatus::Active => counts.active_projects += 1,
                    ProjectStatus::Archived => counts.archived_projects += 1,
                }
                ProjectSummary {
                    id: project.id.clone(),
                    slug: project.slug.clone(),
                    title: project.title.clone(),
                    description: project.description.clone(),
                    environment_ids: project.environment_ids.clone(),
                    codex_home: project.codex_home.clone(),
                    claude_home: project.claude_home.clone(),
                    opencode_home: project.opencode_home.clone(),
                    kimi_home: project.kimi_home.clone(),
                    status: project.status,
                    plan_count: plan_count_by_project
                        .get(&project.id)
                        .copied()
                        .unwrap_or_default(),
                    active_plan_count: active_plan_count_by_project
                        .get(&project.id)
                        .copied()
                        .unwrap_or_default(),
                    plan_status_counts: plan_status_counts_by_project
                        .get(&project.id)
                        .cloned()
                        .unwrap_or_else(empty_plan_status_counts),
                    task_count: self
                        .plans
                        .values()
                        .filter(|plan| plan.project_id == project.id)
                        .map(|plan| {
                            task_count_by_plan
                                .get(&plan.id)
                                .copied()
                                .unwrap_or_default()
                        })
                        .sum(),
                    active_task_count: self
                        .plans
                        .values()
                        .filter(|plan| plan.project_id == project.id)
                        .map(|plan| {
                            active_task_count_by_plan
                                .get(&plan.id)
                                .copied()
                                .unwrap_or_default()
                        })
                        .sum(),
                    task_status_counts: task_status_counts_for_project(
                        &project.id,
                        &self.plans,
                        &status_counts_by_plan,
                    ),
                    updated_at_ms: project.updated_at_ms,
                }
            })
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.id.0.cmp(&right.id.0));

        let mut plans = self
            .plans
            .values()
            .map(|plan| {
                if plan.created_at_ms > now_ms {
                    warnings.push(format!("plan '{}' has a future created_at_ms", plan.id.0));
                }
                if plan.updated_at_ms > now_ms {
                    warnings.push(format!("plan '{}' has a future updated_at_ms", plan.id.0));
                }
                if plan
                    .completed_at_ms
                    .is_some_and(|completed_at_ms| completed_at_ms > now_ms)
                {
                    warnings.push(format!("plan '{}' has a future completed_at_ms", plan.id.0));
                }
                if !plan.status.is_finished() {
                    counts.active_plans += 1;
                }
                if plan.status == PlanStatus::Blocked {
                    counts.blocked_plans += 1;
                }
                match plan.status {
                    PlanStatus::WaitingForValidation => counts.waiting_for_validation_plans += 1,
                    PlanStatus::Passed => counts.passed_plans += 1,
                    PlanStatus::Delivered => counts.delivered_plans += 1,
                    PlanStatus::Failed => counts.failed_plans += 1,
                    PlanStatus::Canceled => counts.canceled_plans += 1,
                    _ => {}
                }
                PlanSummary {
                    id: plan.id.clone(),
                    project_id: plan.project_id.clone(),
                    slug: plan.slug.clone(),
                    title: plan.title.clone(),
                    status: plan.status,
                    outcome: plan.outcome.clone(),
                    task_count: task_count_by_plan
                        .get(&plan.id)
                        .copied()
                        .unwrap_or_default(),
                    active_task_count: active_task_count_by_plan
                        .get(&plan.id)
                        .copied()
                        .unwrap_or_default(),
                    task_status_counts: status_counts_by_plan
                        .get(&plan.id)
                        .cloned()
                        .unwrap_or_else(empty_task_status_counts),
                    updated_at_ms: plan.updated_at_ms,
                }
            })
            .collect::<Vec<_>>();
        plans.sort_by(|left, right| left.id.0.cmp(&right.id.0));

        let mut tasks: Vec<TaskSummary> = self
            .tasks
            .values()
            .map(|task| {
                if task.created_at_ms > now_ms {
                    warnings.push(format!("task '{}' has a future created_at_ms", task.id.0));
                }
                if task.updated_at_ms > now_ms {
                    warnings.push(format!("task '{}' has a future updated_at_ms", task.id.0));
                }
                if task
                    .completed_at_ms
                    .is_some_and(|completed_at_ms| completed_at_ms > now_ms)
                {
                    warnings.push(format!("task '{}' has a future completed_at_ms", task.id.0));
                }

                let dependency_ids = dependencies_by_task
                    .get(&task.id)
                    .cloned()
                    .unwrap_or_default();
                let blocked_by = dependency_ids
                    .iter()
                    .filter(|dependency_id| {
                        self.tasks
                            .get(dependency_id)
                            .map(|dependency| {
                                self.dependency_readiness_blocker(dependency).is_some()
                            })
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let validation = self.validation_state_for_task(&task.id);
                let is_blocked = task.status == TaskStatus::Blocked || !blocked_by.is_empty();

                if !task.status.is_finished() {
                    counts.active_tasks += 1;
                }
                if is_blocked {
                    counts.blocked_tasks += 1;
                }
                match task.status {
                    TaskStatus::WaitingForValidation => counts.waiting_for_validation_tasks += 1,
                    TaskStatus::Passed => counts.passed_tasks += 1,
                    TaskStatus::Delivered => counts.delivered_tasks += 1,
                    TaskStatus::Failed => counts.failed_tasks += 1,
                    TaskStatus::Canceled => counts.canceled_tasks += 1,
                    _ => {}
                }

                TaskSummary {
                    id: task.id.clone(),
                    plan_id: task.plan_id.clone(),
                    slug: task.slug.clone(),
                    title: task.title.clone(),
                    status: task.status,
                    outcome: task.outcome.clone(),
                    evidence: task.evidence.clone(),
                    auto_schedule: task.auto_schedule,
                    run_spec: task.run_spec.clone(),
                    execution: task.execution.clone(),
                    parent: parent_by_child
                        .get(&task.id)
                        .and_then(|parents| parents.first().cloned()),
                    child_count: child_counts.get(&task.id).copied().unwrap_or_default(),
                    dependency_count: dependency_ids.len(),
                    blocked_by,
                    validator_count: validation.validator_ids.len(),
                    validation_blocked_by: validation.unapproved_validator_ids.clone(),
                    unapproved_validator_count: validation.unapproved_validator_ids.len(),
                    failed_validator_count: validation.failed_validator_ids.len(),
                    open_gate_count: if matches!(
                        task.status,
                        TaskStatus::Passed | TaskStatus::Delivered
                    ) {
                        0
                    } else {
                        task.gates.len()
                    },
                    failed_gate_count: if matches!(
                        task.status,
                        TaskStatus::Blocked | TaskStatus::Failed
                    ) {
                        task.blockers.len()
                    } else {
                        0
                    },
                    blocker_count: task.blockers.len(),
                    blockers: task.blockers.clone(),
                    updated_at_ms: task.updated_at_ms,
                }
            })
            .collect();
        tasks.sort_by(|left, right| left.id.0.cmp(&right.id.0));

        let mut task_edges = self
            .task_edges
            .iter()
            .map(|edge| TaskEdgeSummary {
                from: edge.from.clone(),
                to: edge.to.clone(),
                kind: edge.kind,
            })
            .collect::<Vec<_>>();
        task_edges.sort_by(|left, right| {
            left.from
                .0
                .cmp(&right.from.0)
                .then_with(|| left.to.0.cmp(&right.to.0))
                .then_with(|| edge_kind_rank(left.kind).cmp(&edge_kind_rank(right.kind)))
        });

        let mut executions = self
            .tasks
            .values()
            .filter_map(|task| {
                let execution = task.execution.as_ref()?;
                let expected_key = execution.runtime_key();
                if execution.created_at_ms > now_ms {
                    warnings.push(format!(
                        "execution '{}' has a future created_at_ms",
                        expected_key
                    ));
                }
                if execution.updated_at_ms > now_ms {
                    warnings.push(format!(
                        "execution '{}' has a future updated_at_ms",
                        expected_key
                    ));
                }
                if execution.last_seen_ms > now_ms {
                    warnings.push(format!(
                        "execution '{}' has a future last_seen_ms",
                        expected_key
                    ));
                }

                Some(ExecutionSummary {
                    endpoint_id: execution.endpoint_id.clone(),
                    execution_id: execution.execution_id.clone(),
                    launch_profile_id: execution.launch_profile_id.clone(),
                    workspace_path: execution.workspace_path.clone(),
                    bypass_permissions: execution.bypass_permissions,
                    task_id: task.id.clone(),
                    role: execution.role.clone(),
                    kind: execution.kind.clone(),
                    phase: execution.phase,
                    recovery: execution.recovery,
                    pane_id: execution.pane_id.clone(),
                    agent_name: execution.agent_name.clone(),
                    agent_kind: execution.agent_kind.clone(),
                    agent_session: execution.agent_session.clone(),
                    last_seen_ms: Some(execution.last_seen_ms),
                    runtime_state: None,
                })
            })
            .collect::<Vec<_>>();
        executions.sort_by(|left, right| {
            left.endpoint_id
                .cmp(&right.endpoint_id)
                .then_with(|| left.execution_id.cmp(&right.execution_id))
        });

        let cleanup_candidates = Vec::new();
        counts.cleanup_candidates = cleanup_candidates.len();
        warnings.sort();
        warnings.dedup();

        OrchestrationStatus {
            projects,
            plans,
            tasks,
            task_edges,
            executions,
            counts,
            cleanup_candidates,
            warnings,
        }
    }

    pub fn task_dependency_blockers(
        &self,
        task_id: &TaskId,
    ) -> Result<Vec<TaskDependencyBlocker>, String> {
        self.tasks
            .get(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;

        let mut blockers = self
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::DependsOn && &edge.from == task_id)
            .map(|edge| {
                let dependency = self
                    .tasks
                    .get(&edge.to)
                    .ok_or_else(|| format!("task '{}' not found", edge.to.0))?;
                Ok((dependency, self.dependency_readiness_blocker(dependency)))
            })
            .collect::<Result<Vec<_>, String>>()?
            .into_iter()
            .filter_map(|(dependency, blocker)| blocker.map(|blocker| (dependency, blocker)))
            .map(|(dependency, blocker)| TaskDependencyBlocker {
                task_id: dependency.id.clone(),
                status: dependency.status,
                reason: blocker.reason,
                validation_blocked_by: blocker.validation_blocked_by,
            })
            .collect::<Vec<_>>();
        blockers.sort_by(|left, right| left.task_id.0.cmp(&right.task_id.0));
        Ok(blockers)
    }

    pub fn task_superseded_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>, String> {
        self.tasks
            .get(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;
        let mut superseders = self
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::Supersedes && &edge.to == task_id)
            .map(|edge| {
                self.tasks
                    .get(&edge.from)
                    .map(|task| task.id.clone())
                    .ok_or_else(|| format!("task '{}' not found", edge.from.0))
            })
            .collect::<Result<Vec<_>, String>>()?;
        superseders.sort_by(|left, right| left.0.cmp(&right.0));
        superseders.dedup();
        Ok(superseders)
    }

    pub fn create_project(&mut self, input: CreateProject, now_ms: u64) -> Result<Project, String> {
        if input.title.trim().is_empty() {
            return Err("project title must not be empty".into());
        }
        if input.description.trim().is_empty() {
            return Err("project description must not be empty".into());
        }
        validate_environment_ids(&input.environment_ids)?;
        for (field, home) in [
            ("codex_home", input.codex_home.as_deref()),
            ("claude_home", input.claude_home.as_deref()),
            ("opencode_home", input.opencode_home.as_deref()),
            ("kimi_home", input.kimi_home.as_deref()),
        ] {
            validate_coder_home(field, home)?;
        }
        let id = ProjectId(Uuid::new_v4().to_string());
        let base_slug = input.slug.as_deref().unwrap_or(&input.title);
        let slug = self.unique_project_slug(base_slug);
        let project = Project {
            id: id.clone(),
            slug,
            title: input.title,
            description: input.description,
            environment_ids: input.environment_ids,
            codex_home: input.codex_home,
            claude_home: input.claude_home,
            opencode_home: input.opencode_home,
            kimi_home: input.kimi_home,
            status: ProjectStatus::Active,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        self.projects.insert(id, project.clone());
        Ok(project)
    }

    pub fn update_project(
        &mut self,
        project_id: &ProjectId,
        update: UpdateProject,
        now_ms: u64,
    ) -> Result<Project, String> {
        if let Some(ids) = &update.environment_ids {
            validate_environment_ids(ids)?;
        }
        for (field, home) in [
            ("codex_home", &update.codex_home),
            ("claude_home", &update.claude_home),
            ("opencode_home", &update.opencode_home),
            ("kimi_home", &update.kimi_home),
        ] {
            validate_coder_home(field, home.as_ref().and_then(|value| value.as_deref()))?;
        }
        let project = self
            .projects
            .get_mut(project_id)
            .ok_or_else(|| format!("project '{}' not found", project_id.0))?;
        if let Some(ids) = update.environment_ids {
            project.environment_ids = ids;
        }
        if let Some(home) = update.codex_home {
            project.codex_home = home;
        }
        if let Some(home) = update.claude_home {
            project.claude_home = home;
        }
        if let Some(home) = update.opencode_home {
            project.opencode_home = home;
        }
        if let Some(home) = update.kimi_home {
            project.kimi_home = home;
        }
        project.updated_at_ms = now_ms;
        Ok(project.clone())
    }

    pub fn update_project_status(
        &mut self,
        project_id: &ProjectId,
        status: ProjectStatus,
        now_ms: u64,
    ) -> Result<Project, String> {
        let project = self
            .projects
            .get_mut(project_id)
            .ok_or_else(|| format!("project '{}' not found", project_id.0))?;
        project.status = status;
        project.updated_at_ms = now_ms;
        Ok(project.clone())
    }

    pub fn create_plan(&mut self, input: CreatePlan, now_ms: u64) -> Result<Plan, String> {
        if !self.projects.contains_key(&input.project_id) {
            return Err(format!("project '{}' not found", input.project_id.0));
        }
        if input.title.trim().is_empty() {
            return Err("plan title must not be empty".into());
        }
        if input.brief.trim().is_empty() {
            return Err("plan brief must not be empty".into());
        }

        let id = PlanId(format!("plan-{}", self.next_plan_id));
        self.next_plan_id += 1;
        let base_slug = input.slug.as_deref().unwrap_or(&input.title);
        let slug = self.unique_plan_slug(&input.project_id, base_slug);
        let plan = Plan {
            id: id.clone(),
            project_id: input.project_id,
            slug,
            title: input.title,
            brief: input.brief,
            instructions: normalize_plan_instructions(input.instructions),
            status: PlanStatus::Backlog,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            completed_at_ms: None,
            outcome: None,
        };
        self.plans.insert(id, plan.clone());
        Ok(plan)
    }

    pub fn update_plan(
        &mut self,
        plan_id: &PlanId,
        update: UpdatePlan,
        now_ms: u64,
    ) -> Result<Plan, String> {
        if let Some(title) = update.title.as_ref() {
            if title.trim().is_empty() {
                return Err("plan title must not be empty".into());
            }
        }
        if let Some(brief) = update.brief.as_ref() {
            if brief.trim().is_empty() {
                return Err("plan brief must not be empty".into());
            }
        }
        let plan = self
            .plans
            .get_mut(plan_id)
            .ok_or_else(|| format!("plan '{}' not found", plan_id.0))?;
        if let Some(title) = update.title {
            plan.title = title;
        }
        if let Some(brief) = update.brief {
            plan.brief = brief;
        }
        if let Some(instructions) = update.instructions {
            plan.instructions = normalize_plan_instructions(Some(instructions));
        }
        plan.updated_at_ms = now_ms;
        Ok(plan.clone())
    }

    pub fn update_plan_status(
        &mut self,
        plan_id: &PlanId,
        status: PlanStatus,
        outcome: Option<String>,
        now_ms: u64,
    ) -> Result<Plan, String> {
        let plan = self
            .plans
            .get_mut(plan_id)
            .ok_or_else(|| format!("plan '{}' not found", plan_id.0))?;
        plan.status = status;
        if let Some(outcome) = outcome {
            plan.outcome = Some(outcome);
        }
        plan.updated_at_ms = now_ms;
        plan.completed_at_ms = status.is_finished().then_some(now_ms);
        Ok(plan.clone())
    }

    pub fn create_task(&mut self, input: CreateTask, now_ms: u64) -> Result<Task, String> {
        if !self.plans.contains_key(&input.plan_id) {
            return Err(format!("plan '{}' not found", input.plan_id.0));
        }
        if input.title.trim().is_empty() {
            return Err("task title must not be empty".into());
        }
        if input.objective.trim().is_empty() {
            return Err("task objective must not be empty".into());
        }
        if let Some(run_spec) = input.run_spec.as_ref() {
            validate_task_run_spec(run_spec)?;
        }

        let id = TaskId(format!("task-{}", self.next_task_id));
        self.next_task_id += 1;
        let base_slug = input.slug.as_deref().unwrap_or(&input.title);
        let slug = self.unique_task_slug(&input.plan_id, base_slug);
        let task = Task {
            id: id.clone(),
            plan_id: input.plan_id,
            slug,
            title: input.title,
            objective: input.objective,
            scope: input.scope,
            status: TaskStatus::Backlog,
            execution: None,
            gates: input.gates,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            completed_at_ms: None,
            outcome: None,
            blockers: Vec::new(),
            evidence: Vec::new(),
            auto_schedule: input.auto_schedule,
            run_spec: input.run_spec,
            launch_hints: input.launch_hints,
        };

        self.tasks.insert(id, task.clone());
        Ok(task)
    }

    pub fn update_task(
        &mut self,
        task_id: &TaskId,
        update: UpdateTask,
        now_ms: u64,
    ) -> Result<Task, String> {
        if let Some(title) = update.title.as_ref() {
            if title.trim().is_empty() {
                return Err("task title must not be empty".into());
            }
        }
        if let Some(objective) = update.objective.as_ref() {
            if objective.trim().is_empty() {
                return Err("task objective must not be empty".into());
            }
        }
        if let Some(Some(run_spec)) = update.run_spec.as_ref() {
            validate_task_run_spec(run_spec)?;
        }
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;
        if let Some(title) = update.title {
            task.title = title;
        }
        if let Some(objective) = update.objective {
            task.objective = objective;
        }
        if let Some(include_paths) = update.scope.include_paths {
            task.scope.include_paths = include_paths;
        }
        if let Some(exclude_paths) = update.scope.exclude_paths {
            task.scope.exclude_paths = exclude_paths;
        }
        if let Some(notes) = update.scope.notes {
            task.scope.notes = notes;
        }
        if let Some(gates) = update.gates {
            task.gates = gates;
        }
        if let Some(auto_schedule) = update.auto_schedule {
            task.auto_schedule = auto_schedule;
        }
        if let Some(run_spec) = update.run_spec {
            task.run_spec = run_spec;
        }
        if let Some(hints) = update.launch_hints {
            task.launch_hints = hints;
        }
        task.updated_at_ms = now_ms;
        Ok(task.clone())
    }

    pub fn record_execution(
        &mut self,
        task_id: &TaskId,
        mut execution: TaskExecution,
        now_ms: u64,
    ) -> Result<TaskExecution, String> {
        if execution.execution_id.trim().is_empty() {
            return Err("execution execution_id must not be empty".into());
        }
        if execution.endpoint_id.trim().is_empty() {
            return Err("execution endpoint_id must not be empty".into());
        }
        if execution.launch_profile_id.trim().is_empty() {
            return Err("execution launch_profile_id must not be empty".into());
        }
        if execution.workspace_path.trim().is_empty() {
            return Err("execution workspace_path must not be empty".into());
        }

        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;
        // Serialize attempts: a second execution may take a task's binding
        // only when the recorded one is provably finished. This guard makes
        // a stale launch snapshot unable to overwrite a newer reservation.
        if let Some(existing) = task.execution.as_ref() {
            if existing.execution_id == execution.execution_id
                && existing.runtime_generation.is_some()
                && existing.runtime_generation != execution.runtime_generation
            {
                return Err("an execution's resource generation is immutable; create a fresh attempt after reconciliation".into());
            }
            if existing.execution_id != execution.execution_id
                && !existing.phase.allows_replacement()
            {
                return Err(format!(
                    "task '{}' already has execution '{}' in phase {}; only an exited, stopped, or failed execution can be replaced",
                    task_id.0,
                    existing.execution_id,
                    existing.phase.as_str()
                ));
            }
        }
        let same_execution = task
            .execution
            .as_ref()
            .is_some_and(|existing| existing.execution_id == execution.execution_id);
        if !same_execution {
            if let Some(previous) = task.execution.as_ref() {
                self.retained_executions.insert(
                    previous.execution_id.clone(),
                    RetainedExecution {
                        task_id: task_id.clone(),
                        execution: previous.clone(),
                    },
                );
            }
        }
        let created_at_ms = if same_execution {
            task.execution
                .as_ref()
                .map(|existing| existing.created_at_ms)
                .unwrap_or(now_ms)
        } else {
            now_ms
        };
        execution.created_at_ms = created_at_ms;
        execution.updated_at_ms = now_ms;
        execution.last_seen_ms = now_ms;
        task.execution = Some(execution.clone());
        task.updated_at_ms = now_ms;
        Ok(execution)
    }

    pub fn add_task_edge(&mut self, edge: CreateTaskEdge, now_ms: u64) -> Result<TaskEdge, String> {
        self.validate_edge(&edge)?;
        let task_edge = TaskEdge {
            from: edge.from,
            to: edge.to,
            kind: edge.kind,
            created_at_ms: now_ms,
            note: edge.note,
        };
        self.task_edges.push(task_edge.clone());
        Ok(task_edge)
    }

    pub fn remove_task_edge(
        &mut self,
        from: &TaskId,
        to: &TaskId,
        kind: TaskEdgeKind,
        now_ms: u64,
    ) -> Result<(), String> {
        let _ = now_ms;
        let original_len = self.task_edges.len();
        self.task_edges
            .retain(|edge| !(&edge.from == from && &edge.to == to && edge.kind == kind));
        if self.task_edges.len() == original_len {
            return Err("task edge not found".into());
        }
        Ok(())
    }

    pub fn update_task_status(
        &mut self,
        task_id: &TaskId,
        status: TaskStatus,
        now_ms: u64,
    ) -> Result<Task, String> {
        if matches!(status, TaskStatus::Planned | TaskStatus::Running) {
            self.ensure_task_not_superseded(task_id)?;
        }
        if status == TaskStatus::Planned {
            self.ensure_dependencies_finished(task_id)?;
        }
        if status == TaskStatus::Delivered {
            self.ensure_children_delivered(task_id)?;
        }

        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;
        task.status = status;
        task.updated_at_ms = now_ms;
        task.completed_at_ms = status.is_finished().then_some(now_ms);
        Ok(task.clone())
    }

    fn ensure_task_not_superseded(&self, task_id: &TaskId) -> Result<(), String> {
        let superseded_by = self.task_superseded_by(task_id)?;
        if superseded_by.is_empty() {
            return Ok(());
        }
        Err(format!(
            "task '{}' is superseded by {}",
            task_id.0,
            format_task_ids(&superseded_by)
        ))
    }

    fn validate_edge(&self, edge: &CreateTaskEdge) -> Result<(), String> {
        if !self.tasks.contains_key(&edge.from) {
            return Err(format!("task '{}' not found", edge.from.0));
        }
        if !self.tasks.contains_key(&edge.to) {
            return Err(format!("task '{}' not found", edge.to.0));
        }
        if edge.from == edge.to {
            return Err("a task edge cannot point to itself".into());
        }
        let from_plan = &self
            .tasks
            .get(&edge.from)
            .ok_or_else(|| format!("task '{}' not found", edge.from.0))?
            .plan_id;
        let to_plan = &self
            .tasks
            .get(&edge.to)
            .ok_or_else(|| format!("task '{}' not found", edge.to.0))?
            .plan_id;
        if from_plan != to_plan {
            return Err("task edges cannot cross plan boundaries in v1".into());
        }
        if self.task_edges.iter().any(|existing| {
            existing.from == edge.from && existing.to == edge.to && existing.kind == edge.kind
        }) {
            return Err("task edge already exists".into());
        }

        match edge.kind {
            TaskEdgeKind::DependsOn => {
                if self.has_path(&edge.to, &edge.from, TaskEdgeKind::DependsOn) {
                    return Err("DependsOn edge would create a cycle".into());
                }
            }
            TaskEdgeKind::ParentOf => {
                if self.task_edges.iter().any(|existing| {
                    existing.kind == TaskEdgeKind::ParentOf && existing.to == edge.to
                }) {
                    return Err("ParentOf edges allow only one parent per task in v1".into());
                }
                if self.has_path(&edge.to, &edge.from, TaskEdgeKind::ParentOf) {
                    return Err("ParentOf edge would create a cycle".into());
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn unique_project_slug(&self, value: &str) -> String {
        let base = sanitize_slug(value);
        if !self.project_slug_exists(&base) {
            return base;
        }

        let mut suffix = 2;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.project_slug_exists(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn project_slug_exists(&self, slug: &str) -> bool {
        self.projects.values().any(|project| project.slug == slug)
    }

    fn unique_plan_slug(&self, project_id: &ProjectId, value: &str) -> String {
        let base = sanitize_slug(value);
        if !self.plan_slug_exists(project_id, &base) {
            return base;
        }

        let mut suffix = 2;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.plan_slug_exists(project_id, &candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn plan_slug_exists(&self, project_id: &ProjectId, slug: &str) -> bool {
        self.plans
            .values()
            .any(|plan| &plan.project_id == project_id && plan.slug == slug)
    }

    fn unique_task_slug(&self, plan_id: &PlanId, value: &str) -> String {
        let base = sanitize_slug(value);
        if !self.task_slug_exists(plan_id, &base) {
            return base;
        }

        let mut suffix = 2;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !self.task_slug_exists(plan_id, &candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn task_slug_exists(&self, plan_id: &PlanId, slug: &str) -> bool {
        self.tasks
            .values()
            .any(|task| &task.plan_id == plan_id && task.slug == slug)
    }

    fn has_path(&self, start: &TaskId, target: &TaskId, kind: TaskEdgeKind) -> bool {
        let mut stack = vec![start.clone()];
        let mut visited = HashSet::new();
        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) {
                continue;
            }
            if &current == target {
                return true;
            }
            for edge in self
                .task_edges
                .iter()
                .filter(|edge| edge.kind == kind && edge.from == current)
            {
                stack.push(edge.to.clone());
            }
        }
        false
    }

    fn ensure_dependencies_finished(&self, task_id: &TaskId) -> Result<(), String> {
        self.tasks
            .get(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;

        for edge in self
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::DependsOn && &edge.from == task_id)
        {
            let dependency = self
                .tasks
                .get(&edge.to)
                .ok_or_else(|| format!("task '{}' not found", edge.to.0))?;
            if let Some(blocker) = self.dependency_readiness_blocker(dependency) {
                return Err(format!(
                    "task '{}' is blocked by dependency '{}': {}",
                    task_id.0, edge.to.0, blocker.reason
                ));
            }
        }
        Ok(())
    }

    fn dependency_readiness_blocker(&self, dependency: &Task) -> Option<DependencyBlocker> {
        if !dependency_status_allows_readiness(dependency.status) {
            return Some(DependencyBlocker {
                reason: format!("status {:?} is not ready", dependency.status),
                validation_blocked_by: Vec::new(),
            });
        }

        let validation = self.validation_state_for_task(&dependency.id);
        if !validation.unapproved_validator_ids.is_empty() {
            return Some(DependencyBlocker {
                reason: format!(
                    "validation not approved by {}",
                    format_task_ids(&validation.unapproved_validator_ids)
                ),
                validation_blocked_by: validation.unapproved_validator_ids,
            });
        }

        None
    }

    fn validation_state_for_task(&self, task_id: &TaskId) -> TaskValidationState {
        let mut validator_ids = self
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::Validates && &edge.to == task_id)
            .filter_map(|edge| self.tasks.get(&edge.from).map(|task| task.id.clone()))
            .collect::<Vec<_>>();
        validator_ids.sort_by(|left, right| left.0.cmp(&right.0));
        validator_ids.dedup();

        let unapproved_validator_ids = validator_ids
            .iter()
            .filter(|validator_id| {
                self.tasks
                    .get(*validator_id)
                    .map(|validator| !validation_status_approves(validator.status))
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        let failed_validator_ids = validator_ids
            .iter()
            .filter(|validator_id| {
                self.tasks
                    .get(*validator_id)
                    .map(|validator| validation_status_rejects(validator.status))
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();

        TaskValidationState {
            validator_ids,
            unapproved_validator_ids,
            failed_validator_ids,
        }
    }

    fn ensure_children_delivered(&self, task_id: &TaskId) -> Result<(), String> {
        self.tasks
            .get(task_id)
            .ok_or_else(|| format!("task '{}' not found", task_id.0))?;

        for edge in self
            .task_edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::ParentOf && &edge.from == task_id)
        {
            let child = self
                .tasks
                .get(&edge.to)
                .ok_or_else(|| format!("task '{}' not found", edge.to.0))?;
            if !child_status_allows_parent_delivery(child) {
                return Err(format!(
                    "task '{}' cannot be delivered before child '{}' is delivered, canceled, or failed with an outcome",
                    task_id.0, edge.to.0
                ));
            }
        }
        Ok(())
    }
}

fn empty_task_status_counts() -> BTreeMap<TaskStatus, usize> {
    TaskStatus::ALL
        .into_iter()
        .map(|status| (status, 0))
        .collect()
}

fn empty_plan_status_counts() -> BTreeMap<PlanStatus, usize> {
    PlanStatus::ALL
        .into_iter()
        .map(|status| (status, 0))
        .collect()
}

fn task_status_counts_for_project(
    project_id: &ProjectId,
    plans: &HashMap<PlanId, Plan>,
    status_counts_by_plan: &HashMap<PlanId, BTreeMap<TaskStatus, usize>>,
) -> BTreeMap<TaskStatus, usize> {
    let mut counts = empty_task_status_counts();
    for plan in plans.values().filter(|plan| &plan.project_id == project_id) {
        if let Some(plan_counts) = status_counts_by_plan.get(&plan.id) {
            for (status, count) in plan_counts {
                *counts.entry(*status).or_default() += count;
            }
        }
    }
    counts
}

fn normalize_plan_instructions(instructions: Option<String>) -> Option<String> {
    instructions
        .map(|instructions| instructions.trim().to_owned())
        .filter(|instructions| !instructions.is_empty())
}

fn dependency_status_allows_readiness(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Passed | TaskStatus::Delivered | TaskStatus::Canceled
    )
}

fn validation_status_approves(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Passed | TaskStatus::Delivered)
}

fn validation_status_rejects(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Blocked | TaskStatus::Failed)
}

fn child_status_allows_parent_delivery(child: &Task) -> bool {
    matches!(child.status, TaskStatus::Delivered | TaskStatus::Canceled)
        || (child.status == TaskStatus::Failed
            && child
                .outcome
                .as_deref()
                .is_some_and(|outcome| !outcome.trim().is_empty()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DependencyBlocker {
    reason: String,
    validation_blocked_by: Vec<TaskId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TaskValidationState {
    validator_ids: Vec<TaskId>,
    unapproved_validator_ids: Vec<TaskId>,
    failed_validator_ids: Vec<TaskId>,
}

fn format_task_ids(task_ids: &[TaskId]) -> String {
    task_ids
        .iter()
        .map(|task_id| task_id.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_task_run_spec(run_spec: &TaskRunSpec) -> Result<(), String> {
    run_spec.launch_options.validate()?;
    if run_spec.endpoint_id.trim().is_empty() {
        return Err("task run_spec endpoint_id must not be empty".into());
    }
    if run_spec.launch_profile_id.trim().is_empty() {
        return Err("task run_spec launch_profile_id must not be empty".into());
    }
    if run_spec.workspace_path.trim().is_empty() {
        return Err("task run_spec workspace_path must not be empty".into());
    }
    if run_spec.role.trim().is_empty() {
        return Err("task run_spec role must not be empty".into());
    }
    if run_spec.kind.trim().is_empty() {
        return Err("task run_spec kind must not be empty".into());
    }
    match run_spec.template.trim() {
        "task" | "validate" | "review" | "quality-guard" => {}
        other => {
            return Err(format!(
                "task run_spec template must be one of task, validate, review, quality-guard; got '{other}'"
            ));
        }
    }
    if run_spec.instruction.trim().is_empty() {
        return Err("task run_spec instruction must not be empty".into());
    }
    Ok(())
}

fn edge_kind_rank(kind: TaskEdgeKind) -> u8 {
    match kind {
        TaskEdgeKind::ParentOf => 0,
        TaskEdgeKind::DependsOn => 1,
        TaskEdgeKind::Validates => 2,
        TaskEdgeKind::Audits => 3,
        TaskEdgeKind::Supersedes => 4,
        TaskEdgeKind::Related => 5,
    }
}

fn sanitize_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else {
            Some('-')
        };

        if let Some(ch) = next {
            if ch == '-' {
                if !last_was_dash && !slug.is_empty() {
                    slug.push(ch);
                    last_was_dash = true;
                }
            } else {
                slug.push(ch);
                last_was_dash = false;
            }
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "task".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{
        ser::{Error as SerError, Impossible, SerializeStruct},
        Serialize, Serializer,
    };
    use std::{error::Error, fmt};

    fn fixture_project_id() -> ProjectId {
        ProjectId("fixture-project".into())
    }

    fn fixture_plan_id() -> PlanId {
        PlanId("fixture-plan".into())
    }

    fn create_task(title: &str) -> CreateTask {
        CreateTask {
            launch_hints: Default::default(),
            plan_id: fixture_plan_id(),
            title: title.into(),
            objective: format!("Implement {title}"),
            scope: TaskScope::default(),
            gates: Vec::new(),
            slug: None,
            auto_schedule: false,
            run_spec: None,
        }
    }

    fn state_with_project() -> OrchestrationState {
        let mut state = OrchestrationState::new();
        let project_id = fixture_project_id();
        state.projects.insert(
            project_id.clone(),
            Project {
                environment_ids: Default::default(),
                id: project_id,
                slug: "project".into(),
                title: "Project".into(),
                description: "Test project".into(),
                codex_home: None,
                claude_home: None,
                opencode_home: None,
                kimi_home: None,
                status: ProjectStatus::Active,
                created_at_ms: 1,
                updated_at_ms: 1,
            },
        );
        state.plans.insert(
            fixture_plan_id(),
            Plan {
                id: fixture_plan_id(),
                project_id: fixture_project_id(),
                slug: "plan".into(),
                title: "Plan".into(),
                brief: "Detailed plan brief for test tasks.".into(),
                instructions: None,
                status: PlanStatus::Backlog,
                created_at_ms: 1,
                updated_at_ms: 1,
                completed_at_ms: None,
                outcome: None,
            },
        );
        state
    }

    #[test]
    fn project_homes_are_optional_validated_and_clearable() {
        let mut state = OrchestrationState::new();
        let project = state
            .create_project(
                CreateProject {
                    title: "Homes".into(),
                    description: "Config homes".into(),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        assert!(project.codex_home.is_none());
        let mut legacy = serde_json::to_value(&project).unwrap();
        for field in ["codex_home", "claude_home", "opencode_home", "kimi_home"] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(serde_json::from_value::<Project>(legacy).unwrap(), project);
        let update: UpdateProject = serde_json::from_value(serde_json::json!({
            "codex_home": "/node/codex", "claude_home": "~/claude",
            "opencode_home": "/node/opencode", "kimi_home": "/node/kimi"
        }))
        .unwrap();
        let configured = state.update_project(&project.id, update, 101).unwrap();
        assert_eq!(configured.coder_home("codex"), Some("/node/codex"));
        assert_eq!(configured.coder_home("claude"), Some("~/claude"));
        assert_eq!(configured.coder_home("opencode"), Some("/node/opencode"));
        assert_eq!(configured.coder_home("kimi"), Some("/node/kimi"));
        for invalid in ["", "  ", "relative", "/path\ncommand", "/nul\0"] {
            let update: UpdateProject = serde_json::from_value(serde_json::json!({
                "codex_home": "/would-change", "kimi_home": invalid
            }))
            .unwrap();
            assert!(state.update_project(&project.id, update, 102).is_err());
            assert_eq!(state.projects[&project.id], configured);
            assert!(state
                .create_project(
                    CreateProject {
                        title: "Invalid".into(),
                        description: "Invalid".into(),
                        codex_home: Some(invalid.into()),
                        ..Default::default()
                    },
                    102
                )
                .is_err());
        }
        let update: UpdateProject =
            serde_json::from_value(serde_json::json!({"codex_home": null})).unwrap();
        let cleared = state.update_project(&project.id, update, 103).unwrap();
        assert!(cleared.codex_home.is_none());
        assert_eq!(cleared.claude_home, configured.claude_home);
        assert_eq!(cleared.opencode_home, configured.opencode_home);
        assert_eq!(cleared.kimi_home, configured.kimi_home);
        assert_eq!(cleared.updated_at_ms, 103);
        let summary = state.orchestration_status(103).projects.remove(0);
        assert_eq!(summary.kimi_home, cleared.kimi_home);
    }

    #[test]
    fn project_creation_uses_uuid_ids_and_unique_slugs() {
        let mut state = OrchestrationState::new();

        let first = state
            .create_project(
                CreateProject {
                    title: "Project".into(),
                    description: "First test project".into(),
                    slug: None,
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let second = state
            .create_project(
                CreateProject {
                    title: "Project".into(),
                    description: "Second test project".into(),
                    slug: None,
                    ..Default::default()
                },
                101,
            )
            .unwrap();

        assert!(Uuid::parse_str(&first.id.0).is_ok());
        assert!(Uuid::parse_str(&second.id.0).is_ok());
        assert_ne!(first.id, second.id);
        assert_eq!(first.slug, "project");
        assert_eq!(second.slug, "project-2");
    }

    #[test]
    fn plan_instructions_are_stored_trimmed_and_clearable() {
        let mut state = OrchestrationState::new();
        let project = state
            .create_project(
                CreateProject {
                    title: "Project".into(),
                    description: "Test project".into(),
                    slug: None,
                    ..Default::default()
                },
                100,
            )
            .unwrap();

        let plan = state
            .create_plan(
                CreatePlan {
                    project_id: project.id,
                    title: "Plan".into(),
                    brief: "Detailed plan brief.".into(),
                    instructions: Some("  Keep work scoped to this plan.  ".into()),
                    slug: None,
                },
                101,
            )
            .unwrap();

        assert_eq!(
            plan.instructions.as_deref(),
            Some("Keep work scoped to this plan.")
        );

        let updated = state
            .update_plan(
                &plan.id,
                UpdatePlan {
                    title: None,
                    brief: None,
                    instructions: Some("".into()),
                },
                102,
            )
            .unwrap();

        assert_eq!(updated.instructions, None);
    }

    fn execution() -> TaskExecution {
        TaskExecution {
            launch_options: Default::default(),
            native_session_name: None,
            group: ExecutionGroup::Work,
            inspection: false,
            resumed_from: None,
            pane_closed: false,
            report: None,
            execution_id: "exec-a".into(),
            endpoint_id: "node-a".into(),
            runtime_generation: None,
            launch_profile_id: "codex".into(),
            launch_args: Vec::new(),
            launch_env: BTreeMap::new(),
            bypass_permissions: false,
            workspace_path: "/workspace/project".into(),
            role: "implementation-worker".into(),
            kind: "codex".into(),
            skills: vec!["rust".into()],
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            terminal_id: None,
            agent_name: None,
            agent_kind: None,
            agent_session: None,
            phase: ExecutionPhase::Pending,
            recovery: ExecutionRecovery::NeedsReconciliation,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_seen_ms: 0,
        }
    }

    fn run_spec() -> TaskRunSpec {
        TaskRunSpec {
            launch_options: Default::default(),
            endpoint_id: "local".into(),
            launch_profile_id: "codex".into(),
            workspace_path: "/workspace/project".into(),
            bypass_permissions: false,
            role: "implementation-worker".into(),
            kind: "implementation".into(),
            skills: vec!["taskr-developer".into()],
            template: "task".into(),
            instruction: "Implement this task and report validation.".into(),
        }
    }

    #[test]
    fn project_environment_assignments_are_explicit_and_updates_are_atomic() {
        let mut state = OrchestrationState::new();
        let project = state
            .create_project(
                CreateProject {
                    title: "Assignments".into(),
                    description: "Verify project environment policy".into(),
                    environment_ids: vec!["env-main".into(), "env-other".into()],
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        assert!(project.require_environment("env-main").is_ok());
        assert!(project.require_environment("env-unassigned").is_err());
        let failed = state.update_project(
            &project.id,
            UpdateProject {
                environment_ids: Some(vec!["env-main".into(), "env-main".into()]),
                ..Default::default()
            },
            2,
        );
        assert!(failed.is_err());
        assert_eq!(state.projects[&project.id], project);
        let unchanged = state
            .update_project(&project.id, UpdateProject::default(), 3)
            .unwrap();
        assert_eq!(unchanged.environment_ids, project.environment_ids);
        let cleared = state
            .update_project(
                &project.id,
                UpdateProject {
                    environment_ids: Some(vec![]),
                    ..Default::default()
                },
                4,
            )
            .unwrap();
        assert!(cleared.require_environment("env-main").is_err());
        let reloaded: OrchestrationState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(reloaded.projects[&project.id].environment_ids.is_empty());
        assert!(state
            .create_project(
                CreateProject {
                    title: "Invalid".into(),
                    description: "Invalid assignment".into(),
                    environment_ids: vec![" ".into()],
                    ..Default::default()
                },
                5
            )
            .is_err());
    }

    #[test]
    fn model_launch_options_reject_empty_or_option_like_values() {
        for value in ["", "--other-option", "model with spaces", "model\nvalue"] {
            assert!(AgentLaunchOptions {
                model: Some(value.into()),
                reasoning_effort: None
            }
            .validate()
            .is_err());
        }
        assert!(AgentLaunchOptions {
            model: Some("glm-5.3".into()),
            reasoning_effort: Some("max".into())
        }
        .validate()
        .is_ok());
    }

    #[derive(Debug)]
    struct FieldNameError(String);

    impl fmt::Display for FieldNameError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl Error for FieldNameError {}

    impl SerError for FieldNameError {
        fn custom<T: fmt::Display>(msg: T) -> Self {
            Self(msg.to_string())
        }
    }

    struct FieldNameSerializer;

    struct FieldNameStruct {
        fields: Vec<String>,
    }

    impl SerializeStruct for FieldNameStruct {
        type Ok = Vec<String>;
        type Error = FieldNameError;

        fn serialize_field<T: ?Sized + Serialize>(
            &mut self,
            key: &'static str,
            _value: &T,
        ) -> Result<(), Self::Error> {
            self.fields.push(key.into());
            Ok(())
        }

        fn end(self) -> Result<Self::Ok, Self::Error> {
            Ok(self.fields)
        }
    }

    macro_rules! unsupported_serializer_method {
        ($name:ident($($arg:ident: $ty:ty),*) -> $ok:ty) => {
            fn $name(self, $($arg: $ty),*) -> Result<$ok, Self::Error> {
                $(let _ = $arg;)*
                Err(FieldNameError(format!(
                    "{} is not supported by FieldNameSerializer",
                    stringify!($name)
                )))
            }
        };
    }

    impl Serializer for FieldNameSerializer {
        type Ok = Vec<String>;
        type Error = FieldNameError;
        type SerializeSeq = Impossible<Vec<String>, FieldNameError>;
        type SerializeTuple = Impossible<Vec<String>, FieldNameError>;
        type SerializeTupleStruct = Impossible<Vec<String>, FieldNameError>;
        type SerializeTupleVariant = Impossible<Vec<String>, FieldNameError>;
        type SerializeMap = Impossible<Vec<String>, FieldNameError>;
        type SerializeStruct = FieldNameStruct;
        type SerializeStructVariant = Impossible<Vec<String>, FieldNameError>;

        unsupported_serializer_method!(serialize_bool(v: bool) -> Self::Ok);
        unsupported_serializer_method!(serialize_i8(v: i8) -> Self::Ok);
        unsupported_serializer_method!(serialize_i16(v: i16) -> Self::Ok);
        unsupported_serializer_method!(serialize_i32(v: i32) -> Self::Ok);
        unsupported_serializer_method!(serialize_i64(v: i64) -> Self::Ok);
        unsupported_serializer_method!(serialize_u8(v: u8) -> Self::Ok);
        unsupported_serializer_method!(serialize_u16(v: u16) -> Self::Ok);
        unsupported_serializer_method!(serialize_u32(v: u32) -> Self::Ok);
        unsupported_serializer_method!(serialize_u64(v: u64) -> Self::Ok);
        unsupported_serializer_method!(serialize_f32(v: f32) -> Self::Ok);
        unsupported_serializer_method!(serialize_f64(v: f64) -> Self::Ok);
        unsupported_serializer_method!(serialize_char(v: char) -> Self::Ok);
        unsupported_serializer_method!(serialize_str(v: &str) -> Self::Ok);
        unsupported_serializer_method!(serialize_bytes(v: &[u8]) -> Self::Ok);
        unsupported_serializer_method!(serialize_none() -> Self::Ok);
        unsupported_serializer_method!(serialize_unit() -> Self::Ok);
        unsupported_serializer_method!(serialize_unit_struct(name: &'static str) -> Self::Ok);
        unsupported_serializer_method!(serialize_unit_variant(name: &'static str, variant_index: u32, variant: &'static str) -> Self::Ok);
        unsupported_serializer_method!(serialize_seq(len: Option<usize>) -> Self::SerializeSeq);
        unsupported_serializer_method!(serialize_tuple(len: usize) -> Self::SerializeTuple);
        unsupported_serializer_method!(serialize_tuple_struct(name: &'static str, len: usize) -> Self::SerializeTupleStruct);
        unsupported_serializer_method!(serialize_tuple_variant(name: &'static str, variant_index: u32, variant: &'static str, len: usize) -> Self::SerializeTupleVariant);
        unsupported_serializer_method!(serialize_map(len: Option<usize>) -> Self::SerializeMap);
        unsupported_serializer_method!(serialize_struct_variant(name: &'static str, variant_index: u32, variant: &'static str, len: usize) -> Self::SerializeStructVariant);

        fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
            let _ = value;
            Err(FieldNameError(
                "serialize_some is not supported by FieldNameSerializer".into(),
            ))
        }

        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            name: &'static str,
            value: &T,
        ) -> Result<Self::Ok, Self::Error> {
            let _ = (name, value);
            Err(FieldNameError(
                "serialize_newtype_struct is not supported by FieldNameSerializer".into(),
            ))
        }

        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            name: &'static str,
            variant_index: u32,
            variant: &'static str,
            value: &T,
        ) -> Result<Self::Ok, Self::Error> {
            let _ = (name, variant_index, variant, value);
            Err(FieldNameError(
                "serialize_newtype_variant is not supported by FieldNameSerializer".into(),
            ))
        }

        fn serialize_struct(
            self,
            name: &'static str,
            len: usize,
        ) -> Result<Self::SerializeStruct, Self::Error> {
            let _ = name;
            Ok(FieldNameStruct {
                fields: Vec::with_capacity(len),
            })
        }
    }

    #[test]
    fn task_creation_sanitizes_slug_and_sets_timestamps() {
        let mut state = state_with_project();

        let task = state
            .create_task(create_task("  Ship Core Orchestration!! v1  "), 100)
            .unwrap();

        assert_eq!(task.id, TaskId("task-1".into()));
        assert_eq!(task.slug, "ship-core-orchestration-v1");
        assert_eq!(task.status, TaskStatus::Backlog);
        assert_eq!(task.created_at_ms, 100);
        assert_eq!(task.updated_at_ms, 100);
        assert_eq!(task.completed_at_ms, None);
    }

    #[test]
    fn duplicate_slugs_get_short_suffixes() {
        let mut state = state_with_project();

        let first = state.create_task(create_task("Core Task"), 100).unwrap();
        let second = state.create_task(create_task("Core Task"), 101).unwrap();
        let third = state.create_task(create_task("Core---Task"), 102).unwrap();

        assert_eq!(first.slug, "core-task");
        assert_eq!(second.slug, "core-task-2");
        assert_eq!(third.slug, "core-task-3");
    }

    #[test]
    fn task_creation_rejects_legacy_agents_field() {
        let parsed = serde_json::from_value::<CreateTask>(serde_json::json!({
            "plan_id": "fixture-plan",
            "title": "Legacy agent task",
            "objective": "Reject legacy agent data.",
            "agents": []
        }));

        let error = parsed.expect_err("accepted legacy agents field");
        assert!(error.to_string().contains("agents"), "{error}");
    }

    #[test]
    fn task_update_mutates_metadata_without_identity_runtime_or_status_changes() {
        let mut state = state_with_project();
        let mut input = create_task("Original Task");
        input.scope = TaskScope {
            include_paths: vec!["old.rs".into()],
            exclude_paths: vec!["target".into()],
            notes: Some("old notes".into()),
        };
        input.gates = vec!["old gate".into()];
        let task = state.create_task(input, 100).unwrap();
        let related = state.create_task(create_task("Related"), 101).unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: task.id.clone(),
                    to: related.id.clone(),
                    kind: TaskEdgeKind::Related,
                    note: Some("keep".into()),
                },
                120,
            )
            .unwrap();
        state.record_execution(&task.id, execution(), 130).unwrap();
        state
            .update_task_status(&task.id, TaskStatus::Failed, 140)
            .unwrap();

        let original_edges = state.task_edges.clone();
        let original_execution = state.tasks[&task.id].execution.clone();
        let updated = state
            .update_task(
                &task.id,
                UpdateTask {
                    launch_hints: Default::default(),
                    title: Some("Renamed Task".into()),
                    objective: Some("New objective".into()),
                    scope: UpdateTaskScope {
                        include_paths: Some(vec!["new.rs".into()]),
                        exclude_paths: Some(Vec::new()),
                        notes: Some(Some("new notes".into())),
                    },
                    gates: Some(vec!["new gate".into(), "second gate".into()]),
                    auto_schedule: None,
                    run_spec: None,
                },
                200,
            )
            .unwrap();

        assert_eq!(updated.id, task.id);
        assert_eq!(updated.slug, "original-task");
        assert_eq!(updated.title, "Renamed Task");
        assert_eq!(updated.objective, "New objective");
        assert_eq!(updated.scope.include_paths, vec!["new.rs"]);
        assert!(updated.scope.exclude_paths.is_empty());
        assert_eq!(updated.scope.notes.as_deref(), Some("new notes"));
        assert_eq!(updated.gates, vec!["new gate", "second gate"]);
        assert_eq!(updated.status, TaskStatus::Failed);
        assert_eq!(updated.completed_at_ms, Some(140));
        assert_eq!(updated.updated_at_ms, 200);
        assert_eq!(state.task_edges, original_edges);
        assert_eq!(updated.execution, original_execution);
    }

    #[test]
    fn task_update_rejects_legacy_agents_field() {
        let parsed = serde_json::from_value::<UpdateTask>(serde_json::json!({
            "agents": []
        }));

        let error = parsed.expect_err("accepted legacy agents field");
        assert!(error.to_string().contains("agents"), "{error}");
    }

    #[test]
    fn task_update_partial_scope_and_list_semantics() {
        let mut state = state_with_project();
        let mut input = create_task("Scoped Task");
        input.scope = TaskScope {
            include_paths: vec!["a.rs".into()],
            exclude_paths: vec!["target".into()],
            notes: Some("keep notes".into()),
        };
        input.gates = vec!["gate".into()];
        let task = state.create_task(input, 100).unwrap();

        let updated = state
            .update_task(
                &task.id,
                UpdateTask {
                    objective: Some("Only objective changes".into()),
                    ..UpdateTask::default()
                },
                110,
            )
            .unwrap();
        assert_eq!(updated.title, "Scoped Task");
        assert_eq!(updated.scope.include_paths, vec!["a.rs"]);
        assert_eq!(updated.scope.exclude_paths, vec!["target"]);
        assert_eq!(updated.scope.notes.as_deref(), Some("keep notes"));
        assert_eq!(updated.gates, vec!["gate"]);

        let updated = state
            .update_task(
                &task.id,
                UpdateTask {
                    gates: Some(Vec::new()),
                    ..UpdateTask::default()
                },
                120,
            )
            .unwrap();
        assert!(updated.gates.is_empty());
    }

    #[test]
    fn task_run_spec_is_optional_mutable_and_validated() {
        let mut state = state_with_project();
        let mut input = create_task("Auto Task");
        input.run_spec = Some(run_spec());
        let task = state.create_task(input, 100).unwrap();
        assert_eq!(task.run_spec.as_ref().unwrap().launch_profile_id, "codex");

        let updated = state
            .update_task(
                &task.id,
                UpdateTask {
                    run_spec: Some(None),
                    ..UpdateTask::default()
                },
                110,
            )
            .unwrap();
        assert!(updated.run_spec.is_none());

        let mut invalid = run_spec();
        invalid.template = "unsupported".into();
        let error = state
            .update_task(
                &task.id,
                UpdateTask {
                    run_spec: Some(Some(invalid)),
                    ..UpdateTask::default()
                },
                120,
            )
            .unwrap_err();
        assert!(error.contains("template"));
    }

    #[test]
    fn task_update_gate_changes_are_reflected_in_status_counts() {
        fn gate_count(state: &OrchestrationState, task_id: &TaskId) -> usize {
            state
                .orchestration_status(200)
                .tasks
                .into_iter()
                .find(|summary| &summary.id == task_id)
                .unwrap()
                .open_gate_count
        }

        let mut state = state_with_project();
        let task = state.create_task(create_task("Gate Count"), 100).unwrap();
        assert_eq!(gate_count(&state, &task.id), 0);

        state
            .update_task(
                &task.id,
                UpdateTask {
                    gates: Some(vec!["review".into(), "test".into()]),
                    ..UpdateTask::default()
                },
                110,
            )
            .unwrap();
        assert_eq!(gate_count(&state, &task.id), 2);

        state
            .update_task_status(&task.id, TaskStatus::Passed, 120)
            .unwrap();
        assert_eq!(gate_count(&state, &task.id), 0);
    }

    #[test]
    fn task_update_rejects_empty_title_and_objective() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Validated"), 100).unwrap();

        let error = state
            .update_task(
                &task.id,
                UpdateTask {
                    title: Some(" ".into()),
                    ..UpdateTask::default()
                },
                110,
            )
            .unwrap_err();
        assert!(error.contains("title"));

        let error = state
            .update_task(
                &task.id,
                UpdateTask {
                    objective: Some(" ".into()),
                    ..UpdateTask::default()
                },
                120,
            )
            .unwrap_err();
        assert!(error.contains("objective"));
    }

    #[test]
    fn record_execution_records_task_owned_runtime_placement() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        let recorded = state.record_execution(&task.id, execution(), 200).unwrap();

        assert_eq!(recorded.created_at_ms, 200);
        assert_eq!(recorded.updated_at_ms, 200);
        assert_eq!(recorded.last_seen_ms, 200);
        assert_eq!(state.tasks[&task.id].execution, Some(recorded));
        assert_eq!(state.tasks[&task.id].updated_at_ms, 200);
    }

    #[test]
    fn record_execution_refresh_preserves_created_at_ms() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        state.record_execution(&task.id, execution(), 200).unwrap();
        let mut refresh = execution();
        refresh.role = "validator".into();
        let recorded = state.record_execution(&task.id, refresh, 300).unwrap();

        assert_eq!(recorded.created_at_ms, 200);
        assert_eq!(recorded.updated_at_ms, 300);
        assert_eq!(recorded.last_seen_ms, 300);
        assert_eq!(recorded.execution_id, "exec-a");
        assert_eq!(recorded.role, "validator");
        assert_eq!(state.tasks[&task.id].execution, Some(recorded));
    }

    #[test]
    fn record_execution_rejects_replacing_an_unfinished_execution() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        state.record_execution(&task.id, execution(), 200).unwrap();
        let mut replacement = execution();
        replacement.execution_id = "exec-b".into();

        let error = state
            .record_execution(&task.id, replacement, 300)
            .unwrap_err();

        assert!(
            error.contains("exec-a"),
            "error should name the reserved execution: {error}"
        );
        assert_eq!(
            state.tasks[&task.id]
                .execution
                .as_ref()
                .map(|existing| existing.execution_id.as_str()),
            Some("exec-a")
        );
    }

    #[test]
    fn record_execution_replaces_a_finished_execution_with_fresh_created_at_ms() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        let mut finished = execution();
        finished.phase = ExecutionPhase::Stopped;
        finished.pane_id = Some("w1:p1".into());
        state.record_execution(&task.id, finished, 200).unwrap();
        let mut replacement = execution();
        replacement.execution_id = "exec-b".into();
        let recorded = state.record_execution(&task.id, replacement, 300).unwrap();

        assert_eq!(recorded.created_at_ms, 300);
        assert_eq!(recorded.updated_at_ms, 300);
        assert_eq!(recorded.last_seen_ms, 300);
        assert_eq!(recorded.execution_id, "exec-b");
        let retained = &state.retained_executions["exec-a"];
        assert_eq!(retained.task_id, task.id);
        assert_eq!(retained.execution.phase, ExecutionPhase::Stopped);
        assert_eq!(retained.execution.created_at_ms, 200);
        let round_trip: OrchestrationState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(
            round_trip.retained_executions["exec-a"].execution,
            retained.execution
        );
        assert_eq!(state.tasks[&task.id].execution, Some(recorded));
    }

    #[test]
    fn record_execution_allows_in_place_progress_of_the_same_execution() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        state.record_execution(&task.id, execution(), 200).unwrap();
        let mut progress = execution();
        progress.phase = ExecutionPhase::Live;
        progress.recovery = ExecutionRecovery::Reconciled;
        let recorded = state.record_execution(&task.id, progress, 300).unwrap();

        assert_eq!(recorded.created_at_ms, 200);
        assert_eq!(recorded.phase, ExecutionPhase::Live);
        assert_eq!(state.tasks[&task.id].execution, Some(recorded));
    }

    #[test]
    fn record_execution_preserves_backend_owned_workspace_path_exactly() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Backend Workspace"), 100)
            .unwrap();
        let backend_workspace = "/__backend_only__/project/../project";
        let mut record = execution();
        record.workspace_path = backend_workspace.into();

        let recorded = state.record_execution(&task.id, record, 200).unwrap();

        assert_eq!(recorded.workspace_path, backend_workspace);
        assert_eq!(
            state.tasks[&task.id]
                .execution
                .as_ref()
                .unwrap()
                .workspace_path,
            backend_workspace
        );
    }

    #[test]
    fn execution_runtime_key_scopes_by_endpoint() {
        let mut record = execution();
        assert_eq!(record.runtime_key(), "node-a:exec-a");
        record.pane_id = Some("w5:p1".into());
        assert_eq!(record.runtime_key(), "node-a:w5:p1");
        record.endpoint_id = "local".into();
        assert_eq!(record.runtime_key(), "local:w5:p1");
    }

    #[test]
    fn edge_creation_validates_endpoints() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Existing"), 100).unwrap();

        let err = state
            .add_task_edge(
                CreateTaskEdge {
                    from: task.id,
                    to: TaskId("missing".into()),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap_err();

        assert!(err.contains("missing"));
    }

    #[test]
    fn depends_on_edges_reject_cycles() {
        let mut state = state_with_project();
        let a = state.create_task(create_task("A"), 100).unwrap();
        let b = state.create_task(create_task("B"), 100).unwrap();
        let c = state.create_task(create_task("C"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: a.id.clone(),
                    to: b.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: b.id.clone(),
                    to: c.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                201,
            )
            .unwrap();

        let err = state
            .add_task_edge(
                CreateTaskEdge {
                    from: c.id,
                    to: a.id,
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                202,
            )
            .unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn parent_of_edges_reject_multiple_parents() {
        let mut state = state_with_project();
        let parent_a = state.create_task(create_task("Parent A"), 100).unwrap();
        let parent_b = state.create_task(create_task("Parent B"), 100).unwrap();
        let child = state.create_task(create_task("Child"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: parent_a.id,
                    to: child.id.clone(),
                    kind: TaskEdgeKind::ParentOf,
                    note: None,
                },
                200,
            )
            .unwrap();
        let err = state
            .add_task_edge(
                CreateTaskEdge {
                    from: parent_b.id,
                    to: child.id,
                    kind: TaskEdgeKind::ParentOf,
                    note: None,
                },
                201,
            )
            .unwrap_err();

        assert!(err.contains("one parent"));
    }

    #[test]
    fn dependencies_block_ready_until_readiness_allowed_status() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Task"), 100).unwrap();
        let dependency = state.create_task(create_task("Dependency"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: task.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();

        let err = state
            .update_task_status(&task.id, TaskStatus::Planned, 250)
            .unwrap_err();
        assert!(err.contains("not ready"));

        state
            .update_task_status(&dependency.id, TaskStatus::Passed, 300)
            .unwrap();
        let ready = state
            .update_task_status(&task.id, TaskStatus::Planned, 350)
            .unwrap();
        assert_eq!(ready.status, TaskStatus::Planned);
    }

    #[test]
    fn validates_edges_block_dependency_readiness_until_validator_approves() {
        let mut state = state_with_project();
        let downstream = state.create_task(create_task("Downstream"), 100).unwrap();
        let dependency = state
            .create_task(
                CreateTask {
                    gates: vec!["validation approves".into()],
                    ..create_task("Dependency")
                },
                101,
            )
            .unwrap();
        let validator = state.create_task(create_task("Validator"), 102).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: downstream.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: validator.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::Validates,
                    note: Some("validator must approve dependency gates".into()),
                },
                201,
            )
            .unwrap();

        state
            .update_task_status(&dependency.id, TaskStatus::Passed, 300)
            .unwrap();
        state
            .update_task_status(&validator.id, TaskStatus::Failed, 301)
            .unwrap();

        let blockers = state.task_dependency_blockers(&downstream.id).unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].task_id, dependency.id);
        assert_eq!(blockers[0].status, TaskStatus::Passed);
        assert_eq!(
            blockers[0].validation_blocked_by,
            vec![validator.id.clone()]
        );
        assert!(blockers[0].reason.contains("validation not approved"));

        let err = state
            .update_task_status(&downstream.id, TaskStatus::Planned, 350)
            .unwrap_err();
        assert!(err.contains("validation not approved"));

        state
            .update_task_status(&validator.id, TaskStatus::Passed, 400)
            .unwrap();
        let planned = state
            .update_task_status(&downstream.id, TaskStatus::Planned, 450)
            .unwrap();
        assert_eq!(planned.status, TaskStatus::Planned);
    }

    #[test]
    fn supersedes_edges_block_superseded_task_planning_and_running() {
        let mut state = state_with_project();
        let replacement = state.create_task(create_task("Replacement"), 100).unwrap();
        let superseded = state.create_task(create_task("Old Task"), 101).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: replacement.id.clone(),
                    to: superseded.id.clone(),
                    kind: TaskEdgeKind::Supersedes,
                    note: Some("replacement owns the remaining work".into()),
                },
                200,
            )
            .unwrap();

        assert_eq!(
            state.task_superseded_by(&superseded.id).unwrap(),
            vec![replacement.id.clone()]
        );
        let planned_error = state
            .update_task_status(&superseded.id, TaskStatus::Planned, 250)
            .unwrap_err();
        assert!(planned_error.contains("superseded by"));
        let running_error = state
            .update_task_status(&superseded.id, TaskStatus::Running, 251)
            .unwrap_err();
        assert!(running_error.contains("superseded by"));

        let replacement_running = state
            .update_task_status(&replacement.id, TaskStatus::Running, 300)
            .unwrap();
        assert_eq!(replacement_running.status, TaskStatus::Running);
    }

    #[test]
    fn parent_children_block_delivery_but_not_running() {
        let mut state = state_with_project();
        let parent = state.create_task(create_task("Parent"), 100).unwrap();
        let child = state.create_task(create_task("Child"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: parent.id.clone(),
                    to: child.id.clone(),
                    kind: TaskEdgeKind::ParentOf,
                    note: None,
                },
                200,
            )
            .unwrap();

        let running = state
            .update_task_status(&parent.id, TaskStatus::Running, 250)
            .unwrap();
        assert_eq!(running.status, TaskStatus::Running);

        let err = state
            .update_task_status(&parent.id, TaskStatus::Delivered, 300)
            .unwrap_err();
        assert!(err.contains("before child"));

        state
            .update_task_status(&child.id, TaskStatus::Delivered, 350)
            .unwrap();
        let delivered = state
            .update_task_status(&parent.id, TaskStatus::Delivered, 400)
            .unwrap();
        assert_eq!(delivered.status, TaskStatus::Delivered);
        assert_eq!(delivered.completed_at_ms, Some(400));
    }

    #[test]
    fn parent_delivery_allows_canceled_or_failed_child_with_outcome() {
        let mut state = state_with_project();
        let canceled_parent = state
            .create_task(create_task("Canceled Parent"), 100)
            .unwrap();
        let canceled_child = state
            .create_task(create_task("Canceled Child"), 100)
            .unwrap();
        let failed_parent = state
            .create_task(create_task("Failed Parent"), 100)
            .unwrap();
        let failed_child = state.create_task(create_task("Failed Child"), 100).unwrap();

        for (parent, child) in [
            (&canceled_parent, &canceled_child),
            (&failed_parent, &failed_child),
        ] {
            state
                .add_task_edge(
                    CreateTaskEdge {
                        from: parent.id.clone(),
                        to: child.id.clone(),
                        kind: TaskEdgeKind::ParentOf,
                        note: None,
                    },
                    200,
                )
                .unwrap();
        }

        state
            .update_task_status(&canceled_child.id, TaskStatus::Canceled, 250)
            .unwrap();
        let delivered = state
            .update_task_status(&canceled_parent.id, TaskStatus::Delivered, 300)
            .unwrap();
        assert_eq!(delivered.status, TaskStatus::Delivered);

        state
            .update_task_status(&failed_child.id, TaskStatus::Failed, 350)
            .unwrap();
        let err = state
            .update_task_status(&failed_parent.id, TaskStatus::Delivered, 400)
            .unwrap_err();
        assert!(err.contains("failed with an outcome"));

        state.tasks.get_mut(&failed_child.id).unwrap().outcome =
            Some("Operator accepted the failed child outcome.".into());
        let delivered = state
            .update_task_status(&failed_parent.id, TaskStatus::Delivered, 450)
            .unwrap();
        assert_eq!(delivered.status, TaskStatus::Delivered);
    }

    #[test]
    fn status_updates_set_finished_timestamps() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Status Task"), 100).unwrap();

        let running = state
            .update_task_status(&task.id, TaskStatus::Running, 200)
            .unwrap();
        assert_eq!(running.updated_at_ms, 200);
        assert_eq!(running.completed_at_ms, None);

        let failed = state
            .update_task_status(&task.id, TaskStatus::Failed, 250)
            .unwrap();
        assert_eq!(failed.updated_at_ms, 250);
        assert_eq!(failed.completed_at_ms, Some(250));

        let blocked = state
            .update_task_status(&task.id, TaskStatus::Blocked, 300)
            .unwrap();
        assert_eq!(blocked.completed_at_ms, None);
    }

    #[test]
    fn orchestration_status_counts_active_and_finished_tasks() {
        let mut state = state_with_project();
        let backlog = state.create_task(create_task("Backlog"), 100).unwrap();
        let running = state.create_task(create_task("Running"), 100).unwrap();
        let waiting = state.create_task(create_task("Waiting"), 100).unwrap();
        let blocked = state.create_task(create_task("Blocked"), 100).unwrap();
        let passed = state.create_task(create_task("Passed"), 100).unwrap();
        let delivered = state.create_task(create_task("Delivered"), 100).unwrap();
        let failed = state.create_task(create_task("Failed"), 100).unwrap();
        let canceled = state.create_task(create_task("Canceled"), 100).unwrap();

        state
            .update_task_status(&running.id, TaskStatus::Running, 200)
            .unwrap();
        state
            .update_task_status(&waiting.id, TaskStatus::WaitingForValidation, 201)
            .unwrap();
        state
            .update_task_status(&blocked.id, TaskStatus::Blocked, 202)
            .unwrap();
        state
            .update_task_status(&passed.id, TaskStatus::Passed, 203)
            .unwrap();
        state
            .update_task_status(&delivered.id, TaskStatus::Delivered, 204)
            .unwrap();
        state
            .update_task_status(&failed.id, TaskStatus::Failed, 205)
            .unwrap();
        state
            .update_task_status(&canceled.id, TaskStatus::Canceled, 206)
            .unwrap();

        let status = state.orchestration_status(300);

        assert_eq!(status.counts.total_tasks, 8);
        assert_eq!(status.counts.active_tasks, 5);
        assert_eq!(status.counts.blocked_tasks, 1);
        assert_eq!(status.counts.waiting_for_validation_tasks, 1);
        assert_eq!(status.counts.passed_tasks, 1);
        assert_eq!(status.counts.delivered_tasks, 1);
        assert_eq!(status.counts.failed_tasks, 1);
        assert_eq!(status.counts.canceled_tasks, 1);
        assert!(status
            .tasks
            .iter()
            .any(|summary| summary.id == backlog.id && summary.status == TaskStatus::Backlog));
        let project = status.projects.first().expect("project summary");
        assert_eq!(project.task_count, 8);
        assert_eq!(project.active_task_count, 5);
        assert_eq!(project.task_status_counts.len(), TaskStatus::ALL.len());
        assert_eq!(project.task_status_counts[&TaskStatus::Backlog], 1);
        assert_eq!(project.task_status_counts[&TaskStatus::Planned], 0);
        assert_eq!(project.task_status_counts[&TaskStatus::Running], 1);
        assert_eq!(
            project.task_status_counts[&TaskStatus::WaitingForValidation],
            1
        );
        assert_eq!(project.task_status_counts[&TaskStatus::Blocked], 1);
        assert_eq!(project.task_status_counts[&TaskStatus::Passed], 1);
        assert_eq!(project.task_status_counts[&TaskStatus::Delivered], 1);
        assert_eq!(project.task_status_counts[&TaskStatus::Failed], 1);
        assert_eq!(project.task_status_counts[&TaskStatus::Canceled], 1);
    }

    #[test]
    fn project_summaries_include_zero_counts_for_every_task_status() {
        let state = state_with_project();

        let status = state.orchestration_status(300);

        let project = status.projects.first().expect("project summary");
        assert_eq!(project.task_count, 0);
        assert_eq!(project.active_task_count, 0);
        assert_eq!(project.task_status_counts.len(), TaskStatus::ALL.len());
        for task_status in TaskStatus::ALL {
            assert_eq!(project.task_status_counts[&task_status], 0);
        }
    }

    #[test]
    fn orchestration_status_includes_task_execution() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Summary"), 100)
            .unwrap();
        let recorded = state.record_execution(&task.id, execution(), 150).unwrap();

        let status = state.orchestration_status(200);
        let summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == task.id)
            .unwrap();

        assert_eq!(summary.execution, Some(recorded.clone()));
        assert_eq!(status.executions.len(), 1);
        assert_eq!(status.executions[0].task_id, task.id);
        assert_eq!(status.executions[0].execution_id, recorded.execution_id);
        assert_eq!(status.executions[0].endpoint_id, recorded.endpoint_id);
        assert_eq!(status.executions[0].phase, ExecutionPhase::Pending);
        assert_eq!(
            status.executions[0].recovery,
            ExecutionRecovery::NeedsReconciliation
        );
    }

    #[test]
    fn orchestration_status_calculates_blocked_by_from_depends_on() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Task"), 100).unwrap();
        let dependency = state.create_task(create_task("Dependency"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: task.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();

        let status = state.orchestration_status(300);
        let summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == task.id)
            .unwrap();
        assert_eq!(summary.dependency_count, 1);
        assert_eq!(summary.blocked_by, vec![dependency.id.clone()]);
        assert_eq!(status.counts.blocked_tasks, 1);

        state
            .update_task_status(&dependency.id, TaskStatus::Passed, 350)
            .unwrap();
        let status = state.orchestration_status(400);
        let summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == task.id)
            .unwrap();
        assert!(summary.blocked_by.is_empty());
        assert_eq!(status.counts.blocked_tasks, 0);
    }

    #[test]
    fn orchestration_status_reports_validation_blockers() {
        let mut state = state_with_project();
        let downstream = state.create_task(create_task("Downstream"), 100).unwrap();
        let dependency = state
            .create_task(
                CreateTask {
                    gates: vec!["validator approves".into()],
                    ..create_task("Dependency")
                },
                101,
            )
            .unwrap();
        let validator = state.create_task(create_task("Validator"), 102).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: downstream.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: validator.id.clone(),
                    to: dependency.id.clone(),
                    kind: TaskEdgeKind::Validates,
                    note: None,
                },
                201,
            )
            .unwrap();
        state
            .update_task_status(&dependency.id, TaskStatus::Passed, 300)
            .unwrap();
        state
            .update_task_status(&validator.id, TaskStatus::Blocked, 301)
            .unwrap();

        let status = state.orchestration_status(400);
        let dependency_summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == dependency.id)
            .unwrap();
        assert_eq!(dependency_summary.validator_count, 1);
        assert_eq!(dependency_summary.unapproved_validator_count, 1);
        assert_eq!(dependency_summary.failed_validator_count, 1);
        assert_eq!(
            dependency_summary.validation_blocked_by,
            vec![validator.id.clone()]
        );

        let downstream_summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == downstream.id)
            .unwrap();
        assert_eq!(downstream_summary.blocked_by, vec![dependency.id.clone()]);
        assert_eq!(status.counts.blocked_tasks, 2);
    }

    #[test]
    fn orchestration_status_summarizes_parent_and_children() {
        let mut state = state_with_project();
        let parent = state.create_task(create_task("Parent"), 100).unwrap();
        let child_a = state.create_task(create_task("Child A"), 100).unwrap();
        let child_b = state.create_task(create_task("Child B"), 100).unwrap();

        for child in [&child_b, &child_a] {
            state
                .add_task_edge(
                    CreateTaskEdge {
                        from: parent.id.clone(),
                        to: child.id.clone(),
                        kind: TaskEdgeKind::ParentOf,
                        note: None,
                    },
                    200,
                )
                .unwrap();
        }

        let status = state.orchestration_status(300);
        let parent_summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == parent.id)
            .unwrap();
        let child_summary = status
            .tasks
            .iter()
            .find(|summary| summary.id == child_a.id)
            .unwrap();

        assert_eq!(parent_summary.child_count, 2);
        assert_eq!(child_summary.parent, Some(parent.id));
    }

    #[test]
    fn orchestration_status_counts_gates_by_lifecycle_state() {
        let mut state = state_with_project();
        let open = state
            .create_task(
                CreateTask {
                    gates: vec!["review".into(), "test".into()],
                    ..create_task("Open Gates")
                },
                100,
            )
            .unwrap();
        let passed = state
            .create_task(
                CreateTask {
                    gates: vec!["review".into()],
                    ..create_task("Passed Gates")
                },
                100,
            )
            .unwrap();
        let delivered = state
            .create_task(
                CreateTask {
                    gates: vec!["review".into()],
                    ..create_task("Delivered Gates")
                },
                100,
            )
            .unwrap();
        let blocked = state
            .create_task(
                CreateTask {
                    gates: vec!["review".into(), "test".into()],
                    ..create_task("Blocked Gates")
                },
                100,
            )
            .unwrap();

        state
            .update_task_status(&passed.id, TaskStatus::Passed, 200)
            .unwrap();
        state
            .update_task_status(&delivered.id, TaskStatus::Delivered, 201)
            .unwrap();
        state
            .update_task_status(&blocked.id, TaskStatus::Blocked, 202)
            .unwrap();
        state.tasks.get_mut(&blocked.id).unwrap().blockers = vec!["tests failed".into()];

        let status = state.orchestration_status(300);
        let by_id = |task_id: &TaskId| {
            status
                .tasks
                .iter()
                .find(|summary| &summary.id == task_id)
                .unwrap()
        };

        assert_eq!(by_id(&open.id).open_gate_count, 2);
        assert_eq!(by_id(&open.id).failed_gate_count, 0);
        assert_eq!(by_id(&passed.id).open_gate_count, 0);
        assert_eq!(by_id(&delivered.id).open_gate_count, 0);
        assert_eq!(by_id(&blocked.id).open_gate_count, 2);
        assert_eq!(by_id(&blocked.id).failed_gate_count, 1);
        assert_eq!(by_id(&blocked.id).blocker_count, 1);
        assert_eq!(by_id(&blocked.id).blockers, vec!["tests failed"]);
    }

    #[test]
    fn execution_summary_serialization_shape_and_values_are_compact() {
        let mut state = state_with_project();
        let task = state
            .create_task(create_task("Execution Task"), 100)
            .unwrap();
        let mut execution = execution();
        execution.bypass_permissions = true;
        state.record_execution(&task.id, execution, 200).unwrap();

        let status = state.orchestration_status(300);
        let summary = status.executions.first().unwrap();
        let fields = summary.serialize(FieldNameSerializer).unwrap();

        assert_eq!(
            fields,
            vec![
                "endpoint_id",
                "execution_id",
                "launch_profile_id",
                "workspace_path",
                "bypass_permissions",
                "task_id",
                "role",
                "kind",
                "phase",
                "recovery",
                "pane_id",
                "agent_name",
                "agent_kind",
                "agent_session",
                "last_seen_ms",
                "runtime_state"
            ]
        );
        assert_eq!(summary.endpoint_id, "node-a");
        assert_eq!(summary.execution_id, "exec-a");
        assert_eq!(summary.launch_profile_id, "codex");
        assert_eq!(summary.workspace_path, "/workspace/project");
        assert!(summary.bypass_permissions);
        assert_eq!(summary.task_id, task.id);
        assert_eq!(summary.last_seen_ms, Some(200));
        assert_eq!(summary.runtime_state, None);
    }

    #[test]
    fn cleanup_candidate_serialization_shape_is_stable() {
        let candidate = ExecutionCleanupCandidate {
            endpoint_id: "node-a".into(),
            execution_id: "exec-a".into(),
            pane_id: Some("w5:p1".into()),
            agent_name: Some("taskr-task-codex-1".into()),
            reason: "stale".into(),
            created_at_ms: Some(100),
        };

        let fields = candidate.serialize(FieldNameSerializer).unwrap();

        assert_eq!(
            fields,
            vec![
                "endpoint_id",
                "execution_id",
                "pane_id",
                "agent_name",
                "reason",
                "created_at_ms"
            ]
        );
        assert!(!fields.iter().any(|field| field == "last_seen_ms"));
    }

    #[test]
    fn orchestration_status_output_ordering_is_stable() {
        let mut state = state_with_project();
        let first = state.create_task(create_task("First"), 100).unwrap();
        let second = state.create_task(create_task("Second"), 100).unwrap();
        let third = state.create_task(create_task("Third"), 100).unwrap();

        state
            .add_task_edge(
                CreateTaskEdge {
                    from: third.id.clone(),
                    to: first.id.clone(),
                    kind: TaskEdgeKind::DependsOn,
                    note: None,
                },
                200,
            )
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: second.id.clone(),
                    to: first.id.clone(),
                    kind: TaskEdgeKind::ParentOf,
                    note: None,
                },
                201,
            )
            .unwrap();

        let mut execution_b = execution();
        execution_b.execution_id = "exec-b".into();
        execution_b.endpoint_id = "node-b".into();
        state.record_execution(&third.id, execution_b, 250).unwrap();
        let mut execution_a = execution();
        execution_a.execution_id = "exec-a".into();
        execution_a.endpoint_id = "node-a".into();
        state
            .record_execution(&second.id, execution_a, 251)
            .unwrap();

        let status = state.orchestration_status(300);

        assert_eq!(
            status
                .tasks
                .iter()
                .map(|summary| summary.id.0.as_str())
                .collect::<Vec<_>>(),
            vec!["task-1", "task-2", "task-3"]
        );
        assert_eq!(
            status
                .task_edges
                .iter()
                .map(|edge| (edge.from.0.as_str(), edge.to.0.as_str(), edge.kind))
                .collect::<Vec<_>>(),
            vec![
                ("task-2", "task-1", TaskEdgeKind::ParentOf),
                ("task-3", "task-1", TaskEdgeKind::DependsOn)
            ]
        );
        assert_eq!(
            status
                .executions
                .iter()
                .map(|summary| (summary.endpoint_id.as_str(), summary.execution_id.as_str()))
                .collect::<Vec<_>>(),
            vec![("node-a", "exec-a"), ("node-b", "exec-b")]
        );
        assert!(status.cleanup_candidates.is_empty());
        assert_eq!(status.counts.cleanup_candidates, 0);
    }

    #[test]
    fn orchestration_status_reports_suspicious_deserialized_state() {
        let mut state = state_with_project();
        let task = state.create_task(create_task("Task"), 100).unwrap();
        let mut task_execution = execution();
        task_execution.created_at_ms = 500;
        task_execution.updated_at_ms = 500;
        task_execution.last_seen_ms = 500;
        state.tasks.get_mut(&task.id).unwrap().execution = Some(task_execution);
        state.task_edges.push(TaskEdge {
            from: task.id.clone(),
            to: TaskId("missing".into()),
            kind: TaskEdgeKind::DependsOn,
            created_at_ms: 200,
            note: None,
        });

        let status = state.orchestration_status(300);

        assert!(status
            .warnings
            .iter()
            .any(|warning| warning.contains("missing to task 'missing'")));
        assert!(status
            .warnings
            .iter()
            .any(|warning| warning.contains("future created_at_ms")));
        assert!(status
            .warnings
            .iter()
            .any(|warning| warning.contains("future last_seen_ms")));
    }
}
