//! Container adapter over host bindings (e.g. Durable Object ctx.container).
//! No SDK, Tokio, SSH, filesystem or Send requirement enters this module.
use crate::*;
use std::{collections::BTreeMap, future::Future, pin::Pin};

#[derive(Clone, Debug)]
pub struct ContainerEndpoint {
    pub binding: String,
    pub session: Option<String>,
    /// Explicit execution identity. Container exec does not inherit image HOME.
    pub user: String,
    pub environment: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ContainerStatus {
    pub running: bool,
    /// True only after the Herdr API responds, not merely after start().
    pub herdr_ready: bool,
    /// Host-issued incarnation; must change when the process namespace is replaced.
    pub generation: String,
}

#[derive(Clone, Debug)]
pub struct ContainerExec {
    pub binding: String,
    /// Bind exec to this exact instance; do not resolve a replacement implicitly.
    pub generation: String,
    pub user: String,
    pub request: CommandRequest,
}

/// A generation is scoped to its binding, including when a logical route changes.
pub fn resource_namespace(binding: &str, incarnation: &str) -> String {
    format!(
        "{}:{binding}:{}:{incarnation}",
        binding.len(),
        incarnation.len()
    )
}

pub trait ContainerBindings {
    type ControlFuture<'a>: Future<Output = Result<ContainerStatus, RuntimeError>> + 'a
    where
        Self: 'a;
    type ExecFuture<'a>: Future<Output = Result<CommandOutput, RuntimeError>> + 'a
    where
        Self: 'a;
    /// Observe is read-only. Prepare restores required filesystem state and
    /// waits for Herdr readiness. Renew/release maintain per-execution leases
    /// durably; alarms reapply inactivity timeouts after DO restart.
    fn control(&self, binding: String, action: EndpointAction) -> Self::ControlFuture<'_>;
    /// Implement with exec(argv, {stdin, env, cwd, user, signal}). Enforce the
    /// deadline/output bounds, clean up abort listeners and return Unknown on
    /// abort after dispatch. Snapshot handles, never process state, are durable.
    fn exec(&self, request: ContainerExec) -> Self::ExecFuture<'_>;
}

#[derive(Clone, Debug)]
pub struct ContainerTransport<B> {
    pub bindings: B,
    pub endpoints: BTreeMap<String, ContainerEndpoint>,
}
impl<B: ContainerBindings> ContainerTransport<B> {
    fn route(&self, target: &EndpointTarget) -> Result<&ContainerEndpoint, RuntimeError> {
        let route = self.endpoints.get(target.id()).ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCategory::MissingTarget,
                "container_endpoint",
                "unknown container endpoint binding",
            )
            .with_endpoint(target.id())
            .with_certainty(DeliveryCertainty::NotDelivered)
        })?;
        if route.user.is_empty()
            || route
                .environment
                .get("HOME")
                .is_none_or(|home| home.is_empty())
        {
            return Err(RuntimeError::invalid_launch_config(
                "container_endpoint",
                "container execution requires an explicit user and HOME",
            ));
        }
        Ok(route)
    }
    async fn state(
        &self,
        target: EndpointTarget,
        action: EndpointAction,
    ) -> Result<EndpointState, RuntimeError> {
        let route = self.route(&target)?;
        let released = matches!(action, EndpointAction::Release { .. });
        let observing = matches!(action, EndpointAction::Observe);
        let status = self.bindings.control(route.binding.clone(), action).await?;
        if status.generation.is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCategory::InvalidResponse,
                "container_endpoint",
                "missing resource generation",
            ));
        }
        let namespace = resource_namespace(&route.binding, &status.generation);
        if !released
            && target
                .runtime_generation
                .as_deref()
                .is_some_and(|expected| expected != namespace)
        {
            return Err(RuntimeError::new(
                RuntimeErrorCategory::GenerationMismatch,
                "container_endpoint",
                "container replaced; stale resource IDs cannot be used",
            )
            .with_endpoint(target.id())
            .with_certainty(DeliveryCertainty::NotDelivered)
            .with_observed_generation(namespace));
        }
        let ready = status.running && status.herdr_ready;
        if !observing && !released && !ready {
            return Err(RuntimeError::unavailable(
                "container_endpoint",
                "container started but Herdr is not ready",
            )
            .with_certainty(DeliveryCertainty::NotDelivered));
        }
        let generation = Some(namespace);
        Ok(EndpointState {
            target: EndpointTarget::managed(
                target.endpoint_id,
                route.binding.clone(),
                route.session.clone(),
            )
            .fenced(generation.clone()),
            ready,
            generation,
        })
    }
}
impl<B: ContainerBindings> EndpointProvider for ContainerTransport<B> {
    type EndpointFuture<'a>
        = Pin<Box<dyn Future<Output = Result<EndpointState, RuntimeError>> + 'a>>
    where
        Self: 'a;
    fn endpoint(&self, target: EndpointTarget, action: EndpointAction) -> Self::EndpointFuture<'_> {
        Box::pin(self.state(target, action))
    }
    fn managed_endpoints(&self) -> Option<Vec<EndpointInfo>> {
        Some(
            self.endpoints
                .keys()
                .map(|id| EndpointInfo {
                    endpoint_id: id.clone(),
                    label: Some(id.clone()),
                    mode: EndpointMode::Remote,
                    available: None,
                    error: None,
                })
                .collect(),
        )
    }
}
impl<B: ContainerBindings> HerdrTransport for ContainerTransport<B> {
    type CommandFuture<'a>
        = Pin<Box<dyn Future<Output = Result<CommandOutput, RuntimeError>> + 'a>>
    where
        Self: 'a;
    fn execute(&self, mut request: CommandRequest) -> Self::CommandFuture<'_> {
        Box::pin(async move {
            let state = self
                .state(request.target.clone(), EndpointAction::Observe)
                .await?;
            if !state.ready {
                return Err(RuntimeError::unavailable(
                    "container_exec",
                    "endpoint is stopped or not ready; prepare before dispatch",
                )
                .with_certainty(DeliveryCertainty::NotDelivered));
            }
            let route = self.route(&state.target)?;
            let mut environment = route.environment.clone();
            environment.extend(request.env);
            request.env = environment;
            if request.cwd.is_none() {
                request.cwd = route.cwd.clone();
            }
            if request.scope == CommandScope::Herdr {
                let mut args = state.target.kind.prefix_args();
                args.extend(request.args);
                request.args = args;
            }
            request.target = state.target;
            let generation = state.generation.expect("container generation checked");
            let limit = request.output_limit;
            let retain_stderr = request.retain_stderr;
            let mut output = self
                .bindings
                .exec(ContainerExec {
                    binding: route.binding.clone(),
                    generation,
                    user: route.user.clone(),
                    request,
                })
                .await?;
            if output.stdout.len() > limit || (retain_stderr && output.stderr.len() > limit) {
                return Err(RuntimeError::new(
                    RuntimeErrorCategory::UnknownOutcome,
                    "container_exec",
                    "response exceeded its limit",
                )
                .with_certainty(DeliveryCertainty::Unknown));
            }
            if !retain_stderr {
                output.stderr.clear();
            }
            Ok(output)
        })
    }
}
