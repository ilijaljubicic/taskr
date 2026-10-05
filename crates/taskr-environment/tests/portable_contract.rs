//! Companion consumer with no native catalog/store/process or Send bound.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::{ready, Future, Ready},
    rc::Rc,
    task::{Context, Poll, Waker},
};
use taskr_environment::{protocol::ResumeRequirement, EnvironmentCompanion};
use taskr_herdr::*;

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture waited"),
    }
}
#[derive(Clone, Default)]
struct Transport(Rc<RefCell<Vec<CommandRequest>>>);
impl HerdrTransport for Transport {
    type CommandFuture<'a> = Ready<Result<CommandOutput, RuntimeError>>;
    fn execute(&self, request: CommandRequest) -> Self::CommandFuture<'_> {
        let body: serde_json::Value = serde_json::from_slice(&request.stdin).unwrap();
        let restored = request
            .env
            .get("RESTORED_HISTORY")
            .is_some_and(|value| value == "yes");
        self.0.borrow_mut().push(request);
        let output = match body["operation"].as_str().unwrap() {
            "verify_resume" if restored => serde_json::json!({"result":{"history_available":true}}),
            "verify_resume" => {
                serde_json::json!({"error":"Native conversation history is missing"})
            }
            _ => serde_json::json!({"result":{"bundle_digest":"revision1"}}),
        };
        ready(Ok(CommandOutput {
            stdout: output.to_string(),
            stderr: String::new(),
            exit_code: if restored || body["operation"] != "verify_resume" {
                0
            } else {
                1
            },
        }))
    }
}
#[test]
fn pinned_profile_and_history_are_verified_on_the_same_resolved_endpoint() {
    let transport = Transport::default();
    let client = HerdrClient::with_transport(HerdrClientConfig::default(), transport.clone());
    let target = EndpointTarget::managed("worker".into(), "binding1".into(), None)
        .fenced(Some("instance2".into()));
    let companion = EnvironmentCompanion::default();
    let profile = run(companion.request(&client, &target, serde_json::json!({"operation":"verify", "home":"/homes/deployment1", "deployment_id":"deployment1"}))).unwrap();
    assert_eq!(profile["bundle_digest"], "revision1");
    assert!(run(companion.verify_history(
        &client,
        &target,
        &ResumeRequirement {
            home: "/homes/deployment1".into(),
            kind: "codex".into(),
            session: "conversation1".into(),
            workspace_path: "/work/original".into(),
            environment: BTreeMap::new()
        }
    ))
    .is_err());
    run(companion.verify_history(
        &client,
        &target,
        &ResumeRequirement {
            home: "/homes/deployment1".into(),
            kind: "codex".into(),
            session: "conversation1".into(),
            workspace_path: "/work/original".into(),
            environment: BTreeMap::from([("RESTORED_HISTORY".into(), "yes".into())]),
        },
    ))
    .unwrap();
    let requests = transport.0.borrow();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|request| request.target == target && request.scope == CommandScope::Endpoint));
    let history: serde_json::Value = serde_json::from_slice(&requests[2].stdin).unwrap();
    assert_eq!(history["home"], "/homes/deployment1");
    assert_eq!(history["session"], "conversation1");
    assert_eq!(history["workspace_path"], "/work/original");
    assert!(requests.iter().all(|request| !request.retain_stderr));
}
