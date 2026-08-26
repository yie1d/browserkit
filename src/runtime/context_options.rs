use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hash;

use super::ConfigurationFailure;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CanonicalF64(u64);

impl CanonicalF64 {
    fn finite(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let value = if value == 0.0 { 0.0 } else { value };
        Some(Self(value.to_bits()))
    }

    fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl fmt::Debug for CanonicalF64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Viewport {
    width: u32,
    height: u32,
    device_scale_factor: CanonicalF64,
}

impl Viewport {
    pub fn new(width: u32, height: u32) -> Result<Self, ConfigurationFailure> {
        if width == 0 || height == 0 {
            return Err(ConfigurationFailure::InvalidViewport);
        }
        Ok(Self {
            width,
            height,
            device_scale_factor: CanonicalF64::finite(1.0).expect("one is finite"),
        })
    }

    pub fn device_scale_factor(
        mut self,
        device_scale_factor: f64,
    ) -> Result<Self, ConfigurationFailure> {
        let value = CanonicalF64::finite(device_scale_factor)
            .filter(|value| value.get() > 0.0)
            .ok_or(ConfigurationFailure::InvalidViewport)?;
        self.device_scale_factor = value;
        Ok(self)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn scale_factor(&self) -> f64 {
        self.device_scale_factor.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Geolocation {
    latitude: CanonicalF64,
    longitude: CanonicalF64,
    accuracy: CanonicalF64,
}

impl Geolocation {
    pub fn new(latitude: f64, longitude: f64) -> Result<Self, ConfigurationFailure> {
        let latitude = CanonicalF64::finite(latitude)
            .filter(|value| (-90.0..=90.0).contains(&value.get()))
            .ok_or(ConfigurationFailure::InvalidGeolocation)?;
        let longitude = CanonicalF64::finite(longitude)
            .filter(|value| (-180.0..=180.0).contains(&value.get()))
            .ok_or(ConfigurationFailure::InvalidGeolocation)?;
        Ok(Self {
            latitude,
            longitude,
            accuracy: CanonicalF64::finite(0.0).expect("zero is finite"),
        })
    }

    pub fn accuracy(mut self, accuracy: f64) -> Result<Self, ConfigurationFailure> {
        self.accuracy = CanonicalF64::finite(accuracy)
            .filter(|value| value.get() >= 0.0)
            .ok_or(ConfigurationFailure::InvalidGeolocation)?;
        Ok(self)
    }

    pub fn latitude(&self) -> f64 {
        self.latitude.get()
    }

    pub fn longitude(&self) -> f64 {
        self.longitude.get()
    }

    pub fn accuracy_meters(&self) -> f64 {
        self.accuracy.get()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserAgentOverride {
    user_agent: String,
    accept_language: Option<String>,
    platform: Option<String>,
}

impl UserAgentOverride {
    pub fn new(user_agent: impl Into<String>) -> Result<Self, ConfigurationFailure> {
        let user_agent = user_agent.into();
        validate_nonempty_single_line(&user_agent)
            .map_err(|_| ConfigurationFailure::InvalidUserAgent)?;
        Ok(Self {
            user_agent,
            accept_language: None,
            platform: None,
        })
    }

    /// Sets an ordered, comma-separated list of browser language tags.
    ///
    /// This is not a raw HTTP `Accept-Language` field value: parameters such
    /// as `q=` weights are rejected. ASCII optional whitespace around tags is
    /// removed, and Chrome uses the resulting order to derive its HTTP header
    /// weights and `navigator.language`/`navigator.languages` values.
    pub fn accept_language(
        mut self,
        accept_language: impl Into<String>,
    ) -> Result<Self, ConfigurationFailure> {
        self.accept_language = Some(
            normalize_accept_language(&accept_language.into())
                .ok_or(ConfigurationFailure::InvalidAcceptLanguage)?,
        );
        Ok(self)
    }

    pub fn platform(mut self, platform: impl Into<String>) -> Result<Self, ConfigurationFailure> {
        let platform = platform.into();
        validate_nonempty_single_line(&platform)
            .map_err(|_| ConfigurationFailure::InvalidUserAgent)?;
        self.platform = Some(platform);
        Ok(self)
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Returns the normalized browser language list passed to Chrome.
    ///
    /// This value is not the serialized HTTP `Accept-Language` field.
    pub fn accept_language_value(&self) -> Option<&str> {
        self.accept_language.as_deref()
    }

    pub fn platform_value(&self) -> Option<&str> {
        self.platform.as_deref()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct HttpHeaders {
    values: BTreeMap<String, String>,
}

impl HttpHeaders {
    pub fn new<K, V, I>(headers: I) -> Result<Self, ConfigurationFailure>
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut values = BTreeMap::new();
        for (name, value) in headers {
            let name = name.into();
            let value = value.into();
            if !valid_header_name(&name) {
                return Err(ConfigurationFailure::InvalidHeaderName { name });
            }
            if value.contains(['\r', '\n', '\0']) {
                return Err(ConfigurationFailure::InvalidHeaderValue { name });
            }
            let canonical_name = name.to_ascii_lowercase();
            if values.insert(canonical_name.clone(), value).is_some() {
                return Err(ConfigurationFailure::DuplicateHeaderName {
                    name: canonical_name,
                });
            }
        }
        Ok(Self { values })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl fmt::Debug for HttpHeaders {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = formatter.debug_map();
        for (name, value) in &self.values {
            if is_sensitive_header(name) {
                map.entry(name, &"<redacted>");
            } else {
                map.entry(name, value);
            }
        }
        map.finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PermissionName {
    Geolocation,
    Notifications,
    Midi,
    MidiSysex,
    Camera,
    Microphone,
    ClipboardReadWrite,
    ClipboardSanitizedWrite,
    PaymentHandler,
    BackgroundSync,
    Sensors,
    AccessibilityEvents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionSetting {
    Allow,
    Block,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionOverride {
    name: PermissionName,
    setting: PermissionSetting,
    origin: Option<String>,
}

impl PermissionOverride {
    pub fn new(name: PermissionName, setting: PermissionSetting) -> Self {
        Self {
            name,
            setting,
            origin: None,
        }
    }

    pub fn origin(mut self, origin: impl AsRef<str>) -> Result<Self, ConfigurationFailure> {
        self.origin = Some(validate_origin(origin.as_ref())?);
        Ok(self)
    }

    pub fn name(&self) -> PermissionName {
        self.name
    }

    pub fn setting(&self) -> PermissionSetting {
        self.setting
    }

    pub fn origin_value(&self) -> Option<&str> {
        self.origin.as_deref()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProxyOptions {
    server: String,
    bypass_list: Vec<String>,
}

impl ProxyOptions {
    pub fn new(server: impl Into<String>) -> Result<Self, ConfigurationFailure> {
        let server = canonical_proxy_server(&server.into())?;
        Ok(Self {
            server,
            bypass_list: Vec::new(),
        })
    }

    pub fn bypass<I, S>(mut self, entries: I) -> Result<Self, ConfigurationFailure>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut bypass_list = Vec::new();
        for entry in entries {
            let entry = entry.into();
            if entry.is_empty()
                || entry.contains(['\r', '\n', '\0', ',', ';'])
                || entry.chars().any(char::is_whitespace)
            {
                return Err(ConfigurationFailure::InvalidProxyBypassEntry);
            }
            bypass_list.push(entry);
        }
        self.bypass_list = bypass_list;
        Ok(self)
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn bypass_list(&self) -> &[String] {
        &self.bypass_list
    }
}

impl fmt::Debug for ProxyOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyOptions")
            .field("server_configured", &true)
            .field("bypass_list_configured", &!self.bypass_list.is_empty())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
/// Desired configuration for public Page/Frame/OOPIF target routes.
///
/// Workers are not public target routes and never enter the Frame graph. When
/// a worker must be retained for Page network observation, only the user-agent
/// override and extra HTTP headers are replayed onto that worker's Network
/// route; viewport, locale, timezone, geolocation, and HTTPS-error settings
/// remain Page/Frame/OOPIF-only.
pub struct TargetRouteOptions {
    viewport: Option<Viewport>,
    locale: Option<String>,
    timezone: Option<String>,
    geolocation: Option<Geolocation>,
    user_agent: Option<UserAgentOverride>,
    http_headers: HttpHeaders,
}

impl TargetRouteOptions {
    pub fn viewport(mut self, viewport: Viewport) -> Self {
        self.viewport = Some(viewport);
        self
    }

    pub fn locale(mut self, locale: impl Into<String>) -> Result<Self, ConfigurationFailure> {
        let locale = locale.into();
        if !valid_locale(&locale) {
            return Err(ConfigurationFailure::InvalidLocale);
        }
        self.locale = Some(locale);
        Ok(self)
    }

    pub fn timezone(mut self, timezone: impl Into<String>) -> Result<Self, ConfigurationFailure> {
        let timezone = timezone.into();
        if !valid_timezone(&timezone) {
            return Err(ConfigurationFailure::InvalidTimezone);
        }
        self.timezone = Some(timezone);
        Ok(self)
    }

    pub fn geolocation(mut self, geolocation: Geolocation) -> Self {
        self.geolocation = Some(geolocation);
        self
    }

    pub fn user_agent(mut self, user_agent: UserAgentOverride) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    pub fn http_headers(mut self, http_headers: HttpHeaders) -> Self {
        self.http_headers = http_headers;
        self
    }

    pub fn viewport_value(&self) -> Option<Viewport> {
        self.viewport
    }

    pub fn locale_value(&self) -> Option<&str> {
        self.locale.as_deref()
    }

    pub fn timezone_value(&self) -> Option<&str> {
        self.timezone.as_deref()
    }

    pub fn geolocation_value(&self) -> Option<Geolocation> {
        self.geolocation
    }

    pub fn user_agent_override(&self) -> Option<&UserAgentOverride> {
        self.user_agent.as_ref()
    }

    pub fn headers(&self) -> &HttpHeaders {
        &self.http_headers
    }
}

/// Validated, immutable desired configuration for a browser context.
///
/// The default contains no overrides and applying it has no side effects.
/// Proxy and managed-download-directory configuration intentionally live at
/// their ownership-specific APIs rather than in this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ContextOptions {
    target_route: TargetRouteOptions,
    permissions: Vec<PermissionOverride>,
    ignore_https_errors: bool,
}

impl ContextOptions {
    pub fn target_route(mut self, target_route: TargetRouteOptions) -> Self {
        self.target_route = target_route;
        self
    }

    pub fn permission(mut self, permission: PermissionOverride) -> Self {
        if let Some(existing) = self.permissions.iter_mut().find(|existing| {
            existing.name == permission.name && existing.origin == permission.origin
        }) {
            *existing = permission;
        } else {
            self.permissions.push(permission);
        }
        self
    }

    pub fn ignore_https_errors(mut self, ignore_https_errors: bool) -> Self {
        self.ignore_https_errors = ignore_https_errors;
        self
    }

    pub fn target_route_options(&self) -> &TargetRouteOptions {
        &self.target_route
    }

    pub fn permissions(&self) -> &[PermissionOverride] {
        &self.permissions
    }

    pub fn ignore_https_errors_enabled(&self) -> bool {
        self.ignore_https_errors
    }

    pub(crate) fn required_capabilities(&self) -> impl Iterator<Item = super::Capability> {
        let mut capabilities = Vec::with_capacity(3);
        if self.target_route != TargetRouteOptions::default() {
            capabilities.push(super::Capability::RequestRouting);
        }
        if !self.permissions.is_empty() {
            capabilities.push(super::Capability::PermissionOverrides);
        }
        if self.ignore_https_errors {
            capabilities.push(super::Capability::IgnoreHttpsErrors);
        }
        capabilities.into_iter()
    }
}

fn validate_nonempty_single_line(value: &str) -> Result<(), ()> {
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        Err(())
    } else {
        Ok(())
    }
}

fn normalize_accept_language(value: &str) -> Option<String> {
    let mut normalized = Vec::new();
    for entry in value.split(',') {
        let tag = entry.trim_matches([' ', '\t']);
        if !valid_language_tag(tag) {
            return None;
        }
        normalized.push(tag);
    }
    (!normalized.is_empty()).then(|| normalized.join(","))
}

fn valid_language_tag(value: &str) -> bool {
    let mut subtags = value.split('-');
    let primary = subtags.next().unwrap_or_default();
    let valid_primary = (2..=8).contains(&primary.len())
        || matches!(primary.to_ascii_lowercase().as_str(), "i" | "x");
    valid_primary
        && primary.bytes().all(|byte| byte.is_ascii_alphabetic())
        && subtags.all(|subtag| {
            (1..=8).contains(&subtag.len())
                && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn valid_locale(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_timezone(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+'))
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    )
}

fn validate_origin(origin: &str) -> Result<String, ConfigurationFailure> {
    let parsed = url::Url::parse(origin).map_err(|_| ConfigurationFailure::InvalidOrigin)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigurationFailure::InvalidOrigin);
    }
    Ok(parsed.origin().ascii_serialization())
}

fn canonical_proxy_server(server: &str) -> Result<String, ConfigurationFailure> {
    if validate_nonempty_single_line(server).is_err() {
        return Err(ConfigurationFailure::InvalidProxyServer);
    }
    let parsed = url::Url::parse(server).map_err(|_| ConfigurationFailure::InvalidProxyServer)?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConfigurationFailure::ProxyUserInfoNotAllowed);
    }
    if !matches!(parsed.scheme(), "http" | "https" | "socks4" | "socks5")
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConfigurationFailure::InvalidProxyServer);
    }
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_owned(),
        Some(url::Host::Ipv4(address)) => address.to_string(),
        Some(url::Host::Ipv6(address)) => format!("[{address}]"),
        None => return Err(ConfigurationFailure::InvalidProxyServer),
    };
    let mut canonical = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        canonical.push(':');
        canonical.push_str(&port.to_string());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;

    fn hash(value: &impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn finite_values_canonicalize_negative_zero_for_equality_and_hashing() {
        let negative = Geolocation::new(-0.0, -0.0)
            .unwrap()
            .accuracy(-0.0)
            .unwrap();
        let positive = Geolocation::new(0.0, 0.0).unwrap().accuracy(0.0).unwrap();
        assert_eq!(negative, positive);
        assert_eq!(hash(&negative), hash(&positive));
    }

    #[test]
    fn rejects_non_finite_or_out_of_range_numeric_configuration() {
        assert_eq!(
            Geolocation::new(f64::NAN, 0.0),
            Err(ConfigurationFailure::InvalidGeolocation)
        );
        assert_eq!(
            Geolocation::new(0.0, f64::INFINITY),
            Err(ConfigurationFailure::InvalidGeolocation)
        );
        assert_eq!(
            Geolocation::new(91.0, 0.0),
            Err(ConfigurationFailure::InvalidGeolocation)
        );
        assert_eq!(
            Viewport::new(0, 720),
            Err(ConfigurationFailure::InvalidViewport)
        );
    }

    #[test]
    fn validation_blocks_header_origin_and_proxy_argument_injection() {
        assert!(matches!(
            HttpHeaders::new([("x-test", "ok\r\ninjected: true")]),
            Err(ConfigurationFailure::InvalidHeaderValue { .. })
        ));
        assert_eq!(
            PermissionOverride::new(PermissionName::Camera, PermissionSetting::Allow)
                .origin("https://user@example.test"),
            Err(ConfigurationFailure::InvalidOrigin)
        );
        assert_eq!(
            ProxyOptions::new("http://user:secret@proxy.test:8080"),
            Err(ConfigurationFailure::ProxyUserInfoNotAllowed)
        );
        assert_eq!(
            ProxyOptions::new("http://proxy.test:8080\r\n--other-flag"),
            Err(ConfigurationFailure::InvalidProxyServer)
        );
    }

    #[test]
    fn sensitive_header_values_are_redacted() {
        let headers =
            HttpHeaders::new([("authorization", "Bearer secret"), ("x-visible", "visible")])
                .unwrap();
        let debug = format!("{headers:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("visible"));
    }

    #[test]
    fn defaults_have_no_overrides_or_side_effect_requests() {
        let options = ContextOptions::default();
        assert!(options.permissions().is_empty());
        assert!(!options.ignore_https_errors_enabled());
        assert_eq!(
            options.target_route_options(),
            &TargetRouteOptions::default()
        );
    }
}
