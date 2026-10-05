use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::endpoints::EndpointTarget;
use crate::error::{DeliveryCertainty, RuntimeError, RuntimeErrorCategory};

/// Where a worker pane is allocated within the selected endpoint's layout.
#[derive(Debug, Clone, PartialEq)]
pub enum PanePlacement {
    /// Create a fresh workspace (which creates its first tab and root pane).
    NewWorkspace,
    /// Create a new tab in an existing workspace; the tab's root pane is the
    /// worker pane.
    NewTab { workspace_id: String },
    /// Split an existing pane.
    Split {
        pane_id: String,
        direction: SplitDirection,
        ratio: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
    Down,
}

impl SplitDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AllocatePaneRequest {
    pub cwd: String,
    /// Ordered non-secret environment applied to the new pane.
    pub env: BTreeMap<String, String>,
    pub label: Option<String>,
    pub placement: PanePlacement,
    /// Label for a freshly created workspace (NewWorkspace placement only),
    /// Display name only; controllers persist the actual layout IDs.
    pub workspace_label: Option<String>,
}

/// The resources an allocation created or reused. Callers persist these IDs
/// before starting an agent and track which resources this attempt created
/// so rollback can confine itself to newly created ones.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllocatedPane {
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: String,
    pub terminal_id: Option<String>,
    pub created_workspace: bool,
    pub created_tab: bool,
}

/// Live agent description resolved from `agent get`/`agent list` payloads.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentInfo {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub terminal_id: Option<String>,
    /// Native coding-CLI conversation reference, when Herdr reports one.
    pub agent_session: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub raw: Value,
}

impl AgentInfo {
    pub fn parse(value: &Value) -> Self {
        // `agent get` wraps the row in an `agent` object field; a flat
        // `agent list` row carries `agent` as the string agent kind.
        let agent = match value.get("agent") {
            Some(nested @ Value::Object(_)) => nested,
            _ => value,
        };
        Self {
            name: string_field(agent, &["name", "agent_name", "id"]),
            kind: string_field(agent, &["kind", "agent_kind", "type"])
                .or_else(|| string_field(agent, &["agent"])),
            status: string_field(agent, &["status", "agent_status", "state"]),
            pane_id: string_field(agent, &["pane_id", "pane"]),
            workspace_id: string_field(agent, &["workspace_id", "workspace"]),
            tab_id: string_field(agent, &["tab_id", "tab"]),
            terminal_id: string_field(agent, &["terminal_id"]),
            agent_session: agent
                .get("agent_session")
                .or_else(|| agent.get("native_session"))
                .or_else(|| agent.get("session"))
                .and_then(native_session_reference),
            cwd: string_field(agent, &["cwd", "working_directory"]),
            foreground_cwd: string_field(agent, &["foreground_cwd"]),
            raw: agent.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent: AgentInfo,
}
impl PaneInfo {
    pub fn parse(value: &Value) -> Option<Self> {
        let value = value.get("pane").unwrap_or(value);
        Some(Self {
            pane_id: string_field(value, &["pane_id", "id"])?,
            workspace_id: string_field(value, &["workspace_id"]),
            tab_id: string_field(value, &["tab_id"]),
            terminal_id: string_field(value, &["terminal_id"]),
            agent: AgentInfo::parse(value),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
}
impl TabInfo {
    pub fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            tab_id: string_field(value, &["tab_id", "id"])?,
            workspace_id: string_field(value, &["workspace_id"])?,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessInfo {
    pub shell_pid: Option<u64>,
    pub foreground_group: Option<u64>,
    pub foreground_pids: Vec<u64>,
    pub complete: bool,
}
impl ProcessInfo {
    pub fn parse(value: &Value) -> Self {
        let value = value.get("process_info").unwrap_or(value);
        let rows = value.get("foreground_processes").and_then(Value::as_array);
        let pids: Vec<_> = rows
            .into_iter()
            .flatten()
            .filter_map(|row| row.get("pid").and_then(Value::as_u64))
            .collect();
        Self {
            shell_pid: value.get("shell_pid").and_then(Value::as_u64),
            foreground_group: value
                .get("foreground_process_group_id")
                .and_then(Value::as_u64),
            complete: rows.is_some_and(|rows| rows.len() == pids.len()),
            foreground_pids: pids,
        }
    }
    pub fn is_shell(&self) -> bool {
        self.complete
            && self.shell_pid.is_some()
            && self.foreground_group == self.shell_pid
            && !self.foreground_pids.is_empty()
            && self
                .foreground_pids
                .iter()
                .all(|pid| Some(*pid) == self.shell_pid)
    }
}

/// Summary of one workspace on an endpoint, from `workspace list`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceSummary {
    pub workspace_id: String,
    pub label: Option<String>,
    pub tab_count: Option<u64>,
    pub pane_count: Option<u64>,
}

impl WorkspaceSummary {
    pub fn parse(value: &Value) -> Self {
        Self {
            workspace_id: string_field(value, &["workspace_id", "id"]).unwrap_or_default(),
            label: string_field(value, &["label", "name"]),
            tab_count: value.get("tab_count").and_then(Value::as_u64),
            pane_count: value.get("pane_count").and_then(Value::as_u64),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptRequest {
    pub target: String,
    pub text: String,
    pub wait: bool,
    /// When waiting, the lifecycle states that satisfy the wait.
    pub until: Vec<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromptOutcome {
    pub delivered: bool,
    pub status: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
    Detection,
}

impl ReadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Recent => "recent",
            Self::RecentUnwrapped => "recent-unwrapped",
            Self::Detection => "detection",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadResult {
    pub text: String,
    pub truncated: bool,
    pub raw: Value,
}

pub(crate) fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Herdr reports native conversation references either as a bare session ID
/// string or as a structured `{source, agent, kind, value}` object.
fn native_session_reference(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(_) => string_field(value, &["value", "id", "session"]),
        _ => None,
    }
}

pub(crate) fn env_args(env: &BTreeMap<String, String>) -> Vec<String> {
    let mut args = Vec::new();
    for (key, value) in env {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    args
}

/// Extract a pane ID from a creation result, tolerating the several result
/// shapes herdr commands return (`root_pane`, `pane`, bare pane info).
pub(crate) fn pane_id_from_result(value: &Value) -> Option<String> {
    let pane = if value.get("root_pane").is_some() {
        &value["root_pane"]
    } else if value.get("pane").is_some() {
        &value["pane"]
    } else {
        value
    };
    string_field(pane, &["pane_id", "id", "pane"])
}

pub(crate) fn terminal_id_from_result(value: &Value) -> Option<String> {
    let pane = value
        .get("root_pane")
        .or_else(|| value.get("pane"))
        .unwrap_or(value);
    string_field(pane, &["terminal_id"])
}

pub(crate) fn workspace_id_from_result(value: &Value) -> Option<String> {
    let workspace = if value.get("workspace").is_some() {
        &value["workspace"]
    } else {
        value
    };
    string_field(workspace, &["workspace_id", "id"])
}

pub(crate) fn tab_id_from_result(value: &Value) -> Option<String> {
    let tab = if value.get("tab").is_some() {
        &value["tab"]
    } else {
        value
    };
    string_field(tab, &["tab_id", "id"])
}

/// Build the argument vector for a pane allocation command.
pub(crate) fn allocate_args(request: &AllocatePaneRequest) -> (Vec<String>, bool, bool) {
    let mut args = Vec::new();
    let (created_workspace, created_tab) = match &request.placement {
        PanePlacement::NewWorkspace => {
            args.extend(["workspace".into(), "create".into()]);
            if let Some(workspace_label) = &request.workspace_label {
                args.extend(["--label".into(), workspace_label.clone()]);
            }
            (true, true)
        }
        PanePlacement::NewTab { workspace_id } => {
            args.extend([
                "tab".into(),
                "create".into(),
                "--workspace".into(),
                workspace_id.clone(),
            ]);
            (false, true)
        }
        PanePlacement::Split {
            pane_id,
            direction,
            ratio,
        } => {
            args.extend([
                "pane".into(),
                "split".into(),
                "--pane".into(),
                pane_id.clone(),
            ]);
            args.extend(["--direction".into(), direction.as_str().into()]);
            if let Some(ratio) = ratio {
                args.push("--ratio".into());
                args.push(format!("{ratio}"));
            }
            (false, false)
        }
    };
    args.extend(["--cwd".into(), request.cwd.clone()]);
    // `workspace create` has a single --label and it names the workspace
    // itself; appending the per-tab label here would override it (clap keeps
    // the last occurrence), replacing the intended plan-space label.
    if created_tab && !created_workspace {
        if let Some(label) = &request.label {
            args.push("--label".into());
            args.push(label.clone());
        }
    }
    args.extend(env_args(&request.env));
    args.push("--no-focus".into());
    (args, created_workspace, created_tab)
}

/// Build the argument vector for `agent start`.
pub(crate) fn agent_start_args(
    name: &str,
    kind: &str,
    pane_id: &str,
    args: &[String],
    startup_timeout: Option<Duration>,
) -> Vec<String> {
    let mut command = vec![
        "agent".into(),
        "start".into(),
        name.to_owned(),
        "--kind".into(),
        kind.to_owned(),
        "--pane".into(),
        pane_id.to_owned(),
    ];
    if let Some(timeout) = startup_timeout {
        command.push("--timeout".into());
        command.push(timeout.as_millis().to_string());
    }
    if !args.is_empty() {
        command.push("--".into());
        command.extend(args.iter().cloned());
    }
    command
}

pub(crate) fn prompt_args(request: &PromptRequest) -> Vec<String> {
    let mut args = vec!["agent".into(), "prompt".into(), request.target.clone()];
    args.push(request.text.clone());
    if request.wait {
        args.push("--wait".into());
        for state in &request.until {
            args.push("--until".into());
            args.push(state.clone());
        }
        if let Some(timeout) = request.timeout {
            args.push("--timeout".into());
            args.push(timeout.as_millis().to_string());
        }
    }
    args
}

pub(crate) fn read_args(target: &str, source: ReadSource, lines: Option<u32>) -> Vec<String> {
    let mut args = vec![
        "agent".into(),
        "read".into(),
        target.to_owned(),
        "--source".into(),
        source.as_str().into(),
        "--format".into(),
        "text".into(),
    ];
    if let Some(lines) = lines {
        args.push("--lines".into());
        args.push(lines.to_string());
    }
    args
}

pub(crate) fn wait_args(target: &str, until: &[String], timeout: Option<Duration>) -> Vec<String> {
    let mut args = vec!["agent".into(), "wait".into(), target.to_owned()];
    for state in until {
        args.push("--until".into());
        args.push(state.clone());
    }
    if let Some(timeout) = timeout {
        args.push("--timeout".into());
        args.push(timeout.as_millis().to_string());
    }
    args
}

/// Classify a herdr rejection raised by an agent/prompt operation, applying
/// the operation's delivery certainty.
pub(crate) fn classify_herdr_error(
    operation: &'static str,
    target: &EndpointTarget,
    error: &crate::envelope::HerdrCliError,
    certainty: DeliveryCertainty,
) -> RuntimeError {
    let category = match &error.code {
        Some(code) => crate::error::category_for_herdr_code(code),
        None => RuntimeErrorCategory::UnknownOutcome,
    };
    RuntimeError {
        observed_generation: None,
        category,
        operation,
        endpoint: Some(target.endpoint_id.clone()),
        detail: error.message.clone(),
        herdr_code: error.code.clone(),
        delivery_certainty: certainty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(placement: PanePlacement) -> AllocatePaneRequest {
        AllocatePaneRequest {
            cwd: "/workspace/project".into(),
            env: BTreeMap::from([
                ("CODEX_HOME".into(), "~/agents/codex".into()),
                ("TASKR_FLAG".into(), "1".into()),
            ]),
            label: Some("taskr-task-1".into()),
            workspace_label: None,
            placement,
        }
    }

    #[test]
    fn workspace_create_carries_the_plan_space_label() {
        let mut request = request(PanePlacement::NewWorkspace);
        request.workspace_label = Some("taskr-smoke".into());
        let (args, created_workspace, created_tab) = allocate_args(&request);
        let label_values = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| flag.as_str() == "--label")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        assert_eq!(label_values, ["taskr-smoke"]);
        assert!(created_workspace && created_tab);
    }

    #[test]
    fn workspace_create_args_are_explicit_and_unfocused() {
        let (args, created_workspace, created_tab) =
            allocate_args(&request(PanePlacement::NewWorkspace));
        assert_eq!(args[..2], ["workspace".to_owned(), "create".to_owned()]);
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--cwd", "/workspace/project"]));
        // No workspace label means the workspace stays unnamed: the per-tab
        // label must never leak onto the workspace.
        assert!(!args.contains(&"--label".to_owned()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--env", "CODEX_HOME=~/agents/codex"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--env", "TASKR_FLAG=1"]));
        assert!(args.contains(&"--no-focus".to_owned()));
        assert!(created_workspace && created_tab);
    }

    #[test]
    fn tab_and_split_args_target_existing_layout() {
        let (args, created_workspace, created_tab) =
            allocate_args(&request(PanePlacement::NewTab {
                workspace_id: "w3".into(),
            }));
        assert_eq!(args[..2], ["tab".to_owned(), "create".to_owned()]);
        assert!(args.windows(2).any(|pair| pair == ["--workspace", "w3"]));
        assert!(!created_workspace && created_tab);

        let (args, created_workspace, created_tab) =
            allocate_args(&request(PanePlacement::Split {
                pane_id: "w3:p1".into(),
                direction: SplitDirection::Down,
                ratio: Some(0.5),
            }));
        assert_eq!(args[..2], ["pane".to_owned(), "split".to_owned()]);
        assert!(args.windows(2).any(|pair| pair == ["--pane", "w3:p1"]));
        assert!(args.windows(2).any(|pair| pair == ["--direction", "down"]));
        assert!(args.windows(2).any(|pair| pair == ["--ratio", "0.5"]));
        assert!(!created_workspace && !created_tab);
    }

    #[test]
    fn agent_start_args_pass_native_arguments_after_separator() {
        let args = agent_start_args(
            "taskr-task-1-codex-1",
            "codex",
            "w3:p2",
            &["--profile".into(), "implement".into()],
            Some(Duration::from_secs(30)),
        );
        assert_eq!(
            args,
            vec![
                "agent",
                "start",
                "taskr-task-1-codex-1",
                "--kind",
                "codex",
                "--pane",
                "w3:p2",
                "--timeout",
                "30000",
                "--",
                "--profile",
                "implement"
            ]
        );

        let bare = agent_start_args("taskr-x", "codex", "w3:p2", &[], None);
        assert!(!bare.contains(&"--".to_owned()));
        assert!(!bare.contains(&"--timeout".to_owned()));
    }

    #[test]
    fn prompt_args_wait_options_come_after_text() {
        let args = prompt_args(&PromptRequest {
            target: "taskr-worker".into(),
            text: "line one\nline two".into(),
            wait: true,
            until: vec!["working".into(), "blocked".into()],
            timeout: Some(Duration::from_secs(10)),
        });
        assert_eq!(args[0], "agent");
        assert_eq!(args[1], "prompt");
        assert_eq!(args[2], "taskr-worker");
        assert_eq!(args[3], "line one\nline two");
        assert!(args.contains(&"--wait".to_owned()));
        assert!(args.windows(2).any(|pair| pair == ["--until", "working"]));
        assert!(args.windows(2).any(|pair| pair == ["--until", "blocked"]));
        assert!(args.windows(2).any(|pair| pair == ["--timeout", "10000"]));
    }

    #[test]
    fn parse_result_shapes_from_workspace_and_tab_creations() {
        let workspace_result = json!({
            "type": "workspace_created",
            "root_pane": {"pane_id": "w5:p1", "workspace_id": "w5", "tab_id": "w5:t1"},
            "tab": {"tab_id": "w5:t1", "workspace_id": "w5"},
            "workspace": {"workspace_id": "w5"}
        });
        assert_eq!(
            pane_id_from_result(&workspace_result).as_deref(),
            Some("w5:p1")
        );
        assert_eq!(
            workspace_id_from_result(&workspace_result).as_deref(),
            Some("w5")
        );
        assert_eq!(
            tab_id_from_result(&workspace_result).as_deref(),
            Some("w5:t1")
        );
    }

    #[test]
    fn agent_info_parses_known_fields_and_keeps_raw() {
        let info = AgentInfo::parse(&json!({
            "name": "taskr-worker",
            "kind": "codex",
            "status": "idle",
            "pane_id": "w3:p2",
            "agent_session": "codex:abc123",
            "cwd": "/workspace/project"
        }));
        assert_eq!(info.name.as_deref(), Some("taskr-worker"));
        assert_eq!(info.kind.as_deref(), Some("codex"));
        assert_eq!(info.status.as_deref(), Some("idle"));
        assert_eq!(info.pane_id.as_deref(), Some("w3:p2"));
        assert_eq!(info.agent_session.as_deref(), Some("codex:abc123"));
        assert_eq!(info.raw["kind"], "codex");
    }

    #[test]
    fn read_and_wait_args_use_documented_surfaces() {
        assert_eq!(
            read_args("taskr-worker", ReadSource::Recent, Some(200)),
            vec![
                "agent",
                "read",
                "taskr-worker",
                "--source",
                "recent",
                "--format",
                "text",
                "--lines",
                "200"
            ]
        );
        assert_eq!(
            wait_args(
                "taskr-worker",
                &["idle".into(), "done".into()],
                Some(Duration::from_secs(5))
            ),
            vec![
                "agent",
                "wait",
                "taskr-worker",
                "--until",
                "idle",
                "--until",
                "done",
                "--timeout",
                "5000"
            ]
        );
    }
}
