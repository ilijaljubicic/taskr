//! Scoped native environment discovery/deployment. Herdr still owns execution.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use taskr_herdr::{
    EndpointAction, EndpointTarget, HerdrClientConfig, LaunchProfile,
    NativeHerdrClient as HerdrClient,
};

use crate::protocol::{EnvironmentCompanion, PreparedEnvironment, ResumeRequirement};

#[derive(Clone, Debug)]
pub struct CompanionConfig {
    pub python_bin: PathBuf,
    pub ssh_bin: PathBuf,
    /// Child environment additions; never stored or returned in MCP results.
    pub environment: BTreeMap<String, String>,
}
impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            python_bin: "python3".into(),
            ssh_bin: "ssh".into(),
            environment: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedSource {
    pub kind: String,
    pub path: String,
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_root: Option<String>,
    pub relative_home: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_home: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_user_home: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_skills: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRequest {
    pub homes: Option<Vec<String>>,
    pub source_path: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentEnvironment {
    pub source_environment_id: String,
    pub source_home: String,
    #[serde(default)]
    pub display_name: String,
    pub source_revision: String,
    pub kind: String,
    pub cli_version: String,
    pub native_profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_location: Option<ImportedSource>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub environments: Vec<AgentEnvironment>,
    pub issues: Vec<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LaunchChoice {
    pub endpoint_id: String,
    pub profile: LaunchProfile,
    pub deployment: Option<PreparedEnvironment>,
    pub native_profile: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    pub source_environment_id: String,
    pub source_revision: String,
    pub endpoint_id: String,
    /// "endpoint" never exports source login files; "copy" explicitly does.
    pub credential_policy: String,
    pub endpoint_auth_home: Option<String>,
    pub deployment_root: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// Re-export credentials/recheck a ready selection, without overwriting it.
    #[serde(default)]
    pub refresh: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncJob {
    pub sync_job_id: String,
    pub state: String,
    pub request: SyncRequest,
    pub source: AgentEnvironment,
    pub error: Option<String>,
    pub prepared: Option<PreparedEnvironment>,
    pub launch_profile_ids: Vec<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default = "registry_version")]
    version: u32,
    sources: BTreeMap<String, AgentEnvironment>,
    jobs: BTreeMap<String, SyncJob>,
    choices: Vec<LaunchChoice>,
}
fn registry_version() -> u32 {
    1
}

pub struct EnvironmentCatalog {
    registry: Mutex<Registry>,
    db_path: Option<PathBuf>,
    config: CompanionConfig,
    running: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    slots: tokio::sync::Semaphore,
}
impl EnvironmentCatalog {
    pub fn open(store_dir: &Path, config: CompanionConfig) -> Result<Arc<Self>, String> {
        if store_dir.join("mmux.db").exists() && !store_dir.join("taskr.db").exists() {
            return Err("Legacy MMUX database found. Use an already converted Taskr store containing taskr.db or select a separate store directory before opening the environment catalog".into());
        }
        let db_path = store_dir.join("taskr.db");
        let db = Connection::open(&db_path).map_err(|_| "Cannot open environment store")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_environment_registry (
            id INTEGER PRIMARY KEY CHECK(id = 1), state_json TEXT NOT NULL);",
        )
        .map_err(|_| "Cannot initialize environment store")?;
        let raw: Option<String> = db
            .query_row(
                "SELECT state_json FROM agent_environment_registry WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "Cannot read environment store")?;
        let mut registry: Registry = raw
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| "Invalid environment registry; preserve the store and repair it")?
            .unwrap_or_default();
        if registry.version > registry_version() {
            return Err("Environment registry needs a newer controller".into());
        }
        registry.version = registry_version();
        Ok(Arc::new(Self {
            registry: Mutex::new(registry),
            db_path: Some(db_path),
            config,
            running: Mutex::new(HashMap::new()),
            slots: tokio::sync::Semaphore::new(4),
        }))
    }

    /// Fixture construction uses the same catalog rows/resolution, without IO.
    pub fn in_memory(choices: Vec<LaunchChoice>, config: CompanionConfig) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Registry {
                choices,
                ..Registry::default()
            }),
            db_path: None,
            config,
            running: Mutex::new(HashMap::new()),
            slots: tokio::sync::Semaphore::new(4),
        })
    }

    fn update<T>(&self, f: impl FnOnce(&mut Registry) -> Result<T, String>) -> Result<T, String> {
        let mut guard = self
            .registry
            .lock()
            .map_err(|_| "Environment registry lock poisoned")?;
        let mut next = guard.clone();
        let result = f(&mut next)?;
        if let Some(path) = &self.db_path {
            let raw = serde_json::to_string(&next)
                .map_err(|_| "Cannot serialize environment metadata")?;
            let db = Connection::open(path).map_err(|_| "Cannot open environment store")?;
            db.execute(
                "INSERT INTO agent_environment_registry(id,state_json) VALUES(1,?1)
                ON CONFLICT(id) DO UPDATE SET state_json=excluded.state_json",
                params![raw],
            )
            .map_err(|_| "Cannot persist environment metadata")?;
        }
        *guard = next;
        Ok(result)
    }

    pub fn list(&self, endpoint: &str) -> Result<Vec<LaunchChoice>, String> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| "Environment registry lock poisoned")?;
        Ok(registry
            .choices
            .iter()
            .filter(|c| c.endpoint_id == endpoint)
            .cloned()
            .collect())
    }
    pub fn choice(&self, endpoint: &str, requested: Option<&str>) -> Result<LaunchChoice, String> {
        let choices = self.list(endpoint)?;
        if let Some(id) = requested.filter(|id| !id.trim().is_empty()) {
            choices.into_iter().find(|c| c.profile.id == id)
                .ok_or_else(|| format!("launch profile '{id}' is not prepared/enabled for endpoint '{endpoint}'; use admin_environment_sync first"))
        } else if choices.len() == 1 {
            Ok(choices.into_iter().next().unwrap())
        } else {
            Err(
                "select an explicit launch_profile_id from list_launch_profiles(endpoint_id)"
                    .into(),
            )
        }
    }
    pub fn resolve(
        &self,
        endpoint: &str,
        requested: Option<&str>,
    ) -> Result<LaunchProfile, String> {
        Ok(self.choice(endpoint, requested)?.profile)
    }

    pub async fn discover(&self, request: DiscoveryRequest) -> Result<Discovery, String> {
        if request.homes.is_some() && request.source_path.is_some() {
            return Err("homes and source_path are mutually exclusive".into());
        }
        if request
            .source_path
            .as_ref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err("source_path must not be empty".into());
        }
        let cache_root = self
            .db_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.join("agent-environment-sources"))
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("taskr-source-cache-{}", uuid::Uuid::new_v4()))
            });
        let result = self
            .local_request(json!({"operation":"discover", "homes":request.homes,
                "source_path":request.source_path, "cache_root":cache_root}))
            .await?;
        let discovery: Discovery =
            serde_json::from_value(result).map_err(|_| "Invalid companion discovery response")?;
        self.update(|registry| {
            // Refresh selected roots, but preserve other explicitly discovered sources.
            for source in &discovery.environments {
                registry
                    .sources
                    .insert(source.source_environment_id.clone(), source.clone());
            }
            Ok(())
        })?;
        Ok(discovery)
    }

    pub fn status(&self, id: &str) -> Result<SyncJob, String> {
        self.registry
            .lock()
            .map_err(|_| "Environment registry lock poisoned")?
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| format!("sync job '{id}' not found"))
    }

    pub async fn sync(
        self: &Arc<Self>,
        herdr: HerdrClient,
        request: SyncRequest,
    ) -> Result<SyncJob, String> {
        validate_sync(&request)?;
        // Resolve a saved enabled machine before recording a job. No aliases.
        self.transport(&herdr, &request.endpoint_id).await?;
        let job = self.update(|registry| {
            let source = registry
                .sources
                .get(&request.source_environment_id)
                .cloned()
                .ok_or("source environment not discovered; call admin_environment_discover")?;
            if source.source_revision != request.source_revision {
                return Err("source revision is stale; discover again".into());
            }
            if let Some(previous) = registry.jobs.values().find(|j| {
                serde_json::to_value(&j.request).ok() == serde_json::to_value(&request).ok()
                    && (matches!(j.state.as_str(), "queued" | "preparing")
                        || (j.state == "ready" && !request.refresh))
            }) {
                return Ok(previous.clone());
            }
            let job = SyncJob {
                sync_job_id: format!("sync-{}", uuid::Uuid::new_v4().simple()),
                state: "queued".into(),
                request,
                source,
                error: None,
                prepared: None,
                launch_profile_ids: Vec::new(),
            };
            registry.jobs.insert(job.sync_job_id.clone(), job.clone());
            Ok(job)
        })?;
        if job.state != "ready" {
            self.spawn(herdr, job.sync_job_id.clone());
        }
        Ok(job)
    }

    pub fn resume_pending(self: &Arc<Self>, herdr: HerdrClient) -> Result<(), String> {
        let ids = self
            .registry
            .lock()
            .map_err(|_| "Environment registry lock poisoned")?
            .jobs
            .values()
            .filter(|j| matches!(j.state.as_str(), "queued" | "preparing"))
            .map(|j| j.sync_job_id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.spawn(herdr.clone(), id);
        }
        Ok(())
    }

    fn spawn(self: &Arc<Self>, herdr: HerdrClient, id: String) {
        let mut running = self.running.lock().unwrap();
        running.retain(|_, task| !task.is_finished());
        if running.get(&id).is_some_and(|task| !task.is_finished()) {
            return;
        }
        let catalog = self.clone();
        let job_id = id.clone();
        let task = tokio::spawn(async move {
            let _slot = catalog
                .slots
                .acquire()
                .await
                .expect("catalog semaphore remains open");
            let result = catalog.perform(&herdr, &job_id).await;
            if let Err(error) = result {
                let _ = catalog.update(|registry| {
                    let job = registry.jobs.get_mut(&job_id).ok_or("sync job vanished")?;
                    if job.state != "canceled" {
                        job.state = "failed".into();
                        job.error = Some(error);
                    }
                    Ok(())
                });
            }
        });
        running.insert(id, task);
    }

    pub fn cancel(&self, id: &str) -> Result<SyncJob, String> {
        let job = self.update(|registry| {
            let job = registry.jobs.get_mut(id).ok_or("sync job not found")?;
            if job.state == "ready" {
                return Err("ready sync cannot be canceled; deployments are retained".into());
            }
            job.state = "canceled".into();
            Ok(job.clone())
        })?;
        if let Some(task) = self.running.lock().unwrap().remove(id) {
            task.abort();
        }
        Ok(job)
    }

    async fn perform(&self, herdr: &HerdrClient, id: &str) -> Result<(), String> {
        let job = self.update(|registry| {
            let job = registry.jobs.get_mut(id).ok_or("sync job not found")?;
            if job.state == "canceled" {
                return Err("sync canceled".into());
            }
            job.state = "preparing".into();
            Ok(job.clone())
        })?;
        // File contents/credentials only travel in memory through stdio. SQLite
        // contains source metadata, jobs and prepared choices, never the bundle.
        let bundle = self
            .local_request(
                json!({"operation":"export", "source_home":job.source.source_home,
            "kind":job.source.kind, "source_revision":job.request.source_revision,
            "source_location":job.source.source_location,
            "credential_policy":job.request.credential_policy}),
            )
            .await?;
        let transport = self.transport(herdr, &job.request.endpoint_id).await?;
        let result = self.endpoint_request(herdr, &transport, json!({"operation":"prepare", "bundle":bundle,
            "endpoint_auth_home":job.request.endpoint_auth_home, "deployment_root":job.request.deployment_root,
            "dry_run":job.request.dry_run, "remote":transport.kind.mode() == taskr_herdr::EndpointMode::Remote})).await?;
        let prepared: PreparedEnvironment =
            serde_json::from_value(result).map_err(|_| "Invalid preparation response")?;
        let choices = launch_choices(&job.request.endpoint_id, &prepared);
        self.update(|registry| {
            let job = registry.jobs.get_mut(id).ok_or("sync job not found")?;
            if job.state == "canceled" {
                return Err("sync canceled; deployment was not selected".into());
            }
            job.state = "ready".into();
            job.prepared = Some(prepared);
            if !job.request.dry_run {
                job.launch_profile_ids = choices.iter().map(|c| c.profile.id.clone()).collect();
                for choice in choices {
                    registry.choices.retain(|c| {
                        c.endpoint_id != choice.endpoint_id || c.profile.id != choice.profile.id
                    });
                    registry.choices.push(choice);
                }
            }
            Ok(())
        })
    }

    pub async fn verify(
        &self,
        herdr: &HerdrClient,
        endpoint: &str,
        id: &str,
    ) -> Result<(), String> {
        let target = self.transport(herdr, endpoint).await?;
        self.verify_at(herdr, &target, id).await
    }

    async fn transport(
        &self,
        herdr: &HerdrClient,
        endpoint: &str,
    ) -> Result<EndpointTarget, String> {
        let client = herdr.with_ssh_bin(self.config.ssh_bin.clone());
        client
            .endpoint(
                client.target_for_endpoint(endpoint),
                EndpointAction::Observe,
            )
            .await
            .map(|state| state.target)
            .map_err(|_| "Cannot resolve or observe Herdr endpoint".into())
    }
    fn companion(&self) -> EnvironmentCompanion {
        EnvironmentCompanion {
            python: self.config.python_bin.to_string_lossy().into_owned(),
            environment: self.config.environment.clone(),
        }
    }
    async fn local_request(&self, request: Value) -> Result<Value, String> {
        let client = HerdrClient::new(HerdrClientConfig::default())
            .with_ssh_bin(self.config.ssh_bin.clone());
        self.companion()
            .request(&client, &EndpointTarget::local(None), request)
            .await
    }
    async fn endpoint_request(
        &self,
        herdr: &HerdrClient,
        target: &EndpointTarget,
        request: Value,
    ) -> Result<Value, String> {
        self.companion()
            .request(
                &herdr.with_ssh_bin(self.config.ssh_bin.clone()),
                target,
                request,
            )
            .await
    }
    pub async fn verify_at(
        &self,
        herdr: &HerdrClient,
        target: &EndpointTarget,
        id: &str,
    ) -> Result<(), String> {
        let choice = self.choice(target.id(), Some(id))?;
        if let Some(deployment) = choice.deployment {
            let value = self.endpoint_request(herdr, target, json!({"operation":"verify", "home":deployment.home, "deployment_id":deployment.deployment_id})).await?;
            if value["bundle_digest"] != deployment.bundle_digest {
                return Err("Prepared environment revision mismatch".into());
            }
        }
        Ok(())
    }
    pub async fn verify_resume_at(
        &self,
        herdr: &HerdrClient,
        target: &EndpointTarget,
        execution: &taskr_core::orchestration::TaskExecution,
    ) -> Result<(), String> {
        self.verify_at(herdr, target, &execution.launch_profile_id)
            .await?;
        if let Some(deployment) = self
            .choice(target.id(), Some(&execution.launch_profile_id))?
            .deployment
        {
            let kind = execution
                .agent_kind
                .as_deref()
                .ok_or("saved execution has no agent kind")?;
            let session = execution
                .agent_session
                .as_deref()
                .ok_or("saved execution has no native session ID")?;
            self.companion()
                .verify_history(
                    &herdr.with_ssh_bin(self.config.ssh_bin.clone()),
                    target,
                    &ResumeRequirement {
                        home: deployment.home,
                        kind: kind.into(),
                        session: session.into(),
                        workspace_path: execution.workspace_path.clone(),
                        environment: execution.launch_env.clone(),
                    },
                )
                .await?;
        }
        Ok(())
    }
}
use rusqlite::OptionalExtension;

fn validate_sync(request: &SyncRequest) -> Result<(), String> {
    if request.source_environment_id.is_empty()
        || request.source_revision.is_empty()
        || request.endpoint_id.trim().is_empty()
    {
        return Err("source_environment_id, source_revision, and endpoint_id are required".into());
    }
    if !matches!(request.credential_policy.as_str(), "endpoint" | "copy") {
        return Err("credential_policy must be endpoint or copy".into());
    }
    if request.credential_policy == "endpoint"
        && request
            .endpoint_auth_home
            .as_deref()
            .is_none_or(|p| p.trim().is_empty())
    {
        return Err("endpoint credential policy requires endpoint_auth_home".into());
    }
    for path in [
        request.endpoint_auth_home.as_deref(),
        request.deployment_root.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !(path.starts_with('/') || path == "~" || path.starts_with("~/"))
            || path.chars().any(char::is_control)
        {
            return Err(
                "endpoint paths must be absolute or ~/ paths without control characters".into(),
            );
        }
    }
    Ok(())
}

fn launch_choices(endpoint: &str, deployment: &PreparedEnvironment) -> Vec<LaunchChoice> {
    std::iter::once(None)
        .chain(deployment.native_profiles.iter().cloned().map(Some))
        .enumerate()
        .map(|(index, native)| {
            let mut args = if deployment.kind == "codex" {
                vec!["--no-daemon".into(), "--no-alt-screen".into()]
            } else {
                Vec::new()
            };
            if let Some(name) = &native {
                args.extend(["--profile".into(), name.clone()]);
            }
            let mut bypass = args.clone();
            bypass.push(
                if deployment.kind == "codex" {
                    "--dangerously-bypass-approvals-and-sandbox"
                } else {
                    "--dangerously-skip-permissions"
                }
                .into(),
            );
            let variable = if deployment.kind == "codex" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            };
            LaunchChoice {
                endpoint_id: endpoint.into(),
                profile: LaunchProfile {
                    id: format!("{}-p{index}", deployment.deployment_id),
                    agent_kind: deployment.kind.clone(),
                    args,
                    bypass_args: Some(bypass),
                    env: BTreeMap::from([(variable.into(), deployment.home.clone())]),
                    description: Some(format!(
                        "{} / {}",
                        deployment.display_name,
                        native.as_deref().unwrap_or("base")
                    )),
                },
                deployment: Some(deployment.clone()),
                native_profile: native,
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
