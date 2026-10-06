#![allow(dead_code)]

use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use taskr_core::orchestration::OrchestrationState;
use taskr_core::SnapshotStore;

use crate::store_paths::{ensure_store_dir, resolve_store_path};

const DB_FILE_NAME: &str = "taskr.db";
const SNAPSHOT_VERSION: i64 = 7;

/// TASKR host implementation; SQL, migrations and filesystem paths stay here.
#[derive(Clone)]
pub(crate) struct SqliteOrchestrationStore {
    db_path: PathBuf,
}

impl SnapshotStore for SqliteOrchestrationStore {
    type Error = String;

    fn load(&self) -> Result<Option<OrchestrationState>, Self::Error> {
        SqliteOrchestrationStore::load(self)
    }

    fn save(&self, state: &OrchestrationState, now_ms: u64) -> Result<(), Self::Error> {
        SqliteOrchestrationStore::save(self, state, now_ms)
    }
}

impl SqliteOrchestrationStore {
    pub(crate) fn open(store_path: PathBuf) -> Result<Self, String> {
        let store_path = resolve_store_path(Some(&store_path))?;
        ensure_store_dir(&store_path)?;
        let db_path = store_path.join(DB_FILE_NAME);
        let store = Self { db_path };
        store.initialize()?;
        Ok(store)
    }

    pub(crate) fn load(&self) -> Result<Option<OrchestrationState>, String> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| {
                format!(
                    "failed to load orchestration snapshot from '{}': {}",
                    self.db_path.display(),
                    error
                )
            })?;

        let Some((version, state_json)) = row else {
            return Ok(None);
        };
        if version > SNAPSHOT_VERSION {
            return Err(format!(
                "unsupported orchestration snapshot version {} in '{}' (this build supports up to {SNAPSHOT_VERSION})",
                version,
                self.db_path.display()
            ));
        }

        if version < SNAPSHOT_VERSION {
            self.backup_before_migration(&connection, version)?;
        }

        let original_state_json = state_json;
        let state_json = migrate_orchestration_snapshot_json(&original_state_json, version)
            .map_err(|error| {
                format!(
                    "failed to migrate orchestration snapshot from '{}': {}",
                    self.db_path.display(),
                    error
                )
            })?;

        let state = serde_json::from_str(&state_json).map_err(|error| {
            format!(
                "failed to deserialize orchestration snapshot from '{}': {}",
                self.db_path.display(),
                error
            )
        })?;
        if version < SNAPSHOT_VERSION {
            // Persist the upgrade once. A concurrent writer must not be
            // overwritten with a migration of an older snapshot.
            let changed = connection
                .execute(
                    "UPDATE orchestration_snapshots SET version = ?1, state_json = ?2
                 WHERE id = 1 AND version = ?3 AND state_json = ?4",
                    params![SNAPSHOT_VERSION, state_json, version, original_state_json],
                )
                .map_err(|error| format!("failed to persist store migration: {error}"))?;
            if changed != 1 {
                return Err("store changed during migration; reopen it before continuing".into());
            }
        }
        Ok(Some(state))
    }

    /// Copy the pre-migration database aside so the operator can roll back.
    fn backup_before_migration(&self, connection: &Connection, version: i64) -> Result<(), String> {
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_row| Ok(()))
            .optional()
            .ok();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let file_name = self
            .db_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("taskr.db")
            .to_owned();
        let backup_path = self
            .db_path
            .with_file_name(format!("{file_name}.v{version}-{stamp}.bak"));
        std::fs::copy(&self.db_path, &backup_path).map_err(|error| {
            format!(
                "failed to back up pre-migration store '{}' to '{}': {}",
                self.db_path.display(),
                backup_path.display(),
                error
            )
        })?;
        Ok(())
    }

    pub(crate) fn save(&self, state: &OrchestrationState, now_ms: u64) -> Result<(), String> {
        let updated_at_ms = i64::try_from(now_ms).map_err(|_| {
            format!(
                "orchestration snapshot timestamp {} exceeds SQLite INTEGER range",
                now_ms
            )
        })?;
        let state_json = serde_json::to_string(state).map_err(|error| {
            format!(
                "failed to serialize orchestration snapshot for '{}': {}",
                self.db_path.display(),
                error
            )
        })?;

        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET
                    version = excluded.version,
                    state_json = excluded.state_json,
                    updated_at_ms = excluded.updated_at_ms",
                params![SNAPSHOT_VERSION, state_json, updated_at_ms],
            )
            .map_err(|error| {
                format!(
                    "failed to save orchestration snapshot to '{}': {}",
                    self.db_path.display(),
                    error
                )
            })?;
        Ok(())
    }

    fn initialize(&self) -> Result<(), String> {
        let connection = self.connect()?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS orchestration_snapshots (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    version INTEGER NOT NULL,
                    state_json TEXT NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );",
            )
            .map_err(|error| {
                format!(
                    "failed to initialize orchestration store '{}': {}",
                    self.db_path.display(),
                    error
                )
            })?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection, String> {
        Connection::open(&self.db_path).map_err(|error| {
            format!(
                "failed to open orchestration store '{}': {}",
                self.db_path.display(),
                error
            )
        })
    }
}

fn migrate_orchestration_snapshot_json(
    state_json: &str,
    version: i64,
) -> Result<String, serde_json::Error> {
    let mut value = serde_json::from_str::<Value>(state_json)?;
    if version < 2 {
        migrate_v1_to_v2(&mut value);
    }
    if version < 3 {
        migrate_v2_to_v3(&mut value);
    }
    // Remove this legacy edge-kind migration after September 1, 2026, once
    // active orchestration snapshots have been migrated by normal loads/saves.
    migrate_legacy_blocks_edges(&mut value);
    drop_removed_refines_edges(&mut value);
    add_missing_plan_instructions(&mut value);
    if version < 7 {
        if let Some(projects) = value.get_mut("projects").and_then(Value::as_object_mut) {
            for project in projects.values_mut().filter_map(Value::as_object_mut) {
                project
                    .entry("environment_ids")
                    .or_insert_with(|| json!([]));
            }
        }
    }
    serde_json::to_string(&value)
}

/// v1 → v2: tmux task sessions become Herdr-shaped task executions.
///
/// Domain data (projects, plans, tasks, edges) is preserved untouched. Every
/// migrated runtime binding is marked NeedsReconciliation: TASKR never infers
/// a Herdr endpoint mapping from an old node label.
fn migrate_v1_to_v2(value: &mut Value) {
    let Some(tasks) = value.get_mut("tasks").and_then(Value::as_object_mut) else {
        return;
    };

    for task in tasks.values_mut() {
        let Some(object) = task.as_object_mut() else {
            continue;
        };

        if let Some(run_spec) = object.get_mut("run_spec").filter(|spec| spec.is_object()) {
            let spec = run_spec.as_object_mut().unwrap();
            if let Some(node_id) = spec.remove("node_id") {
                spec.insert(
                    "endpoint_id".into(),
                    Value::String(migrate_endpoint_id(&node_id)),
                );
            }
            if let Some(profile) = spec.remove("profile") {
                spec.insert("launch_profile_id".into(), profile);
            }
        }

        let Some(session) = object
            .remove("session")
            .filter(|session| session.is_object())
        else {
            continue;
        };
        let session = session.as_object().unwrap();
        let node_id = session
            .get("node_id")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let session_name = session
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let endpoint_id = migrate_endpoint_id(&node_id);
        let execution_id = format!(
            "migrated-{endpoint_id}-{}",
            sanitize_execution_id_part(&session_name)
        );

        let execution = serde_json::json!({
            "execution_id": execution_id,
            "endpoint_id": endpoint_id,
            "launch_profile_id": session.get("profile").cloned().unwrap_or(Value::Null),
            "launch_args": [],
            "launch_env": {},
            "bypass_permissions": session.get("bypass_permissions").cloned().unwrap_or(Value::Bool(false)),
            "workspace_path": session.get("workspace_path").cloned().unwrap_or(Value::Null),
            "role": session.get("role").cloned().unwrap_or(Value::Null),
            "kind": session.get("kind").cloned().unwrap_or(Value::Null),
            "skills": session.get("skills").cloned().unwrap_or(Value::Array(Vec::new())),
            "workspace_id": Value::Null,
            "tab_id": Value::Null,
            "pane_id": Value::Null,
            "agent_name": Value::Null,
            "agent_kind": Value::Null,
            "agent_session": Value::Null,
            "phase": "unavailable",
            "recovery": "needs_reconciliation",
            "created_at_ms": session.get("created_at_ms").cloned().unwrap_or(Value::Number(0.into())),
            "updated_at_ms": session.get("updated_at_ms").cloned().unwrap_or(Value::Number(0.into())),
            "last_seen_ms": session.get("last_seen_ms").cloned().unwrap_or(Value::Number(0.into())),
        });
        object.insert("execution".into(), execution);
    }
}

/// Intermediate v2 conversion only: v3 marks remote legacy labels unresolved
/// before any snapshot is deserialized or used by the runtime.
fn migrate_endpoint_id(node_id: &Value) -> String {
    match node_id.as_str().map(str::trim) {
        Some("local") => "local".into(),
        Some(other) if !other.is_empty() => other.to_owned(),
        _ => "local".into(),
    }
}

/// Consume the provisional alias map and store resolved endpoints directly.
/// v2 did not record provenance for unbound remote run specs, so ambiguous
/// specs require explicit confirmation instead of guessing a machine.
fn migrate_v2_to_v3(value: &mut Value) {
    use crate::endpoint_migration::{legacy_node_id, unresolved_endpoint_id};

    let aliases = value
        .as_object_mut()
        .and_then(|object| object.remove("endpoint_mappings"))
        .and_then(|aliases| aliases.as_object().cloned())
        .unwrap_or_default();
    let Some(tasks) = value.get_mut("tasks").and_then(Value::as_object_mut) else {
        return;
    };
    for task in tasks.values_mut() {
        let modern_endpoint = task.get("execution").and_then(|execution| {
            let id = execution.get("execution_id")?.as_str()?;
            let endpoint = execution.get("endpoint_id")?.as_str()?;
            (!id.starts_with("migrated-") && legacy_node_id(endpoint).is_none())
                .then(|| endpoint.to_owned())
        });
        let migrated_execution = task
            .get("execution")
            .and_then(|execution| execution.get("execution_id"))
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("migrated-"));
        for field in ["run_spec", "execution"] {
            // Existing Herdr executions already have frozen endpoint IDs;
            // an alias must never retarget their live bindings.
            if field == "execution" && !migrated_execution {
                continue;
            }
            let Some(endpoint) = task
                .get_mut(field)
                .and_then(|record| record.get_mut("endpoint_id"))
            else {
                continue;
            };
            let Some(old) = endpoint.as_str() else {
                continue;
            };
            if old == "local" || legacy_node_id(old).is_some() {
                continue;
            }
            if field == "run_spec" && modern_endpoint.as_deref() == Some(old) {
                continue;
            }
            let resolved = aliases
                .get(old)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .filter(|id| legacy_node_id(id).is_none());
            *endpoint = Value::String(match resolved {
                Some(profile) => profile.to_owned(),
                None => unresolved_endpoint_id(old),
            });
        }
    }
}

fn sanitize_execution_id_part(text: &str) -> String {
    let mut part = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            part.push(ch);
        } else {
            part.push('-');
        }
    }
    if part.is_empty() {
        part.push('x');
    }
    part
}

fn migrate_legacy_blocks_edges(value: &mut Value) {
    let Some(edges) = value.get_mut("task_edges").and_then(Value::as_array_mut) else {
        return;
    };

    for edge in edges.iter_mut() {
        let Some(object) = edge.as_object_mut() else {
            continue;
        };
        let is_blocks = object
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "Blocks");
        if !is_blocks {
            continue;
        }
        let from = object.remove("from");
        let to = object.remove("to");
        object.insert("kind".into(), Value::String("DependsOn".into()));
        if let Some(from) = from {
            object.insert("to".into(), from);
        }
        if let Some(to) = to {
            object.insert("from".into(), to);
        }
    }

    let mut seen = std::collections::HashSet::new();
    edges.retain(|edge| match edge.as_object().and_then(edge_dedupe_key) {
        Some(key) => seen.insert(key),
        None => true,
    });
}

fn drop_removed_refines_edges(value: &mut Value) {
    let Some(edges) = value.get_mut("task_edges").and_then(Value::as_array_mut) else {
        return;
    };
    edges.retain(|edge| {
        edge.as_object()
            .and_then(|object| object.get("kind"))
            .and_then(Value::as_str)
            != Some("Refines")
    });
}

fn add_missing_plan_instructions(value: &mut Value) {
    let Some(plans) = value.get_mut("plans").and_then(Value::as_object_mut) else {
        return;
    };

    for plan in plans.values_mut() {
        let Some(object) = plan.as_object_mut() else {
            continue;
        };
        object.entry("instructions").or_insert_with(|| Value::Null);
    }
}

fn edge_dedupe_key(edge: &Map<String, Value>) -> Option<(String, String, String)> {
    Some((
        edge.get("from")?.as_str()?.to_owned(),
        edge.get("to")?.as_str()?.to_owned(),
        edge.get("kind")?.as_str()?.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    use taskr_core::orchestration::{
        CreatePlan, CreateProject, CreateTask, CreateTaskEdge, ExecutionPhase, ExecutionRecovery,
        PlanId, ProjectId, TaskEdgeKind, TaskId, TaskScope,
    };

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
            scope: TaskScope {
                include_paths: vec!["src".into()],
                exclude_paths: vec!["target".into()],
                notes: Some("focused scope".into()),
            },
            gates: vec!["cargo test".into()],
            slug: None,
            auto_schedule: false,
            run_spec: None,
        }
    }

    fn create_project() -> CreateProject {
        CreateProject {
            title: "Project".into(),
            description: "Store test project".into(),
            slug: None,
            ..Default::default()
        }
    }

    fn v1_session_json(node_id: &str, session: &str) -> Value {
        serde_json::json!({
            "node_id": node_id,
            "session": session,
            "profile": "codex",
            "workspace_path": "/workspace/project",
            "bypass_permissions": false,
            "role": "implementation-worker",
            "kind": "codex",
            "skills": ["rust"],
            "created_at_ms": 300,
            "updated_at_ms": 310,
            "last_seen_ms": 320
        })
    }

    fn v1_run_spec_json(node_id: &str) -> Value {
        serde_json::json!({
            "node_id": node_id,
            "profile": "codex",
            "workspace_path": "/workspace/project",
            "bypass_permissions": false,
            "role": "implementation-worker",
            "kind": "implementation",
            "skills": ["taskr-developer"],
            "template": "task",
            "instruction": "Implement this task and report validation."
        })
    }

    fn populated_state() -> OrchestrationState {
        let mut state = OrchestrationState::new();
        let project = state.create_project(create_project(), 99).unwrap();
        let plan = state.create_plan(create_plan(project.id), 100).unwrap();
        let parent = state
            .create_task(create_task(plan.id.clone(), "Parent"), 101)
            .unwrap();
        let child = state
            .create_task(create_task(plan.id.clone(), "Child"), 102)
            .unwrap();
        state
            .add_task_edge(
                CreateTaskEdge {
                    from: parent.id.clone(),
                    to: child.id.clone(),
                    kind: TaskEdgeKind::ParentOf,
                    note: Some("breakdown".into()),
                },
                200,
            )
            .unwrap();
        state
    }

    fn write_v1_snapshot(store: &SqliteOrchestrationStore, tasks_value: &Value) {
        let mut value = serde_json::to_value(populated_state()).unwrap();
        value["tasks"] = tasks_value.clone();
        let state_json = serde_json::to_string(&value).unwrap();
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, 1, ?1, 450)",
                params![state_json],
            )
            .unwrap();
    }

    #[test]
    fn open_creates_database_in_store_path() {
        let dir = unique_temp_dir("taskr-store-create");
        let db_path = dir.join(DB_FILE_NAME);

        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();

        assert_eq!(store.db_path, db_path);
        assert!(store.db_path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_missing_snapshot_returns_none() {
        let dir = unique_temp_dir("taskr-store-empty");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();

        assert!(store.load().unwrap().is_none());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_and_load_round_trips_tasks_edges_and_executions() {
        let dir = unique_temp_dir("taskr-store-roundtrip");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let mut state = populated_state();
        let child = state
            .create_task(create_task(PlanId("plan-1".into()), "Exec Child"), 103)
            .unwrap();
        let execution = taskr_core::orchestration::TaskExecution {
            launch_options: Default::default(),
            native_session_name: None,
            group: taskr_core::orchestration::ExecutionGroup::Work,
            inspection: false,
            resumed_from: None,
            pane_closed: false,
            report: None,
            execution_id: "exec-1".into(),
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
            workspace_id: Some("w5".into()),
            tab_id: Some("w5:t1".into()),
            pane_id: Some("w5:p1".into()),
            terminal_id: None,
            agent_name: Some("taskr-child-codex-1".into()),
            agent_kind: Some("codex".into()),
            agent_session: None,
            phase: ExecutionPhase::Live,
            recovery: ExecutionRecovery::Reconciled,
            created_at_ms: 300,
            updated_at_ms: 310,
            last_seen_ms: 320,
        };
        state.record_execution(&child.id, execution, 330).unwrap();

        store.save(&state, 400).unwrap();
        let loaded = store.load().unwrap().unwrap();

        assert_eq!(loaded.tasks.len(), 3);
        assert_eq!(loaded.plans.len(), 1);
        assert_eq!(loaded.task_edges, state.task_edges);
        assert_eq!(
            loaded.tasks.get(&child.id).unwrap().execution,
            state.tasks.get(&child.id).unwrap().execution
        );
        assert_eq!(loaded.next_plan_id, state.next_plan_id);
        assert_eq!(loaded.next_task_id, state.next_task_id);
        assert_eq!(
            loaded.tasks.get(&TaskId("task-1".into())).unwrap().gates,
            vec!["cargo test"]
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_migrates_v1_sessions_to_needs_reconciliation_executions() {
        let dir = unique_temp_dir("taskr-store-v1-sessions");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let tasks = serde_json::json!({
            "task-1": serde_json::json!({
                "id": "task-1",
                "plan_id": "plan-1",
                "slug": "parent",
                "title": "Parent",
                "objective": "Objective for Parent",
                "scope": {"include_paths": [], "exclude_paths": [], "notes": null},
                "status": "Backlog",
                "session": v1_session_json("local", "worker-a"),
                "run_spec": v1_run_spec_json("local"),
                "gates": ["cargo test"],
                "created_at_ms": 101,
                "updated_at_ms": 101,
                "completed_at_ms": null,
                "outcome": null,
                "blockers": [],
                "evidence": [],
                "auto_schedule": false
            })
        });
        write_v1_snapshot(&store, &tasks);

        let loaded = store.load().unwrap().unwrap();
        let task = loaded.tasks.get(&TaskId("task-1".into())).unwrap();
        let execution = task.execution.as_ref().unwrap();

        assert_eq!(execution.execution_id, "migrated-local-worker-a");
        assert_eq!(execution.endpoint_id, "local");
        assert_eq!(execution.launch_profile_id, "codex");
        assert_eq!(execution.workspace_path, "/workspace/project");
        assert_eq!(execution.role, "implementation-worker");
        assert_eq!(execution.phase, ExecutionPhase::Unavailable);
        assert_eq!(execution.recovery, ExecutionRecovery::NeedsReconciliation);
        assert!(execution.pane_id.is_none());
        assert!(execution.agent_name.is_none());
        assert_eq!(execution.created_at_ms, 300);
        assert_eq!(execution.last_seen_ms, 320);
        let run_spec = task.run_spec.as_ref().unwrap();
        assert_eq!(run_spec.endpoint_id, "local");
        assert_eq!(run_spec.launch_profile_id, "codex");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_migrates_v1_remote_sessions_without_inventing_endpoint_mapping() {
        let dir = unique_temp_dir("taskr-store-v1-remote");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let tasks = serde_json::json!({
            "task-1": serde_json::json!({
                "id": "task-1",
                "plan_id": "plan-1",
                "slug": "parent",
                "title": "Parent",
                "objective": "Objective for Parent",
                "scope": {"include_paths": [], "exclude_paths": [], "notes": null},
                "status": "Backlog",
                "session": v1_session_json("node-a", "worker-a"),
                "gates": [],
                "created_at_ms": 101,
                "updated_at_ms": 101,
                "completed_at_ms": null,
                "outcome": null,
                "blockers": [],
                "evidence": [],
                "auto_schedule": false
            })
        });
        write_v1_snapshot(&store, &tasks);

        let loaded = store.load().unwrap().unwrap();
        let execution = loaded.tasks[&TaskId("task-1".into())]
            .execution
            .as_ref()
            .unwrap();

        assert_eq!(execution.endpoint_id, "legacy-node:node-a");
        assert_eq!(execution.phase, ExecutionPhase::Unavailable);
        assert_eq!(execution.recovery, ExecutionRecovery::NeedsReconciliation);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_v1_backs_up_database_before_migration() {
        let dir = unique_temp_dir("taskr-store-v1-backup");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let tasks = serde_json::json!({});
        write_v1_snapshot(&store, &tasks);

        store.load().unwrap().unwrap();

        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with("taskr.db.v1-") && name.ends_with(".bak"))
                    .unwrap_or(false)
            })
            .count();
        assert!(backups >= 1, "expected a v1 backup file in {:?}", dir);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_v2_consumes_aliases_rewrites_records_and_persists_upgrade_once() {
        let dir = unique_temp_dir("taskr-store-v2-endpoint-migration");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let mut value = serde_json::to_value(populated_state()).unwrap();
        value["tasks"]["task-1"]["session"] = v1_session_json("node-a", "worker-a");
        value["tasks"]["task-1"]["run_spec"] = v1_run_spec_json("node-a");
        migrate_v1_to_v2(&mut value);
        let original_execution = value["tasks"]["task-1"]["execution"].clone();
        value["endpoint_mappings"] = serde_json::json!({"node-a": "remote-a"});
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
             VALUES (1, 2, ?1, 450)",
                params![serde_json::to_string(&value).unwrap()],
            )
            .unwrap();

        let loaded = store.load().unwrap().unwrap();
        let task = &loaded.tasks[&TaskId("task-1".into())];
        assert_eq!(task.run_spec.as_ref().unwrap().endpoint_id, "remote-a");
        let mut expected_execution = original_execution;
        expected_execution["endpoint_id"] = serde_json::json!("remote-a");
        expected_execution["terminal_id"] = Value::Null;
        assert_eq!(
            serde_json::to_value(task.execution.as_ref().unwrap()).unwrap(),
            serde_json::to_value(
                serde_json::from_value::<taskr_core::orchestration::TaskExecution>(
                    expected_execution
                )
                .unwrap()
            )
            .unwrap()
        );
        assert!(serde_json::to_value(&loaded)
            .unwrap()
            .get("endpoint_mappings")
            .is_none());
        let stored: (i64, String) = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0, SNAPSHOT_VERSION);
        assert!(!stored.1.contains("endpoint_mappings"));
        let count_backups = || {
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
                .count()
        };
        let backups = count_backups();
        assert_eq!(backups, 1);
        assert_eq!(
            serde_json::to_value(store.load().unwrap().unwrap()).unwrap(),
            serde_json::to_value(loaded).unwrap()
        );
        assert_eq!(count_backups(), backups);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v2_endpoint_migration_marks_ambiguous_specs_and_preserves_modern_bindings() {
        let mut value = serde_json::json!({
            "endpoint_mappings": {"profile-a": "profile-b"},
            "tasks": {
                "modern": {
                    "run_spec": {"endpoint_id": "profile-a"},
                    "execution": {"execution_id": "exec-actual", "endpoint_id": "profile-a", "pane_id": "w1:p1"}
                },
                "unbound": {"run_spec": {"endpoint_id": "node-unmapped"}},
                "mapped": {
                    "run_spec": {"endpoint_id": "profile-a"},
                    "execution": {"execution_id": "migrated-profile-a-old", "endpoint_id": "profile-a", "phase": "unavailable"}
                }
            }
        });
        let before = value["tasks"]["modern"].clone();
        migrate_v2_to_v3(&mut value);
        assert_eq!(value["tasks"]["modern"], before);
        assert_eq!(
            value["tasks"]["unbound"]["run_spec"]["endpoint_id"],
            "legacy-node:node-unmapped"
        );
        assert_eq!(
            value["tasks"]["mapped"]["run_spec"]["endpoint_id"],
            "profile-b"
        );
        assert_eq!(
            value["tasks"]["mapped"]["execution"]["endpoint_id"],
            "profile-b"
        );
        assert!(value.get("endpoint_mappings").is_none());
    }

    #[test]
    fn load_rejects_newer_snapshot_versions() {
        let dir = unique_temp_dir("taskr-store-newer");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let state_json = serde_json::to_string(&populated_state()).unwrap();
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, ?1, ?2, 460)",
                params![SNAPSHOT_VERSION + 1, state_json],
            )
            .unwrap();

        let error = store.load().unwrap_err();

        assert!(
            error.contains("unsupported orchestration snapshot version"),
            "{error}"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_migrates_legacy_blocks_edges_to_depends_on() {
        let dir = unique_temp_dir("taskr-store-legacy-blocks");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let state = populated_state();
        let mut value = serde_json::to_value(&state).unwrap();
        let edges = value
            .get_mut("task_edges")
            .and_then(Value::as_array_mut)
            .unwrap();
        edges[0]["kind"] = Value::String("Blocks".into());
        let state_json = serde_json::to_string(&value).unwrap();

        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, ?1, ?2, 450)",
                params![SNAPSHOT_VERSION, state_json],
            )
            .unwrap();

        let loaded = store.load().unwrap().unwrap();

        assert_eq!(loaded.task_edges.len(), 1);
        assert_eq!(loaded.task_edges[0].kind, TaskEdgeKind::DependsOn);
        assert_eq!(loaded.task_edges[0].from, TaskId("task-2".into()));
        assert_eq!(loaded.task_edges[0].to, TaskId("task-1".into()));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn load_migrates_missing_plan_instructions_to_none() {
        let dir = unique_temp_dir("taskr-store-plan-instructions");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let state = populated_state();
        let mut value = serde_json::to_value(&state).unwrap();
        let plans = value
            .get_mut("plans")
            .and_then(Value::as_object_mut)
            .unwrap();
        for plan in plans.values_mut() {
            plan.as_object_mut().unwrap().remove("instructions");
        }
        let state_json = serde_json::to_string(&value).unwrap();

        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, ?1, ?2, 451)",
                params![SNAPSHOT_VERSION, state_json],
            )
            .unwrap();

        let loaded = store.load().unwrap().unwrap();

        assert_eq!(
            loaded
                .plans
                .get(&PlanId("plan-1".into()))
                .unwrap()
                .instructions,
            None
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_overwrites_singleton_snapshot_row() {
        let dir = unique_temp_dir("taskr-store-overwrite");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let first = populated_state();
        let mut second = OrchestrationState::new();
        let project = second.create_project(create_project(), 499).unwrap();
        let plan = second.create_plan(create_plan(project.id), 500).unwrap();
        second
            .create_task(create_task(plan.id, "Replacement"), 501)
            .unwrap();

        store.save(&first, 400).unwrap();
        store.save(&second, 600).unwrap();
        let loaded = store.load().unwrap().unwrap();
        let row_count: i64 = store
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM orchestration_snapshots", [], |row| {
                row.get(0)
            })
            .unwrap();

        assert_eq!(row_count, 1);
        assert_eq!(loaded.tasks.len(), 1);
        assert!(loaded.tasks.contains_key(&TaskId("task-1".into())));
        assert!(loaded.task_edges.is_empty());
        assert!(loaded.tasks.values().all(|task| task.execution.is_none()));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_json_returns_clear_error() {
        let dir = unique_temp_dir("taskr-store-corrupt-json");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms)
                 VALUES (1, ?1, '{bad json', 700)",
                params![SNAPSHOT_VERSION],
            )
            .unwrap();

        let error = store.load().unwrap_err();

        assert!(
            error.contains("failed to migrate orchestration snapshot")
                || error.contains("failed to deserialize orchestration snapshot"),
            "{error}"
        );
        assert!(error.contains("taskr.db"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backup_failure_blocks_migration() {
        let dir = unique_temp_dir("taskr-store-backup-fail");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let tasks = serde_json::json!({});
        write_v1_snapshot(&store, &tasks);
        // Making the store directory read-only forces the backup copy to fail.
        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o500);
            fs::set_permissions(&dir, permissions.clone()).unwrap();
        }

        let result = store.load();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o700);
            fs::set_permissions(&dir, permissions).unwrap();
        }
        #[cfg(unix)]
        assert!(
            result.is_err(),
            "expected backup failure to block migration"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn migrated_v2_state_reserializes_as_current_version() {
        let dir = unique_temp_dir("taskr-store-v1-resave");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let tasks = serde_json::json!({
            "task-1": serde_json::json!({
                "id": "task-1",
                "plan_id": "plan-1",
                "slug": "parent",
                "title": "Parent",
                "objective": "Objective for Parent",
                "scope": {"include_paths": [], "exclude_paths": [], "notes": null},
                "status": "Backlog",
                "session": v1_session_json("local", "worker-a"),
                "gates": [],
                "created_at_ms": 101,
                "updated_at_ms": 101,
                "completed_at_ms": null,
                "outcome": null,
                "blockers": [],
                "evidence": [],
                "auto_schedule": false
            })
        });
        write_v1_snapshot(&store, &tasks);

        let loaded = store.load().unwrap().unwrap();
        store.save(&loaded, 500).unwrap();

        let (version, _): (i64, String) = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(version, SNAPSHOT_VERSION);

        let reloaded = store.load().unwrap().unwrap();
        assert_eq!(
            reloaded.tasks[&TaskId("task-1".into())]
                .execution
                .as_ref()
                .unwrap()
                .recovery,
            ExecutionRecovery::NeedsReconciliation
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_four_upgrade_preserves_native_conversations_and_adds_defaults() {
        let dir = unique_temp_dir("taskr-store-v4-conversations");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let mut old = serde_json::to_value(populated_state()).unwrap();
        old.as_object_mut().unwrap().remove("plan_layouts");
        old["tasks"]["task-1"]["outcome"] = serde_json::json!("Accepted result");
        old["tasks"]["task-1"]["evidence"] = serde_json::json!(["Original evidence"]);
        old["tasks"]["task-1"]["execution"] = serde_json::json!({
            "execution_id":"exec-v4", "endpoint_id":"local", "launch_profile_id":"codex",
            "launch_args":["--no-alt-screen"], "launch_env":{"CODEX_HOME":"/endpoint/.codex"},
            "bypass_permissions":false, "workspace_path":"/endpoint/repo", "role":"worker",
            "kind":"implementation", "skills":[], "workspace_id":"w4", "tab_id":"w4:t1",
            "pane_id":"w4:p1", "terminal_id":"original-terminal", "agent_name":"original-agent",
            "agent_kind":"codex", "agent_session":"original-native-uuid", "phase":"stopped",
            "recovery":"reconciled", "created_at_ms":100, "updated_at_ms":200, "last_seen_ms":200
        });
        store.connect().unwrap().execute(
            "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms) VALUES (1, 4, ?1, 700)",
            params![old.to_string()],
        ).unwrap();
        let state = store.load().unwrap().unwrap();
        let task = &state.tasks[&TaskId("task-1".into())];
        let execution = task.execution.as_ref().unwrap();
        assert_eq!(
            execution.agent_session.as_deref(),
            Some("original-native-uuid")
        );
        assert_eq!(execution.terminal_id.as_deref(), Some("original-terminal"));
        assert_eq!(execution.launch_env["CODEX_HOME"], "/endpoint/.codex");
        assert!(!execution.inspection);
        assert!(!execution.pane_closed);
        assert!(execution.report.is_none());
        assert_eq!(task.outcome.as_deref(), Some("Accepted result"));
        assert_eq!(task.evidence, ["Original evidence"]);
        assert!(state.plan_layouts.is_empty());
        let backup = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("taskr.db.v4-")
            })
            .unwrap();
        let backed_up: (i64, String) = Connection::open(backup.path())
            .unwrap()
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(backed_up, (4, old.to_string()));
        store.load().unwrap();
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("taskr.db.v4-"))
                .count(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_five_upgrade_preserves_all_bindings_without_inventing_generations() {
        let dir = unique_temp_dir("taskr-store-v5-generations");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let execution: taskr_core::orchestration::TaskExecution = serde_json::from_value(
            serde_json::json!({
                "execution_id":"exec-v5", "endpoint_id":"local", "launch_profile_id":"codex",
                "launch_args":["--profile", "original"], "launch_env":{"CODEX_HOME":"/endpoint/.codex"},
                "bypass_permissions":false, "workspace_path":"/endpoint/repo", "role":"worker",
                "kind":"implementation", "skills":[], "workspace_id":"w5", "tab_id":"w5:t1",
                "pane_id":"w5:p1", "terminal_id":"original-terminal", "agent_name":"original-agent",
                "agent_kind":"codex", "agent_session":"native-v5", "phase":"stopped",
                "recovery":"reconciled", "pane_closed":true,
                "report":{"task_status":"Delivered", "outcome":"Accepted", "evidence":["Proof"]},
                "created_at_ms":100, "updated_at_ms":200, "last_seen_ms":200
            }),
        ).unwrap();
        let mut current = serde_json::to_value(execution).unwrap();
        current
            .as_object_mut()
            .unwrap()
            .remove("runtime_generation");
        let mut previous = current.clone();
        previous["execution_id"] = serde_json::json!("exec-v5-previous");
        previous["agent_session"] = serde_json::json!("native-v5-previous");
        let mut old = serde_json::to_value(populated_state()).unwrap();
        old["tasks"]["task-1"]["execution"] = current;
        old["retained_executions"] = serde_json::json!({
            "exec-v5-previous":{"task_id":"task-1", "execution":previous}
        });
        old["plan_layouts"] = serde_json::json!({
            "plan-1:local":{
                "plan_id":"plan-1", "endpoint_id":"local", "workspace_id":"w5",
                "tabs":[{"tab_id":"w5:t1", "group":"work", "ordinal":1}]
            }
        });
        store.connect().unwrap().execute(
            "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms) VALUES (1, 5, ?1, 700)",
            params![old.to_string()],
        ).unwrap();

        let loaded = store.load().unwrap().unwrap();
        let task = &loaded.tasks[&TaskId("task-1".into())];
        assert!(task
            .execution
            .as_ref()
            .unwrap()
            .runtime_generation
            .is_none());
        assert!(loaded.retained_executions["exec-v5-previous"]
            .execution
            .runtime_generation
            .is_none());
        assert!(loaded.plan_layouts["plan-1:local"]
            .runtime_generation
            .is_none());
        let mut expected = old.clone();
        expected["tasks"]["task-1"]["execution"]["runtime_generation"] = Value::Null;
        expected["retained_executions"]["exec-v5-previous"]["execution"]["runtime_generation"] =
            Value::Null;
        expected["plan_layouts"]["plan-1:local"]["runtime_generation"] = Value::Null;
        assert_eq!(serde_json::to_value(&loaded).unwrap(), expected);
        let (version, json): (i64, String) = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(version, SNAPSHOT_VERSION);
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), old);
        let backup = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("taskr.db.v5-")
            })
            .unwrap();
        let original: (i64, String) = Connection::open(backup.path())
            .unwrap()
            .query_row(
                "SELECT version, state_json FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(original, (5, old.to_string()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_three_upgrade_backs_up_once_and_preserves_records() {
        let dir = unique_temp_dir("taskr-store-v3-retained-layouts");
        let store = SqliteOrchestrationStore::open(dir.clone()).unwrap();
        let expected = populated_state();
        let mut old = serde_json::to_value(&expected).unwrap();
        old.as_object_mut().unwrap().remove("retained_executions");
        store.connect().unwrap().execute(
            "INSERT INTO orchestration_snapshots (id, version, state_json, updated_at_ms) VALUES (1, 3, ?1, 700)",
            params![old.to_string()],
        ).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert!(loaded.retained_executions.is_empty());
        let version: i64 = store
            .connect()
            .unwrap()
            .query_row(
                "SELECT version FROM orchestration_snapshots WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SNAPSHOT_VERSION);
        let backups = || {
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("taskr.db.v3-")
                })
                .count()
        };
        assert_eq!(backups(), 1);
        store.load().unwrap();
        assert_eq!(backups(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn execution_id_sanitizes_odd_session_names() {
        assert_eq!(sanitize_execution_id_part("worker a/1"), "worker-a-1");
        assert_eq!(sanitize_execution_id_part(""), "x");
        assert_eq!(sanitize_execution_id_part("ok-name_2"), "ok-name_2");
    }
}
