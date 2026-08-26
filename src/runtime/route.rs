use std::collections::HashMap;
use std::sync::Arc;

use cdpkit::emulation::methods::{
    ClearDeviceMetricsOverride, ClearGeolocationOverride, SetDeviceMetricsOverride,
    SetGeolocationOverride, SetLocaleOverride, SetTimezoneOverride,
};
use cdpkit::network::methods::{
    Disable as DisableNetwork, Enable as EnableNetwork, SetExtraHttpHeaders, SetUserAgentOverride,
};
use cdpkit::network::types::Headers;
use cdpkit::security::methods::{
    Disable as DisableSecurity, Enable as EnableSecurity, SetIgnoreCertificateErrors,
};
use parking_lot::Mutex;

use super::{
    BrowserError, CleanupFailure, CloseCoordinator, CloseReport, ContextOptions, OperationPhase,
    OwnershipCleanupError, Page, PendingOwnershipGuard, PendingOwnershipRegistry,
    RetainedOwnership,
};

#[derive(Debug, Clone, Copy)]
pub(super) enum RouteConfigurationStep {
    DeviceMetrics,
    Locale,
    Timezone,
    UserAgent,
    Geolocation,
    Network,
    ExtraHttpHeaders,
    Security,
    IgnoreCertificateErrors,
}

pub(super) type AppliedRouteConfiguration = Arc<Mutex<Vec<RouteConfigurationStep>>>;

pub(super) fn applied_configuration() -> AppliedRouteConfiguration {
    Arc::new(Mutex::new(Vec::new()))
}

/// Page-rooted ownership for every route mutation installed by browserkit.
///
/// The retained handles are indexed by flattened CDP session identity while
/// the underlying pending registry owns cancellation-safe cleanup execution and
/// completion reporting. Removing a route only takes the handle under the
/// lock; protocol cleanup is always scheduled after the lock is released.
#[derive(Debug, Clone)]
pub(super) struct RetainedRouteRegistry {
    pending: PendingOwnershipRegistry,
    retained: Arc<Mutex<HashMap<String, RetainedOwnership>>>,
    close: Arc<CloseCoordinator>,
}

impl RetainedRouteRegistry {
    pub(super) fn new() -> Self {
        Self {
            pending: PendingOwnershipRegistry::new(),
            retained: Arc::new(Mutex::new(HashMap::new())),
            close: Arc::new(CloseCoordinator::new()),
        }
    }

    fn register(
        &self,
        route: cdpkit::Session,
        applied: AppliedRouteConfiguration,
        baseline_user_agent: String,
    ) -> RouteConfigurationGuard {
        let session_id = route.id().to_owned();
        let resource = format!("route:{session_id}");
        let pending = self.pending.register(resource, move || async move {
            rollback_route(route, applied, baseline_user_agent).await
        });
        RouteConfigurationGuard {
            registry: self.clone(),
            session_id,
            pending: Some(pending),
        }
    }

    pub(super) async fn cleanup(
        &self,
        session_id: &str,
    ) -> Option<Result<(), OwnershipCleanupError>> {
        let retained = self.retained.lock().remove(session_id);
        match retained {
            Some(retained) => Some(retained.cleanup().await),
            None => None,
        }
    }

    pub(super) fn schedule(&self, session_id: &str) {
        let retained = self.retained.lock().remove(session_id);
        if let Some(retained) = retained {
            retained.schedule();
        }
    }

    #[cfg(test)]
    pub(super) fn schedule_all(&self) {
        let retained = std::mem::take(&mut *self.retained.lock());
        for ownership in retained.into_values() {
            ownership.schedule();
        }
    }

    pub(super) fn finalize_destroyed_route(&self) -> CloseReport {
        self.retained.lock().clear();
        let mut report = CloseReport::new("route configurations");
        for (resource, result) in self.pending.abandon_all() {
            report = match result {
                Ok(()) => report.closed(resource),
                Err(error) if error.is_missing_session() || error.is_missing_target() => {
                    report.closed(resource)
                }
                Err(error) => report.failed(resource, error.to_string()),
            };
        }
        report
    }

    pub(super) async fn cleanup_all(&self) -> CloseReport {
        let registry = self.clone();
        self.close
            .run(async move {
                // Taking retained handles prevents a concurrent detach notification from
                // scheduling the same route while cleanup_all claims the registry entry.
                let retained = std::mem::take(&mut *registry.retained.lock());
                drop(retained);
                let mut report = CloseReport::new("route configurations");
                for (resource, result) in registry.pending.cleanup_all().await {
                    match result {
                        Ok(()) => report = report.closed(resource),
                        Err(error) => report = report.failed(resource, error.to_string()),
                    }
                }
                report
            })
            .await
    }
}

pub(super) struct RouteConfigurationGuard {
    registry: RetainedRouteRegistry,
    session_id: String,
    pending: Option<PendingOwnershipGuard>,
}

impl RouteConfigurationGuard {
    pub(super) fn retain(mut self) {
        let ownership = self
            .pending
            .take()
            .expect("route configuration guard is armed")
            .retain();
        let previous = self
            .registry
            .retained
            .lock()
            .insert(self.session_id.clone(), ownership);
        debug_assert!(previous.is_none(), "route session registered twice");
        if let Some(previous) = previous {
            previous.schedule();
        }
    }

    pub(super) async fn cleanup(mut self) -> Result<(), OwnershipCleanupError> {
        self.pending
            .take()
            .expect("route configuration guard is armed")
            .cleanup()
            .await
    }
}

fn has_main_route_configuration(options: &ContextOptions) -> bool {
    options.target_route_options().viewport_value().is_some()
        || has_every_route_configuration(options)
}

/// Returns whether configuration must be installed on every routed target.
/// Viewport is deliberately excluded because it belongs only to the top-level
/// page target and must never be replayed into OOPIF sessions.
pub(super) fn has_every_route_configuration(options: &ContextOptions) -> bool {
    let route = options.target_route_options();
    route.locale_value().is_some()
        || route.timezone_value().is_some()
        || route.user_agent_override().is_some()
        || route.geolocation_value().is_some()
        || !route.headers().is_empty()
        || options.ignore_https_errors_enabled()
}

/// Worker attachments only support Network-domain route configuration.
/// Emulation, viewport, geolocation, and Security options remain Page/Frame/OOPIF-only.
pub(super) fn has_auxiliary_network_configuration(options: &ContextOptions) -> bool {
    let route = options.target_route_options();
    route.user_agent_override().is_some() || !route.headers().is_empty()
}

pub(super) async fn configure_auxiliary_network_route(
    options: &ContextOptions,
    route: &cdpkit::Session,
) -> Result<(), BrowserError> {
    let route_options = options.target_route_options();
    if let Some(user_agent) = route_options.user_agent_override() {
        let mut command = SetUserAgentOverride::new(user_agent.user_agent().to_owned());
        if let Some(accept_language) = user_agent.accept_language_value() {
            command = command.with_accept_language(accept_language.to_owned());
        }
        if let Some(platform) = user_agent.platform_value() {
            command = command.with_platform(platform.to_owned());
        }
        command
            .send(route)
            .await
            .map_err(|error| route_error("Network.setUserAgentOverride", error))?;
    }
    if !route_options.headers().is_empty() {
        SetExtraHttpHeaders::new(headers(route_options.headers()))
            .send(route)
            .await
            .map_err(|error| route_error("Network.setExtraHTTPHeaders", error))?;
    }
    Ok(())
}

pub(super) struct MainRouteConfiguration {
    options: ContextOptions,
    route: cdpkit::Session,
    applied: AppliedRouteConfiguration,
}

/// Prepares main-route configuration without awaiting protocol I/O.
///
/// Callers that coordinate broader creation cleanup can take ownership of the
/// returned guard before [`apply_main_route`] reaches its first cancellation
/// point.
pub(super) fn prepare_main_route(
    page: &Page,
) -> Result<Option<(MainRouteConfiguration, RouteConfigurationGuard)>, BrowserError> {
    let options = page.owner_session()?.context_options().clone();
    if !has_main_route_configuration(&options) {
        return Ok(None);
    }
    let applied = applied_configuration();
    let route = page.cdp_session().clone();
    let rollback = page.route_configurations().register(
        route.clone(),
        Arc::clone(&applied),
        page.runtime()
            .capabilities()
            .metadata()
            .user_agent()
            .to_owned(),
    );
    Ok(Some((
        MainRouteConfiguration {
            options,
            route,
            applied,
        },
        rollback,
    )))
}

pub(super) async fn apply_main_route(
    configuration: &MainRouteConfiguration,
) -> Result<(), BrowserError> {
    apply_route(
        &configuration.options,
        &configuration.route,
        &configuration.applied,
        true,
    )
    .await
}

pub(super) async fn configure_main_route(
    page: &Page,
) -> Result<Option<RouteConfigurationGuard>, BrowserError> {
    let Some((configuration, rollback)) = prepare_main_route(page)? else {
        return Ok(None);
    };
    match apply_main_route(&configuration).await {
        Ok(()) => Ok(Some(rollback)),
        Err(error) => Err(rollback_failure(error, rollback, page.target_id()).await),
    }
}

pub(super) async fn configure_oopif_route(
    page: &Page,
    options: &ContextOptions,
    route: &cdpkit::Session,
    applied: &AppliedRouteConfiguration,
) -> Result<RouteConfigurationGuard, BrowserError> {
    let rollback = page.route_configurations().register(
        route.clone(),
        Arc::clone(applied),
        page.runtime()
            .capabilities()
            .metadata()
            .user_agent()
            .to_owned(),
    );
    match apply_route(options, route, applied, false).await {
        Ok(()) => Ok(rollback),
        Err(error) => Err(rollback_failure(error, rollback, route.id()).await),
    }
}

async fn apply_route(
    options: &ContextOptions,
    route: &cdpkit::Session,
    applied: &Mutex<Vec<RouteConfigurationStep>>,
    include_viewport: bool,
) -> Result<(), BrowserError> {
    let route_options = options.target_route_options();
    if include_viewport {
        if let Some(viewport) = route_options.viewport_value() {
            mark(applied, RouteConfigurationStep::DeviceMetrics);
            SetDeviceMetricsOverride::new(
                viewport.width() as i64,
                viewport.height() as i64,
                viewport.scale_factor(),
                false,
            )
            .send(route)
            .await
            .map_err(|error| route_error("Emulation.setDeviceMetricsOverride", error))?;
        }
    }
    if let Some(locale) = route_options.locale_value() {
        mark(applied, RouteConfigurationStep::Locale);
        SetLocaleOverride::new()
            .with_locale(locale.to_owned())
            .send(route)
            .await
            .map_err(|error| route_error("Emulation.setLocaleOverride", error))?;
    }
    if let Some(timezone) = route_options.timezone_value() {
        mark(applied, RouteConfigurationStep::Timezone);
        SetTimezoneOverride::new(timezone.to_owned())
            .send(route)
            .await
            .map_err(|error| route_error("Emulation.setTimezoneOverride", error))?;
    }
    if let Some(user_agent) = route_options.user_agent_override() {
        mark(applied, RouteConfigurationStep::UserAgent);
        let mut command = SetUserAgentOverride::new(user_agent.user_agent().to_owned());
        if let Some(accept_language) = user_agent.accept_language_value() {
            command = command.with_accept_language(accept_language.to_owned());
        }
        if let Some(platform) = user_agent.platform_value() {
            command = command.with_platform(platform.to_owned());
        }
        command
            .send(route)
            .await
            .map_err(|error| route_error("Network.setUserAgentOverride", error))?;
    }
    if let Some(geolocation) = route_options.geolocation_value() {
        mark(applied, RouteConfigurationStep::Geolocation);
        SetGeolocationOverride::new()
            .with_latitude(geolocation.latitude())
            .with_longitude(geolocation.longitude())
            .with_accuracy(geolocation.accuracy_meters())
            .send(route)
            .await
            .map_err(|error| route_error("Emulation.setGeolocationOverride", error))?;
    }
    if !route_options.headers().is_empty() {
        mark(applied, RouteConfigurationStep::Network);
        EnableNetwork::new()
            .send(route)
            .await
            .map_err(|error| route_error("Network.enable", error))?;
        mark(applied, RouteConfigurationStep::ExtraHttpHeaders);
        SetExtraHttpHeaders::new(headers(route_options.headers()))
            .send(route)
            .await
            .map_err(|error| route_error("Network.setExtraHTTPHeaders", error))?;
    }
    if options.ignore_https_errors_enabled() {
        mark(applied, RouteConfigurationStep::Security);
        EnableSecurity::new()
            .send(route)
            .await
            .map_err(|error| route_error("Security.enable", error))?;
        mark(applied, RouteConfigurationStep::IgnoreCertificateErrors);
        SetIgnoreCertificateErrors::new(true)
            .send(route)
            .await
            .map_err(|error| route_error("Security.setIgnoreCertificateErrors", error))?;
    }
    Ok(())
}

fn mark(applied: &Mutex<Vec<RouteConfigurationStep>>, step: RouteConfigurationStep) {
    applied.lock().push(step);
}

fn headers(headers: &super::HttpHeaders) -> Headers {
    serde_json::Value::Object(
        headers
            .iter()
            .map(|(name, value)| (name.to_owned(), serde_json::Value::String(value.to_owned())))
            .collect(),
    )
}

fn route_error(operation: &'static str, error: cdpkit::CdpError) -> BrowserError {
    BrowserError::cdp_operation(operation, OperationPhase::Dispatch, error)
}

async fn rollback_failure(
    error: BrowserError,
    rollback: RouteConfigurationGuard,
    identity: &str,
) -> BrowserError {
    match rollback.cleanup().await {
        Ok(()) => error,
        Err(cleanup_error) => error.with_cleanup_failure(CleanupFailure::new(
            format!("route:{identity}"),
            cleanup_error.to_string(),
        )),
    }
}

pub(super) async fn rollback_route(
    route: cdpkit::Session,
    applied: AppliedRouteConfiguration,
    baseline_user_agent: String,
) -> Result<(), OwnershipCleanupError> {
    let steps = applied.lock().clone();
    let mut failures = Vec::new();
    for step in steps.into_iter().rev() {
        let result = match step {
            RouteConfigurationStep::IgnoreCertificateErrors => {
                SetIgnoreCertificateErrors::new(false).send(&route).await
            }
            RouteConfigurationStep::Security => DisableSecurity::new().send(&route).await,
            RouteConfigurationStep::ExtraHttpHeaders => {
                SetExtraHttpHeaders::new(serde_json::json!({}))
                    .send(&route)
                    .await
            }
            RouteConfigurationStep::Network => DisableNetwork::new().send(&route).await,
            RouteConfigurationStep::Geolocation => {
                ClearGeolocationOverride::new().send(&route).await
            }
            RouteConfigurationStep::UserAgent => {
                SetUserAgentOverride::new(baseline_user_agent.clone())
                    .send(&route)
                    .await
            }
            RouteConfigurationStep::Timezone => {
                SetTimezoneOverride::new(String::new()).send(&route).await
            }
            RouteConfigurationStep::Locale => SetLocaleOverride::new().send(&route).await,
            RouteConfigurationStep::DeviceMetrics => {
                ClearDeviceMetricsOverride::new().send(&route).await
            }
        };
        if let Err(error) = result {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(OwnershipCleanupError::Other(failures.join("; ")))
    }
}
