//! Environment setup/resume protocol using the same resolved endpoint as Herdr.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use taskr_herdr::{
    CommandRequest, CommandScope, EndpointTarget, HerdrClient, HerdrTransport, TargetKind,
};

pub const COMPANION: &str = include_str!("../companion.py");
pub const MAX_RESPONSE: usize = 128 * 1024 * 1024;

/// Dependency metadata only; command bodies, configuration and credential values
/// travel on companion stdin and are never persisted in the registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentDependency {
    pub kind: String,
    pub reference: String,
    pub profiles: Vec<Option<String>>,
    pub context: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileReadiness {
    pub native_profile: Option<String>,
    pub state: String,
    pub missing_environment: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedEnvironment {
    pub deployment_id: String,
    pub bundle_digest: String,
    pub source_environment_id: String,
    pub source_revision: String,
    pub kind: String,
    #[serde(default)]
    pub display_name: String,
    pub native_profiles: Vec<String>,
    pub home: String,
    pub cli_version: String,
    pub credential_policy: String,
    pub authentication: String,
    #[serde(default)]
    pub dependencies: Vec<EnvironmentDependency>,
    #[serde(default)]
    pub profile_readiness: Vec<ProfileReadiness>,
}

#[derive(Clone, Debug)]
pub struct EnvironmentCompanion {
    pub python: String,
    /// Only explicit non-secret runtime configuration. Bundles travel on stdin.
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct ResumeRequirement {
    pub home: String,
    pub kind: String,
    pub session: String,
    pub workspace_path: String,
    pub environment: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct HistoryObservation {
    history_available: bool,
}
impl Default for EnvironmentCompanion {
    fn default() -> Self {
        Self {
            python: "python3".into(),
            environment: BTreeMap::new(),
        }
    }
}
impl EnvironmentCompanion {
    pub async fn request<T: HerdrTransport>(
        &self,
        client: &HerdrClient<T>,
        target: &EndpointTarget,
        request: Value,
    ) -> Result<Value, String> {
        let output = client.execute_endpoint(CommandRequest {
            target: target.clone(), scope: CommandScope::Endpoint,
            program: if matches!(target.kind, TargetKind::Machine { .. }) { "python3".into() } else { self.python.clone() },
            args: vec!["-c".into(), COMPANION.into()],
            stdin: serde_json::to_vec(&request).map_err(|_| "Cannot encode companion request")?,
            env: self.environment.clone(), cwd: None, timeout: Duration::from_secs(300), output_limit: MAX_RESPONSE, retain_stderr: false,
        }).await.map_err(|_| "Environment companion transport failed; unknown outcome requires verification before retry")?;
        let value: Value = serde_json::from_str(&output.stdout)
            .map_err(|_| "Invalid environment companion response")?;
        if !output.success() {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Environment preparation failed")
                .to_owned());
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| "Environment companion returned no result".into())
    }
    pub async fn prepare<T: HerdrTransport>(
        &self,
        client: &HerdrClient<T>,
        target: &EndpointTarget,
        request: Value,
    ) -> Result<PreparedEnvironment, String> {
        serde_json::from_value(self.request(client, target, request).await?)
            .map_err(|_| "Invalid preparation response".into())
    }
    pub async fn verify_history<T: HerdrTransport>(
        &self,
        client: &HerdrClient<T>,
        target: &EndpointTarget,
        requirement: &ResumeRequirement,
    ) -> Result<(), String> {
        let mut companion = self.clone();
        companion
            .environment
            .extend(requirement.environment.clone());
        let result = companion.request(client, target, json!({"operation":"verify_resume", "home":requirement.home, "kind":requirement.kind, "session":requirement.session, "workspace_path":requirement.workspace_path})).await?;
        let result: HistoryObservation =
            serde_json::from_value(result).map_err(|_| "Invalid native history observation")?;
        if !result.history_available {
            return Err("Native conversation history has not been restored".into());
        }
        Ok(())
    }
}
