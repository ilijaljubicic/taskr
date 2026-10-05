use std::net::SocketAddr;
use std::sync::Arc;

use taskr_herdr::NativeHerdrClient as HerdrClient;

use crate::orchestration_actor::OrchestrationHandle;
use crate::{run_mcp_http_server, ControllerPolicy, ResolvedLaunchProfiles};

pub(crate) struct LocalRuntimeConfig {
    pub(crate) bind: SocketAddr,
    pub(crate) herdr: HerdrClient,
    pub(crate) launch_profiles: Arc<ResolvedLaunchProfiles>,
    pub(crate) policy: Arc<ControllerPolicy>,
    pub(crate) mcp_token: Option<Arc<String>>,
    pub(crate) orchestration: OrchestrationHandle,
}

pub(crate) struct LocalRuntime {
    config: LocalRuntimeConfig,
}

impl LocalRuntime {
    pub(crate) fn new(config: LocalRuntimeConfig) -> Self {
        Self { config }
    }

    pub(crate) async fn run(self) -> Result<(), String> {
        run_mcp_http_server(self.config).await
    }
}
