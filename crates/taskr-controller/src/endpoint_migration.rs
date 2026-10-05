//! One-time conversion of unresolved legacy node references to Herdr endpoints.
//! Resolved records store the endpoint ID directly; there is no routing alias map.

use std::collections::BTreeMap;

use serde::Serialize;
use taskr_core::orchestration::OrchestrationState;

const LEGACY_ENDPOINT_PREFIX: &str = "legacy-node:";

pub(crate) fn unresolved_endpoint_id(node_id: &str) -> String {
    format!("{LEGACY_ENDPOINT_PREFIX}{node_id}")
}

pub(crate) fn legacy_node_id(endpoint_id: &str) -> Option<&str> {
    endpoint_id.strip_prefix(LEGACY_ENDPOINT_PREFIX)
}

pub(crate) fn require_resolved_endpoint(endpoint_id: &str) -> Result<(), String> {
    if let Some(node_id) = legacy_node_id(endpoint_id) {
        return Err(format!(
            "legacy node '{node_id}' needs endpoint_migrate before it can be used; \
             select a saved Herdr machine profile explicitly"
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub(crate) struct EndpointMigrationReport {
    pub legacy_node_id: String,
    pub endpoint_id: String,
    pub run_specs_updated: usize,
    pub executions_updated: usize,
    pub task_ids: Vec<String>,
}

/// Called within the orchestration handle's clone/save/commit transaction.
/// Only marked legacy references are eligible, so modern records cannot be
/// retargeted even if their profile ID happens to equal an old node label.
pub(crate) fn migrate_endpoint_records(
    state: &mut OrchestrationState,
    node_id: &str,
    endpoint_id: &str,
    now_ms: u64,
) -> Result<EndpointMigrationReport, String> {
    let node_id = node_id.trim();
    let endpoint_id = endpoint_id.trim();
    if node_id.is_empty() || node_id == "local" {
        return Err("legacy_node_id must be a nonempty remote node ID".into());
    }
    if endpoint_id.is_empty() {
        return Err("endpoint_id must not be empty".into());
    }
    require_resolved_endpoint(endpoint_id)?;
    let unresolved = unresolved_endpoint_id(node_id);
    let mut report = EndpointMigrationReport {
        legacy_node_id: node_id.into(),
        endpoint_id: endpoint_id.into(),
        run_specs_updated: 0,
        executions_updated: 0,
        task_ids: Vec::new(),
    };
    for task in state.tasks.values_mut() {
        let mut changed = false;
        if let Some(spec) = task.run_spec.as_mut() {
            if spec.endpoint_id == unresolved {
                spec.endpoint_id = endpoint_id.into();
                report.run_specs_updated += 1;
                changed = true;
            }
        }
        if let Some(execution) = task.execution.as_mut() {
            if execution.endpoint_id == unresolved {
                execution.endpoint_id = endpoint_id.into();
                execution.updated_at_ms = now_ms;
                report.executions_updated += 1;
                changed = true;
            }
        }
        if changed {
            task.updated_at_ms = now_ms;
            report.task_ids.push(task.id.0.clone());
        }
    }
    report.task_ids.sort();
    Ok(report)
}

/// Discover outstanding work from the records themselves, without saving
/// aliases or inferring anything from the machine catalog.
pub(crate) fn pending_endpoint_migrations(state: &OrchestrationState) -> BTreeMap<String, usize> {
    let mut pending: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for task in state.tasks.values() {
        for endpoint in task
            .run_spec
            .as_ref()
            .map(|spec| spec.endpoint_id.as_str())
            .into_iter()
            .chain(
                task.execution
                    .as_ref()
                    .map(|execution| execution.endpoint_id.as_str()),
            )
        {
            if let Some(node_id) = legacy_node_id(endpoint) {
                pending
                    .entry(node_id.into())
                    .or_default()
                    .push(task.id.0.clone());
            }
        }
    }
    for tasks in pending.values_mut() {
        tasks.sort();
        tasks.dedup();
    }
    pending
        .into_iter()
        .map(|(node_id, tasks)| (node_id, tasks.len()))
        .collect()
}
