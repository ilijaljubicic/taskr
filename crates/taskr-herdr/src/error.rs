use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeErrorCategory {
    EndpointUnavailable,
    EndpointDisabled,
    UnsupportedCapability,
    InvalidLaunchConfiguration,
    MissingTarget,
    OccupiedPane,
    AgentNotReady,
    AgentBlocked,
    OccupantMismatch,
    GenerationMismatch,
    Timeout,
    Canceled,
    UnknownOutcome,
    InvalidResponse,
}

impl RuntimeErrorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointUnavailable => "endpoint_unavailable",
            Self::EndpointDisabled => "endpoint_disabled",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::InvalidLaunchConfiguration => "invalid_launch_configuration",
            Self::MissingTarget => "missing_target",
            Self::OccupiedPane => "occupied_pane",
            Self::AgentNotReady => "agent_not_ready",
            Self::AgentBlocked => "agent_blocked",
            Self::OccupantMismatch => "occupant_mismatch",
            Self::GenerationMismatch => "generation_mismatch",
            Self::Timeout => "timeout",
            Self::Canceled => "canceled",
            Self::UnknownOutcome => "unknown_outcome",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

/// Whether a mutating operation is known to have taken effect.
///
/// A transport timeout may mean the operation already happened on the Herdr
/// server; such failures are not automatically retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryCertainty {
    Delivered,
    NotDelivered,
    Unknown,
}

impl DeliveryCertainty {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::NotDelivered => "not_delivered",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub category: RuntimeErrorCategory,
    pub operation: &'static str,
    pub endpoint: Option<String>,
    pub detail: String,
    pub herdr_code: Option<String>,
    pub delivery_certainty: DeliveryCertainty,
    /// Positive namespace observation, when a generation mismatch is reported.
    pub observed_generation: Option<String>,
}

impl RuntimeError {
    pub fn new(
        category: RuntimeErrorCategory,
        operation: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            category,
            operation,
            endpoint: None,
            detail: detail.into(),
            herdr_code: None,
            delivery_certainty: DeliveryCertainty::Unknown,
            observed_generation: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn with_herdr_code(mut self, code: Option<String>) -> Self {
        self.herdr_code = code;
        self
    }

    pub fn with_certainty(mut self, certainty: DeliveryCertainty) -> Self {
        self.delivery_certainty = certainty;
        self
    }

    pub fn with_observed_generation(mut self, generation: String) -> Self {
        self.observed_generation = Some(generation);
        self
    }

    pub fn invalid_launch_config(operation: &'static str, detail: impl Into<String>) -> Self {
        Self::new(
            RuntimeErrorCategory::InvalidLaunchConfiguration,
            operation,
            detail,
        )
        .with_certainty(DeliveryCertainty::NotDelivered)
    }

    pub fn unavailable(operation: &'static str, detail: impl Into<String>) -> Self {
        Self::new(RuntimeErrorCategory::EndpointUnavailable, operation, detail)
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self.category, RuntimeErrorCategory::Timeout)
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.operation, self.category.as_str())?;
        if let Some(endpoint) = &self.endpoint {
            write!(f, " (endpoint {endpoint})")?;
        }
        write!(f, ": {}", self.detail)?;
        if let Some(code) = &self.herdr_code {
            write!(f, " [herdr: {code}]")?;
        }
        if self.delivery_certainty != DeliveryCertainty::Unknown {
            write!(f, " [delivery: {}]", self.delivery_certainty.as_str())?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

/// Map a herdr CLI error code to a runtime error category.
pub fn category_for_herdr_code(code: &str) -> RuntimeErrorCategory {
    match code {
        "agent_not_found"
        | "pane_not_found"
        | "workspace_not_found"
        | "tab_not_found"
        | "machine_not_found"
        | "session_not_found" => RuntimeErrorCategory::MissingTarget,
        "timeout" | "timed_out" => RuntimeErrorCategory::Timeout,
        "agent_blocked" => RuntimeErrorCategory::AgentBlocked,
        "agent_prompt_stalled" | "agent_not_ready" | "agent_starting" => {
            RuntimeErrorCategory::AgentNotReady
        }
        "pane_occupied" | "pane_in_use" | "agent_exists" => RuntimeErrorCategory::OccupiedPane,
        "unsupported" | "not_supported" | "unknown_command" => {
            RuntimeErrorCategory::UnsupportedCapability
        }
        _ => RuntimeErrorCategory::UnknownOutcome,
    }
}
