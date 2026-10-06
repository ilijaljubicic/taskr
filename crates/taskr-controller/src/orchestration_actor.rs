#![allow(dead_code)]

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use taskr_core::orchestration::{
    CreatePlan, CreateProject, CreateTask, CreateTaskEdge, ExecutionPhase, ExecutionRecovery,
    OrchestrationState, OrchestrationStatus, Plan, PlanId, PlanStatus, Project, ProjectId,
    ProjectStatus, Task, TaskEdge, TaskEdgeKind, TaskExecution, TaskId, TaskStatus, UpdatePlan,
    UpdateProject, UpdateTask,
};
use taskr_core::Orchestrator;

use crate::store::SqliteOrchestrationStore;
use crate::store_paths::resolve_store_path;
use crate::{now_ms, prune_finished_plans, task_status_allows_runtime_cleanup};

#[derive(Clone)]
pub(crate) struct OrchestrationHandle {
    inner: Arc<Mutex<OrchestrationRuntimeState>>,
}

type OrchestrationRuntimeState = Orchestrator<SqliteOrchestrationStore>;

impl OrchestrationHandle {
    pub(crate) fn open(store_path: Option<&Path>) -> Result<Self, String> {
        let store_path = resolve_store_path(store_path)?;
        Self::from_store(SqliteOrchestrationStore::open(store_path)?)
    }

    pub(crate) fn from_store(store: SqliteOrchestrationStore) -> Result<Self, String> {
        let service = Orchestrator::open(store)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(service)),
        })
    }

    pub(crate) fn snapshot(&self) -> Result<OrchestrationState, String> {
        Ok(self.lock()?.snapshot())
    }

    pub(crate) fn status(&self) -> Result<OrchestrationStatus, String> {
        Ok(self.lock()?.state().orchestration_status(now_ms()))
    }

    pub(crate) fn create_project(&self, input: CreateProject) -> Result<Project, String> {
        self.mutate(|state, now_ms| state.create_project(input, now_ms))
    }

    pub(crate) fn update_project(
        &self,
        project_id: ProjectId,
        update: UpdateProject,
    ) -> Result<Project, String> {
        self.mutate(|state, now_ms| state.update_project(&project_id, update, now_ms))
    }

    pub(crate) fn update_project_status(
        &self,
        project_id: ProjectId,
        status: ProjectStatus,
    ) -> Result<Project, String> {
        self.mutate(|state, now_ms| state.update_project_status(&project_id, status, now_ms))
    }

    pub(crate) fn create_plan(&self, input: CreatePlan) -> Result<Plan, String> {
        self.mutate(|state, now_ms| state.create_plan(input, now_ms))
    }

    pub(crate) fn update_plan(&self, plan_id: PlanId, update: UpdatePlan) -> Result<Plan, String> {
        self.mutate(|state, now_ms| state.update_plan(&plan_id, update, now_ms))
    }

    pub(crate) fn update_plan_status(
        &self,
        plan_id: PlanId,
        status: PlanStatus,
        outcome: Option<String>,
    ) -> Result<Plan, String> {
        self.mutate(|state, now_ms| state.update_plan_status(&plan_id, status, outcome, now_ms))
    }

    pub(crate) fn create_task(&self, input: CreateTask) -> Result<Task, String> {
        self.mutate(|state, now_ms| state.create_task(input, now_ms))
    }

    pub(crate) fn update_task(&self, task_id: TaskId, update: UpdateTask) -> Result<Task, String> {
        self.mutate(|state, now_ms| state.update_task(&task_id, update, now_ms))
    }

    pub(crate) fn record_execution(
        &self,
        task_id: TaskId,
        execution: TaskExecution,
    ) -> Result<TaskExecution, String> {
        self.mutate(|state, now_ms| state.record_execution(&task_id, execution, now_ms))
    }

    pub(crate) fn record_execution_if_current(
        &self,
        task_id: TaskId,
        execution: TaskExecution,
    ) -> Result<TaskExecution, String> {
        self.mutate(|state, now_ms| {
            let current = state
                .tasks
                .get(&task_id)
                .and_then(|task| task.execution.as_ref());
            if current.is_none_or(|current| current.execution_id != execution.execution_id) {
                return Err("execution changed while its worker was being closed".into());
            }
            state.record_execution(&task_id, execution, now_ms)
        })
    }

    pub(crate) fn record_launch_progress(
        &self,
        task_id: TaskId,
        execution: TaskExecution,
    ) -> Result<TaskExecution, String> {
        self.mutate(|state, now_ms| {
            let task = state
                .tasks
                .get(&task_id)
                .ok_or_else(|| "task not found".to_owned())?;
            if task_status_allows_runtime_cleanup(task.status) && !execution.inspection {
                return Err(format!(
                    "task '{}' is {:?}; launch cannot continue",
                    task_id.0, task.status
                ));
            }
            if task.execution.as_ref().is_some_and(|current| {
                current.execution_id == execution.execution_id
                    && matches!(
                        current.phase,
                        ExecutionPhase::Stopped | ExecutionPhase::Exited
                    )
            }) {
                return Err("execution was closed while launching".into());
            }
            state.record_execution(&task_id, execution, now_ms)
        })
    }

    pub(crate) fn mark_execution_running(
        &self,
        task_id: TaskId,
        execution_id: &str,
        outcome: String,
    ) -> Result<Task, String> {
        self.mutate(|state, now_ms| {
            let task = state
                .tasks
                .get(&task_id)
                .ok_or_else(|| "task not found".to_owned())?;
            if task_status_allows_runtime_cleanup(task.status)
                || !task.execution.as_ref().is_some_and(|execution| {
                    execution.execution_id == execution_id
                        && execution.phase == ExecutionPhase::Live
                })
            {
                return Err("task finished or execution changed before launch completion".into());
            }
            state.update_task_status(&task_id, TaskStatus::Running, now_ms)?;
            let task = state.tasks.get_mut(&task_id).unwrap();
            task.outcome = Some(outcome);
            Ok(task.clone())
        })
    }

    pub(crate) fn mark_execution_blocked(
        &self,
        task_id: TaskId,
        execution_id: &str,
        outcome: String,
        blockers: Option<Vec<String>>,
        only_running: bool,
    ) -> Result<bool, String> {
        self.mutate(|state, now_ms| {
            let task = state
                .tasks
                .get(&task_id)
                .ok_or_else(|| "task not found".to_owned())?;
            if task_status_allows_runtime_cleanup(task.status)
                || task
                    .execution
                    .as_ref()
                    .is_some_and(|execution| execution.inspection)
                || (only_running && task.status != TaskStatus::Running)
                || task
                    .execution
                    .as_ref()
                    .is_none_or(|execution| execution.execution_id != execution_id)
            {
                return Ok(false);
            }
            state.update_task_status(&task_id, TaskStatus::Blocked, now_ms)?;
            let task = state.tasks.get_mut(&task_id).unwrap();
            task.outcome = Some(outcome);
            if let Some(blockers) = blockers {
                task.blockers = blockers;
            }
            Ok(true)
        })
    }

    /// Replace the durable execution binding with a new attempt identity.
    /// Used by the launch transaction when a previous attempt failed.
    pub(crate) fn replace_execution(
        &self,
        task_id: TaskId,
        execution: TaskExecution,
    ) -> Result<TaskExecution, String> {
        self.mutate(|state, now_ms| state.record_execution(&task_id, execution, now_ms))
    }

    pub(crate) fn record_plan_layout(
        &self,
        key: String,
        layout: taskr_core::orchestration::PlanLayout,
    ) -> Result<(), String> {
        self.mutate(|state, _| {
            if !state.plans.contains_key(&layout.plan_id) {
                return Err("plan removed during layout allocation".into());
            }
            state.plan_layouts.insert(key, layout);
            Ok(())
        })
    }

    pub(crate) fn add_task_edge(&self, input: CreateTaskEdge) -> Result<TaskEdge, String> {
        self.mutate(|state, now_ms| state.add_task_edge(input, now_ms))
    }

    pub(crate) fn remove_task_edge(
        &self,
        from: TaskId,
        to: TaskId,
        kind: TaskEdgeKind,
    ) -> Result<(), String> {
        self.mutate(|state, now_ms| state.remove_task_edge(&from, &to, kind, now_ms))
    }

    pub(crate) fn update_task_status(
        &self,
        task_id: TaskId,
        status: TaskStatus,
    ) -> Result<Task, String> {
        self.mutate(|state, now_ms| state.update_task_status(&task_id, status, now_ms))
    }

    pub(crate) fn update_task_status_details(
        &self,
        task_id: TaskId,
        status: TaskStatus,
        outcome: Option<String>,
        blockers: Option<Vec<String>>,
        evidence: Option<Vec<String>>,
    ) -> Result<Task, String> {
        self.mutate(|state, now_ms| {
            let task = state
                .tasks
                .get(&task_id)
                .ok_or_else(|| format!("task '{}' not found", task_id.0))?;

            let outcome = outcome.map(|outcome| outcome.trim().to_owned());
            let gated_passed_or_delivered = !task.gates.is_empty()
                && matches!(status, TaskStatus::Passed | TaskStatus::Delivered);
            if gated_passed_or_delivered
                && outcome.as_deref().is_some_and(|outcome| outcome.is_empty())
            {
                return Err(format!(
                    "task '{}' has gates; outcome must not be empty before setting status to {:?}",
                    task_id.0, status
                ));
            }

            let has_operator_outcome = outcome
                .as_deref()
                .is_some_and(|outcome| !outcome.trim().is_empty());
            if gated_passed_or_delivered && !has_operator_outcome {
                return Err(format!(
                    "task '{}' has gates; outcome is required before setting status to {:?}",
                    task_id.0, status
                ));
            }

            state.update_task_status(&task_id, status, now_ms)?;
            let task = state
                .tasks
                .get_mut(&task_id)
                .ok_or_else(|| format!("task '{}' not found", task_id.0))?;
            if let Some(outcome) = outcome {
                task.outcome = Some(outcome.trim().to_owned());
            }
            if let Some(blockers) = blockers {
                task.blockers = blockers;
            }
            if let Some(evidence) = evidence {
                task.evidence = evidence;
            }
            task.updated_at_ms = now_ms;
            Ok(task.clone())
        })
    }

    pub(crate) fn namespace_lost(&self, execution_id: &str, observed: &str) -> Result<(), String> {
        self.mutate(|state, now| {
            taskr_core::coordination::namespace_lost(state, execution_id, observed, now)
        })
    }

    pub(crate) fn migrate_endpoint(
        &self,
        legacy_node_id: &str,
        endpoint_id: &str,
    ) -> Result<crate::endpoint_migration::EndpointMigrationReport, String> {
        self.mutate(|state, now_ms| {
            crate::endpoint_migration::migrate_endpoint_records(
                state,
                legacy_node_id,
                endpoint_id,
                now_ms,
            )
        })
    }

    pub(crate) fn delete_project(
        &self,
        project_id_or_slug: &str,
    ) -> Result<DeletedProjectReport, String> {
        self.mutate(|state, _now_ms| {
            let selector = project_id_or_slug.trim();
            let project_id = state
                .projects
                .values()
                .find(|project| project.id.0 == selector || project.slug == selector)
                .map(|project| project.id.clone())
                .ok_or_else(|| format!("project '{selector}' not found"))?;
            let plan_ids = state
                .plans
                .values()
                .filter(|plan| plan.project_id == project_id)
                .map(|plan| plan.id.clone())
                .collect::<Vec<_>>();
            let task_ids = state
                .tasks
                .values()
                .filter(|task| plan_ids.contains(&task.plan_id))
                .map(|task| task.id.clone())
                .collect::<Vec<_>>();
            let deleted_edge_count = state
                .task_edges
                .iter()
                .filter(|edge| task_ids.contains(&edge.from) || task_ids.contains(&edge.to))
                .count();
            state
                .task_edges
                .retain(|edge| !task_ids.contains(&edge.from) && !task_ids.contains(&edge.to));
            for plan_id in &plan_ids {
                state.plans.remove(plan_id);
            }
            state
                .plan_layouts
                .retain(|_, layout| !plan_ids.contains(&layout.plan_id));
            for task_id in &task_ids {
                if let Some(task) = state.tasks.remove(task_id) {
                    if let Some(execution) = task.execution.filter(|execution| {
                        execution.phase == ExecutionPhase::Stopped && execution.pane_id.is_some()
                    }) {
                        state.retained_executions.insert(
                            execution.execution_id.clone(),
                            taskr_core::orchestration::RetainedExecution {
                                task_id: task_id.clone(),
                                execution,
                            },
                        );
                    }
                }
            }
            let project = state
                .projects
                .remove(&project_id)
                .ok_or_else(|| format!("project '{selector}' not found"))?;
            Ok(DeletedProjectReport {
                project,
                deleted_plan_count: plan_ids.len(),
                deleted_task_count: task_ids.len(),
                deleted_edge_count,
            })
        })
    }

    pub(crate) fn prune_stale_execution_records(
        &self,
        live_runtime_keys: &HashSet<String>,
        observed_endpoints: &HashSet<String>,
        dry_run: bool,
        include_stale_execution_records: bool,
        include_finished_plans: bool,
        older_than_days: Option<u64>,
    ) -> Result<LocalPruneStoreReport, String> {
        let now = now_ms();
        let cutoff_ms = older_than_days
            .map(|days| {
                days.checked_mul(86_400_000)
                    .and_then(|duration_ms| now.checked_sub(duration_ms))
                    .ok_or_else(|| format!("--older-than-days value {days} is too large"))
            })
            .transpose()?;
        if dry_run {
            let guard = self.lock()?;
            let candidates = if include_stale_execution_records {
                stale_execution_candidates(
                    guard.state(),
                    live_runtime_keys,
                    observed_endpoints,
                    cutoff_ms,
                )
            } else {
                Vec::new()
            };
            let mut preview = guard.snapshot();
            for candidate in &candidates {
                if let Some(task) = preview.tasks.get_mut(&TaskId(candidate.task_id.clone())) {
                    if task
                        .execution
                        .as_ref()
                        .is_some_and(|execution| execution.execution_id == candidate.execution_id)
                    {
                        task.execution = None;
                    }
                }
                preview.retained_executions.remove(&candidate.execution_id);
            }
            let pruned_plan_count =
                prune_finished_plans(&mut preview, include_finished_plans, cutoff_ms);
            return Ok(LocalPruneStoreReport {
                dry_run,
                include_stale_execution_records,
                include_finished_plans,
                pruned_execution_count: candidates.len(),
                pruned_plan_count,
                candidates,
            });
        }

        self.mutate(|state, _now_ms| {
            let candidates = if include_stale_execution_records {
                stale_execution_candidates(state, live_runtime_keys, observed_endpoints, cutoff_ms)
            } else {
                Vec::new()
            };
            for candidate in &candidates {
                if let Some(task) = state.tasks.get_mut(&TaskId(candidate.task_id.clone())) {
                    if task
                        .execution
                        .as_ref()
                        .is_some_and(|execution| execution.execution_id == candidate.execution_id)
                    {
                        task.execution = None;
                    }
                }
                state.retained_executions.remove(&candidate.execution_id);
            }
            let pruned_plan_count = prune_finished_plans(state, include_finished_plans, cutoff_ms);
            Ok(LocalPruneStoreReport {
                dry_run,
                include_stale_execution_records,
                include_finished_plans,
                pruned_execution_count: candidates.len(),
                pruned_plan_count,
                candidates,
            })
        })
    }

    fn mutate<T>(
        &self,
        apply: impl FnOnce(&mut OrchestrationState, u64) -> Result<T, String>,
    ) -> Result<T, String> {
        self.lock()?
            .mutate(now_ms(), apply)
            .map_err(|error| error.to_string())
    }

    fn lock(&self) -> Result<MutexGuard<'_, OrchestrationRuntimeState>, String> {
        self.inner
            .lock()
            .map_err(|_| "orchestration state lock poisoned".to_owned())
    }
}

/// A durable execution record whose runtime resources were not observed live.
/// Only records attached to finished tasks are eligible, and only on
/// endpoints whose full agent AND pane inventories were observed: a partial
/// or failed observation can never prove a binding's runtime is gone, so
/// such bindings are kept.
fn stale_execution_candidates(
    state: &OrchestrationState,
    live_runtime_keys: &HashSet<String>,
    observed_endpoints: &HashSet<String>,
    cutoff_ms: Option<u64>,
) -> Vec<LocalPruneExecutionCandidate> {
    let current = state.tasks.values().filter_map(|task| {
        task.execution.as_ref().map(|execution| {
            (
                &task.id,
                execution,
                task_status_allows_runtime_cleanup(task.status),
            )
        })
    });
    let retained = state
        .retained_executions
        .values()
        .map(|retained| (&retained.task_id, &retained.execution, true));
    let mut candidates = current
        .chain(retained)
        .filter_map(|(task_id, execution, finished)| {
            if taskr_core::coordination::unresolved_allocation(execution) {
                return None;
            }
            if !observed_endpoints.contains(&execution.endpoint_id) {
                return None;
            }
            if live_runtime_keys.contains(&execution.runtime_key()) {
                return None;
            }
            if execution.agent_name.as_ref().is_some_and(|name| {
                live_runtime_keys.contains(&taskr_core::orchestration::runtime_key_for(
                    &execution.endpoint_id,
                    execution.runtime_generation.as_deref(),
                    name,
                ))
            }) {
                return None;
            }
            if cutoff_ms.is_some_and(|cutoff_ms| execution.last_seen_ms > cutoff_ms) {
                return None;
            }
            if !finished && execution.phase != ExecutionPhase::Stopped {
                return None;
            }
            let reason = if execution.recovery == ExecutionRecovery::NeedsReconciliation {
                "unreconciled execution binding attached only to finished tasks".into()
            } else {
                "execution binding missing from live endpoint observation".into()
            };
            Some(LocalPruneExecutionCandidate {
                key: execution.runtime_key(),
                endpoint_id: execution.endpoint_id.clone(),
                execution_id: execution.execution_id.clone(),
                pane_id: execution.pane_id.clone(),
                agent_name: execution.agent_name.clone(),
                task_id: task_id.0.clone(),
                last_seen_ms: execution.last_seen_ms,
                reason,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.endpoint_id
            .cmp(&right.endpoint_id)
            .then_with(|| left.execution_id.cmp(&right.execution_id))
            .then_with(|| left.key.cmp(&right.key))
    });
    candidates
}

/// Result of deleting a project: the removed project plus the counts of the
/// plans, task cards, and edges that went away with it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DeletedProjectReport {
    pub project: Project,
    pub deleted_plan_count: usize,
    pub deleted_task_count: usize,
    pub deleted_edge_count: usize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct LocalPruneExecutionCandidate {
    pub(crate) key: String,
    pub(crate) endpoint_id: String,
    pub(crate) execution_id: String,
    pub(crate) pane_id: Option<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) task_id: String,
    pub(crate) last_seen_ms: u64,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct LocalPruneStoreReport {
    pub(crate) dry_run: bool,
    pub(crate) include_stale_execution_records: bool,
    pub(crate) include_finished_plans: bool,
    pub(crate) pruned_execution_count: usize,
    pub(crate) pruned_plan_count: usize,
    pub(crate) candidates: Vec<LocalPruneExecutionCandidate>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};
    use taskr_core::orchestration::{ExecutionPhase, TaskScope};

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
    }

    fn create_plan(project_id: ProjectId) -> CreatePlan {
        CreatePlan {
            project_id,
            title: "Plan".into(),
            brief: "Detailed test plan brief.".into(),
            instructions: None,
            slug: None,
        }
    }

    fn create_task(plan_id: PlanId, title: &str) -> CreateTask {
        CreateTask {
            launch_hints: Default::default(),
            plan_id,
            title: title.into(),
            objective: format!("Objective for {title}"),
            scope: TaskScope::default(),
            gates: Vec::new(),
            slug: None,
            auto_schedule: false,
            run_spec: None,
        }
    }

    fn create_project() -> CreateProject {
        CreateProject {
            title: "Project".into(),
            description: "Actor test project".into(),
            slug: None,
            ..Default::default()
        }
    }

    fn execution(execution_id: &str) -> TaskExecution {
        TaskExecution {
            launch_options: Default::default(),
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
            launch_args: Vec::new(),
            launch_env: Default::default(),
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
            phase: ExecutionPhase::Live,
            recovery: ExecutionRecovery::Reconciled,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_seen_ms: 0,
        }
    }

    #[test]
    fn startup_with_empty_store_uses_empty_state() {
        let dir = unique_temp_dir("taskr-orchestration-empty");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();

        let state = handle.snapshot().unwrap();

        assert!(state.tasks.is_empty());
        assert!(state.task_edges.is_empty());
        assert_eq!(state.next_task_id, 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn startup_with_existing_snapshot_loads_previous_state() {
        let dir = unique_temp_dir("taskr-orchestration-existing");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let mut state = OrchestrationState::new();
        let project = state.create_project(create_project(), 99).unwrap();
        let plan = state.create_plan(create_plan(project.id), 100).unwrap();
        let task = state
            .create_task(create_task(plan.id, "Persisted"), 101)
            .unwrap();
        state
            .record_execution(&task.id, execution("exec-a"), 200)
            .unwrap();
        store.save(&state, 300).unwrap();

        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let loaded = handle.snapshot().unwrap();

        assert_eq!(loaded.tasks.len(), 1);
        assert!(loaded.tasks.contains_key(&task.id));
        assert!(loaded.tasks.get(&task.id).unwrap().execution.is_some());
        assert_eq!(loaded.next_task_id, state.next_task_id);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn successful_mutation_persists_state() {
        let dir = unique_temp_dir("taskr-orchestration-persist");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();

        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id)).unwrap();
        let task = handle.create_task(create_task(plan.id, "Saved")).unwrap();
        let reloaded = OrchestrationHandle::open(Some(&dir)).unwrap();
        let state = reloaded.snapshot().unwrap();

        assert!(state.tasks.contains_key(&task.id));
        assert_eq!(state.next_task_id, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn task_execution_edge_and_status_mutations_persist() {
        let dir = unique_temp_dir("taskr-orchestration-mutations");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id)).unwrap();
        let parent = handle
            .create_task(create_task(plan.id.clone(), "Parent"))
            .unwrap();
        let child = handle.create_task(create_task(plan.id, "Child")).unwrap();

        handle
            .add_task_edge(CreateTaskEdge {
                from: parent.id.clone(),
                to: child.id.clone(),
                kind: TaskEdgeKind::ParentOf,
                note: Some("breakdown".into()),
            })
            .unwrap();
        handle
            .update_task_status(child.id.clone(), TaskStatus::Running)
            .unwrap();
        handle
            .record_execution(child.id.clone(), execution("exec-a"))
            .unwrap();
        handle
            .remove_task_edge(parent.id.clone(), child.id.clone(), TaskEdgeKind::ParentOf)
            .unwrap();

        let reloaded = OrchestrationHandle::open(Some(&dir)).unwrap();
        let state = reloaded.snapshot().unwrap();
        let loaded_child = state.tasks.get(&child.id).unwrap();

        assert_eq!(loaded_child.status, TaskStatus::Running);
        assert!(loaded_child.execution.is_some());
        assert!(state.task_edges.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_mutation_leaves_previous_snapshot_intact() {
        let dir = unique_temp_dir("taskr-orchestration-failed");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id)).unwrap();
        let task = handle
            .create_task(create_task(plan.id, "Only Task"))
            .unwrap();

        let error = handle
            .record_execution(TaskId("missing".into()), execution("exec-a"))
            .unwrap_err();
        let reloaded = OrchestrationHandle::open(Some(&dir)).unwrap();
        let state = reloaded.snapshot().unwrap();

        assert!(error.contains("task 'missing' not found"));
        assert_eq!(state.tasks.len(), 1);
        assert!(state.tasks.contains_key(&task.id));
        assert!(state.tasks.get(&task.id).unwrap().execution.is_none());
        assert_eq!(state.next_task_id, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_stale_store_records_prunes_finished_plans_and_contained_tasks() {
        let dir = unique_temp_dir("taskr-orchestration-prune-plans");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id.clone())).unwrap();
        let parent = handle
            .create_task(create_task(plan.id.clone(), "Parent"))
            .unwrap();
        let child = handle
            .create_task(create_task(plan.id.clone(), "Child"))
            .unwrap();
        handle
            .add_task_edge(CreateTaskEdge {
                from: parent.id.clone(),
                to: child.id.clone(),
                kind: TaskEdgeKind::Related,
                note: None,
            })
            .unwrap();
        handle
            .record_execution(child.id.clone(), execution("exec-a"))
            .unwrap();
        handle
            .update_task_status(parent.id.clone(), TaskStatus::Delivered)
            .unwrap();
        handle
            .update_task_status(child.id.clone(), TaskStatus::Delivered)
            .unwrap();
        handle
            .update_plan_status(plan.id.clone(), PlanStatus::Delivered, Some("done".into()))
            .unwrap();

        let live = HashSet::new();
        let mut observed = HashSet::new();
        observed.insert("local".into());
        let dry_run = handle
            .prune_stale_execution_records(&live, &observed, true, true, true, None)
            .unwrap();
        assert_eq!(dry_run.pruned_execution_count, 1);
        assert_eq!(dry_run.pruned_plan_count, 1);
        assert_eq!(handle.snapshot().unwrap().plans.len(), 1);

        let pruned = handle
            .prune_stale_execution_records(&live, &observed, false, true, true, None)
            .unwrap();
        assert_eq!(pruned.pruned_execution_count, 1);
        assert_eq!(pruned.pruned_plan_count, 1);
        let state = handle.snapshot().unwrap();
        assert!(state.projects.contains_key(&project.id));
        assert!(state.plans.is_empty());
        assert!(state.tasks.is_empty());
        assert!(state.task_edges.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_keeps_executions_still_observed_live() {
        let dir = unique_temp_dir("taskr-orchestration-prune-live");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id)).unwrap();
        let task = handle
            .create_task(create_task(plan.id, "Finished But Live"))
            .unwrap();
        let mut recorded = execution("exec-a");
        recorded.pane_id = Some("w5:p1".into());
        handle.record_execution(task.id.clone(), recorded).unwrap();
        handle
            .update_task_status(task.id.clone(), TaskStatus::Delivered)
            .unwrap();

        let mut live = HashSet::new();
        live.insert("local:w5:p1".into());
        let mut observed = HashSet::new();
        observed.insert("local".into());
        let report = handle
            .prune_stale_execution_records(&live, &observed, false, true, false, None)
            .unwrap();

        assert_eq!(report.pruned_execution_count, 0);
        assert!(handle
            .snapshot()
            .unwrap()
            .tasks
            .get(&task.id)
            .unwrap()
            .execution
            .is_some());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_create_calls_serialize_task_ids_and_slugs() {
        let dir = unique_temp_dir("taskr-orchestration-concurrent");
        let handle = OrchestrationHandle::open(Some(&dir)).unwrap();
        let project = handle.create_project(create_project()).unwrap();
        let plan = handle.create_plan(create_plan(project.id)).unwrap();

        let threads = (0..12)
            .map(|_| {
                let handle = handle.clone();
                let plan_id = plan.id.clone();
                thread::spawn(move || {
                    handle
                        .create_task(create_task(plan_id, "Same Title"))
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let tasks = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let ids = tasks
            .iter()
            .map(|task| task.id.0.clone())
            .collect::<HashSet<_>>();
        let slugs = tasks
            .iter()
            .map(|task| task.slug.clone())
            .collect::<HashSet<_>>();
        let reloaded = OrchestrationHandle::open(Some(&dir)).unwrap();
        let state = reloaded.snapshot().unwrap();

        assert_eq!(ids.len(), 12);
        assert_eq!(slugs.len(), 12);
        assert_eq!(state.tasks.len(), 12);
        assert_eq!(state.next_task_id, 13);
        let _ = fs::remove_dir_all(dir);
    }
}
