use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use browserkit::runtime::{
    ActionCompletion, BrowserError, BrowserRuntime, Capability, CapabilityAvailability,
    CapabilityScope, ConfigurationFailure, ContextOptions, Geolocation, HttpHeaders,
    OperationPhase, PermissionName, PermissionOverride, PermissionSetting, ProxyOptions,
    TargetRouteOptions, UserAgentOverride, VersionKnowledge, Viewport,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn version_server(product: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut write, mut read) = websocket.split();
        let mut methods = Vec::new();
        while let Some(message) = read.next().await {
            match message.unwrap() {
                Message::Text(text) => {
                    let command: Value = serde_json::from_str(&text).unwrap();
                    let id = command["id"].as_u64().unwrap();
                    let method = command["method"].as_str().unwrap().to_owned();
                    methods.push(method.clone());
                    assert_eq!(method, "Browser.getVersion");
                    write
                        .send(Message::Text(
                            json!({
                                "id": id,
                                "result": {
                                    "protocolVersion": "1.3",
                                    "product": product,
                                    "revision": "@revision",
                                    "userAgent": "BrowserKit Test",
                                    "jsVersion": "12.3"
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
                Message::Ping(payload) => write.send(Message::Pong(payload)).await.unwrap(),
                Message::Close(_) => {
                    let _ = write.send(Message::Close(None)).await;
                    break;
                }
                _ => {}
            }
        }
        methods
    });
    (format!("ws://{address}"), server)
}

#[tokio::test]
async fn connect_snapshots_version_once_before_runtime_use() {
    let (endpoint, server) = version_server("Chrome/123.0.6312.86").await;
    let runtime = BrowserRuntime::connect(endpoint).await.unwrap();
    let capabilities = runtime.capabilities();

    assert_eq!(capabilities.metadata().product(), "Chrome/123.0.6312.86");
    assert_eq!(
        capabilities.metadata().version(),
        VersionKnowledge::Known {
            major: 123,
            minor: 0,
            build: 6312,
            patch: 86,
        }
    );
    assert_eq!(
        capabilities
            .status(
                CapabilityScope::DefaultContext,
                Capability::DownloadObservation
            )
            .availability(),
        CapabilityAvailability::Available
    );

    assert!(runtime.close().await.is_complete());
    assert_eq!(server.await.unwrap(), vec!["Browser.getVersion"]);
}

#[tokio::test]
async fn malformed_browser_product_keeps_version_knowledge_unknown() {
    let (endpoint, server) = version_server("Chromium/not-a-version").await;
    let runtime = BrowserRuntime::connect(endpoint).await.unwrap();
    assert_eq!(
        runtime.capabilities().metadata().version(),
        VersionKnowledge::Unknown
    );
    assert!(runtime.close().await.is_complete());
    assert_eq!(server.await.unwrap(), vec!["Browser.getVersion"]);
}

#[test]
fn validated_options_are_canonical_hashable_and_secret_safe() {
    let viewport = Viewport::new(1280, 720).unwrap();
    let negative_zero = Geolocation::new(-0.0, -0.0).unwrap();
    let positive_zero = Geolocation::new(0.0, 0.0).unwrap();
    assert_eq!(negative_zero, positive_zero);
    let mut left = DefaultHasher::new();
    negative_zero.hash(&mut left);
    let mut right = DefaultHasher::new();
    positive_zero.hash(&mut right);
    assert_eq!(left.finish(), right.finish());

    let headers = HttpHeaders::new([
        ("authorization", "Bearer top-secret"),
        ("x-browserkit", "foundation"),
    ])
    .unwrap();
    assert!(!format!("{headers:?}").contains("top-secret"));

    let route = TargetRouteOptions::default()
        .viewport(viewport)
        .geolocation(Geolocation::new(51.5, -0.12).unwrap())
        .user_agent(UserAgentOverride::new("BrowserKit/1.0").unwrap())
        .http_headers(headers);
    let permission = PermissionOverride::new(PermissionName::Geolocation, PermissionSetting::Allow)
        .origin("https://example.test")
        .unwrap();
    let options = ContextOptions::default()
        .target_route(route)
        .permission(permission)
        .ignore_https_errors(true);
    assert!(options.ignore_https_errors_enabled());

    let user_agent = UserAgentOverride::new("BrowserKit/1.0")
        .unwrap()
        .accept_language("fr-CA, fr")
        .unwrap();
    assert_eq!(user_agent.accept_language_value(), Some("fr-CA,fr"));
    let tab_ows = UserAgentOverride::new("BrowserKit/1.0")
        .unwrap()
        .accept_language("\tfr-CA,\tfr\t")
        .unwrap();
    assert_eq!(tab_ows.accept_language_value(), Some("fr-CA,fr"));
    for invalid in [
        "",
        "fr-CA,,fr",
        "fr-CA,fr;q=0.8",
        "fr-CA=fr",
        "*",
        "fr_CA",
        "-fr",
        "fr-",
        "fr--CA",
        "fr-CA\r,fr",
    ] {
        assert_eq!(
            UserAgentOverride::new("BrowserKit/1.0")
                .unwrap()
                .accept_language(invalid),
            Err(ConfigurationFailure::InvalidAcceptLanguage),
            "accepted invalid browser language list {invalid:?}"
        );
    }

    assert!(matches!(
        Geolocation::new(f64::NAN, 0.0),
        Err(ConfigurationFailure::InvalidGeolocation)
    ));
    assert!(matches!(
        HttpHeaders::new([("x-injected", "safe\r\nset-cookie: bad")]),
        Err(ConfigurationFailure::InvalidHeaderValue { .. })
    ));
    assert!(matches!(
        PermissionOverride::new(PermissionName::Camera, PermissionSetting::Block)
            .origin("https://user@example.test"),
        Err(ConfigurationFailure::InvalidOrigin)
    ));
    assert!(matches!(
        ProxyOptions::new("http://user:secret@proxy.test:8080"),
        Err(ConfigurationFailure::ProxyUserInfoNotAllowed)
    ));
}

#[test]
fn proxy_options_are_canonical_hash_equivalent_and_debug_redacted() {
    let first = ProxyOptions::new("HTTP://PROXY.TEST:80")
        .unwrap()
        .bypass(["secret.internal.test"])
        .unwrap();
    let second = ProxyOptions::new("http://proxy.test/")
        .unwrap()
        .bypass(["secret.internal.test"])
        .unwrap();

    assert_eq!(first.server(), "http://proxy.test");
    assert_eq!(first, second);
    let mut first_hash = DefaultHasher::new();
    first.hash(&mut first_hash);
    let mut second_hash = DefaultHasher::new();
    second.hash(&mut second_hash);
    assert_eq!(first_hash.finish(), second_hash.finish());

    let ipv6_with_default_port = ProxyOptions::new("HTTPS://[2001:DB8::1]:443").unwrap();
    let ipv6_without_port = ProxyOptions::new("https://[2001:db8::1]").unwrap();
    assert_eq!(ipv6_with_default_port.server(), "https://[2001:db8::1]");
    assert_eq!(ipv6_with_default_port, ipv6_without_port);

    for invalid in [
        "ftp://proxy.test",
        "http://proxy.test/path",
        "http://proxy.test/?query",
        "http://proxy.test/#fragment",
        "http://user@proxy.test",
        "http://proxy.test\r\n--other-flag",
    ] {
        assert!(ProxyOptions::new(invalid).is_err(), "accepted {invalid:?}");
    }

    let debug = format!("{first:?}");
    assert!(!debug.contains("proxy.test"));
    assert!(!debug.contains("secret.internal.test"));
    assert!(debug.contains("server_configured: true"));
    assert!(debug.contains("bypass_list_configured: true"));
}

#[test]
fn configuration_errors_are_preflight_and_structured() {
    let failure = ConfigurationFailure::ConflictingTypedLaunchArgument;
    let error = BrowserError::configuration("launch browser", failure.clone());
    assert_eq!(error.configuration_failure(), Some(&failure));
    assert_eq!(error.phase(), OperationPhase::Preparation);
    assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
}
