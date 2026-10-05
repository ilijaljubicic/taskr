//! Public API consumer with single-threaded, non-Send container bindings.
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    future::{ready, Future, Ready},
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};
use taskr_herdr::containers::*;
use taskr_herdr::*;

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("fixture unexpectedly waited"),
    }
}
#[derive(Default)]
struct State {
    running: bool,
    ready: bool,
    starts: usize,
    generation: String,
    leases: BTreeSet<String>,
    controls: Vec<EndpointAction>,
    commands: Vec<ContainerExec>,
    refuse_readiness: bool,
    reply: Option<Result<CommandOutput, RuntimeError>>,
}
#[derive(Clone)]
struct Bindings(Rc<RefCell<State>>);
impl ContainerBindings for Bindings {
    type ControlFuture<'a> = Ready<Result<ContainerStatus, RuntimeError>>;
    type ExecFuture<'a> = Ready<Result<CommandOutput, RuntimeError>>;
    fn control(&self, _: String, action: EndpointAction) -> Self::ControlFuture<'_> {
        let mut state = self.0.borrow_mut();
        state.controls.push(action.clone());
        match action {
            EndpointAction::Prepare { lease_id } => {
                state.leases.insert(lease_id);
                if !state.running {
                    state.starts += 1;
                    state.running = true;
                }
                state.ready = !state.refuse_readiness;
            }
            EndpointAction::Renew { lease_id } => {
                state.leases.insert(lease_id);
            }
            EndpointAction::Release { lease_id } => {
                state.leases.remove(&lease_id);
            }
            EndpointAction::Observe => {}
        }
        ready(Ok(ContainerStatus {
            running: state.running,
            herdr_ready: state.ready,
            generation: state.generation.clone(),
        }))
    }
    fn exec(&self, request: ContainerExec) -> Self::ExecFuture<'_> {
        let mut state = self.0.borrow_mut();
        assert_eq!(
            request.generation,
            resource_namespace(&request.binding, &state.generation),
            "binding must fence the actual exec instance"
        );
        state.commands.push(request);
        ready(state.reply.take().unwrap_or_else(|| Ok(CommandOutput { stdout: "{\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"tab_id\":\"w1:t1\",\"workspace_id\":\"w1\",\"terminal_id\":\"term1\"}}}".into(), stderr: String::new(), exit_code: 0 })))
    }
}
fn fixture() -> (
    HerdrClient<ContainerTransport<Bindings>>,
    Rc<RefCell<State>>,
) {
    let state = Rc::new(RefCell::new(State {
        generation: "instance-1".into(),
        ..Default::default()
    }));
    let transport = ContainerTransport {
        bindings: Bindings(state.clone()),
        endpoints: BTreeMap::from([(
            "worker".into(),
            ContainerEndpoint {
                binding: "durable-worker".into(),
                session: Some("main".into()),
                user: "agent".into(),
                environment: BTreeMap::from([
                    ("HOME".into(), "/home/agent".into()),
                    ("PATH".into(), "/usr/local/bin:/usr/bin".into()),
                ]),
                cwd: Some("/work".into()),
            },
        )]),
    };
    (
        HerdrClient::with_transport(HerdrClientConfig::default(), transport),
        state,
    )
}
fn prepare(client: &HerdrClient<ContainerTransport<Bindings>>) -> EndpointTarget {
    run(client.endpoint(
        client.target_for_endpoint("worker"),
        EndpointAction::Prepare {
            lease_id: "exec1".into(),
        },
    ))
    .unwrap()
    .target
}
fn allocation() -> AllocatePaneRequest {
    AllocatePaneRequest {
        cwd: "/work".into(),
        env: BTreeMap::new(),
        label: None,
        workspace_label: None,
        placement: PanePlacement::NewWorkspace,
    }
}

#[test]
fn discovery_and_observation_do_not_start_a_cold_container() {
    let (client, state) = fixture();
    assert_eq!(
        run(client.list_endpoints()).unwrap()[0].endpoint_id,
        "worker"
    );
    assert!(state.borrow().controls.is_empty());
    let endpoint = run(client.endpoint(
        client.target_for_endpoint("worker"),
        EndpointAction::Observe,
    ))
    .unwrap();
    assert!(!endpoint.ready);
    assert_eq!(state.borrow().starts, 0);
    let error = run(client.allocate_pane(&endpoint.target, &allocation())).unwrap_err();
    assert_eq!(error.delivery_certainty, DeliveryCertainty::NotDelivered);
    assert!(state.borrow().commands.is_empty());
    assert_eq!(state.borrow().starts, 0);
}
#[test]
fn prepare_requires_api_readiness_and_retains_independent_execution_leases() {
    let (client, state) = fixture();
    state.borrow_mut().refuse_readiness = true;
    let error = run(client.endpoint(
        client.target_for_endpoint("worker"),
        EndpointAction::Prepare {
            lease_id: "exec1".into(),
        },
    ))
    .unwrap_err();
    assert_eq!(error.category, RuntimeErrorCategory::EndpointUnavailable);
    assert!(state.borrow().running);
    assert!(state.borrow().commands.is_empty());
    state.borrow_mut().refuse_readiness = false;
    let target = prepare(&client);
    run(client.endpoint(
        target.clone(),
        EndpointAction::Renew {
            lease_id: "audit-exec".into(),
        },
    ))
    .unwrap();
    run(client.endpoint(
        target,
        EndpointAction::Release {
            lease_id: "exec1".into(),
        },
    ))
    .unwrap();
    assert_eq!(state.borrow().leases, BTreeSet::from(["audit-exec".into()]));
    assert_eq!(state.borrow().starts, 1);
}
#[test]
fn herdr_and_companion_use_the_same_binding_generation_and_explicit_identity() {
    let (client, state) = fixture();
    let target = prepare(&client);
    let allocated = run(client.allocate_pane(&target, &allocation())).unwrap();
    assert_eq!(allocated.pane_id, "w1:p1");
    state.borrow_mut().reply = Some(Ok(CommandOutput {
        stdout: "{}".into(),
        stderr: "secret diagnostics".into(),
        exit_code: 0,
    }));
    let output = run(client.execute_endpoint(CommandRequest {
        target,
        scope: CommandScope::Endpoint,
        program: "python3".into(),
        args: vec!["-c".into(), "companion".into()],
        stdin: b"bundle-on-stdin".to_vec(),
        env: BTreeMap::from([("CODEX_HOME".into(), "/homes/deployment-v1".into())]),
        cwd: Some("/work/project".into()),
        timeout: Duration::from_secs(3),
        output_limit: 4096,
        retain_stderr: false,
    }))
    .unwrap();
    assert!(output.stderr.is_empty());
    let state = state.borrow();
    let herdr = &state.commands[0];
    let companion = &state.commands[1];
    assert_eq!(
        (
            herdr.binding.as_str(),
            herdr.generation.as_str(),
            herdr.user.as_str()
        ),
        (
            "durable-worker",
            resource_namespace("durable-worker", "instance-1").as_str(),
            "agent"
        )
    );
    assert_eq!(herdr.request.args[..2], ["--session", "main"]);
    assert!(!herdr.request.args.contains(&"--machine".into()));
    assert_eq!(companion.binding, herdr.binding);
    assert_eq!(companion.generation, herdr.generation);
    assert_eq!(companion.request.env["HOME"], "/home/agent");
    assert_eq!(companion.request.env["CODEX_HOME"], "/homes/deployment-v1");
    assert_eq!(companion.request.stdin, b"bundle-on-stdin");
    assert_eq!(companion.request.cwd.as_deref(), Some("/work/project"));
    assert_eq!(companion.request.timeout, Duration::from_secs(3));
}
#[test]
fn uncertain_allocation_is_preserved_and_not_retried_by_the_adapter() {
    let (client, state) = fixture();
    let target = prepare(&client);
    state.borrow_mut().reply = Some(Err(RuntimeError::new(
        RuntimeErrorCategory::Timeout,
        "exec",
        "receipt lost",
    )
    .with_certainty(DeliveryCertainty::Unknown)));
    let error = run(client.allocate_pane(&target, &allocation())).unwrap_err();
    assert_eq!(error.delivery_certainty, DeliveryCertainty::Unknown);
    assert_eq!(state.borrow().commands.len(), 1);
}
#[test]
fn reused_public_ids_after_restart_cannot_be_read_or_closed_through_old_generation() {
    let (client, state) = fixture();
    let target = prepare(&client);
    run(client.allocate_pane(&target, &allocation())).unwrap();
    state.borrow_mut().generation = "instance-2".into();
    for error in [
        run(client.pane_get(&target, "w1:p1")).unwrap_err(),
        run(client.pane_close(&target, "w1:p1")).unwrap_err(),
    ] {
        assert_eq!(error.category, RuntimeErrorCategory::GenerationMismatch);
        assert_eq!(error.delivery_certainty, DeliveryCertainty::NotDelivered);
    }
    assert_eq!(state.borrow().commands.len(), 1);
    // Reopening a conversation prepares a fresh generation; it never reuses IDs.
    let fresh = prepare(&client);
    assert_ne!(target.runtime_generation, fresh.runtime_generation);
}
#[test]
fn unknown_endpoint_and_missing_identity_fail_before_binding_calls() {
    let (mut client, state) = fixture();
    assert!(run(client.endpoint(
        client.target_for_endpoint("missing"),
        EndpointAction::Observe
    ))
    .is_err());
    let mut transport = client.transport().clone();
    transport
        .endpoints
        .get_mut("worker")
        .unwrap()
        .environment
        .remove("HOME");
    client = HerdrClient::with_transport(HerdrClientConfig::default(), transport);
    assert!(run(client.endpoint(
        client.target_for_endpoint("worker"),
        EndpointAction::Observe
    ))
    .is_err());
    assert!(state.borrow().controls.is_empty());
}
