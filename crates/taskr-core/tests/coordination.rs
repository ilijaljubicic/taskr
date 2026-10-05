use taskr_core::{coordination::*, orchestration::*};

fn execution(phase: ExecutionPhase) -> TaskExecution {
    serde_json::from_value(serde_json::json!({
        "execution_id":"exec1", "endpoint_id":"worker", "launch_profile_id":"deployment-1",
        "bypass_permissions":false, "workspace_path":"/work", "role":"worker", "kind":"code", "skills":[],
        "phase":phase, "recovery":"needs_reconciliation", "created_at_ms":1, "updated_at_ms":1, "last_seen_ms":1,
        "agent_name":"worker1", "agent_session":"conversation1", "terminal_id":"terminal1", "runtime_generation":"instance1"
    })).unwrap()
}
fn state() -> (OrchestrationState, TaskId, TaskId) {
    let mut state = OrchestrationState::new();
    let project = state
        .create_project(
            CreateProject {
                title: "Project".into(),
                description: "Fixture".into(),
                ..Default::default()
            },
            1,
        )
        .unwrap();
    let plan = state
        .create_plan(
            CreatePlan {
                project_id: project.id,
                title: "Plan".into(),
                brief: "Fixture".into(),
                instructions: None,
                slug: None,
            },
            1,
        )
        .unwrap();
    let mut task = |title: &str| {
        state
            .create_task(
                CreateTask {
                    plan_id: plan.id.clone(),
                    title: title.into(),
                    objective: "Fixture".into(),
                    scope: TaskScope::default(),
                    gates: vec![],
                    slug: None,
                    auto_schedule: false,
                    run_spec: None,
                },
                1,
            )
            .unwrap()
            .id
    };
    let worker = task("Worker");
    let audit = task("Audit");
    state
        .record_execution(&worker, execution(ExecutionPhase::Live), 2)
        .unwrap();
    state
        .add_task_edge(
            CreateTaskEdge {
                from: audit.clone(),
                to: worker.clone(),
                kind: TaskEdgeKind::Audits,
                note: None,
            },
            2,
        )
        .unwrap();
    (state, worker, audit)
}
#[test]
fn launch_intents_after_restart_reconcile_instead_of_repeating_effects() {
    let mut execution = execution(ExecutionPhase::Pending);
    let mut facts = LaunchFacts::default();
    assert_eq!(launch_step(&execution, facts), LaunchStep::PrepareEndpoint);
    facts.endpoint_prepared = true;
    assert_eq!(
        launch_step(&execution, facts),
        LaunchStep::VerifyEnvironment
    );
    facts.environment_verified = true;
    assert_eq!(launch_step(&execution, facts), LaunchStep::Allocate);
    execution.phase = ExecutionPhase::Allocating;
    assert_eq!(
        launch_step(&execution, facts),
        LaunchStep::ReconcileAllocation
    );
    assert!(unresolved_allocation(&execution));
    execution.pane_id = Some("w1:p1".into());
    assert_eq!(launch_step(&execution, facts), LaunchStep::PersistPlacement);
    execution.phase = ExecutionPhase::Starting;
    assert_eq!(
        launch_step(&execution, LaunchFacts::default()),
        LaunchStep::ReconcileAgent
    );
    facts.agent_start_dispatched = false; // Only this fresh continuation has not dispatched its intent.
    assert_eq!(launch_step(&execution, facts), LaunchStep::StartAgent);
    facts.agent_start_dispatched = true;
    assert_eq!(launch_step(&execution, facts), LaunchStep::ReconcileAgent);
    facts.occupant_verified = true;
    assert_eq!(launch_step(&execution, facts), LaunchStep::PersistLive);
    execution.phase = ExecutionPhase::Live;
    assert_eq!(launch_step(&execution, facts), LaunchStep::SubmitPrompt);
    execution.inspection = true;
    assert_eq!(launch_step(&execution, facts), LaunchStep::Complete);
}
#[test]
fn unknown_or_applied_effects_never_authorize_a_new_execution() {
    for outcome in [EffectOutcome::Unknown, EffectOutcome::Applied] {
        let phase = allocation_failure_phase(outcome);
        assert!(!phase.allows_replacement());
        assert_eq!(phase, ExecutionPhase::Allocating);
        assert_eq!(startup_failure_phase(outcome), ExecutionPhase::Starting);
        assert!(!startup_failure_phase(outcome).allows_replacement());
    }
    assert!(allocation_failure_phase(EffectOutcome::NotApplied).allows_replacement());
    assert!(startup_failure_phase(EffectOutcome::NotApplied).allows_replacement());
}
#[test]
fn audit_completion_releases_the_successful_worker_but_inspection_remains_open() {
    let (mut state, worker, audit) = state();
    state.tasks.get_mut(&worker).unwrap().status = TaskStatus::Passed;
    assert_eq!(
        cleanup_decision(&state, &state.tasks[&worker]),
        CleanupDecision::AuditHold(vec![audit.0.clone()])
    );
    assert!(needs_endpoint_lease(
        state.tasks[&worker].execution.as_ref().unwrap()
    ));
    state.tasks.get_mut(&audit).unwrap().status = TaskStatus::Passed;
    assert_eq!(
        cleanup_decision(&state, &state.tasks[&worker]),
        CleanupDecision::Close
    );
    state
        .tasks
        .get_mut(&worker)
        .unwrap()
        .execution
        .as_mut()
        .unwrap()
        .inspection = true;
    assert_eq!(
        cleanup_decision(&state, &state.tasks[&worker]),
        CleanupDecision::Keep
    );
}
#[test]
fn cancellation_overrides_an_unfinished_audit() {
    let (mut state, worker, _) = state();
    state.tasks.get_mut(&worker).unwrap().status = TaskStatus::Canceled;
    assert_eq!(
        cleanup_decision(&state, &state.tasks[&worker]),
        CleanupDecision::Close
    );
}
#[test]
fn layout_split_requires_every_occupant_to_be_owned_by_the_same_group() {
    let layout = PlanLayout {
        plan_id: PlanId("plan1".into()),
        endpoint_id: "worker".into(),
        runtime_generation: Some("instance1".into()),
        workspace_id: "w1".into(),
        tabs: vec![PlanTab {
            tab_id: "t1".into(),
            group: ExecutionGroup::Work,
            ordinal: 1,
        }],
    };
    let mut panes = vec![LayoutPane {
        pane_id: "p1".into(),
        workspace_id: "w1".into(),
        tab_id: "t1".into(),
        owned_by_group: true,
    }];
    assert_eq!(
        select_placement(Some(&layout), ExecutionGroup::Work, &panes, 2),
        LayoutPlacement::Split {
            pane_id: "p1".into()
        }
    );
    panes[0].owned_by_group = false;
    assert_eq!(
        select_placement(Some(&layout), ExecutionGroup::Work, &panes, 2),
        LayoutPlacement::NewTab {
            workspace_id: "w1".into()
        }
    );
    assert_eq!(
        select_placement(None, ExecutionGroup::Work, &panes, 2),
        LayoutPlacement::NewSpace
    );
    assert!(!generations_match(Some("instance1"), Some("instance2")));
    assert!(!generations_match(None, None));
}
#[test]
fn ownership_and_frozen_generation_survive_store_rehydration() {
    let (mut state, task, _) = state();
    let owner = state.tasks[&task].execution.as_ref().unwrap();
    assert!(owns_agent(
        owner,
        &AgentObservation {
            name: owner.agent_name.clone(),
            terminal_id: owner.terminal_id.clone(),
            session: owner.agent_session.clone()
        }
    ));
    assert!(!owns_agent(
        owner,
        &AgentObservation {
            name: owner.agent_name.clone(),
            terminal_id: Some("reused".into()),
            session: owner.agent_session.clone()
        }
    ));
    let mut changed = owner.clone();
    changed.runtime_generation = Some("instance2".into());
    assert!(state.record_execution(&task, changed, 3).is_err());
    let restored: OrchestrationState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(
        restored.tasks[&task]
            .execution
            .as_ref()
            .unwrap()
            .runtime_generation
            .as_deref(),
        Some("instance1")
    );
}
#[test]
fn resume_blocks_unresolved_work_and_preserves_native_arguments() {
    let source = execution(ExecutionPhase::Stopped);
    assert!(resume_allowed(&source, Some(&execution(ExecutionPhase::Allocating))).is_err());
    assert!(resume_allowed(&source, None).is_ok());
    assert_eq!(
        resume_args(
            "codex",
            &[
                "--profile".into(),
                "review".into(),
                "resume".into(),
                "old".into()
            ],
            "saved"
        )
        .unwrap(),
        ["--profile", "review", "resume", "saved"]
    );
    assert!(resume_args("codex", &[], "--latest").is_err());
}

#[test]
fn namespace_loss_needs_positive_evidence_and_preserves_the_conversation_and_report() {
    let (mut state, task, _) = state();
    let original = state.tasks[&task].execution.clone().unwrap();
    assert!(namespace_lost(&mut state, "exec1", "", 3).is_err());
    assert!(namespace_lost(&mut state, "exec1", "instance1", 3).is_err());
    namespace_lost(&mut state, "exec1", "instance2", 3).unwrap();
    let closed = state.tasks[&task].execution.as_ref().unwrap();
    assert_eq!(closed.phase, ExecutionPhase::Exited);
    assert!(closed.pane_closed);
    assert_eq!(closed.runtime_generation, original.runtime_generation);
    assert_eq!(closed.agent_session, original.agent_session);
    assert_eq!(closed.terminal_id, original.terminal_id);
    assert!(closed.report.is_some());
    assert_ne!(
        runtime_key_for("worker", Some("instance1"), "w1:p1"),
        runtime_key_for("worker", Some("instance2"), "w1:p1")
    );
}
