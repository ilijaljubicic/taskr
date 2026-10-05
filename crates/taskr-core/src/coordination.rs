//! Pure lifecycle policy shared by native actors and single-threaded hosts.
//! Hosts perform effects, supply observations/time and persist each decision.
use crate::orchestration::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectOutcome {
    Applied,
    NotApplied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchStep {
    PrepareEndpoint,
    VerifyEnvironment,
    Allocate,
    ReconcileAllocation,
    PersistPlacement,
    StartAgent,
    ReconcileAgent,
    PersistLive,
    SubmitPrompt,
    Complete,
}
/// Facts known to this continuation. On host restart an Allocating/Starting
/// intent is already dispatched unless a durable command receipt proves otherwise.
#[derive(Clone, Copy, Debug)]
pub struct LaunchFacts {
    pub endpoint_prepared: bool,
    pub environment_verified: bool,
    pub agent_start_dispatched: bool,
    pub occupant_verified: bool,
    pub prompt_delivered: bool,
}
impl Default for LaunchFacts {
    fn default() -> Self {
        Self {
            endpoint_prepared: false,
            environment_verified: false,
            agent_start_dispatched: true,
            occupant_verified: false,
            prompt_delivered: false,
        }
    }
}
pub fn launch_step(execution: &TaskExecution, facts: LaunchFacts) -> LaunchStep {
    match execution.phase {
        ExecutionPhase::Pending if !facts.endpoint_prepared => LaunchStep::PrepareEndpoint,
        ExecutionPhase::Pending if !facts.environment_verified => LaunchStep::VerifyEnvironment,
        ExecutionPhase::Pending => LaunchStep::Allocate,
        ExecutionPhase::Allocating if execution.pane_id.is_none() => {
            LaunchStep::ReconcileAllocation
        }
        ExecutionPhase::Allocating => LaunchStep::PersistPlacement,
        ExecutionPhase::Starting if !facts.agent_start_dispatched => LaunchStep::StartAgent,
        ExecutionPhase::Starting if !facts.occupant_verified => LaunchStep::ReconcileAgent,
        ExecutionPhase::Starting => LaunchStep::PersistLive,
        ExecutionPhase::Live if execution.inspection || facts.prompt_delivered => {
            LaunchStep::Complete
        }
        ExecutionPhase::Live => LaunchStep::SubmitPrompt,
        _ => LaunchStep::Complete,
    }
}
pub fn allocation_failure_phase(outcome: EffectOutcome) -> ExecutionPhase {
    match outcome {
        EffectOutcome::NotApplied => ExecutionPhase::Failed,
        _ => ExecutionPhase::Allocating,
    }
}
pub fn startup_failure_phase(outcome: EffectOutcome) -> ExecutionPhase {
    match outcome {
        EffectOutcome::NotApplied => ExecutionPhase::Failed,
        _ => ExecutionPhase::Starting,
    }
}
pub fn unresolved_allocation(execution: &TaskExecution) -> bool {
    execution.phase == ExecutionPhase::Allocating && execution.pane_id.is_none()
}
pub fn generations_match(expected: Option<&str>, observed: Option<&str>) -> bool {
    matches!((expected, observed), (Some(a), Some(b)) if a == b)
}
pub fn final_for_cleanup(status: TaskStatus) -> bool {
    status == TaskStatus::Passed || status.is_finished()
}

pub fn audit_holds(state: &OrchestrationState, task_id: &TaskId) -> Vec<String> {
    let Some(task) = state.tasks.get(task_id) else {
        return Vec::new();
    };
    if task.execution.as_ref().is_some_and(|e| e.inspection)
        || !matches!(task.status, TaskStatus::Passed | TaskStatus::Delivered)
        || !task
            .execution
            .as_ref()
            .is_some_and(|e| e.phase != ExecutionPhase::Stopped)
    {
        return Vec::new();
    }
    let mut audits: Vec<_> = state
        .task_edges
        .iter()
        .filter(|edge| edge.kind == TaskEdgeKind::Audits && &edge.to == task_id)
        .filter_map(|edge| state.tasks.get(&edge.from))
        .filter(|audit| !final_for_cleanup(audit.status))
        .map(|audit| audit.id.0.clone())
        .collect();
    audits.sort();
    audits.dedup();
    audits
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupDecision {
    Keep,
    AuditHold(Vec<String>),
    Close,
}
pub fn cleanup_decision(state: &OrchestrationState, task: &Task) -> CleanupDecision {
    let Some(execution) = &task.execution else {
        return CleanupDecision::Keep;
    };
    if !final_for_cleanup(task.status)
        || execution.inspection
        || (execution.phase == ExecutionPhase::Stopped && execution.pane_closed)
    {
        return CleanupDecision::Keep;
    }
    let holds = audit_holds(state, &task.id);
    if holds.is_empty() {
        CleanupDecision::Close
    } else {
        CleanupDecision::AuditHold(holds)
    }
}
pub fn needs_endpoint_lease(execution: &TaskExecution) -> bool {
    !execution.pane_closed && (!execution.phase.allows_replacement() || execution.pane_id.is_some())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentObservation {
    pub name: Option<String>,
    pub terminal_id: Option<String>,
    pub session: Option<String>,
}
pub fn owns_agent(execution: &TaskExecution, observed: &AgentObservation) -> bool {
    if execution
        .terminal_id
        .as_ref()
        .is_some_and(|id| observed.terminal_id.as_ref() != Some(id))
    {
        return false;
    }
    if execution
        .agent_name
        .as_ref()
        .is_some_and(|name| observed.name.as_ref() != Some(name))
    {
        return false;
    }
    match (&execution.agent_session, &observed.session) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlacement {
    NewSpace,
    NewTab { workspace_id: String },
    Split { pane_id: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    /// Host has verified endpoint, generation, immutable terminal and group ownership.
    pub owned_by_group: bool,
}
pub fn select_placement(
    layout: Option<&PlanLayout>,
    group: ExecutionGroup,
    panes: &[LayoutPane],
    limit: usize,
) -> LayoutPlacement {
    let Some(layout) = layout else {
        return LayoutPlacement::NewSpace;
    };
    for tab in layout.tabs.iter().filter(|tab| tab.group == group) {
        let occupants: Vec<_> = panes
            .iter()
            .filter(|pane| pane.workspace_id == layout.workspace_id && pane.tab_id == tab.tab_id)
            .collect();
        if !occupants.is_empty()
            && occupants.len() < limit
            && occupants.iter().all(|pane| pane.owned_by_group)
        {
            return LayoutPlacement::Split {
                pane_id: occupants[0].pane_id.clone(),
            };
        }
    }
    LayoutPlacement::NewTab {
        workspace_id: layout.workspace_id.clone(),
    }
}

pub fn resume_args(kind: &str, saved: &[String], session: &str) -> Result<Vec<String>, String> {
    if session.trim().is_empty()
        || session.starts_with('-')
        || session.chars().any(char::is_control)
    {
        return Err("saved native session ID is invalid".into());
    }
    let flag = match kind {
        "codex" => "resume",
        "claude" => "--resume",
        "opencode" | "kimi" => "--session",
        _ => {
            return Err(format!(
                "native resume is not supported for agent kind '{kind}'"
            ))
        }
    };
    let mut args = saved.to_vec();
    if args.len() >= 2 && args[args.len() - 2] == flag {
        args.truncate(args.len() - 2);
    }
    args.extend([flag.to_owned(), session.to_owned()]);
    Ok(args)
}
pub fn resume_allowed(
    source: &TaskExecution,
    current: Option<&TaskExecution>,
) -> Result<(), String> {
    if current.is_some_and(|e| !e.phase.allows_replacement()) {
        return Err(
            "task already has a live or unresolved execution; stop it before resuming".into(),
        );
    }
    if !source.phase.allows_replacement() {
        return Err("only a finished execution can be resumed".into());
    }
    Ok(())
}

/// Apply a positively observed namespace replacement; an unavailable endpoint
/// or missing identity is never proof of loss. Preserve all conversation/report IDs.
pub fn namespace_lost(
    state: &mut OrchestrationState,
    execution_id: &str,
    observed: &str,
    now_ms: u64,
) -> Result<(), String> {
    fn closed(execution: &TaskExecution, observed: &str) -> Result<TaskExecution, String> {
        if observed.is_empty()
            || execution
                .runtime_generation
                .as_deref()
                .is_none_or(|expected| expected == observed)
        {
            return Err(
                "namespace loss requires a positively observed different generation".into(),
            );
        }
        let mut updated = execution.clone();
        updated.phase = ExecutionPhase::Exited;
        updated.pane_closed = true;
        updated.recovery = ExecutionRecovery::Reconciled;
        Ok(updated)
    }
    if let Some(task) = state.tasks.values().find(|task| {
        task.execution
            .as_ref()
            .is_some_and(|e| e.execution_id == execution_id)
    }) {
        let id = task.id.clone();
        let mut updated = closed(task.execution.as_ref().unwrap(), observed)?;
        if updated.report.is_none() {
            updated.report = Some(ExecutionReport {
                task_status: task.status,
                outcome: task.outcome.clone(),
                evidence: task.evidence.clone(),
            });
        }
        state.record_execution(&id, updated, now_ms)?;
        return Ok(());
    }
    let row = state
        .retained_executions
        .get_mut(execution_id)
        .ok_or("execution changed before namespace loss could be recorded")?;
    row.execution = closed(&row.execution, observed)?;
    row.execution.updated_at_ms = now_ms;
    row.execution.last_seen_ms = now_ms;
    Ok(())
}
