use serde::Serialize;
use serde_json::Value;

/// Stable ID of the local Herdr endpoint in TASKR's endpoint catalog.
pub const LOCAL_ENDPOINT_ID: &str = "local";

/// How the herdr CLI reaches a server for an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    /// The local Herdr server, optionally through a named persistent session.
    Local { session: Option<String> },
    /// A saved SSH machine profile owned by Herdr/OpenSSH.
    Machine { profile: String },
    /// Logical binding selected by a container host; never an SSH profile.
    Managed {
        binding: String,
        session: Option<String>,
    },
}

impl TargetKind {
    /// Global CLI flags that select this target; placed before the subcommand.
    pub fn prefix_args(&self) -> Vec<String> {
        match self {
            Self::Local {
                session: Some(session),
            }
            | Self::Managed {
                session: Some(session),
                ..
            } => {
                vec!["--session".into(), session.clone()]
            }
            Self::Local { session: None } | Self::Managed { session: None, .. } => Vec::new(),
            Self::Machine { profile } => vec!["--machine".into(), profile.clone()],
        }
    }

    pub fn mode(&self) -> EndpointMode {
        match self {
            Self::Local { .. } => EndpointMode::Local,
            Self::Machine { .. } | Self::Managed { .. } => EndpointMode::Remote,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointMode {
    Local,
    Remote,
}

/// A resolved endpoint reference: stable TASKR ID plus the Herdr CLI target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointTarget {
    pub endpoint_id: String,
    pub kind: TargetKind,
    /// Expected resource incarnation. A mismatched generation forbids dispatch.
    pub runtime_generation: Option<String>,
    /// Resolved native route, supplied only by the endpoint provider.
    pub ssh_target: Option<String>,
}

impl EndpointTarget {
    pub fn local(session: Option<String>) -> Self {
        Self {
            endpoint_id: LOCAL_ENDPOINT_ID.to_owned(),
            kind: TargetKind::Local { session },
            runtime_generation: None,
            ssh_target: None,
        }
    }

    pub fn machine(endpoint_id: String, profile: String) -> Self {
        Self {
            endpoint_id,
            kind: TargetKind::Machine { profile },
            runtime_generation: None,
            ssh_target: None,
        }
    }

    pub fn managed(endpoint_id: String, binding: String, session: Option<String>) -> Self {
        Self {
            endpoint_id,
            kind: TargetKind::Managed { binding, session },
            runtime_generation: None,
            ssh_target: None,
        }
    }
    pub fn fenced(mut self, generation: Option<String>) -> Self {
        self.runtime_generation = generation;
        self
    }

    pub fn id(&self) -> &str {
        &self.endpoint_id
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EndpointInfo {
    pub endpoint_id: String,
    pub label: Option<String>,
    pub mode: EndpointMode,
    /// None means availability has not been probed.
    pub available: Option<bool>,
    pub error: Option<String>,
}

/// Parse `herdr machine list --json` output into endpoint catalog entries.
///
/// The catalog is owned by Herdr/OpenSSH; TASKR derives endpoint IDs from the
/// saved profile IDs and never from mutable display labels.
pub fn parse_machine_list(value: &Value) -> Vec<EndpointInfo> {
    let entries = match value {
        Value::Array(entries) => entries.clone(),
        Value::Object(object) => object
            .get("machines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    entries
        .iter()
        .filter_map(|entry| {
            let profile = string_field(entry, &["id", "name", "profile", "machine"])?;
            let label = string_field(entry, &["label", "display_name", "description"]);
            Some(EndpointInfo {
                endpoint_id: profile.clone(),
                label,
                mode: EndpointMode::Remote,
                available: None,
                error: None,
            })
        })
        .collect()
}

/// Parse `herdr machine status <profile> --json` output for reachability.
pub fn parse_machine_status(value: &Value) -> Result<bool, String> {
    match value {
        Value::Object(object) => {
            for key in ["reachable", "available", "online", "up", "ok"] {
                if let Some(flag) = object.get(key) {
                    return Ok(flag.as_bool().unwrap_or(false));
                }
            }
            if let Some(status) = object.get("status").and_then(Value::as_str) {
                let lowered = status.to_ascii_lowercase();
                return Ok(matches!(
                    lowered.as_str(),
                    "reachable" | "available" | "online" | "ok" | "up" | "connected"
                ));
            }
            // A successful status payload without an explicit flag counts as
            // reachable; the CLI exits non-zero on unreachable machines.
            Ok(true)
        }
        _ => Ok(true),
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn target_prefix_args_select_session_or_machine() {
        assert_eq!(
            EndpointTarget::local(None).kind.prefix_args(),
            Vec::<String>::new()
        );
        assert_eq!(
            EndpointTarget::local(Some("taskr".into()))
                .kind
                .prefix_args(),
            vec!["--session".to_owned(), "taskr".to_owned()]
        );
        assert_eq!(
            EndpointTarget::machine("box".into(), "box".into())
                .kind
                .prefix_args(),
            vec!["--machine".to_owned(), "box".to_owned()]
        );
        assert_eq!(EndpointTarget::local(None).kind.mode(), EndpointMode::Local);
        assert_eq!(
            EndpointTarget::machine("box".into(), "box".into())
                .kind
                .mode(),
            EndpointMode::Remote
        );
    }

    #[test]
    fn parses_machine_list_entries() {
        let entries = parse_machine_list(&json!([
            {"id": "workbox", "label": "worker-a"},
            {"name": "buildbox"}
        ]));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].endpoint_id, "workbox");
        assert_eq!(entries[0].label.as_deref(), Some("worker-a"));
        assert_eq!(entries[0].mode, EndpointMode::Remote);
        assert_eq!(entries[1].endpoint_id, "buildbox");
    }

    #[test]
    fn parses_machine_status_flags() {
        assert!(parse_machine_status(&json!({"reachable": true})).unwrap());
        assert!(!parse_machine_status(&json!({"reachable": false})).unwrap());
        assert!(!parse_machine_status(&json!({"status": "unreachable"})).unwrap());
        assert!(parse_machine_status(&json!({"status": "reachable"})).unwrap());
        assert!(parse_machine_status(&json!({})).unwrap());
    }
}
