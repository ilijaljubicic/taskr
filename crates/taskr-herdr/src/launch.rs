use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;

/// Live agent names must match Herdr's naming rule.
pub const AGENT_NAME_PREFIX: &str = "taskr-";
const AGENT_NAME_MAX_LEN: usize = 32;

/// Environment variables that scope a coding CLI's configuration home on the
/// execution endpoint. The mapping is by Herdr agent kind.
pub fn home_env_var_for_kind(agent_kind: &str) -> Option<&'static str> {
    match agent_kind {
        "codex" => Some("CODEX_HOME"),
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "opencode" => Some("OPENCODE_CONFIG_DIR"),
        "kimi" => Some("KIMI_CODE_HOME"),
        _ => None,
    }
}

/// A named launch preset: TASKR-owned configuration data describing how to
/// start one agent kind. It carries no screen markers, paste strategy, or
/// terminal commands; Herdr owns execution and detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchProfile {
    pub id: String,
    pub agent_kind: String,
    /// Native agent arguments passed after `agent start ... --`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Arguments used instead of `args` when a launch requests permission
    /// bypass. Absent means the preset does not support bypass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_args: Option<Vec<String>>,
    /// Non-secret configuration environment applied when the worker pane is
    /// created (`--env KEY=VALUE`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl LaunchProfile {
    pub fn effective_args(&self, bypass_permissions: bool) -> Result<&Vec<String>, RuntimeError> {
        if bypass_permissions {
            self.bypass_args.as_ref().ok_or_else(|| {
                RuntimeError::invalid_launch_config(
                    "resolve_launch_profile",
                    format!(
                        "launch profile '{}' does not define bypass_args; bypass_permissions=true is not supported",
                        self.id
                    ),
                )
            })
        } else {
            Ok(&self.args)
        }
    }

    /// Configuration environment for the worker pane, with an optional
    /// per-project configuration-home override applied for the preset's
    /// agent kind. `~`-relative paths are passed through unresolved; they
    /// are interpreted on the selected endpoint.
    pub fn effective_env(&self, project_home: Option<&str>) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        if let Some(home) = project_home {
            if let Some(variable) = home_env_var_for_kind(&self.agent_kind) {
                env.insert(variable.to_owned(), home.to_owned());
            }
        }
        env
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLaunch {
    pub endpoint_id: String,
    pub launch_profile_id: String,
    pub agent_kind: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub bypass_permissions: bool,
    pub workspace_path: String,
    pub skills: Vec<String>,
    pub agent_name: String,
}

pub fn validate_agent_name(name: &str) -> Result<(), RuntimeError> {
    let mut chars = name.chars();
    let valid = !name.is_empty()
        && name.len() <= AGENT_NAME_MAX_LEN
        && chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
    if valid {
        Ok(())
    } else {
        Err(RuntimeError::invalid_launch_config(
            "generate_agent_name",
            format!(
                "agent name '{name}' must match [a-z][a-z0-9_-]{{0,31}} (max {AGENT_NAME_MAX_LEN} chars)"
            ),
        ))
    }
}

fn sanitize_name_part(text: &str) -> String {
    let mut result = String::new();
    let mut previous_dash = true;
    for ch in text.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            result.push(ch);
            previous_dash = false;
        } else if ch.is_ascii_uppercase() {
            result.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            result.push('-');
            previous_dash = true;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    result
}

/// Generate an `taskr-` prefixed live agent name within Herdr's 32-character
/// limit. The attempt suffix keeps replacement attempts distinct; Herdr
/// enforces liveness uniqueness per server.
pub fn generate_agent_name(task_slug: &str, kind: &str, attempt: u64) -> String {
    let mut base = format!(
        "{}{}",
        AGENT_NAME_PREFIX,
        sanitize_name_part(&format!("{task_slug}-{kind}"))
    );
    let suffix = format!("-{attempt}");
    let keep = AGENT_NAME_MAX_LEN.saturating_sub(suffix.len());
    base.truncate(keep);
    while base.ends_with('-') {
        base.pop();
    }
    let name = format!("{base}{suffix}");
    debug_assert!(validate_agent_name(&name).is_ok(), "generated name {name}");
    name
}

/// Validate one launch profile's data.
pub fn validate_launch_profile(profile: &LaunchProfile) -> Result<(), RuntimeError> {
    if profile.id.is_empty() {
        return Err(RuntimeError::invalid_launch_config(
            "validate_launch_profile",
            "launch profile id must not be empty",
        ));
    }
    let id_chars = profile
        .id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
    if !id_chars
        || !profile
            .id
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
    {
        return Err(RuntimeError::invalid_launch_config(
            "validate_launch_profile",
            format!(
                "launch profile id '{}' must match [a-z][a-z0-9_-]*",
                profile.id
            ),
        ));
    }
    if profile.agent_kind.is_empty()
        || !profile
            .agent_kind
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(RuntimeError::invalid_launch_config(
            "validate_launch_profile",
            format!(
                "launch profile '{}' has invalid agent_kind '{}'",
                profile.id, profile.agent_kind
            ),
        ));
    }
    for (key, value) in &profile.env {
        if key.is_empty()
            || !key
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
            || key.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        {
            return Err(RuntimeError::invalid_launch_config(
                "validate_launch_profile",
                format!(
                    "launch profile '{}' has invalid environment key '{key}'",
                    profile.id
                ),
            ));
        }
        if value.contains('\0') {
            return Err(RuntimeError::invalid_launch_config(
                "validate_launch_profile",
                format!(
                    "launch profile '{}' environment value for '{key}' contains NUL",
                    profile.id
                ),
            ));
        }
    }
    if profile.bypass_args.as_ref().is_some_and(Vec::is_empty) {
        return Err(RuntimeError::invalid_launch_config(
            "validate_launch_profile",
            format!("launch profile '{}' defines empty bypass_args", profile.id),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(id: &str, kind: &str) -> LaunchProfile {
        LaunchProfile {
            id: id.into(),
            agent_kind: kind.into(),
            args: Vec::new(),
            bypass_args: None,
            env: BTreeMap::new(),
            description: None,
        }
    }

    #[test]
    fn validates_agent_names() {
        assert!(validate_agent_name("taskr-task-1-codex").is_ok());
        assert!(validate_agent_name("taskr-a_b-c9").is_ok());
        assert!(validate_agent_name("Codex").is_err());
        assert!(validate_agent_name("1abc").is_err());
        assert!(validate_agent_name("-abc").is_err());
        assert!(validate_agent_name("").is_err());
        assert!(validate_agent_name(&"a".repeat(33)).is_err());
        assert!(validate_agent_name(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn generates_prefixed_names_within_limit() {
        let name = generate_agent_name("Task 12: Orchestration/Core!", "Model Owner", 3);
        assert!(name.starts_with("taskr-task-12-orchestration"), "{name}");
        assert!(name.ends_with("-3"), "{name}");
        assert!(name.len() <= 32);
        validate_agent_name(&name).unwrap();

        let long = generate_agent_name(
            "a-very-long-task-slug-that-keeps-going-and-going",
            "implementation",
            17,
        );
        assert!(long.len() <= 32);
        assert!(long.ends_with("-17"));
        validate_agent_name(&long).unwrap();

        let simple = generate_agent_name("worker", "codex", 1);
        assert_eq!(simple, "taskr-worker-codex-1");
    }

    #[test]
    fn home_env_mapping_matches_agent_kinds() {
        assert_eq!(home_env_var_for_kind("codex"), Some("CODEX_HOME"));
        assert_eq!(home_env_var_for_kind("claude"), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(
            home_env_var_for_kind("opencode"),
            Some("OPENCODE_CONFIG_DIR")
        );
        assert_eq!(home_env_var_for_kind("kimi"), Some("KIMI_CODE_HOME"));
        assert_eq!(home_env_var_for_kind("gemini"), None);
    }

    #[test]
    fn effective_env_applies_project_home_override() {
        let mut preset = profile("codex-implement", "codex");
        preset
            .env
            .insert("CODEX_HOME".into(), "/srv/agents/codex".into());
        preset.env.insert("TASKR_FLAG".into(), "1".into());
        let env = preset.effective_env(Some("~/project codex"));
        assert_eq!(
            env.get("CODEX_HOME").map(String::as_str),
            Some("~/project codex")
        );
        assert_eq!(env.get("TASKR_FLAG").map(String::as_str), Some("1"));

        let no_kind_home = profile("gemini-review", "gemini");
        let env = no_kind_home.effective_env(Some("/ignored"));
        assert!(env.is_empty());
    }

    #[test]
    fn effective_args_requires_bypass_support() {
        let mut preset = profile("codex-implement", "codex");
        preset.args = vec!["--profile".into(), "implement".into()];
        preset.bypass_args = Some(vec!["--dangerously-bypass-approvals-and-sandbox".into()]);
        assert_eq!(
            preset.effective_args(true).unwrap(),
            &vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()]
        );
        assert_eq!(
            preset.effective_args(false).unwrap(),
            &vec!["--profile".to_owned(), "implement".to_owned()]
        );

        let plain = profile("plain", "codex");
        assert!(plain.effective_args(true).is_err());
    }

    #[test]
    fn validates_profile_data() {
        assert!(validate_launch_profile(&profile("codex-implement", "codex")).is_ok());
        assert!(validate_launch_profile(&profile("Codex", "codex")).is_err());
        assert!(validate_launch_profile(&profile("ok", "")).is_err());
        assert!(validate_launch_profile(&profile("ok", "co dex")).is_err());

        let mut bad_env = profile("ok", "codex");
        bad_env.env.insert("lower-key".into(), "v".into());
        assert!(validate_launch_profile(&bad_env).is_err());

        let mut empty_bypass = profile("ok", "codex");
        empty_bypass.bypass_args = Some(Vec::new());
        assert!(validate_launch_profile(&empty_bypass).is_err());
    }

    #[test]
    fn profile_serde_round_trips_without_screen_fields() {
        let text = r#"{"id":"codex-implement","agent_kind":"codex","args":["--profile","implement"],"bypass_args":["--yolo"],"env":{"CODEX_HOME":"/srv"},"description":"implement"}"#;
        let profile: LaunchProfile = serde_json::from_str(text).unwrap();
        assert_eq!(
            profile,
            LaunchProfile {
                id: "codex-implement".into(),
                agent_kind: "codex".into(),
                args: vec!["--profile".into(), "implement".into()],
                bypass_args: Some(vec!["--yolo".into()]),
                env: BTreeMap::from([("CODEX_HOME".into(), "/srv".into())]),
                description: Some("implement".into()),
            }
        );
        let value = serde_json::to_value(&profile).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "codex-implement",
                "agent_kind": "codex",
                "args": ["--profile", "implement"],
                "bypass_args": ["--yolo"],
                "env": {"CODEX_HOME": "/srv"},
                "description": "implement"
            })
        );
    }
}
