//! Native process/SSH implementation of the portable ports.
use crate::*;
use std::{future::Future, pin::Pin, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type NativeHerdrClient = HerdrClient<NativeTransport>;

#[derive(Clone, Debug)]
pub struct NativeTransport {
    pub config: HerdrClientConfig,
    pub ssh_bin: std::path::PathBuf,
}
impl HerdrClient<NativeTransport> {
    pub fn new(config: HerdrClientConfig) -> Self {
        Self::with_transport(
            config.clone(),
            NativeTransport {
                config,
                ssh_bin: "ssh".into(),
            },
        )
    }
    pub fn with_ssh_bin(&self, ssh_bin: std::path::PathBuf) -> Self {
        let mut client = self.clone();
        client.transport.ssh_bin = ssh_bin;
        client
    }
}

fn failure(
    target: &EndpointTarget,
    category: RuntimeErrorCategory,
    certainty: DeliveryCertainty,
    detail: &str,
) -> RuntimeError {
    RuntimeError::new(category, "transport", detail)
        .with_endpoint(target.id())
        .with_certainty(certainty)
}
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl NativeTransport {
    async fn unchecked(&self, request: CommandRequest) -> Result<CommandOutput, RuntimeError> {
        let mut command = match (&request.scope, &request.target.kind) {
            (_, TargetKind::Managed { .. }) => {
                return Err(failure(
                    &request.target,
                    RuntimeErrorCategory::UnsupportedCapability,
                    DeliveryCertainty::NotDelivered,
                    "native transport has no container binding",
                ))
            }
            (CommandScope::Herdr, _) => {
                let mut command = tokio::process::Command::new(&request.program);
                command
                    .args(request.target.kind.prefix_args())
                    .args(&request.args);
                command
            }
            (CommandScope::Endpoint, TargetKind::Machine { .. }) => {
                let route = request.target.ssh_target.as_deref().ok_or_else(|| {
                    failure(
                        &request.target,
                        RuntimeErrorCategory::InvalidLaunchConfiguration,
                        DeliveryCertainty::NotDelivered,
                        "companion requires a provider-resolved endpoint",
                    )
                })?;
                let mut command = tokio::process::Command::new(&self.ssh_bin);
                command.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=15",
                    "--",
                    route,
                ]);
                // SSH executes via a remote shell. Quote only the command vector;
                // bundles/credentials stay on stdin and never enter argv.
                let mut args = vec![request.program.clone()];
                args.extend(request.args.clone());
                let mut remote = Vec::new();
                if let Some(cwd) = &request.cwd {
                    remote.push(format!("cd {} &&", quote(cwd)));
                }
                if !request.env.is_empty() {
                    remote.push("env".into());
                    for (key, value) in &request.env {
                        if key.is_empty()
                            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        {
                            return Err(failure(
                                &request.target,
                                RuntimeErrorCategory::InvalidLaunchConfiguration,
                                DeliveryCertainty::NotDelivered,
                                "invalid environment key",
                            ));
                        }
                        remote.push(quote(&format!("{key}={value}")));
                    }
                }
                remote.extend(args.iter().map(|arg| quote(arg)));
                command.arg(remote.join(" "));
                command
            }
            (CommandScope::Endpoint, TargetKind::Local { .. }) => {
                let mut command = tokio::process::Command::new(&request.program);
                command.args(&request.args);
                if let Some(cwd) = &request.cwd {
                    command.current_dir(cwd);
                }
                command
            }
        };
        command
            .envs(&request.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut attempts = 0;
        let mut child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error) if error.raw_os_error() == Some(26) && attempts < 5 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(_) => {
                    return Err(failure(
                        &request.target,
                        RuntimeErrorCategory::EndpointUnavailable,
                        DeliveryCertainty::NotDelivered,
                        "cannot spawn endpoint command",
                    ))
                }
            }
        };
        let mut input = child.stdin.take().expect("piped stdin");
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let limit = request.output_limit;
        let read = async {
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let n = stdout.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                if bytes.len().saturating_add(n) > limit {
                    return Err(std::io::Error::other("output limit exceeded"));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Ok::<_, std::io::Error>(bytes)
        };
        let drain = async {
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let n = stderr.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                if request.retain_stderr {
                    let remaining = limit.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&chunk[..n.min(remaining)]);
                }
            }
            Ok::<_, std::io::Error>(bytes)
        };
        let write = async {
            input.write_all(&request.stdin).await?;
            input.shutdown().await?;
            drop(input);
            Ok::<_, std::io::Error>(())
        };
        let run = async { tokio::try_join!(child.wait(), write, read, drain) };
        match tokio::time::timeout(request.timeout, run).await {
            Ok(Ok((status, (), stdout, stderr))) => Ok(CommandOutput {
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit_code: status.code().unwrap_or(-1),
            }),
            Ok(Err(_)) => Err(failure(
                &request.target,
                RuntimeErrorCategory::UnknownOutcome,
                DeliveryCertainty::Unknown,
                "endpoint command I/O failed or response exceeded its limit",
            )),
            Err(_) => {
                let _ = child.start_kill();
                Err(failure(
                    &request.target,
                    RuntimeErrorCategory::Timeout,
                    DeliveryCertainty::Unknown,
                    "endpoint command deadline expired; reconcile before retry",
                ))
            }
        }
    }

    fn cli_request(&self, target: EndpointTarget, args: Vec<String>) -> CommandRequest {
        CommandRequest {
            target,
            scope: CommandScope::Herdr,
            program: self.config.bin.to_string_lossy().into_owned(),
            args,
            stdin: Vec::new(),
            env: Default::default(),
            cwd: None,
            timeout: Duration::from_secs(15),
            output_limit: 1024 * 1024,
            retain_stderr: false,
        }
    }

    async fn resolve(&self, mut target: EndpointTarget) -> Result<EndpointTarget, RuntimeError> {
        if let TargetKind::Machine { profile } = &target.kind {
            if target.ssh_target.is_none() {
                let output = self
                    .unchecked(self.cli_request(
                        EndpointTarget::local(self.config.local_session.clone()),
                        vec!["machine".into(), "list".into(), "--json".into()],
                    ))
                    .await?;
                let value =
                    envelope::parse_envelope(&output.stdout, &output.stderr, output.success())
                        .map_err(|_| {
                            failure(
                                &target,
                                RuntimeErrorCategory::InvalidResponse,
                                DeliveryCertainty::NotDelivered,
                                "invalid Herdr machine catalog",
                            )
                        })?;
                let entries = value
                    .as_array()
                    .or_else(|| value.get("machines").and_then(serde_json::Value::as_array))
                    .ok_or_else(|| {
                        failure(
                            &target,
                            RuntimeErrorCategory::InvalidResponse,
                            DeliveryCertainty::NotDelivered,
                            "invalid Herdr machine catalog",
                        )
                    })?;
                let row = entries
                    .iter()
                    .find(|row| row["id"].as_str() == Some(profile))
                    .ok_or_else(|| {
                        failure(
                            &target,
                            RuntimeErrorCategory::MissingTarget,
                            DeliveryCertainty::NotDelivered,
                            "unknown Herdr machine profile",
                        )
                    })?;
                if row["enabled"] == false {
                    return Err(failure(
                        &target,
                        RuntimeErrorCategory::EndpointDisabled,
                        DeliveryCertainty::NotDelivered,
                        "Herdr machine is disabled",
                    ));
                }
                let route = row["target"]
                    .as_str()
                    .filter(|route| {
                        !route.is_empty()
                            && !route.starts_with('-')
                            && !route.chars().any(char::is_control)
                    })
                    .ok_or_else(|| {
                        failure(
                            &target,
                            RuntimeErrorCategory::InvalidLaunchConfiguration,
                            DeliveryCertainty::NotDelivered,
                            "Herdr machine has no valid SSH target",
                        )
                    })?;
                target.ssh_target = Some(route.to_owned());
            }
        }
        Ok(target)
    }

    async fn observe(&self, target: EndpointTarget) -> Result<EndpointState, RuntimeError> {
        let output = self
            .unchecked(self.cli_request(
                target.clone(),
                vec!["status".into(), "server".into(), "--json".into()],
            ))
            .await?;
        let status =
            envelope::parse_envelope(&output.stdout, &output.stderr, output.success()).ok();
        let mut generation = status
            .as_ref()
            .and_then(|value| value.get("runtime_generation"))
            .and_then(serde_json::Value::as_str)
            .filter(|generation| !generation.is_empty())
            .map(str::to_owned);
        if generation.is_none() {
            if let Some(socket) = status.as_ref().and_then(|value| value["socket"].as_str()) {
                match &target.kind {
                    TargetKind::Local { .. } => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            if let Ok(metadata) = std::fs::metadata(socket) {
                                generation = Some(format!(
                                    "socket:{}:{}:{}:{}",
                                    metadata.dev(),
                                    metadata.ino(),
                                    metadata.ctime(),
                                    metadata.ctime_nsec()
                                ));
                            }
                        }
                    }
                    TargetKind::Machine { .. } => {
                        let request = CommandRequest {
                            target: target.clone(), scope: CommandScope::Endpoint, program: "python3".into(),
                            args: vec!["-c".into(), "import os,sys; s=os.stat(sys.argv[1]); print('socket:%s:%s:%s' % (s.st_dev,s.st_ino,s.st_ctime_ns))".into(), socket.into()],
                            stdin: Vec::new(), env: Default::default(), cwd: None, timeout: Duration::from_secs(15), output_limit: 4096, retain_stderr: false,
                        };
                        if let Ok(output) = self.unchecked(request).await {
                            if output.success() {
                                generation = Some(output.stdout.trim().to_owned());
                            }
                        }
                    }
                    TargetKind::Managed { .. } => {}
                }
            }
        }
        // Some old CLIs/fixtures lack status metadata. Probe readiness without
        // inventing an incarnation from protocol version or mutable labels.
        let ready = if status.as_ref().and_then(|v| v["running"].as_bool()) == Some(false) {
            false
        } else {
            let output = self
                .unchecked(self.cli_request(target.clone(), vec!["api".into(), "snapshot".into()]))
                .await?;
            envelope::parse_envelope(&output.stdout, &output.stderr, output.success()).is_ok()
        };
        if let (Some(route), Some(namespace)) = (target.ssh_target.as_deref(), generation.as_mut())
        {
            *namespace = format!("route:{}:{}:{}", route.len(), route, namespace);
        }
        if target.runtime_generation.is_some() && generation.is_none() {
            return Err(failure(
                &target,
                RuntimeErrorCategory::EndpointUnavailable,
                DeliveryCertainty::NotDelivered,
                "cannot observe endpoint resource generation; recorded IDs retained",
            ));
        }
        if target.runtime_generation.is_some() && target.runtime_generation != generation {
            return Err(failure(
                &target,
                RuntimeErrorCategory::GenerationMismatch,
                DeliveryCertainty::NotDelivered,
                "endpoint resource generation changed; old IDs cannot be used",
            )
            .with_observed_generation(generation.clone().expect("observed generation")));
        }
        Ok(EndpointState {
            target: target.fenced(generation.clone()),
            ready,
            generation,
        })
    }
}

impl HerdrTransport for NativeTransport {
    type CommandFuture<'a> =
        Pin<Box<dyn Future<Output = Result<CommandOutput, RuntimeError>> + Send + 'a>>;
    fn execute(&self, mut request: CommandRequest) -> Self::CommandFuture<'_> {
        Box::pin(async move {
            if request.target.runtime_generation.is_some() {
                let target = self.resolve(request.target.clone()).await?;
                let state = self.observe(target).await?;
                if !state.ready {
                    return Err(failure(
                        &request.target,
                        RuntimeErrorCategory::EndpointUnavailable,
                        DeliveryCertainty::NotDelivered,
                        "endpoint is not ready",
                    ));
                }
                request.target = state.target;
            }
            self.unchecked(request).await
        })
    }
}
impl EndpointProvider for NativeTransport {
    type EndpointFuture<'a> =
        Pin<Box<dyn Future<Output = Result<EndpointState, RuntimeError>> + Send + 'a>>;
    fn endpoint(&self, target: EndpointTarget, action: EndpointAction) -> Self::EndpointFuture<'_> {
        Box::pin(async move {
            if matches!(action, EndpointAction::Release { .. }) {
                return Ok(EndpointState {
                    generation: target.runtime_generation.clone(),
                    target,
                    ready: false,
                });
            }
            let target = self.resolve(target).await?;
            let state = self.observe(target).await?;
            if !matches!(action, EndpointAction::Observe) && !state.ready {
                return Err(failure(
                    &state.target,
                    RuntimeErrorCategory::EndpointUnavailable,
                    DeliveryCertainty::NotDelivered,
                    "Herdr must be started/provisioned on the native endpoint",
                ));
            }
            Ok(state)
        })
    }
}
