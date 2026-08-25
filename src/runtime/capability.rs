use super::BrowserOwnership;

/// A browser-runtime feature whose availability can be queried synchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Capability {
    RequestRouting,
    PermissionOverrides,
    IgnoreHttpsErrors,
    Proxy,
    DownloadObservation,
    ManagedDownloadPath,
    Pdf,
    PartitionedCookies,
}

impl Capability {
    const ALL: [Self; 8] = [
        Self::RequestRouting,
        Self::PermissionOverrides,
        Self::IgnoreHttpsErrors,
        Self::Proxy,
        Self::DownloadObservation,
        Self::ManagedDownloadPath,
        Self::Pdf,
        Self::PartitionedCookies,
    ];

    const fn index(self) -> usize {
        match self {
            Self::RequestRouting => 0,
            Self::PermissionOverrides => 1,
            Self::IgnoreHttpsErrors => 2,
            Self::Proxy => 3,
            Self::DownloadObservation => 4,
            Self::ManagedDownloadPath => 5,
            Self::Pdf => 6,
            Self::PartitionedCookies => 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityAvailability {
    Available,
    Conditional,
    Unavailable,
}

/// Structured explanation for conditional or unavailable capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapabilityReason {
    AttachedDefaultContextIsImmutable,
    RequiresBrowserLaunchConfiguration,
    IsolatedContextRequired,
    BrowserHeadlessModeUnknown,
    HeadlessBrowserRequired,
    BrowserVersionDependent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityScope {
    DefaultContext,
    IsolatedContext,
    BrowserLaunch,
    BrowserContextCreation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityStatus {
    capability: Capability,
    availability: CapabilityAvailability,
    reason: Option<CapabilityReason>,
    scope: CapabilityScope,
}

impl CapabilityStatus {
    const fn available(capability: Capability, scope: CapabilityScope) -> Self {
        Self {
            capability,
            availability: CapabilityAvailability::Available,
            reason: None,
            scope,
        }
    }

    const fn conditional(
        capability: Capability,
        reason: CapabilityReason,
        scope: CapabilityScope,
    ) -> Self {
        Self {
            capability,
            availability: CapabilityAvailability::Conditional,
            reason: Some(reason),
            scope,
        }
    }

    const fn unavailable(
        capability: Capability,
        reason: CapabilityReason,
        scope: CapabilityScope,
    ) -> Self {
        Self {
            capability,
            availability: CapabilityAvailability::Unavailable,
            reason: Some(reason),
            scope,
        }
    }

    pub fn capability(&self) -> Capability {
        self.capability
    }

    pub fn availability(&self) -> CapabilityAvailability {
        self.availability
    }

    pub fn reason(&self) -> Option<CapabilityReason> {
        self.reason
    }

    /// Returns where this capability is configured or exercised.
    pub fn scope(&self) -> CapabilityScope {
        self.scope
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilitySet {
    scope: CapabilityScope,
    statuses: [CapabilityStatus; 8],
}

impl CapabilitySet {
    fn new(scope: CapabilityScope) -> Self {
        Self {
            scope,
            statuses: Capability::ALL
                .map(|capability| CapabilityStatus::available(capability, scope)),
        }
    }

    fn set(&mut self, status: CapabilityStatus) {
        self.statuses[status.capability.index()] = status;
    }

    pub fn scope(&self) -> CapabilityScope {
        self.scope
    }

    pub fn status(&self, capability: Capability) -> &CapabilityStatus {
        &self.statuses[capability.index()]
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CapabilityStatus> {
        self.statuses.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionKnowledge {
    Known {
        major: u32,
        minor: u32,
        build: u32,
        patch: u32,
    },
    Unknown,
}

impl VersionKnowledge {
    fn parse_product(product: &str) -> Self {
        let Some((_, version)) = product.rsplit_once('/') else {
            return Self::Unknown;
        };
        let mut components = version.split('.');
        let parsed = (
            components.next().and_then(|value| value.parse().ok()),
            components.next().and_then(|value| value.parse().ok()),
            components.next().and_then(|value| value.parse().ok()),
            components.next().and_then(|value| value.parse().ok()),
        );
        if components.next().is_some() {
            return Self::Unknown;
        }
        match parsed {
            (Some(major), Some(minor), Some(build), Some(patch)) => Self::Known {
                major,
                minor,
                build,
                patch,
            },
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeadlessMode {
    Headless,
    Headed,
    Unknown,
}

/// Immutable metadata captured with one `Browser.getVersion` call at startup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserMetadata {
    protocol_version: String,
    product: String,
    revision: String,
    user_agent: String,
    js_version: String,
    version: VersionKnowledge,
    headless_mode: HeadlessMode,
}

impl BrowserMetadata {
    pub(crate) fn new(
        protocol_version: String,
        product: String,
        revision: String,
        user_agent: String,
        js_version: String,
        headless_mode: HeadlessMode,
    ) -> Self {
        let version = VersionKnowledge::parse_product(&product);
        Self {
            protocol_version,
            product,
            revision,
            user_agent,
            js_version,
            version,
            headless_mode,
        }
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    pub fn product(&self) -> &str {
        &self.product
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn js_version(&self) -> &str {
        &self.js_version
    }

    pub fn version(&self) -> VersionKnowledge {
        self.version
    }

    pub fn headless_mode(&self) -> HeadlessMode {
        self.headless_mode
    }
}

/// Immutable capability snapshot for the runtime's default and isolated contexts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeCapabilities {
    metadata: BrowserMetadata,
    default_context: CapabilitySet,
    isolated_context: CapabilitySet,
}

impl RuntimeCapabilities {
    pub(crate) fn derive(
        metadata: BrowserMetadata,
        ownership: BrowserOwnership,
        launch_proxy_configured: bool,
    ) -> Self {
        let default_context = capability_set(
            CapabilityScope::DefaultContext,
            ownership,
            metadata.headless_mode,
            launch_proxy_configured,
        );
        let isolated_context = capability_set(
            CapabilityScope::IsolatedContext,
            ownership,
            metadata.headless_mode,
            launch_proxy_configured,
        );
        Self {
            metadata,
            default_context,
            isolated_context,
        }
    }

    pub fn metadata(&self) -> &BrowserMetadata {
        &self.metadata
    }

    pub fn for_scope(&self, scope: CapabilityScope) -> &CapabilitySet {
        match scope {
            CapabilityScope::DefaultContext | CapabilityScope::BrowserLaunch => {
                &self.default_context
            }
            CapabilityScope::IsolatedContext | CapabilityScope::BrowserContextCreation => {
                &self.isolated_context
            }
        }
    }

    pub fn status(&self, scope: CapabilityScope, capability: Capability) -> &CapabilityStatus {
        self.for_scope(scope).status(capability)
    }
}

fn capability_set(
    scope: CapabilityScope,
    ownership: BrowserOwnership,
    headless_mode: HeadlessMode,
    launch_proxy_configured: bool,
) -> CapabilitySet {
    let mut set = CapabilitySet::new(scope);
    if scope == CapabilityScope::DefaultContext && ownership == BrowserOwnership::Attached {
        for capability in [
            Capability::RequestRouting,
            Capability::PermissionOverrides,
            Capability::IgnoreHttpsErrors,
        ] {
            set.set(CapabilityStatus::unavailable(
                capability,
                CapabilityReason::AttachedDefaultContextIsImmutable,
                scope,
            ));
        }
    }

    if scope == CapabilityScope::DefaultContext {
        let proxy = match (ownership, launch_proxy_configured) {
            (BrowserOwnership::Attached, _) => CapabilityStatus::unavailable(
                Capability::Proxy,
                CapabilityReason::RequiresBrowserLaunchConfiguration,
                CapabilityScope::BrowserLaunch,
            ),
            (BrowserOwnership::Launched, true) => {
                CapabilityStatus::available(Capability::Proxy, CapabilityScope::BrowserLaunch)
            }
            (BrowserOwnership::Launched, false) => CapabilityStatus::conditional(
                Capability::Proxy,
                CapabilityReason::RequiresBrowserLaunchConfiguration,
                CapabilityScope::BrowserLaunch,
            ),
        };
        set.set(proxy);
        set.set(CapabilityStatus::unavailable(
            Capability::ManagedDownloadPath,
            CapabilityReason::IsolatedContextRequired,
            scope,
        ));
    } else {
        set.set(CapabilityStatus::available(
            Capability::Proxy,
            CapabilityScope::BrowserContextCreation,
        ));
    }

    let pdf = match (ownership, headless_mode) {
        (BrowserOwnership::Attached, _) => CapabilityStatus::conditional(
            Capability::Pdf,
            CapabilityReason::BrowserHeadlessModeUnknown,
            scope,
        ),
        (BrowserOwnership::Launched, HeadlessMode::Headless) => {
            CapabilityStatus::available(Capability::Pdf, scope)
        }
        (BrowserOwnership::Launched, HeadlessMode::Headed | HeadlessMode::Unknown) => {
            CapabilityStatus::unavailable(
                Capability::Pdf,
                CapabilityReason::HeadlessBrowserRequired,
                scope,
            )
        }
    };
    set.set(pdf);
    set.set(CapabilityStatus::conditional(
        Capability::PartitionedCookies,
        CapabilityReason::BrowserVersionDependent,
        scope,
    ));
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(headless_mode: HeadlessMode) -> BrowserMetadata {
        BrowserMetadata::new(
            "1.3".into(),
            "Chrome/123.0.6312.86".into(),
            "@revision".into(),
            "test".into(),
            "12.3".into(),
            headless_mode,
        )
    }

    fn availability(
        capabilities: &RuntimeCapabilities,
        scope: CapabilityScope,
        capability: Capability,
    ) -> CapabilityAvailability {
        capabilities.status(scope, capability).availability()
    }

    #[test]
    fn pure_matrix_covers_attached_and_launched_default_and_isolated_scopes() {
        let attached = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Unknown),
            BrowserOwnership::Attached,
            false,
        );
        for capability in [
            Capability::RequestRouting,
            Capability::PermissionOverrides,
            Capability::IgnoreHttpsErrors,
            Capability::Proxy,
        ] {
            assert_eq!(
                availability(&attached, CapabilityScope::DefaultContext, capability),
                CapabilityAvailability::Unavailable
            );
            assert_eq!(
                availability(&attached, CapabilityScope::IsolatedContext, capability),
                CapabilityAvailability::Available
            );
        }
        for scope in [
            CapabilityScope::DefaultContext,
            CapabilityScope::IsolatedContext,
        ] {
            assert_eq!(
                availability(&attached, scope, Capability::DownloadObservation),
                CapabilityAvailability::Available
            );
            assert_eq!(
                availability(&attached, scope, Capability::Pdf),
                CapabilityAvailability::Conditional
            );
        }
        assert_eq!(
            availability(
                &attached,
                CapabilityScope::DefaultContext,
                Capability::ManagedDownloadPath
            ),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            availability(
                &attached,
                CapabilityScope::IsolatedContext,
                Capability::ManagedDownloadPath
            ),
            CapabilityAvailability::Available
        );

        let launched_headless = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Headless),
            BrowserOwnership::Launched,
            true,
        );
        let launched_headed = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Headed),
            BrowserOwnership::Launched,
            false,
        );
        for scope in [
            CapabilityScope::DefaultContext,
            CapabilityScope::IsolatedContext,
        ] {
            for capability in [
                Capability::RequestRouting,
                Capability::PermissionOverrides,
                Capability::IgnoreHttpsErrors,
            ] {
                assert_eq!(
                    availability(&launched_headless, scope, capability),
                    CapabilityAvailability::Available
                );
            }
            assert_eq!(
                availability(&launched_headless, scope, Capability::Pdf),
                CapabilityAvailability::Available
            );
            assert_eq!(
                availability(&launched_headed, scope, Capability::Pdf),
                CapabilityAvailability::Unavailable
            );
        }
        assert_eq!(
            availability(
                &launched_headless,
                CapabilityScope::DefaultContext,
                Capability::Proxy
            ),
            CapabilityAvailability::Available
        );
        assert_eq!(
            availability(
                &launched_headed,
                CapabilityScope::DefaultContext,
                Capability::Proxy
            ),
            CapabilityAvailability::Conditional
        );
        for capabilities in [&launched_headless, &launched_headed] {
            assert_eq!(
                availability(
                    capabilities,
                    CapabilityScope::IsolatedContext,
                    Capability::Proxy
                ),
                CapabilityAvailability::Available
            );
        }
        for scope in [
            CapabilityScope::DefaultContext,
            CapabilityScope::IsolatedContext,
        ] {
            assert_eq!(
                availability(&launched_headless, scope, Capability::PartitionedCookies),
                CapabilityAvailability::Conditional
            );
        }
    }

    #[test]
    fn proxy_capability_reflects_configuration_owner_and_launch_truth() {
        let attached = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Unknown),
            BrowserOwnership::Attached,
            false,
        );
        let attached_default = attached.status(CapabilityScope::DefaultContext, Capability::Proxy);
        assert_eq!(
            attached_default.availability(),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            attached_default.reason(),
            Some(CapabilityReason::RequiresBrowserLaunchConfiguration)
        );
        assert_eq!(attached_default.scope(), CapabilityScope::BrowserLaunch);
        let attached_isolated =
            attached.status(CapabilityScope::IsolatedContext, Capability::Proxy);
        assert_eq!(
            attached_isolated.availability(),
            CapabilityAvailability::Available
        );
        assert_eq!(
            attached_isolated.scope(),
            CapabilityScope::BrowserContextCreation
        );

        let configured = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Headless),
            BrowserOwnership::Launched,
            true,
        );
        let configured_default =
            configured.status(CapabilityScope::DefaultContext, Capability::Proxy);
        assert_eq!(
            configured_default.availability(),
            CapabilityAvailability::Available
        );
        assert_eq!(configured_default.reason(), None);
        assert_eq!(configured_default.scope(), CapabilityScope::BrowserLaunch);

        let unconfigured = RuntimeCapabilities::derive(
            metadata(HeadlessMode::Headless),
            BrowserOwnership::Launched,
            false,
        );
        let unconfigured_default =
            unconfigured.status(CapabilityScope::DefaultContext, Capability::Proxy);
        assert_eq!(
            unconfigured_default.availability(),
            CapabilityAvailability::Conditional
        );
        assert_eq!(
            unconfigured_default.reason(),
            Some(CapabilityReason::RequiresBrowserLaunchConfiguration)
        );
        assert_eq!(unconfigured_default.scope(), CapabilityScope::BrowserLaunch);
        assert_eq!(
            unconfigured
                .status(CapabilityScope::IsolatedContext, Capability::Proxy)
                .scope(),
            CapabilityScope::BrowserContextCreation
        );
    }

    #[test]
    fn download_and_partitioned_cookie_capabilities_match_existing_backends() {
        for ownership in [BrowserOwnership::Attached, BrowserOwnership::Launched] {
            let capabilities = RuntimeCapabilities::derive(
                metadata(if ownership == BrowserOwnership::Attached {
                    HeadlessMode::Unknown
                } else {
                    HeadlessMode::Headless
                }),
                ownership,
                false,
            );
            assert_eq!(
                availability(
                    &capabilities,
                    CapabilityScope::DefaultContext,
                    Capability::DownloadObservation,
                ),
                CapabilityAvailability::Available
            );
            assert_eq!(
                availability(
                    &capabilities,
                    CapabilityScope::DefaultContext,
                    Capability::ManagedDownloadPath,
                ),
                CapabilityAvailability::Unavailable
            );
            for capability in [
                Capability::DownloadObservation,
                Capability::ManagedDownloadPath,
            ] {
                assert_eq!(
                    availability(&capabilities, CapabilityScope::IsolatedContext, capability,),
                    CapabilityAvailability::Available
                );
            }
            for scope in [
                CapabilityScope::DefaultContext,
                CapabilityScope::IsolatedContext,
            ] {
                assert_eq!(
                    availability(&capabilities, scope, Capability::PartitionedCookies),
                    CapabilityAvailability::Conditional
                );
            }
        }
    }

    #[test]
    fn malformed_or_partial_product_versions_are_unknown() {
        assert_eq!(
            VersionKnowledge::parse_product("Chromium/not-a-version"),
            VersionKnowledge::Unknown
        );
        assert_eq!(
            VersionKnowledge::parse_product("Chrome/123.0.1"),
            VersionKnowledge::Unknown
        );
    }
}
