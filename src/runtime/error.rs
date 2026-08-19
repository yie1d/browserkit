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

#[derive(Debug)]
enum BrowserErrorSource {
    None,
    Cdp(Box<cdpkit::CdpError>),
    Io(std::io::Error),
}

#[derive(Debug)]
pub struct BrowserError {
    message: String,
    operation: Option<String>,
    phase: OperationPhase,
    action_completed: ActionCompletion,
    cleanup_failures: Vec<CleanupFailure>,
    source: BrowserErrorSource,
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
            source: BrowserErrorSource::None,
        }
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
}
