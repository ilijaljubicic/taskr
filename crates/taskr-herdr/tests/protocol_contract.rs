//! Regression contracts for the installed Herdr 0.9.3 API schema and CLI transport.
//! No Herdr server, coding agent, or network connection is started by these tests.
//! Run: `cargo test -p taskr-herdr --test protocol_contract`.

use serde_json::{json, Value};
use taskr_herdr::AgentInfo;

fn native_session() -> Value {
    json!({
        "source": "herdr:codex",
        "agent": "codex",
        "kind": "id",
        "value": "native-review"
    })
}

// `success_response.$defs.AgentInfo` uses `agent` for the kind, whereas
// `agent get` wraps this entire object in a result field also named `agent`.
fn agent_row() -> Value {
    json!({
        "name": "taskr-review",
        "agent": "codex",
        "agent_status": "idle",
        "pane_id": "w1:p1",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "terminal_id": "terminal-1",
        "focused": false,
        "revision": 7,
        "cwd": "/remote/repo",
        "foreground_cwd": "/remote/repo/subdir",
        "interactive_ready": true,
        "launch_pending": false,
        "agent_session": native_session()
    })
}

#[test]
fn flat_agent_list_row_keeps_identity_state_and_placement() {
    let row = agent_row();
    let parsed = AgentInfo::parse(&row);

    assert_eq!(parsed.name.as_deref(), Some("taskr-review"));
    assert_eq!(parsed.kind.as_deref(), Some("codex"));
    assert_eq!(parsed.status.as_deref(), Some("idle"));
    assert_eq!(parsed.pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(parsed.workspace_id.as_deref(), Some("w1"));
    assert_eq!(parsed.tab_id.as_deref(), Some("w1:t1"));
    assert_eq!(parsed.cwd.as_deref(), Some("/remote/repo"));
    assert_eq!(
        parsed.foreground_cwd.as_deref(),
        Some("/remote/repo/subdir")
    );
    assert_eq!(parsed.raw, row);
}

#[test]
fn wrapped_agent_get_row_keeps_executable_agent_kind() {
    let row = agent_row();
    let parsed = AgentInfo::parse(&json!({"type": "agent_info", "agent": row}));

    assert_eq!(parsed.name.as_deref(), Some("taskr-review"));
    assert_eq!(parsed.kind.as_deref(), Some("codex"));
    assert_eq!(parsed.status.as_deref(), Some("idle"));
    assert_eq!(parsed.pane_id.as_deref(), Some("w1:p1"));
}

#[test]
fn structured_native_session_is_available_as_a_conversation_reference() {
    let parsed = AgentInfo::parse(&json!({"type": "agent_info", "agent": agent_row()}));
    let serialized = serde_json::to_value(parsed).unwrap();
    let reference = &serialized["agent_session"];

    // Either a typed reference or its native ID is usable by the controller;
    // retaining the object only in an opaque `raw` field is insufficient.
    assert!(
        reference == &native_session() || reference == &json!("native-review"),
        "Herdr's structured native conversation reference was lost: {reference}"
    );
}

#[test]
fn unrecognized_agent_row_keeps_pane_identity_without_inventing_kind() {
    let mut row = agent_row();
    row["name"] = Value::Null;
    row["agent"] = Value::Null;
    row["agent_status"] = json!("shelling");
    row["pane_id"] = json!("w1:p2");
    row["agent_session"] = Value::Null;
    let parsed = AgentInfo::parse(&row);

    assert_eq!(parsed.kind, None);
    assert_eq!(parsed.name, None);
    assert_eq!(parsed.pane_id.as_deref(), Some("w1:p2"));
    assert_eq!(parsed.status.as_deref(), Some("shelling"));
    assert_eq!(parsed.raw, row);
}

#[cfg(unix)]
mod transport {
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use taskr_herdr::{EndpointTarget, HerdrClientConfig, NativeHerdrClient as HerdrClient};

    struct FakeCli {
        dir: PathBuf,
        client: HerdrClient,
    }

    impl FakeCli {
        fn new(body: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "taskr-herdr-transport-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            let bin = dir.join("herdr");
            std::fs::write(&bin, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                dir,
                client: HerdrClient::new(HerdrClientConfig {
                    bin,
                    local_session: None,
                }),
            }
        }
    }

    impl Drop for FakeCli {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn json_stdout_larger_than_a_pipe_buffer_does_not_deadlock() {
        let cli = FakeCli::new(
            "printf '%s' '{\"id\":\"cli:contract\",\"result\":{\"text\":\"'\n\
             printf '%262144s' ''\n\
             printf '%s' '\"}}'",
        );
        let result = cli
            .client
            .execute_json(
                &EndpointTarget::local(None),
                &["agent".into(), "list".into()],
                Duration::from_secs(3),
            )
            .await
            .expect("large valid stdout must complete without timing out");

        assert_eq!(result["text"].as_str().unwrap().len(), 262_144);
    }

    #[tokio::test]
    async fn large_stderr_is_drained_while_stdout_is_still_open() {
        let cli = FakeCli::new(
            "printf '%262144s' '' >&2\n\
             printf '%s' '{\"id\":\"cli:contract\",\"result\":{\"ready\":true}}'",
        );
        let result = cli
            .client
            .execute_json(
                &EndpointTarget::local(None),
                &["agent".into(), "get".into(), "taskr-review".into()],
                Duration::from_secs(3),
            )
            .await
            .expect("stderr must be drained concurrently with stdout and child exit");

        assert_eq!(result, json!({"ready": true}));
    }

    #[tokio::test]
    async fn small_valid_json_response_still_succeeds() {
        let cli = FakeCli::new("printf '%s' '{\"id\":\"cli:agent:list\",\"result\":{\"type\":\"agent_list\",\"agents\":[]}}'");
        let result = cli
            .client
            .execute_json(
                &EndpointTarget::local(None),
                &["agent".into(), "list".into()],
                Duration::from_secs(3),
            )
            .await
            .unwrap();

        assert_eq!(result, json!({"type": "agent_list", "agents": []}));
    }
}
