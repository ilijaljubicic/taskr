//! Runtime-neutral command and endpoint ports. Futures deliberately need not be Send.
use std::{collections::BTreeMap, future::Future, time::Duration};

use crate::{EndpointTarget, RuntimeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandScope {
    /// Run the Herdr CLI against the selected server.
    Herdr,
    /// Run a companion on that same execution endpoint, never on the controller.
    Endpoint,
}

#[derive(Clone, Debug)]
pub struct CommandRequest {
    pub target: EndpointTarget,
    pub scope: CommandScope,
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    /// Explicit additions to the host-selected execution identity/environment.
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub timeout: Duration,
    pub output_limit: usize,
    /// Companion diagnostics can contain credentials. Drain without retaining them.
    pub retain_stderr: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

pub trait HerdrTransport {
    type CommandFuture<'a>: Future<Output = Result<CommandOutput, RuntimeError>> + 'a
    where
        Self: 'a;
    /// A deadline/cancellation after dispatch must report Unknown, not NotDelivered.
    /// Implementations must drain bounded output, cancel only this command and
    /// never kill the managed agent or stop a shared endpoint on timeout.
    fn execute(&self, request: CommandRequest) -> Self::CommandFuture<'_>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointAction {
    /// Read-only; must not start a stopped container/server or install software.
    Observe,
    /// May restore/start; returns only after the Herdr API is ready.
    Prepare { lease_id: String },
    /// Keep the existing generation available; must not silently restart it.
    Renew { lease_id: String },
    /// Release just this execution's lease, not another worker's endpoint.
    Release { lease_id: String },
}

#[derive(Clone, Debug)]
pub struct EndpointState {
    pub target: EndpointTarget,
    pub ready: bool,
    /// Resource namespace/incarnation, not Herdr's protocol version.
    pub generation: Option<String>,
}

pub trait EndpointProvider {
    type EndpointFuture<'a>: Future<Output = Result<EndpointState, RuntimeError>> + 'a
    where
        Self: 'a;
    /// Resolve logical IDs and manage lifetime through the host's catalog/bindings.
    /// The returned target must be used for both companions and Herdr commands.
    fn endpoint(&self, target: EndpointTarget, action: EndpointAction) -> Self::EndpointFuture<'_>;
    /// Managed hosts supply their binding catalog; native hosts use Herdr's catalog.
    fn managed_endpoints(&self) -> Option<Vec<crate::EndpointInfo>> {
        None
    }
}
