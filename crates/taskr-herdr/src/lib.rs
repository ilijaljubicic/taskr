//! The single typed TASKR boundary to Herdr.
//!
//! TASKR constructs structured requests; this client turns them into herdr
//! CLI argument vectors (never shell command strings), runs them against the
//! explicitly selected endpoint (local session or saved SSH machine), and
//! parses Herdr's JSON envelopes into typed results and errors. It is not a
//! backend registry: there is exactly one execution implementation behind
//! this boundary.

pub mod containers;
mod endpoints;
mod envelope;
mod error;
mod launch;
#[cfg(feature = "native")]
mod native;
mod ops;
pub mod transport;
#[cfg(feature = "native")]
pub use native::{NativeHerdrClient, NativeTransport};
pub use transport::{
    CommandOutput, CommandRequest, CommandScope, EndpointAction, EndpointProvider, EndpointState,
    HerdrTransport,
};

pub use endpoints::{
    parse_machine_list, parse_machine_status, EndpointInfo, EndpointMode, EndpointTarget,
    TargetKind, LOCAL_ENDPOINT_ID,
};
pub use envelope::HerdrCliError;
pub use error::{category_for_herdr_code, DeliveryCertainty, RuntimeError, RuntimeErrorCategory};
pub use launch::{
    generate_agent_name, home_env_var_for_kind, validate_agent_name, validate_launch_profile,
    LaunchProfile, ResolvedLaunch, AGENT_NAME_PREFIX,
};
pub use ops::{
    AgentInfo, AllocatePaneRequest, AllocatedPane, PaneInfo, PanePlacement, ProcessInfo,
    PromptOutcome, PromptRequest, ReadResult, ReadSource, SplitDirection, TabInfo,
    WorkspaceSummary,
};

use ops::classify_herdr_error;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Client construction settings.
#[derive(Debug, Clone)]
pub struct HerdrClientConfig {
    /// Installed herdr executable; defaults to `herdr` from PATH.
    pub bin: PathBuf,
    /// Explicit local Herdr session selection. Does not affect remote
    /// endpoints, which always use their saved machine profile's session.
    pub local_session: Option<String>,
}

impl Default for HerdrClientConfig {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("herdr"),
            local_session: None,
        }
    }
}

/// How long a herdr command may run before TASKR kills the CLI process.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// The sole TASKR-to-Herdr client, backed by the installed `herdr` CLI for
/// both local and saved-machine (SSH-forwarded) operations.
#[derive(Debug, Clone)]
pub struct HerdrClient<T> {
    config: HerdrClientConfig,
    transport: T,
}

impl<T: HerdrTransport> HerdrClient<T> {
    pub fn with_transport(config: HerdrClientConfig, transport: T) -> Self {
        Self { config, transport }
    }
    pub fn transport(&self) -> &T {
        &self.transport
    }
    pub async fn endpoint(
        &self,
        target: EndpointTarget,
        action: EndpointAction,
    ) -> Result<EndpointState, RuntimeError>
    where
        T: EndpointProvider,
    {
        self.transport.endpoint(target, action).await
    }
    pub async fn execute_endpoint(
        &self,
        request: CommandRequest,
    ) -> Result<CommandOutput, RuntimeError> {
        self.transport.execute(request).await
    }

    pub fn config(&self) -> &HerdrClientConfig {
        &self.config
    }

    /// Resolve an endpoint ID against the client's local session selection.
    /// Remote endpoint IDs are used verbatim as saved machine profiles.
    pub fn target_for_endpoint(&self, endpoint_id: &str) -> EndpointTarget {
        if endpoint_id == LOCAL_ENDPOINT_ID {
            EndpointTarget::local(self.config.local_session.clone())
        } else {
            EndpointTarget::machine(endpoint_id.to_owned(), endpoint_id.to_owned())
        }
    }

    /// Run a JSON-mode herdr command and return the parsed result value.
    pub async fn execute_json(
        &self,
        target: &EndpointTarget,
        args: &[String],
        timeout: Duration,
    ) -> Result<Value, RuntimeError> {
        let output = self.run_command(target, args, timeout).await?;
        envelope::parse_envelope(&output.stdout, &output.stderr, output.success()).map_err(
            |error| {
                classify_herdr_error("herdr_command", target, &error, DeliveryCertainty::Unknown)
            },
        )
    }

    /// Run a text-mode herdr command (reads) and return raw stdout.
    pub async fn execute_text(
        &self,
        target: &EndpointTarget,
        args: &[String],
        timeout: Duration,
    ) -> Result<String, RuntimeError> {
        let output = self.run_command(target, args, timeout).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(cli_error("herdr_command", target, &output))
        }
    }

    /// Discover the endpoint catalog: the local endpoint plus saved remote
    /// machine profiles. Availability is not probed here.
    pub async fn list_endpoints(&self) -> Result<Vec<EndpointInfo>, RuntimeError>
    where
        T: EndpointProvider,
    {
        if let Some(endpoints) = self.transport.managed_endpoints() {
            return Ok(endpoints);
        }
        let local = EndpointInfo {
            endpoint_id: endpoints::LOCAL_ENDPOINT_ID.to_owned(),
            label: Some("Local Herdr server".to_owned()),
            mode: EndpointMode::Local,
            available: None,
            error: None,
        };
        let target = EndpointTarget::local(self.config.local_session.clone());
        let value = self
            .execute_json(
                &target,
                &["machine".into(), "list".into(), "--json".into()],
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        let mut infos = vec![local];
        infos.extend(endpoints::parse_machine_list(&value));
        Ok(infos)
    }

    /// Probe one endpoint's availability without side effects.
    pub async fn check_endpoint(&self, target: &EndpointTarget) -> Result<bool, RuntimeError>
    where
        T: EndpointProvider,
    {
        self.endpoint(target.clone(), EndpointAction::Observe)
            .await
            .map(|state| state.ready)
    }

    /// Allocate a worker pane and return the actual resource IDs.
    pub async fn allocate_pane(
        &self,
        target: &EndpointTarget,
        request: &AllocatePaneRequest,
    ) -> Result<AllocatedPane, RuntimeError> {
        let (args, created_workspace, created_tab) = ops::allocate_args(request);
        let result = self
            .execute_json(target, &args, DEFAULT_COMMAND_TIMEOUT)
            .await
            .map_err(|error| match error.category {
                RuntimeErrorCategory::MissingTarget
                | RuntimeErrorCategory::OccupiedPane
                | RuntimeErrorCategory::InvalidLaunchConfiguration
                | RuntimeErrorCategory::UnsupportedCapability
                | RuntimeErrorCategory::EndpointDisabled => {
                    error.with_certainty(DeliveryCertainty::NotDelivered)
                }
                _ => error,
            })?;
        let pane_id = ops::pane_id_from_result(&result).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCategory::InvalidResponse,
                "allocate_pane",
                format!("herdr allocation result is missing a pane id: {result}"),
            )
            .with_endpoint(target.endpoint_id.clone())
        })?;
        Ok(AllocatedPane {
            workspace_id: ops::workspace_id_from_result(&result),
            tab_id: ops::tab_id_from_result(&result),
            pane_id,
            terminal_id: ops::terminal_id_from_result(&result),
            created_workspace,
            created_tab,
        })
    }

    /// Start a managed agent in the given pane and return the observed
    /// agent placement. A herdr startup timeout is an unknown outcome: the
    /// agent may or may not have started.
    pub async fn start_agent(
        &self,
        target: &EndpointTarget,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
        startup_timeout: Option<Duration>,
    ) -> Result<AgentInfo, RuntimeError> {
        launch::validate_agent_name(name)?;
        let command = ops::agent_start_args(
            name,
            kind,
            pane_id,
            args,
            startup_timeout.or(Some(Duration::from_secs(30))),
        );
        let timeout = startup_timeout.unwrap_or(Duration::from_secs(30)) + Duration::from_secs(10);
        let result = self.execute_json(target, &command, timeout).await?;
        Ok(AgentInfo::parse(&result))
    }

    /// Inspect one agent by name or pane ID.
    pub async fn agent_get(
        &self,
        target: &EndpointTarget,
        agent_ref: &str,
    ) -> Result<AgentInfo, RuntimeError> {
        let result = self
            .execute_json(
                target,
                &["agent".into(), "get".into(), agent_ref.to_owned()],
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        Ok(AgentInfo::parse(&result))
    }

    /// List endpoint workspaces so callers can verify persisted plan-space IDs.
    pub async fn workspace_list(
        &self,
        target: &EndpointTarget,
    ) -> Result<Vec<WorkspaceSummary>, RuntimeError> {
        let result = self
            .execute_json(
                target,
                &["workspace".into(), "list".into()],
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        inventory_rows(&result, "workspaces", target)?
            .iter()
            .map(|row| {
                let workspace = WorkspaceSummary::parse(row);
                if workspace.workspace_id.is_empty() {
                    Err(inventory_error(target, "invalid workspace identity"))
                } else {
                    Ok(workspace)
                }
            })
            .collect()
    }

    /// List recognized live agents on one endpoint. Incomplete inventories fail closed.
    pub async fn agent_list(
        &self,
        target: &EndpointTarget,
    ) -> Result<Vec<AgentInfo>, RuntimeError> {
        let result = self
            .execute_json(
                target,
                &["agent".into(), "list".into()],
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        Ok(inventory_rows(&result, "agents", target)?
            .iter()
            .map(AgentInfo::parse)
            .collect())
    }

    /// List all panes on one endpoint.
    pub async fn pane_list(&self, target: &EndpointTarget) -> Result<Value, RuntimeError> {
        self.execute_json(
            target,
            &["pane".into(), "list".into()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    }

    pub async fn panes(&self, target: &EndpointTarget) -> Result<Vec<PaneInfo>, RuntimeError> {
        let result = self.pane_list(target).await?;
        inventory_rows(&result, "panes", target)?
            .iter()
            .map(|row| {
                PaneInfo::parse(row).ok_or_else(|| inventory_error(target, "invalid pane identity"))
            })
            .collect()
    }
    pub async fn tabs(&self, target: &EndpointTarget) -> Result<Vec<TabInfo>, RuntimeError> {
        let result = self.tab_list(target).await?;
        inventory_rows(&result, "tabs", target)?
            .iter()
            .map(|row| {
                TabInfo::parse(row).ok_or_else(|| inventory_error(target, "invalid tab identity"))
            })
            .collect()
    }
    pub async fn pane(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
    ) -> Result<PaneInfo, RuntimeError> {
        let result = self.pane_get(target, pane_id).await?;
        PaneInfo::parse(&result).ok_or_else(|| inventory_error(target, "invalid pane identity"))
    }
    pub async fn process_info(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
    ) -> Result<ProcessInfo, RuntimeError> {
        Ok(ProcessInfo::parse(
            &self.pane_process_info(target, pane_id).await?,
        ))
    }

    /// Inspect one pane.
    pub async fn pane_get(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
    ) -> Result<Value, RuntimeError> {
        self.execute_json(
            target,
            &["pane".into(), "get".into(), pane_id.to_owned()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    }

    pub async fn tab_list(&self, target: &EndpointTarget) -> Result<Value, RuntimeError> {
        self.execute_json(
            target,
            &["tab".into(), "list".into()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    }

    pub async fn rename_layout(
        &self,
        target: &EndpointTarget,
        area: &str,
        id: &str,
        label: &str,
    ) -> Result<(), RuntimeError> {
        self.execute_json(
            target,
            &[area.into(), "rename".into(), id.into(), label.into()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// Inspect endpoint-owned foreground processes; never inspect host PIDs for
    /// a remote pane on the controller machine.
    pub async fn pane_process_info(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
    ) -> Result<Value, RuntimeError> {
        self.execute_json(
            target,
            &[
                "pane".into(),
                "process-info".into(),
                "--pane".into(),
                pane_id.to_owned(),
            ],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    }

    /// Submit a prompt to an agent. Delivery is acknowledged separately from
    /// work acceptance; a stalled prompt is an unknown delivery outcome.
    pub async fn agent_prompt(
        &self,
        target: &EndpointTarget,
        request: &PromptRequest,
    ) -> Result<PromptOutcome, RuntimeError> {
        let args = ops::prompt_args(request);
        let timeout = request.timeout.unwrap_or_default() + Duration::from_secs(15);
        match self.execute_json(target, &args, timeout).await {
            Ok(result) => Ok(PromptOutcome {
                delivered: true,
                status: ops::string_field(&result, &["status", "agent_status"]),
                raw: result,
            }),
            Err(error) => {
                let certainty = match error.category {
                    RuntimeErrorCategory::AgentBlocked | RuntimeErrorCategory::MissingTarget => {
                        DeliveryCertainty::NotDelivered
                    }
                    RuntimeErrorCategory::AgentNotReady | RuntimeErrorCategory::Timeout => {
                        DeliveryCertainty::Unknown
                    }
                    _ => DeliveryCertainty::Unknown,
                };
                Err(error.with_certainty(certainty))
            }
        }
    }

    /// Read an agent's terminal output as text.
    pub async fn agent_read(
        &self,
        target: &EndpointTarget,
        agent_ref: &str,
        source: ReadSource,
        lines: Option<u32>,
    ) -> Result<ReadResult, RuntimeError> {
        let args = ops::read_args(agent_ref, source, lines);
        let text = self
            .execute_text(target, &args, DEFAULT_COMMAND_TIMEOUT)
            .await?;
        // Bounded read: `--lines` bounds the source; flag long returns so
        // callers can re-read with tighter limits.
        let truncated = lines
            .map(|limit| text.lines().count() >= limit as usize)
            .unwrap_or(false);
        Ok(ReadResult {
            text,
            truncated,
            raw: Value::Null,
        })
    }

    /// Read a pane's raw terminal output as text (agent-independent).
    pub async fn pane_read(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
        lines: Option<u32>,
    ) -> Result<ReadResult, RuntimeError> {
        let mut args = vec!["pane".into(), "read".into(), pane_id.to_owned()];
        if let Some(lines) = lines {
            args.push("--lines".into());
            args.push(lines.to_string());
        }
        let text = self
            .execute_text(target, &args, DEFAULT_COMMAND_TIMEOUT)
            .await?;
        let truncated = lines
            .map(|limit| text.lines().count() >= limit as usize)
            .unwrap_or(false);
        Ok(ReadResult {
            text,
            truncated,
            raw: Value::Null,
        })
    }

    /// Wait until an agent reaches one of the requested lifecycle states.
    /// A timeout here observes only; it never stops the worker.
    pub async fn agent_wait(
        &self,
        target: &EndpointTarget,
        agent_ref: &str,
        until: &[String],
        timeout: Duration,
    ) -> Result<AgentInfo, RuntimeError> {
        let args = ops::wait_args(agent_ref, until, Some(timeout));
        let result = self
            .execute_json(target, &args, timeout + Duration::from_secs(10))
            .await?;
        Ok(AgentInfo::parse(&result))
    }

    /// Send deliberate key presses to an agent (interactive dialogs).
    pub async fn agent_send_keys(
        &self,
        target: &EndpointTarget,
        agent_ref: &str,
        keys: &[String],
    ) -> Result<(), RuntimeError> {
        let mut args = vec!["agent".into(), "send-keys".into(), agent_ref.to_owned()];
        args.extend(keys.iter().map(|key| {
            if key.eq_ignore_ascii_case("escape") || key.eq_ignore_ascii_case("esc") {
                "esc".to_owned()
            } else {
                key.clone()
            }
        }));
        self.execute_json(target, &args, DEFAULT_COMMAND_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// Run a shell command in a pane. Callers must verify the pane belongs
    /// to a live task execution and that its foreground is an available
    /// shell before invoking this.
    pub async fn pane_run(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
        command: &[String],
    ) -> Result<(), RuntimeError> {
        let mut args = vec!["pane".into(), "run".into(), pane_id.to_owned()];
        args.extend(command.iter().cloned());
        self.execute_json(target, &args, DEFAULT_COMMAND_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// Close one pane. Only valid for panes TASKR owns and has selected for
    /// cleanup; never closes a server, workspace, or unrelated pane.
    pub async fn pane_close(
        &self,
        target: &EndpointTarget,
        pane_id: &str,
    ) -> Result<(), RuntimeError> {
        self.execute_json(
            target,
            &["pane".into(), "close".into(), pane_id.to_owned()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
        .map(|_| ())
    }

    /// Full live snapshot of one endpoint for reconciliation.
    pub async fn snapshot(&self, target: &EndpointTarget) -> Result<Value, RuntimeError> {
        self.execute_json(
            target,
            &["api".into(), "snapshot".into()],
            DEFAULT_COMMAND_TIMEOUT,
        )
        .await
    }

    async fn run_command(
        &self,
        target: &EndpointTarget,
        args: &[String],
        timeout: Duration,
    ) -> Result<CommandOutput, RuntimeError> {
        self.transport
            .execute(CommandRequest {
                target: target.clone(),
                scope: CommandScope::Herdr,
                program: self.config.bin.to_string_lossy().into_owned(),
                args: args.to_vec(),
                stdin: Vec::new(),
                env: Default::default(),
                cwd: None,
                timeout,
                output_limit: 16 * 1024 * 1024,
                retain_stderr: true,
            })
            .await
    }
}

fn inventory_error(target: &EndpointTarget, detail: &str) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCategory::InvalidResponse, "inventory", detail)
        .with_endpoint(target.id())
        .with_certainty(DeliveryCertainty::NotDelivered)
}
fn inventory_rows<'a>(
    value: &'a Value,
    key: &str,
    target: &EndpointTarget,
) -> Result<&'a Vec<Value>, RuntimeError> {
    value
        .as_array()
        .or_else(|| value.get(key).and_then(Value::as_array))
        .ok_or_else(|| inventory_error(target, "incomplete resource inventory"))
}

fn cli_error(
    operation: &'static str,
    target: &EndpointTarget,
    output: &CommandOutput,
) -> RuntimeError {
    let detail = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_owned()
    } else {
        output.stderr.trim().to_owned()
    };
    RuntimeError::new(RuntimeErrorCategory::UnknownOutcome, operation, detail)
        .with_endpoint(target.endpoint_id.clone())
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_fake_herdr(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-herdr");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "taskr-herdr-{name}-{}-{unique}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn executes_json_commands_against_selected_local_session() {
        let dir = temp_dir("json");
        let bin = write_fake_herdr(
            &dir,
            r#"echo '{"id":"cli:pane:get","result":{"type":"pane_info","pane":{"pane_id":"w3:p2"}}}'"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: Some("taskr".into()),
        });
        let target = client.target_for_endpoint("local");
        assert_eq!(
            target.kind.prefix_args(),
            vec!["--session".to_owned(), "taskr".to_owned()]
        );
        let value = client
            .execute_json(
                &target,
                &["pane".into(), "get".into(), "w3:p2".into()],
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(value["pane"]["pane_id"], "w3:p2");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn escape_aliases_use_the_canonical_cli_key_without_changing_literal_keys() {
        let dir = temp_dir("escape-keys");
        let bin = write_fake_herdr(
            &dir,
            r#"printf '%s\n' "$@" >"$0.args"; echo '{"result":{}}'"#,
        );
        let args_path = PathBuf::from(format!("{}.args", bin.display()));
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        client
            .agent_send_keys(
                &client.target_for_endpoint("local"),
                "worker",
                &["Escape".into(), "ESC".into(), "escape".into(), "T".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(args_path).unwrap(),
            "agent\nsend-keys\nworker\nesc\nesc\nesc\nT\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn maps_herdr_error_codes_to_runtime_errors() {
        let dir = temp_dir("errors");
        let bin = write_fake_herdr(
            &dir,
            r#"echo '{"error":{"code":"pane_not_found","message":"pane w5:p1 not found"}}'; exit 1"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let error = client
            .execute_json(
                &target,
                &["pane".into(), "get".into(), "w5:p1".into()],
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(error.category, RuntimeErrorCategory::MissingTarget);
        assert_eq!(error.herdr_code.as_deref(), Some("pane_not_found"));
        assert_eq!(error.endpoint.as_deref(), Some("local"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn spawns_failures_are_endpoint_unavailable() {
        let client = HerdrClient::new(HerdrClientConfig {
            bin: PathBuf::from("/nonexistent/herdr-binary"),
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let error = client
            .execute_json(
                &target,
                &["agent".into(), "list".into()],
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(error.category, RuntimeErrorCategory::EndpointUnavailable);
    }

    #[tokio::test]
    async fn parses_workspace_list() {
        let dir = temp_dir("workspace-list");
        let bin = write_fake_herdr(
            &dir,
            r#"echo '{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w3","label":"~","tab_count":1,"pane_count":1},{"workspace_id":"w9","label":"taskr-smoke","tab_count":4,"pane_count":5}]}}'"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let workspaces = client.workspace_list(&target).await.unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].workspace_id, "w3");
        assert_eq!(workspaces[0].label.as_deref(), Some("~"));
        assert_eq!(workspaces[1].label.as_deref(), Some("taskr-smoke"));
        assert_eq!(workspaces[1].tab_count, Some(4));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn parses_allocation_results() {
        let dir = temp_dir("allocate");
        let bin = write_fake_herdr(
            &dir,
            r#"echo '{"id":"cli:workspace:create","result":{"type":"workspace_created","root_pane":{"pane_id":"w5:p1","tab_id":"w5:t1","workspace_id":"w5"},"tab":{"tab_id":"w5:t1"},"workspace":{"workspace_id":"w5"}}}'"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let allocated = client
            .allocate_pane(
                &target,
                &AllocatePaneRequest {
                    cwd: "/workspace/project".into(),
                    env: Default::default(),
                    label: Some("taskr-task-1".into()),
                    workspace_label: None,
                    placement: PanePlacement::NewWorkspace,
                },
            )
            .await
            .unwrap();
        assert_eq!(allocated.pane_id, "w5:p1");
        assert_eq!(allocated.tab_id.as_deref(), Some("w5:t1"));
        assert_eq!(allocated.workspace_id.as_deref(), Some("w5"));
        assert!(allocated.created_workspace);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn start_agent_startup_timeout_is_unknown_outcome() {
        let dir = temp_dir("start-timeout");
        let bin = write_fake_herdr(
            &dir,
            r#"echo '{"error":{"code":"timeout","message":"timed out waiting for agent startup"},"id":"cli:agent:start"}'; exit 1"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let error = client
            .start_agent(
                &target,
                "taskr-worker",
                "pi",
                "w5:p1",
                &[],
                Some(Duration::from_millis(500)),
            )
            .await
            .unwrap_err();
        assert_eq!(error.category, RuntimeErrorCategory::Timeout);
        assert_eq!(error.delivery_certainty, DeliveryCertainty::Unknown);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn command_timeout_kills_process_and_reports_timeout() {
        let dir = temp_dir("kill");
        let bin = write_fake_herdr(&dir, "sleep 30");
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: None,
        });
        let target = client.target_for_endpoint("local");
        let error = client
            .execute_json(
                &target,
                &["agent".into(), "list".into()],
                Duration::from_millis(300),
            )
            .await
            .unwrap_err();
        assert_eq!(error.category, RuntimeErrorCategory::Timeout);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn machine_targets_use_machine_prefix_not_session() {
        let dir = temp_dir("machine");
        let bin = write_fake_herdr(
            &dir,
            r#"for arg in "$@"; do [ "$arg" = "--session" ] && echo BAD && exit 1; done
echo '{"id":"cli:agent:list","result":{"type":"agent_list","agents":[{"name":"taskr-worker","kind":"codex","status":"idle"}]}}'"#,
        );
        let client = HerdrClient::new(HerdrClientConfig {
            bin,
            local_session: Some("taskr".into()),
        });
        let target = client.target_for_endpoint("workbox");
        assert_eq!(
            target.kind.prefix_args(),
            vec!["--machine".to_owned(), "workbox".to_owned()]
        );
        let agents = client.agent_list(&target).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("taskr-worker"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
