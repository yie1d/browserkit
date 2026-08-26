#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationPhase {
    Preparation,
    Observation,
    Dispatch,
    Confirmation,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCompletion {
    NotStarted,
    Completed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConfigurationFailure {
    InvalidViewport,
    InvalidGeolocation,
    InvalidLocale,
    InvalidTimezone,
    InvalidUserAgent,
    InvalidAcceptLanguage,
    InvalidHeaderName {
        name: String,
    },
    InvalidHeaderValue {
        name: String,
    },
    DuplicateHeaderName {
        name: String,
    },
    InvalidOrigin,
    InvalidProxyServer,
    ProxyUserInfoNotAllowed,
    InvalidProxyBypassEntry,
    ConflictingTypedLaunchArgument,
    UnsupportedCapability {
        capability: super::Capability,
        reason: super::CapabilityReason,
    },
    ImmutableDefaultSessionOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageFailure {
    OpaqueOrigin,
    InvalidOrigin,
    QuotaExceeded,
    AccessDenied,
    InvalidInput,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactFailure {
    InvalidOptions,
    InvalidPath,
    EmptyRegion,
    RegionTooLarge,
    InvalidData,
    TooLarge {
        max_bytes: usize,
        observed_bytes: usize,
    },
    Unsupported,
}

/// A structured failure produced while resolving or checking a locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorFailure {
    NotFound,
    Ambiguous { match_count: usize },
    NotVisible,
    Disabled,
    Unstable,
    Obscured,
    NotEditable,
    NotCheckable,
    NotUncheckable,
    NotSelectable,
    NotFileInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupFailure {
    resource: String,
    message: String,
}

impl CleanupFailure {
    pub fn new(resource: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            message: message.into(),
        }
    }

    pub fn resource(&self) -> &str {
        &self.resource
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFailure {
    frame_id: super::FrameId,
    target_id: String,
    session_id: String,
}

impl RouteFailure {
    pub(crate) fn new(
        frame_id: impl Into<String>,
        target_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            frame_id: super::FrameId::new(frame_id),
            target_id: target_id.into(),
            session_id: session_id.into(),
        }
    }

    pub fn frame_id(&self) -> &super::FrameId {
        &self.frame_id
    }

    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFieldError {
    field: String,
    message: String,
}

impl TargetFieldError {
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }

    pub fn field(&self) -> &str {
        &self.field
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFailure {
    target_id: Option<String>,
    session_id: Option<String>,
    target_type: Option<String>,
    target_url: Option<String>,
    event_error: Option<String>,
    field_errors: Vec<TargetFieldError>,
}

impl TargetFailure {
    pub(crate) fn new(
        target_id: Option<String>,
        session_id: Option<String>,
        target_type: Option<String>,
    ) -> Self {
        Self {
            target_id,
            session_id,
            target_type,
            target_url: None,
            event_error: None,
            field_errors: Vec::new(),
        }
    }

    pub(crate) fn with_target_url(mut self, target_url: Option<String>) -> Self {
        self.target_url = target_url;
        self
    }

    pub(crate) fn with_event_error(mut self, event_error: impl Into<String>) -> Self {
        self.event_error = Some(event_error.into());
        self
    }

    pub(crate) fn with_field_errors(mut self, field_errors: Vec<TargetFieldError>) -> Self {
        self.field_errors = field_errors;
        self
    }

    pub fn target_id(&self) -> Option<&str> {
        self.target_id.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn target_type(&self) -> Option<&str> {
        self.target_type.as_deref()
    }

    pub fn target_url(&self) -> Option<&str> {
        self.target_url.as_deref()
    }

    pub fn event_error(&self) -> Option<&str> {
        self.event_error.as_deref()
    }

    pub fn field_errors(&self) -> &[TargetFieldError] {
        &self.field_errors
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitFailure {
    condition: String,
    scope: String,
    elapsed: std::time::Duration,
    last_observation: Option<String>,
}

impl WaitFailure {
    pub fn new(
        condition: impl Into<String>,
        scope: impl Into<String>,
        elapsed: std::time::Duration,
        last_observation: Option<String>,
    ) -> Self {
        Self {
            condition: condition.into(),
            scope: scope.into(),
            elapsed,
            last_observation,
        }
    }
    pub fn condition(&self) -> &str {
        &self.condition
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn elapsed(&self) -> std::time::Duration {
        self.elapsed
    }
    pub fn last_observation(&self) -> Option<&str> {
        self.last_observation.as_deref()
    }
}

#[derive(Debug)]
enum BrowserErrorSource {
    None,
    Cdp(Box<cdpkit::CdpError>),
    Io(std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrowserErrorDetails {
    Capability(super::CapabilityStatus),
    Configuration(ConfigurationFailure),
    Locator(LocatorFailure),
    Wait(WaitFailure),
    JavaScript(super::JavaScriptException),
    Storage(StorageFailure),
    Artifact(ArtifactFailure),
}

#[derive(Debug)]
pub struct BrowserError {
    message: String,
    operation: Option<String>,
    phase: OperationPhase,
    action_completed: ActionCompletion,
    cleanup_failures: Vec<CleanupFailure>,
    details: Option<Box<BrowserErrorDetails>>,
    route_failure: Option<Box<RouteFailure>>,
    target_failure: Option<Box<TargetFailure>>,
    source: BrowserErrorSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserErrorSnapshot {
    message: String,
    operation: Option<String>,
    phase: OperationPhase,
    action_completed: ActionCompletion,
    cleanup_failures: Vec<CleanupFailure>,
    details: Option<Box<BrowserErrorDetails>>,
    route_failure: Option<RouteFailure>,
    target_failure: Option<TargetFailure>,
}

impl BrowserErrorSnapshot {
    pub(crate) fn capture(error: &BrowserError) -> Self {
        Self {
            message: error.message.clone(),
            operation: error.operation.clone(),
            phase: error.phase,
            action_completed: error.action_completed,
            cleanup_failures: error.cleanup_failures.clone(),
            details: error.details.clone(),
            route_failure: error.route_failure.as_deref().cloned(),
            target_failure: error.target_failure.as_deref().cloned(),
        }
    }

    pub(crate) fn restore(&self) -> BrowserError {
        BrowserError {
            message: self.message.clone(),
            operation: self.operation.clone(),
            phase: self.phase,
            action_completed: self.action_completed,
            cleanup_failures: self.cleanup_failures.clone(),
            details: self.details.clone(),
            route_failure: self.route_failure.clone().map(Box::new),
            target_failure: self.target_failure.clone().map(Box::new),
            source: BrowserErrorSource::None,
        }
    }
}

impl BrowserError {
    pub fn operation(operation: impl Into<String>, phase: OperationPhase) -> Self {
        let operation = operation.into();
        Self {
            message: format!("browser operation '{operation}' failed during {phase:?}"),
            operation: Some(operation),
            phase,
            action_completed: ActionCompletion::NotStarted,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::None,
        }
    }

    pub fn configuration(operation: impl Into<String>, failure: ConfigurationFailure) -> Self {
        Self::operation(operation, OperationPhase::Preparation).with_configuration_failure(failure)
    }

    pub(crate) fn unsupported_capability(
        operation: impl Into<String>,
        status: super::CapabilityStatus,
    ) -> Self {
        let capability = status.capability();
        Self::operation(operation, OperationPhase::Preparation)
            .with_message(format!(
                "capability {capability:?} is unavailable for this session"
            ))
            .with_capability_status(status)
    }

    fn with_capability_status(mut self, status: super::CapabilityStatus) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Capability(status)));
        self
    }

    fn with_configuration_failure(mut self, failure: ConfigurationFailure) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Configuration(failure)));
        self
    }

    pub fn with_action_completion(mut self, completion: ActionCompletion) -> Self {
        self.action_completed = completion;
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn with_cleanup_failure(mut self, failure: CleanupFailure) -> Self {
        self.cleanup_failures.push(failure);
        self
    }

    pub(crate) fn with_route_failure(mut self, failure: RouteFailure) -> Self {
        if self.route_failure.is_none() {
            self.route_failure = Some(Box::new(failure));
        }
        self
    }

    pub(crate) fn with_target_failure(mut self, failure: TargetFailure) -> Self {
        if self.target_failure.is_none() {
            self.target_failure = Some(Box::new(failure));
        }
        self
    }

    pub(crate) fn stable_snapshot(&self) -> BrowserErrorSnapshot {
        BrowserErrorSnapshot::capture(self)
    }

    pub(crate) fn cleanup_failures_owned(&self) -> Vec<CleanupFailure> {
        self.cleanup_failures.clone()
    }

    pub(crate) fn with_wait_failure(mut self, failure: WaitFailure) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Wait(failure)));
        self
    }

    pub(crate) fn with_javascript_exception(
        mut self,
        exception: super::JavaScriptException,
    ) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::JavaScript(exception)));
        self
    }

    pub(crate) fn with_storage_failure(mut self, failure: StorageFailure) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Storage(failure)));
        self
    }

    pub(crate) fn with_artifact_failure(mut self, failure: ArtifactFailure) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Artifact(failure)));
        self
    }

    pub(crate) fn io_operation(
        operation: impl Into<String>,
        phase: OperationPhase,
        error: std::io::Error,
    ) -> Self {
        let operation = operation.into();
        Self {
            message: format!("browser operation '{operation}' failed during {phase:?}: {error}"),
            operation: Some(operation),
            phase,
            action_completed: ActionCompletion::NotStarted,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::Io(error),
        }
    }

    #[allow(dead_code)] // Used by Task 2 resolution before Task 4 wires actions to it.
    pub(crate) fn with_cleanup_failures_from(mut self, other: &Self) -> Self {
        self.cleanup_failures
            .extend(other.cleanup_failures.iter().cloned());
        self
    }

    pub(crate) fn cdp_operation(
        operation: impl Into<String>,
        phase: OperationPhase,
        error: cdpkit::CdpError,
    ) -> Self {
        let operation = operation.into();
        Self {
            message: format!("browser operation '{operation}' failed during {phase:?}: {error}"),
            operation: Some(operation),
            phase,
            action_completed: ActionCompletion::NotStarted,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::Cdp(Box::new(error)),
        }
    }

    pub(crate) fn sensitive_cdp_operation(
        operation: impl Into<String>,
        phase: OperationPhase,
        completion: ActionCompletion,
        error: &cdpkit::CdpError,
    ) -> Self {
        let operation = operation.into();
        let category = match error {
            cdpkit::CdpError::Protocol { code, .. } => format!("protocol error {code}"),
            cdpkit::CdpError::ConnectionClosed => "connection closed".to_owned(),
            cdpkit::CdpError::ChannelClosed => "response channel closed".to_owned(),
            cdpkit::CdpError::Timeout => "command timed out".to_owned(),
            _ => "redacted CDP failure".to_owned(),
        };
        Self {
            message: format!("browser operation '{operation}' failed during {phase:?}: {category}"),
            operation: Some(operation),
            phase,
            action_completed: completion,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::None,
        }
    }

    // Consumed by the internal Task 2 resolver before Task 4 exposes actions.
    #[allow(dead_code)]
    pub(crate) fn with_locator_failure(mut self, failure: LocatorFailure) -> Self {
        self.details = Some(Box::new(BrowserErrorDetails::Locator(failure)));
        self
    }

    pub fn operation_name(&self) -> Option<&str> {
        self.operation.as_deref()
    }

    pub fn phase(&self) -> OperationPhase {
        self.phase
    }

    pub fn action_completed(&self) -> ActionCompletion {
        self.action_completed
    }

    pub fn outcome_unknown(&self) -> bool {
        self.action_completed == ActionCompletion::Unknown
    }

    pub fn cleanup_failures(&self) -> &[CleanupFailure] {
        &self.cleanup_failures
    }

    pub fn route_failure(&self) -> Option<&RouteFailure> {
        self.route_failure.as_deref()
    }

    pub fn target_failure(&self) -> Option<&TargetFailure> {
        self.target_failure.as_deref()
    }

    pub fn details(&self) -> Option<&BrowserErrorDetails> {
        self.details.as_deref()
    }

    pub fn configuration_failure(&self) -> Option<&ConfigurationFailure> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Configuration(failure)) => Some(failure),
            _ => None,
        }
    }

    pub fn capability_status(&self) -> Option<&super::CapabilityStatus> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Capability(status)) => Some(status),
            _ => None,
        }
    }

    pub fn locator_failure(&self) -> Option<&LocatorFailure> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Locator(failure)) => Some(failure),
            _ => None,
        }
    }

    pub fn wait_failure(&self) -> Option<&WaitFailure> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Wait(failure)) => Some(failure),
            _ => None,
        }
    }

    pub fn javascript_exception(&self) -> Option<&super::JavaScriptException> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::JavaScript(exception)) => Some(exception),
            _ => None,
        }
    }

    pub fn storage_failure(&self) -> Option<&StorageFailure> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Storage(failure)) => Some(failure),
            _ => None,
        }
    }

    pub fn artifact_failure(&self) -> Option<&ArtifactFailure> {
        match self.details.as_deref() {
            Some(BrowserErrorDetails::Artifact(failure)) => Some(failure),
            _ => None,
        }
    }
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BrowserError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.source {
            BrowserErrorSource::None => None,
            BrowserErrorSource::Cdp(error) => Some(error.as_ref()),
            BrowserErrorSource::Io(error) => Some(error),
        }
    }
}

impl From<cdpkit::CdpError> for BrowserError {
    fn from(error: cdpkit::CdpError) -> Self {
        Self {
            message: format!("CDP operation failed: {error}"),
            operation: None,
            phase: OperationPhase::Dispatch,
            action_completed: ActionCompletion::Unknown,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::Cdp(Box::new(error)),
        }
    }
}

impl From<std::io::Error> for BrowserError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: format!("browser runtime I/O failed: {error}"),
            operation: None,
            phase: OperationPhase::Preparation,
            action_completed: ActionCompletion::NotStarted,
            cleanup_failures: Vec::new(),
            details: None,
            route_failure: None,
            target_failure: None,
            source: BrowserErrorSource::Io(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "inspect CloseReport because cleanup may be partial"]
pub struct CloseReport {
    scope: String,
    closed_resources: Vec<String>,
    failures: Vec<CleanupFailure>,
}

impl CloseReport {
    pub fn new(scope: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            closed_resources: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn closed(mut self, resource: impl Into<String>) -> Self {
        self.closed_resources.push(resource.into());
        self
    }

    pub fn failed(mut self, resource: impl Into<String>, message: impl Into<String>) -> Self {
        self.failures.push(CleanupFailure::new(resource, message));
        self
    }

    pub fn merge(mut self, mut other: Self) -> Self {
        self.closed_resources.append(&mut other.closed_resources);
        self.failures.append(&mut other.failures);
        self
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn closed_resources(&self) -> &[String] {
        &self.closed_resources
    }

    pub fn failures(&self) -> &[CleanupFailure] {
        &self.failures
    }

    pub fn attempted_count(&self) -> usize {
        self.closed_resources.len() + self.failures.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_error_preserves_unknown_outcome_and_cleanup_failures() {
        let error = BrowserError::operation("click", OperationPhase::Dispatch)
            .with_action_completion(ActionCompletion::Unknown)
            .with_cleanup_failure(CleanupFailure::new("detach target", "connection closed"));

        assert_eq!(error.operation_name(), Some("click"));
        assert_eq!(error.phase(), OperationPhase::Dispatch);
        assert_eq!(error.action_completed(), ActionCompletion::Unknown);
        assert!(error.outcome_unknown());
        assert_eq!(error.cleanup_failures().len(), 1);
    }

    #[test]
    fn close_report_never_hides_partial_cleanup() {
        let report = CloseReport::new("session")
            .closed("page:one")
            .failed("context:ctx", "Target.disposeBrowserContext failed");

        assert_eq!(report.scope(), "session");
        assert!(!report.is_complete());
        assert_eq!(report.closed_resources(), &["page:one"]);
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.attempted_count(), 2);
    }

    #[test]
    fn close_report_merge_preserves_every_success_and_failure() {
        let report = CloseReport::new("runtime").closed("connection").merge(
            CloseReport::new("session")
                .closed("page:one")
                .failed("page:two", "target disappeared"),
        );

        assert_eq!(report.closed_resources().len(), 2);
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.attempted_count(), 3);
        assert!(!report.is_complete());
    }

    #[test]
    fn wait_timeout_exposes_structured_diagnostics() {
        let error = BrowserError::operation("wait for title", OperationPhase::Confirmation)
            .with_wait_failure(WaitFailure::new(
                "title contains Orders",
                "page:target-1",
                std::time::Duration::from_millis(250),
                Some("title was Loading".to_owned()),
            ));

        let failure = error.wait_failure().expect("wait diagnostics");
        assert_eq!(failure.condition(), "title contains Orders");
        assert_eq!(failure.scope(), "page:target-1");
        assert_eq!(failure.elapsed(), std::time::Duration::from_millis(250));
        assert_eq!(failure.last_observation(), Some("title was Loading"));
    }
}
