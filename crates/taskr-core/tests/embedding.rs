//! Public API consumer: no TASKR, SQL, filesystem, execution adapter or runtime.
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use taskr_core::orchestration::{
    CreatePlan, CreateProject, CreateTask, OrchestrationState, TaskScope,
};
use taskr_core::{MutationError, Orchestrator, SnapshotStore};

#[derive(Debug, PartialEq, Eq)]
struct StoreError(&'static str);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for StoreError {}

#[derive(Default)]
struct Saved {
    snapshot: Option<OrchestrationState>,
    saved_at_ms: Option<u64>,
    writes: usize,
    reject_load: bool,
    reject_save: bool,
}
#[derive(Clone, Default)]
struct MemoryStore(Rc<RefCell<Saved>>);

impl SnapshotStore for MemoryStore {
    type Error = StoreError;

    fn load(&self) -> Result<Option<OrchestrationState>, StoreError> {
        let saved = self.0.borrow();
        if saved.reject_load {
            Err(StoreError("cannot read snapshot"))
        } else {
            Ok(saved.snapshot.clone())
        }
    }

    fn save(&self, snapshot: &OrchestrationState, now_ms: u64) -> Result<(), StoreError> {
        let mut saved = self.0.borrow_mut();
        if saved.reject_save {
            return Err(StoreError("cannot commit snapshot"));
        }
        saved.snapshot = Some(snapshot.clone());
        saved.saved_at_ms = Some(now_ms);
        saved.writes += 1;
        Ok(())
    }
}

fn project() -> CreateProject {
    CreateProject {
        title: "Reusable orchestration".into(),
        description: "A host-defined store with no execution adapter".into(),
        ..Default::default()
    }
}

#[test]
fn new_store_is_empty_without_writing_a_snapshot() {
    let store = MemoryStore::default();
    let service = Orchestrator::open(store.clone()).unwrap();
    assert!(service.state().projects.is_empty());
    assert_eq!(service.state().next_plan_id, 1);
    assert_eq!(store.0.borrow().writes, 0);
}

#[test]
fn composed_domain_operations_commit_once_and_reopen_through_public_api() {
    let store = MemoryStore::default();
    let mut service = Orchestrator::open(store.clone()).unwrap();
    let task = service
        .mutate(42, |state, now| {
            let project = state.create_project(project(), now)?;
            let plan = state.create_plan(
                CreatePlan {
                    project_id: project.id,
                    title: "Embedded plan".into(),
                    brief: "Implement a task through the public library API".into(),
                    instructions: None,
                    slug: None,
                },
                now,
            )?;
            state.create_task(
                CreateTask {
                    plan_id: plan.id,
                    title: "Embedded task".into(),
                    objective: "Use a host-defined memory store".into(),
                    scope: TaskScope::default(),
                    gates: vec![],
                    slug: None,
                    auto_schedule: false,
                    run_spec: None,
                },
                now,
            )
        })
        .unwrap();
    assert_eq!(store.0.borrow().writes, 1);
    assert_eq!(store.0.borrow().saved_at_ms, Some(42));
    drop(service);
    let reopened = Orchestrator::open(store).unwrap();
    assert_eq!(reopened.state().tasks[&task.id], task);
    assert_eq!(task.created_at_ms, 42);
    assert_eq!(reopened.state().projects.len(), 1);
    assert_eq!(reopened.state().plans.len(), 1);
}

#[test]
fn rejected_second_operation_discards_earlier_changes_and_never_saves() {
    let store = MemoryStore::default();
    let mut service = Orchestrator::open(store.clone()).unwrap();
    let result = service.mutate(42, |state, now| {
        state.create_project(project(), now)?;
        state.create_project(
            CreateProject {
                title: "".into(),
                ..project()
            },
            now,
        )
    });
    assert!(matches!(result, Err(MutationError::Rejected(_))));
    assert!(service.state().projects.is_empty());
    assert!(store.0.borrow().snapshot.is_none());
    assert_eq!(store.0.borrow().writes, 0);
}

#[test]
fn failed_commit_keeps_previous_snapshot_and_id_counters_for_retry() {
    let store = MemoryStore::default();
    let mut service = Orchestrator::open(store.clone()).unwrap();
    let project = service
        .mutate(1, |state, now| state.create_project(project(), now))
        .unwrap();
    let input = CreatePlan {
        project_id: project.id,
        title: "Retry".into(),
        brief: "An atomic retry after storage recovers".into(),
        instructions: None,
        slug: None,
    };
    store.0.borrow_mut().reject_save = true;
    let error = service
        .mutate(2, |state, now| state.create_plan(input.clone(), now))
        .unwrap_err();
    assert_eq!(
        error,
        MutationError::Persistence(StoreError("cannot commit snapshot"))
    );
    assert!(std::error::Error::source(&error).is_some());
    assert!(service.state().plans.is_empty());
    assert_eq!(service.state().next_plan_id, 1);
    assert!(store.0.borrow().snapshot.as_ref().unwrap().plans.is_empty());
    assert_eq!(store.0.borrow().saved_at_ms, Some(1));
    store.0.borrow_mut().reject_save = false;
    let plan = service
        .mutate(3, |state, now| state.create_plan(input, now))
        .unwrap();
    assert_eq!(plan.id.0, "plan-1");
    assert_eq!(store.0.borrow().saved_at_ms, Some(3));
}

#[test]
fn load_error_does_not_create_an_empty_service_or_write_over_data() {
    let store = MemoryStore::default();
    store.0.borrow_mut().reject_load = true;
    let result = Orchestrator::open(store.clone());
    assert!(matches!(result, Err(StoreError("cannot read snapshot"))));
    assert_eq!(store.0.borrow().writes, 0);
}

#[test]
fn snapshots_are_detached_from_committed_service_state() {
    let store = MemoryStore::default();
    let service = Orchestrator::open(store.clone()).unwrap();
    let mut preview = service.snapshot();
    preview.create_project(project(), 99).unwrap();
    assert!(service.state().projects.is_empty());
    assert_eq!(store.0.borrow().writes, 0);
}

#[test]
fn store_selected_at_runtime_uses_the_same_service_contract() {
    let store: Box<dyn SnapshotStore<Error = StoreError>> = Box::new(MemoryStore::default());
    let mut service = Orchestrator::open(store).unwrap();
    service
        .mutate(42, |state, now| state.create_project(project(), now))
        .unwrap();
    assert_eq!(service.state().projects.len(), 1);
}

#[test]
fn serialized_snapshot_rehydrates_after_host_eviction() {
    // A host may persist JSON in a Durable Object SQL row. Reconstruct the
    // service from bytes only, without retaining any prior in-memory state.
    #[derive(Default)]
    struct JsonStore(RefCell<Option<String>>);
    impl SnapshotStore for JsonStore {
        type Error = serde_json::Error;

        fn load(&self) -> Result<Option<OrchestrationState>, Self::Error> {
            self.0
                .borrow()
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
        }

        fn save(&self, state: &OrchestrationState, _: u64) -> Result<(), Self::Error> {
            let encoded = serde_json::to_string(state)?;
            *self.0.borrow_mut() = Some(encoded);
            Ok(())
        }
    }

    let store = Rc::new(JsonStore::default());
    // The wrapper makes the fixture's durable bytes observable from the host.
    struct HostStore(Rc<JsonStore>);
    impl SnapshotStore for HostStore {
        type Error = serde_json::Error;

        fn load(&self) -> Result<Option<OrchestrationState>, Self::Error> {
            self.0.load()
        }

        fn save(&self, state: &OrchestrationState, now: u64) -> Result<(), Self::Error> {
            self.0.save(state, now)
        }
    }
    let mut service = Orchestrator::open(HostStore(store.clone())).unwrap();
    let project = service
        .mutate(100, |state, now| state.create_project(project(), now))
        .unwrap();
    let input = CreatePlan {
        project_id: project.id.clone(),
        title: "Durable plan".into(),
        brief: "Survive service eviction".into(),
        instructions: None,
        slug: None,
    };
    let plan = service
        .mutate(101, |state, now| state.create_plan(input.clone(), now))
        .unwrap();
    drop(service);
    let bytes = store.0.borrow().clone();
    drop(store);

    let restored_store = JsonStore(RefCell::new(bytes));
    let mut reopened = Orchestrator::open(restored_store).unwrap();
    assert_eq!(reopened.state().projects[&project.id], project);
    assert_eq!(reopened.state().plans[&plan.id], plan);
    let next_plan = reopened
        .mutate(102, |state, now| {
            state.create_plan(
                CreatePlan {
                    title: "Next plan".into(),
                    ..input
                },
                now,
            )
        })
        .unwrap();
    assert_eq!(next_plan.id.0, "plan-2");
    assert_eq!(next_plan.created_at_ms, 102);
}
