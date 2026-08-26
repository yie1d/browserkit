use std::sync::Arc;
use std::time::Duration;

use browserkit::runtime::{
    ActionCompletion, BrowserRuntime, Capability, CapabilityAvailability, CapabilityReason,
    CapabilityScope, ConfigurationFailure, ContextOptions, DefaultSessionOptions, Geolocation,
    HttpHeaders, IsolatedSessionOptions, NetworkObservationOptions, OperationPhase, PermissionName,
    PermissionOverride, PermissionSetting, ProxyOptions, SessionEvent, TargetRouteOptions,
    UserAgentOverride, Viewport,
};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Copy, Default)]
struct ServerBehavior {
    fail_permission: bool,
    hold_permission: bool,
    fail_method: Option<&'static str>,
    hold_method: Option<&'static str>,
    delay_method: Option<&'static str>,
    hold_cleanup_response_method: Option<&'static str>,
    emit_oopif: bool,
    emit_nested_oopif: bool,
    emit_oopif_after_main_autoattach: u8,
    fail_oopif_method: Option<&'static str>,
    hold_oopif_method: Option<&'static str>,
    fail_cleanup_methods: &'static [&'static str],
    fail_detach: bool,
    emit_oopif_detach_on_runtime_enable: bool,
    navigation_event_before_ack: bool,
    delay_navigation_commit: bool,
    omit_navigation_commit: bool,
    navigation_event_loader: Option<&'static str>,
    navigation_commit_url: Option<&'static str>,
    navigation_final_loader: Option<&'static str>,
}

struct FakeServer {
    endpoint: String,
    commands: Arc<Mutex<Vec<Value>>>,
    permission_seen: Arc<Notify>,
    held_method_seen: Arc<Notify>,
    delayed_method_seen: Arc<Notify>,
    release_delayed_method: Arc<Notify>,
    cleanup_response_seen: Arc<Notify>,
    release_cleanup_response: Arc<Notify>,
    target_close_seen: Arc<Notify>,
    navigation_ack_seen: Arc<Notify>,
    release_navigation_commit: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

fn frame_tree(loader_id: &str, url: &str) -> Value {
    json!({
        "frameTree": {
            "frame": {
                "id": "main",
                "loaderId": loader_id,
                "url": url,
                "domainAndRegistry": "",
                "securityOrigin": "null",
                "mimeType": "text/html",
                "secureContextType": "InsecureScheme",
                "crossOriginIsolatedContextType": "NotIsolated",
                "gatedAPIFeatures": []
            }
        }
    })
}

fn frame_navigated(session_id: &Value, loader_id: &str, url: &str) -> Value {
    json!({
        "method": "Page.frameNavigated",
        "sessionId": session_id,
        "params": {
            "frame": {
                "id": "main",
                "loaderId": loader_id,
                "url": url,
                "domainAndRegistry": "example.test",
                "securityOrigin": "https://example.test",
                "mimeType": "text/html",
                "secureContextType": "Secure",
                "crossOriginIsolatedContextType": "NotIsolated",
                "gatedAPIFeatures": []
            },
            "type": "Navigation"
        }
    })
}

async fn start_server(behavior: ServerBehavior) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let permission_seen = Arc::new(Notify::new());
    let held_method_seen = Arc::new(Notify::new());
    let delayed_method_seen = Arc::new(Notify::new());
    let release_delayed_method = Arc::new(Notify::new());
    let cleanup_response_seen = Arc::new(Notify::new());
    let release_cleanup_response = Arc::new(Notify::new());
    let target_close_seen = Arc::new(Notify::new());
    let navigation_ack_seen = Arc::new(Notify::new());
    let release_navigation_commit = Arc::new(Notify::new());
    let server_commands = Arc::clone(&commands);
    let server_permission_seen = Arc::clone(&permission_seen);
    let server_held_method_seen = Arc::clone(&held_method_seen);
    let server_delayed_method_seen = Arc::clone(&delayed_method_seen);
    let server_release_delayed_method = Arc::clone(&release_delayed_method);
    let server_cleanup_response_seen = Arc::clone(&cleanup_response_seen);
    let server_release_cleanup_response = Arc::clone(&release_cleanup_response);
    let server_target_close_seen = Arc::clone(&target_close_seen);
    let server_navigation_ack_seen = Arc::clone(&navigation_ack_seen);
    let server_release_navigation_commit = Arc::clone(&release_navigation_commit);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (write, mut read) = websocket.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let mut failed_method = false;
        let mut held_method = false;
        let mut emitted_oopif = false;
        let mut emitted_nested_oopif = false;
        let mut emitted_oopif_detach = false;
        let mut main_autoattach_count = 0_u8;
        let mut current_main_loader = "loader-main".to_owned();
        let mut current_main_url = "about:blank".to_owned();
        while let Some(message) = read.next().await {
            match message.unwrap() {
                Message::Text(text) => {
                    let command: Value = serde_json::from_str(&text).unwrap();
                    let id = command["id"].as_u64().unwrap();
                    let method = command["method"].as_str().unwrap();
                    let route_session_id = command.get("sessionId").and_then(Value::as_str);
                    server_commands.lock().push(command.clone());
                    if method == "Target.closeTarget" {
                        server_target_close_seen.notify_one();
                    }
                    if method == "Browser.setPermission" {
                        server_permission_seen.notify_waiters();
                        if behavior.hold_permission {
                            continue;
                        }
                        if behavior.fail_permission {
                            write.lock().await
                                .send(Message::Text(
                                    json!({
                                        "id": id,
                                        "error": {"code": -32000, "message": "injected permission failure"}
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await
                                .unwrap();
                            continue;
                        }
                    }
                    let is_oopif = matches!(
                        route_session_id,
                        Some("oopif-session-1" | "oopif-session-2")
                    );
                    let is_route_cleanup = match method {
                        "Emulation.clearDeviceMetricsOverride"
                        | "Emulation.clearGeolocationOverride"
                        | "Network.disable"
                        | "Security.disable" => true,
                        "Emulation.setLocaleOverride" => command["params"].get("locale").is_none(),
                        "Emulation.setTimezoneOverride" => command["params"]["timezoneId"] == "",
                        "Network.setUserAgentOverride" => {
                            command["params"]["userAgent"] == "BrowserKit Test"
                        }
                        "Network.setExtraHTTPHeaders" => command["params"]["headers"] == json!({}),
                        "Security.setIgnoreCertificateErrors" => {
                            command["params"]["ignore"] == false
                        }
                        _ => false,
                    };
                    if behavior.fail_detach && method == "Target.detachFromTarget" {
                        let mut response = json!({
                            "id": id,
                            "error": {"code": -32000, "message": "injected detach failure"}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .lock()
                            .await
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        continue;
                    }
                    if is_route_cleanup && behavior.fail_cleanup_methods.contains(&method) {
                        let mut response = json!({
                            "id": id,
                            "error": {"code": -32000, "message": format!("injected cleanup {method} failure")}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .lock()
                            .await
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        continue;
                    }
                    if behavior.hold_oopif_method == Some(method) && is_oopif && !held_method {
                        held_method = true;
                        server_held_method_seen.notify_one();
                        continue;
                    }
                    if behavior.hold_method == Some(method) && !held_method {
                        held_method = true;
                        server_held_method_seen.notify_one();
                        continue;
                    }
                    if behavior.delay_method == Some(method) {
                        server_delayed_method_seen.notify_one();
                        server_release_delayed_method.notified().await;
                    }
                    if (behavior.fail_method == Some(method)
                        || (behavior.fail_oopif_method == Some(method) && is_oopif))
                        && !failed_method
                    {
                        failed_method = true;
                        let mut response = json!({
                            "id": id,
                            "error": {"code": -32000, "message": format!("injected {method} failure")}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .lock()
                            .await
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        continue;
                    }
                    if is_route_cleanup && behavior.hold_cleanup_response_method == Some(method) {
                        let mut response = json!({"id": id, "result": {}});
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        let write = Arc::clone(&write);
                        let release = Arc::clone(&server_release_cleanup_response);
                        server_cleanup_response_seen.notify_one();
                        tokio::spawn(async move {
                            release.notified().await;
                            write
                                .lock()
                                .await
                                .send(Message::Text(response.to_string().into()))
                                .await
                                .unwrap();
                        });
                        continue;
                    }
                    if method == "Target.setAutoAttach"
                        && route_session_id == Some("page-session-1")
                    {
                        main_autoattach_count = main_autoattach_count.saturating_add(1);
                    }
                    if method == "Target.setAutoAttach"
                        && route_session_id == Some("page-session-1")
                        && behavior.emit_oopif
                        && !emitted_oopif
                        && main_autoattach_count >= behavior.emit_oopif_after_main_autoattach.max(1)
                    {
                        emitted_oopif = true;
                        write.lock().await
                            .send(Message::Text(
                                json!({
                                    "method": "Target.attachedToTarget",
                                    "sessionId": "page-session-1",
                                    "params": {
                                        "sessionId": "oopif-session-1",
                                        "targetInfo": {
                                            "targetId": "oopif-frame-1",
                                            "type": "iframe",
                                            "title": "",
                                            "url": "https://oopif.test/",
                                            "attached": true,
                                            "canAccessOpener": false,
                                            "parentFrameId": "main",
                                            "browserContextId": "context-1"
                                        },
                                        "waitingForDebugger": command["params"]["waitForDebuggerOnStart"]
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    if method == "Target.setAutoAttach"
                        && route_session_id == Some("oopif-session-1")
                        && behavior.emit_nested_oopif
                        && !emitted_nested_oopif
                    {
                        emitted_nested_oopif = true;
                        write.lock().await
                            .send(Message::Text(
                                json!({
                                    "method": "Target.attachedToTarget",
                                    "sessionId": "oopif-session-1",
                                    "params": {
                                        "sessionId": "oopif-session-2",
                                        "targetInfo": {
                                            "targetId": "oopif-frame-2",
                                            "type": "iframe",
                                            "title": "",
                                            "url": "https://nested-oopif.test/",
                                            "attached": true,
                                            "canAccessOpener": false,
                                            "parentFrameId": "oopif-frame-1",
                                            "browserContextId": "context-1"
                                        },
                                        "waitingForDebugger": command["params"]["waitForDebuggerOnStart"]
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    if method == "Runtime.enable"
                        && behavior.emit_oopif_detach_on_runtime_enable
                        && !emitted_oopif_detach
                    {
                        emitted_oopif_detach = true;
                        write
                            .lock()
                            .await
                            .send(Message::Text(
                                json!({
                                    "method": "Target.detachedFromTarget",
                                    "params": {
                                        "sessionId": "oopif-session-1",
                                        "targetId": "oopif-frame-1"
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    if method == "Page.navigate" {
                        let requested_url = command["params"]["url"].as_str().unwrap().to_owned();
                        let committed_url = behavior
                            .navigation_commit_url
                            .map(str::to_owned)
                            .unwrap_or_else(|| requested_url.clone());
                        let event_loader = behavior.navigation_event_loader.unwrap_or("loader-nav");
                        let event = frame_navigated(
                            command.get("sessionId").unwrap(),
                            event_loader,
                            &committed_url,
                        );
                        if behavior.navigation_event_before_ack {
                            current_main_loader = event_loader.to_owned();
                            current_main_url = committed_url.clone();
                            write
                                .lock()
                                .await
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        let mut response = json!({
                            "id": id,
                            "result": {"frameId": "main", "loaderId": "loader-nav"}
                        });
                        if let Some(session_id) = command.get("sessionId") {
                            response["sessionId"] = session_id.clone();
                        }
                        write
                            .lock()
                            .await
                            .send(Message::Text(response.to_string().into()))
                            .await
                            .unwrap();
                        server_navigation_ack_seen.notify_one();
                        if behavior.omit_navigation_commit {
                            continue;
                        }
                        if behavior.delay_navigation_commit {
                            server_release_navigation_commit.notified().await;
                        }
                        if !behavior.navigation_event_before_ack {
                            current_main_loader = event_loader.to_owned();
                            current_main_url = committed_url;
                            write
                                .lock()
                                .await
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        if let Some(final_loader) = behavior.navigation_final_loader {
                            current_main_loader = final_loader.to_owned();
                            current_main_url = "https://example.test/superseding".to_owned();
                        }
                        continue;
                    }
                    let result = match method {
                        "Browser.getVersion" => json!({
                            "protocolVersion": "1.3",
                            "product": "Chrome/123.0.6312.86",
                            "revision": "@revision",
                            "userAgent": "BrowserKit Test",
                            "jsVersion": "12.3"
                        }),
                        "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                        "Target.createBrowserContext" => json!({"browserContextId": "context-1"}),
                        "Target.createTarget" => json!({"targetId": "target-1"}),
                        "Target.attachToTarget" => json!({"sessionId": "page-session-1"}),
                        "Target.getTargetInfo" => json!({
                            "targetInfo": {
                                "targetId": "target-1",
                                "type": "page",
                                "title": "",
                                "url": "about:blank",
                                "attached": false,
                                "canAccessOpener": false,
                                "browserContextId": "context-1"
                            }
                        }),
                        "Page.getFrameTree" => match route_session_id {
                            Some("oopif-session-1") => json!({
                                "frameTree": {
                                    "frame": {
                                        "id": "oopif-frame-1",
                                        "parentId": "main",
                                        "loaderId": "loader-oopif-1",
                                        "url": "https://oopif.test/",
                                        "domainAndRegistry": "oopif.test",
                                        "securityOrigin": "https://oopif.test",
                                        "mimeType": "text/html",
                                        "secureContextType": "Secure",
                                        "crossOriginIsolatedContextType": "NotIsolated",
                                        "gatedAPIFeatures": []
                                    }
                                }
                            }),
                            Some("oopif-session-2") => json!({
                                "frameTree": {
                                    "frame": {
                                        "id": "oopif-frame-2",
                                        "parentId": "oopif-frame-1",
                                        "loaderId": "loader-oopif-2",
                                        "url": "https://nested-oopif.test/",
                                        "domainAndRegistry": "nested-oopif.test",
                                        "securityOrigin": "https://nested-oopif.test",
                                        "mimeType": "text/html",
                                        "secureContextType": "Secure",
                                        "crossOriginIsolatedContextType": "NotIsolated",
                                        "gatedAPIFeatures": []
                                    }
                                }
                            }),
                            _ => frame_tree(&current_main_loader, &current_main_url),
                        },
                        "Target.closeTarget" => json!({"success": true}),
                        _ => json!({}),
                    };
                    let mut response = json!({"id": id, "result": result});
                    if let Some(session_id) = command.get("sessionId") {
                        response["sessionId"] = session_id.clone();
                    }
                    write
                        .lock()
                        .await
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                    if method == "Target.closeTarget" {
                        write
                            .lock()
                            .await
                            .send(Message::Text(
                                json!({
                                    "method": "Target.targetDestroyed",
                                    "params": {"targetId": "target-1"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                }
                Message::Ping(payload) => write
                    .lock()
                    .await
                    .send(Message::Pong(payload))
                    .await
                    .unwrap(),
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    FakeServer {
        endpoint: format!("ws://{address}"),
        commands,
        permission_seen,
        held_method_seen,
        delayed_method_seen,
        release_delayed_method,
        cleanup_response_seen,
        release_cleanup_response,
        target_close_seen,
        navigation_ack_seen,
        release_navigation_commit,
        server,
    }
}

fn methods(commands: &Mutex<Vec<Value>>) -> Vec<String> {
    commands
        .lock()
        .iter()
        .map(|command| command["method"].as_str().unwrap().to_owned())
        .collect()
}

async fn wait_for_method(commands: &Mutex<Vec<Value>>, expected: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if methods(commands).iter().any(|method| method == expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fake CDP server did not receive {expected}"));
}

#[tokio::test]
async fn attached_default_mutation_is_typed_unavailable_with_zero_configuration_dispatch() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let context = ContextOptions::default()
        .target_route(TargetRouteOptions::default().viewport(Viewport::new(800, 600).unwrap()));

    let error = runtime
        .default_session_with(DefaultSessionOptions::default().context(context))
        .await
        .unwrap_err();
    let status = error.capability_status().expect("typed capability status");
    assert_eq!(status.capability(), Capability::RequestRouting);
    assert_eq!(status.availability(), CapabilityAvailability::Unavailable);
    assert_eq!(error.phase(), OperationPhase::Preparation);
    assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
    assert_eq!(methods(&fake.commands), vec!["Browser.getVersion"]);

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn runtime_and_session_proxy_capabilities_share_one_immutable_snapshot() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();

    let runtime_default = *runtime
        .capabilities()
        .status(CapabilityScope::DefaultContext, Capability::Proxy);
    assert_eq!(
        runtime_default.reason(),
        Some(CapabilityReason::RequiresBrowserLaunchConfiguration)
    );
    assert_eq!(runtime_default.scope(), CapabilityScope::BrowserLaunch);
    let default_session = runtime.default_session().await.unwrap();
    assert_eq!(
        *default_session.capabilities().status(Capability::Proxy),
        runtime_default
    );

    let runtime_isolated = *runtime
        .capabilities()
        .status(CapabilityScope::IsolatedContext, Capability::Proxy);
    assert_eq!(
        runtime_isolated.availability(),
        CapabilityAvailability::Available
    );
    assert_eq!(
        runtime_isolated.scope(),
        CapabilityScope::BrowserContextCreation
    );
    let isolated_session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        *isolated_session.capabilities().status(Capability::Proxy),
        runtime_isolated
    );

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn no_arg_default_rejects_an_existing_custom_default_without_changing_identity() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let network = NetworkObservationOptions::default().retained_state_max_bytes(1024 * 1024);
    let options = DefaultSessionOptions::default().network_observation(network);
    let first = runtime.default_session_with(options.clone()).await.unwrap();
    let same = runtime.default_session_with(options).await.unwrap();
    assert_eq!(first.id(), same.id());

    let error = runtime.default_session().await.unwrap_err();
    assert_eq!(
        error.configuration_failure(),
        Some(&ConfigurationFailure::ImmutableDefaultSessionOptions)
    );
    assert_eq!(first.id(), same.id());
    assert_eq!(
        methods(&fake.commands),
        vec![
            "Browser.getVersion",
            "Target.getBrowserContexts",
            "Target.setDiscoverTargets"
        ]
    );

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

fn permission_context() -> ContextOptions {
    ContextOptions::default()
        .permission(
            PermissionOverride::new(PermissionName::Geolocation, PermissionSetting::Allow)
                .origin("https://example.test")
                .unwrap(),
        )
        .permission(
            PermissionOverride::new(PermissionName::Notifications, PermissionSetting::Block)
                .origin("https://notifications.test")
                .unwrap(),
        )
}

#[tokio::test]
async fn isolated_proxy_and_permission_are_encoded_before_session_configuration() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let proxy = ProxyOptions::new("http://proxy.test:8080")
        .unwrap()
        .bypass(["localhost", "*.internal.test"])
        .unwrap();
    let context = permission_context();
    let session = runtime
        .isolated_session(
            IsolatedSessionOptions::default()
                .context(context.clone())
                .proxy(proxy),
        )
        .await
        .unwrap();

    assert_eq!(session.context_options(), &context);
    assert_eq!(
        session.capabilities().scope(),
        browserkit::runtime::CapabilityScope::IsolatedContext
    );
    {
        let commands = fake.commands.lock();
        let create = commands
            .iter()
            .find(|command| command["method"] == "Target.createBrowserContext")
            .unwrap();
        assert_eq!(create["params"]["proxyServer"], "http://proxy.test:8080");
        assert_eq!(
            create["params"]["proxyBypassList"],
            "localhost,*.internal.test"
        );
        let permissions = commands
            .iter()
            .filter(|command| command["method"] == "Browser.setPermission")
            .collect::<Vec<_>>();
        assert_eq!(permissions.len(), 2);
        assert_eq!(permissions[0]["params"]["browserContextId"], "context-1");
        assert_eq!(permissions[0]["params"]["origin"], "https://example.test");
        assert_eq!(
            permissions[0]["params"]["permission"]["name"],
            "geolocation"
        );
        assert_eq!(permissions[0]["params"]["setting"], "granted");
        assert_eq!(permissions[1]["params"]["browserContextId"], "context-1");
        assert_eq!(
            permissions[1]["params"]["origin"],
            "https://notifications.test"
        );
        assert_eq!(
            permissions[1]["params"]["permission"]["name"],
            "notifications"
        );
        assert_eq!(permissions[1]["params"]["setting"], "denied");
        let methods = commands
            .iter()
            .map(|command| command["method"].as_str().unwrap())
            .collect::<Vec<_>>();
        let create_index = methods
            .iter()
            .position(|method| *method == "Target.createBrowserContext")
            .unwrap();
        let discover_index = methods
            .iter()
            .position(|method| *method == "Target.setDiscoverTargets")
            .unwrap();
        for permission_index in methods
            .iter()
            .enumerate()
            .filter_map(|(index, method)| (*method == "Browser.setPermission").then_some(index))
        {
            assert!(create_index < permission_index);
            assert!(permission_index < discover_index);
        }
    }

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn isolated_permission_failure_disposes_pending_context() {
    let fake = start_server(ServerBehavior {
        fail_permission: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();

    let error = runtime
        .isolated_session(IsolatedSessionOptions::default().context(permission_context()))
        .await
        .unwrap_err();
    assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
    wait_for_method(&fake.commands, "Target.disposeBrowserContext").await;
    assert_eq!(
        methods(&fake.commands),
        vec![
            "Browser.getVersion",
            "Target.createBrowserContext",
            "Browser.setPermission",
            "Target.disposeBrowserContext"
        ]
    );

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_isolated_permission_configuration_disposes_pending_context() {
    let fake = start_server(ServerBehavior {
        hold_permission: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let creating_runtime = runtime.clone();
    let creating = tokio::spawn(async move {
        creating_runtime
            .isolated_session(IsolatedSessionOptions::default().context(permission_context()))
            .await
    });
    fake.permission_seen.notified().await;
    creating.abort();
    let _ = creating.await;

    wait_for_method(&fake.commands, "Target.disposeBrowserContext").await;
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

fn full_route_context() -> ContextOptions {
    let headers = HttpHeaders::new([("x-browserkit", "first-request")]).unwrap();
    let route = TargetRouteOptions::default()
        .viewport(
            Viewport::new(1280, 720)
                .unwrap()
                .device_scale_factor(1.5)
                .unwrap(),
        )
        .locale("en-US")
        .unwrap()
        .timezone("Europe/London")
        .unwrap()
        .user_agent(
            UserAgentOverride::new("BrowserKit Route/1.0")
                .unwrap()
                .accept_language("fr-CA,fr")
                .unwrap()
                .platform("TestOS")
                .unwrap(),
        )
        .geolocation(
            Geolocation::new(51.5, -0.12)
                .unwrap()
                .accuracy(5.0)
                .unwrap(),
        )
        .http_headers(headers);
    ContextOptions::default()
        .target_route(route)
        .ignore_https_errors(true)
}

#[tokio::test]
async fn new_page_configures_blank_target_before_requested_navigation() {
    let fake = start_server(ServerBehavior {
        navigation_event_before_ack: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let context = full_route_context();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(context))
        .await
        .unwrap();
    let page = session
        .new_page("https://example.test/first")
        .await
        .unwrap();

    let commands = fake.commands.lock().clone();
    let create = commands
        .iter()
        .find(|command| command["method"] == "Target.createTarget")
        .unwrap();
    assert_eq!(create["params"]["url"], "about:blank");
    assert_eq!(create["params"]["browserContextId"], "context-1");
    let navigate = commands
        .iter()
        .find(|command| command["method"] == "Page.navigate")
        .unwrap();
    assert_eq!(navigate["params"]["url"], "https://example.test/first");
    let methods = commands
        .iter()
        .map(|command| command["method"].as_str().unwrap())
        .collect::<Vec<_>>();
    let navigate_index = methods
        .iter()
        .position(|method| *method == "Page.navigate")
        .unwrap();
    let network_enable_index = methods
        .iter()
        .position(|method| *method == "Network.enable")
        .unwrap();
    let headers_index = methods
        .iter()
        .position(|method| *method == "Network.setExtraHTTPHeaders")
        .unwrap();
    assert!(network_enable_index < headers_index);
    for configured in [
        "Emulation.setDeviceMetricsOverride",
        "Emulation.setLocaleOverride",
        "Emulation.setTimezoneOverride",
        "Network.setUserAgentOverride",
        "Emulation.setGeolocationOverride",
        "Network.enable",
        "Network.setExtraHTTPHeaders",
        "Security.enable",
        "Security.setIgnoreCertificateErrors",
    ] {
        let index = methods
            .iter()
            .position(|method| *method == configured)
            .unwrap_or_else(|| panic!("missing route command {configured}: {methods:?}"));
        assert!(
            index < navigate_index,
            "{configured} must precede navigation"
        );
    }
    let viewport = commands
        .iter()
        .find(|command| command["method"] == "Emulation.setDeviceMetricsOverride")
        .unwrap();
    assert_eq!(viewport["params"]["mobile"], false);
    assert_eq!(viewport["params"]["width"], 1280);
    assert_eq!(viewport["params"]["height"], 720);
    assert_eq!(viewport["sessionId"], "page-session-1");
    assert_eq!(
        commands
            .iter()
            .filter(|command| command["method"] == "Emulation.setDeviceMetricsOverride")
            .count(),
        1,
        "viewport is configured only on the top-level main route"
    );
    let user_agent = commands
        .iter()
        .find(|command| command["method"] == "Network.setUserAgentOverride")
        .unwrap();
    assert_eq!(user_agent["params"]["userAgent"], "BrowserKit Route/1.0");
    assert_eq!(user_agent["params"]["acceptLanguage"], "fr-CA,fr");
    let headers = commands
        .iter()
        .find(|command| command["method"] == "Network.setExtraHTTPHeaders")
        .unwrap();
    assert_eq!(
        headers["params"]["headers"]["x-browserkit"],
        "first-request"
    );
    drop(commands);

    assert_eq!(page.target_id(), "target-1");
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn about_blank_uses_initialized_document_without_redundant_navigation() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();

    let page = session.new_page("about:blank").await.unwrap();
    assert_eq!(page.target_id(), "target-1");
    let methods = methods(&fake.commands);
    assert!(!methods.iter().any(|method| method == "Page.navigate"));
    assert!(
        methods
            .iter()
            .filter(|method| method.as_str() == "Page.getFrameTree")
            .count()
            >= 2,
        "initial FrameStore identity and the publish fence must both be observed"
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn redirected_navigation_publishes_matching_committed_document_identity() {
    let requested_url = "https://example.test/redirect";
    let final_url = "https://example.test/final";
    let fake = start_server(ServerBehavior {
        navigation_commit_url: Some(final_url),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    let page = session.new_page(requested_url).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event(),
        SessionEvent::PageCreated { target_id, .. } if target_id == page.target_id()
    ));
    let commands = fake.commands.lock().clone();
    let navigate = commands
        .iter()
        .find(|command| command["method"] == "Page.navigate")
        .unwrap();
    assert_eq!(navigate["params"]["url"], requested_url);
    let final_identity = commands
        .iter()
        .rposition(|command| command["method"] == "Page.getFrameTree")
        .expect("final FrameTree identity check");
    assert!(
        final_identity
            > commands
                .iter()
                .position(|command| command["method"] == "Page.navigate")
                .unwrap()
    );
    drop(commands);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn wrong_navigation_loader_never_publishes_and_cleans_up() {
    let fake = start_server(ServerBehavior {
        navigation_event_loader: Some("loader-other"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    let error = session
        .new_page("https://example.test/wrong-loader")
        .await
        .unwrap_err();
    assert_eq!(error.phase(), OperationPhase::Confirmation);
    assert_eq!(error.action_completed(), ActionCompletion::Completed);
    assert!(error.to_string().contains("superseded"));
    wait_for_method(&fake.commands, "Target.closeTarget").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn superseded_final_identity_never_publishes_and_cleans_up() {
    let fake = start_server(ServerBehavior {
        navigation_final_loader: Some("loader-superseding"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    let error = session
        .new_page("https://example.test/superseded")
        .await
        .unwrap_err();
    assert_eq!(error.phase(), OperationPhase::Confirmation);
    assert_eq!(error.action_completed(), ActionCompletion::Completed);
    assert!(error.to_string().contains("superseded"));
    wait_for_method(&fake.commands, "Target.closeTarget").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn attach_page_applies_route_before_return_without_navigating() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    assert_eq!(page.target_id(), "target-1");
    assert!(!methods(&fake.commands)
        .iter()
        .any(|method| method == "Page.navigate"));
    assert!(methods(&fake.commands)
        .iter()
        .any(|method| method == "Network.setExtraHTTPHeaders"));

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn new_page_route_failure_rolls_back_in_reverse_and_closes_unpublished_target() {
    let fake = start_server(ServerBehavior {
        fail_method: Some("Network.setExtraHTTPHeaders"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    assert!(session.new_page("https://example.test/fail").await.is_err());
    wait_for_method(&fake.commands, "Target.closeTarget").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );

    let methods = methods(&fake.commands);
    let failed = methods
        .iter()
        .position(|method| method == "Network.setExtraHTTPHeaders")
        .unwrap();
    let rollback = [
        "Network.setExtraHTTPHeaders",
        "Network.disable",
        "Emulation.clearGeolocationOverride",
        "Network.setUserAgentOverride",
        "Emulation.setTimezoneOverride",
        "Emulation.setLocaleOverride",
        "Emulation.clearDeviceMetricsOverride",
        "Target.closeTarget",
    ];
    let mut cursor = failed;
    for expected in rollback {
        cursor += methods[cursor + 1..]
            .iter()
            .position(|method| method == expected)
            .unwrap_or_else(|| panic!("missing rollback {expected}: {methods:?}"))
            + 1;
    }

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_fail_rollback_retains_single_ordered_cleanup_owner() {
    let fake = start_server(ServerBehavior {
        fail_method: Some("Network.setExtraHTTPHeaders"),
        hold_cleanup_response_method: Some("Network.setExtraHTTPHeaders"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/cancel-fail-rollback")
            .await
    });

    // This gate is reached only after the configuration error has entered
    // PageCreationTransaction::fail and dispatched its first rollback command.
    fake.cleanup_response_seen.notified().await;
    creating.abort();
    let _ = creating.await;

    let mut closing = Box::pin(session.close());
    tokio::select! {
        biased;
        report = &mut closing => {
            panic!("session close completed before route rollback release: {report:?}")
        }
        () = std::future::ready(()) => {}
    }

    assert!(
        !fake
            .commands
            .lock()
            .iter()
            .any(|command| command["method"] == "Target.closeTarget"),
        "target cleanup must not dispatch while rollback response is held"
    );

    fake.release_cleanup_response.notify_one();
    fake.target_close_seen.notified().await;
    let close_report = closing.await;
    assert!(close_report.is_complete(), "{close_report:?}");

    let captured = fake.commands.lock().clone();
    let failed = captured
        .iter()
        .position(|command| {
            command["method"] == "Network.setExtraHTTPHeaders"
                && command["params"]["headers"] != json!({})
        })
        .expect("failing route configuration command");
    let rollback = [
        "Network.setExtraHTTPHeaders",
        "Network.disable",
        "Emulation.clearGeolocationOverride",
        "Network.setUserAgentOverride",
        "Emulation.setTimezoneOverride",
        "Emulation.setLocaleOverride",
        "Emulation.clearDeviceMetricsOverride",
        "Target.closeTarget",
    ];
    let mut cursor = failed;
    for expected in rollback {
        cursor += captured[cursor + 1..]
            .iter()
            .position(|command| command["method"] == expected)
            .unwrap_or_else(|| panic!("missing ordered cleanup {expected}: {captured:?}"))
            + 1;
    }
    assert_eq!(
        captured
            .iter()
            .filter(|command| {
                command["method"] == "Network.setExtraHTTPHeaders"
                    && command["params"]["headers"] == json!({})
            })
            .count(),
        1,
        "route rollback registry token must execute exactly once"
    );
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Target.closeTarget")
            .count(),
        1,
        "pending target registry token must execute exactly once"
    );
    drop(captured);

    let mut published = false;
    while let Some(event) = events.next().await {
        if let Ok(event) = event {
            published |= matches!(event.event(), SessionEvent::PageCreated { .. });
        }
    }
    assert!(!published, "failed page creation must never publish a Page");

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn attach_route_failure_rolls_back_before_detach_and_never_publishes() {
    let fake = start_server(ServerBehavior {
        fail_method: Some("Network.setExtraHTTPHeaders"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    assert!(session.attach_page("target-1").await.is_err());
    wait_for_method(&fake.commands, "Target.detachFromTarget").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    let methods = methods(&fake.commands);
    let clear = methods
        .iter()
        .rposition(|method| method == "Emulation.clearDeviceMetricsOverride")
        .unwrap();
    let detach = methods
        .iter()
        .position(|method| method == "Target.detachFromTarget")
        .unwrap();
    assert!(clear < detach);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_route_configuration_orders_cleanup_before_concurrent_close() {
    let fake = start_server(ServerBehavior {
        hold_method: Some("Network.setExtraHTTPHeaders"),
        delay_method: Some("Target.closeTarget"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/cancel-config")
            .await
    });
    fake.held_method_seen.notified().await;
    creating.abort();
    let _ = creating.await;

    let closing_session = session.clone();
    let closing = tokio::spawn(async move { closing_session.close().await });
    fake.delayed_method_seen.notified().await;
    assert!(
        !closing.is_finished(),
        "session close must wait for the transaction's ordered target cleanup"
    );
    let captured = fake.commands.lock().clone();
    let rollback = captured
        .iter()
        .position(|command| command["method"] == "Emulation.clearDeviceMetricsOverride")
        .expect("route rollback must complete before target cleanup dispatch");
    let close_target = captured
        .iter()
        .position(|command| command["method"] == "Target.closeTarget")
        .unwrap();
    assert!(rollback < close_target);
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Emulation.clearDeviceMetricsOverride")
            .count(),
        1
    );
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Target.closeTarget")
            .count(),
        1
    );
    drop(captured);

    fake.release_delayed_method.notify_one();
    let close_report = closing.await.unwrap();
    assert!(close_report.is_complete(), "{close_report:?}");
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_new_page_commit_wait_cleans_up_without_publishing() {
    let fake = start_server(ServerBehavior {
        omit_navigation_commit: true,
        delay_method: Some("Security.disable"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/cancel-navigation")
            .await
    });
    fake.navigation_ack_seen.notified().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    assert!(!creating.is_finished());
    creating.abort();
    let _ = creating.await;
    fake.delayed_method_seen.notified().await;

    let closing_session = session.clone();
    let closing = tokio::spawn(async move { closing_session.close().await });
    tokio::task::yield_now().await;
    assert!(
        !closing.is_finished(),
        "session close must drain cancellation cleanup admission"
    );
    fake.release_delayed_method.notify_one();
    let close_report = closing.await.unwrap();
    assert!(close_report.is_complete(), "{close_report:?}");

    wait_for_method(&fake.commands, "Emulation.clearDeviceMetricsOverride").await;
    wait_for_method(&fake.commands, "Target.closeTarget").await;
    let captured = fake.commands.lock().clone();
    let rollback = captured
        .iter()
        .position(|command| command["method"] == "Emulation.clearDeviceMetricsOverride")
        .unwrap();
    let close_target = captured
        .iter()
        .position(|command| command["method"] == "Target.closeTarget")
        .unwrap();
    assert!(rollback < close_target);
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Target.closeTarget")
            .count(),
        1
    );
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Emulation.clearDeviceMetricsOverride")
            .count(),
        1
    );
    drop(captured);
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn ack_before_commit_does_not_publish_until_identity_is_confirmed() {
    let fake = start_server(ServerBehavior {
        delay_navigation_commit: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/published")
            .await
    });

    fake.navigation_ack_seen.notified().await;
    assert!(!creating.is_finished());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    fake.release_navigation_commit.notify_one();

    let page = creating.await.unwrap().unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event(),
        SessionEvent::PageCreated { target_id, .. } if target_id == page.target_id()
    ));
    let methods = methods(&fake.commands);
    assert_eq!(
        methods
            .iter()
            .filter(|method| method.as_str() == "Page.navigate")
            .count(),
        1
    );
    let navigate_index = methods
        .iter()
        .position(|method| method == "Page.navigate")
        .unwrap();
    assert_eq!(
        methods[navigate_index + 1..]
            .iter()
            .filter(|method| method.as_str() == "Page.getFrameTree")
            .count(),
        1,
        "new_page performs one final authoritative identity check"
    );
    assert!(
        !methods
            .iter()
            .any(|method| method.as_str() == "Runtime.evaluate"),
        "new_page must not wait for a load state"
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn default_page_close_waits_for_commit_publish_and_ownership_handoff() {
    let fake = start_server(ServerBehavior {
        delay_navigation_commit: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime.default_session().await.unwrap();
    let mut events = session.subscribe_events().await.unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/handoff")
            .await
    });

    fake.navigation_ack_seen.notified().await;
    let closing_session = session.clone();
    let closing = tokio::spawn(async move { closing_session.close().await });
    tokio::task::yield_now().await;
    assert!(
        !closing.is_finished(),
        "session close must wait for the admitted creation handoff"
    );
    fake.release_navigation_commit.notify_one();

    let page = creating.await.unwrap().unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event(),
        SessionEvent::PageCreated { target_id, .. } if target_id == page.target_id()
    ));
    let close_report = closing.await.unwrap();
    assert!(close_report.is_complete(), "{close_report:?}");
    let captured = fake.commands.lock().clone();
    assert_eq!(
        captured
            .iter()
            .filter(|command| command["method"] == "Target.closeTarget")
            .count(),
        1,
        "the pending target token must become retained Page ownership exactly once"
    );
    drop(captured);

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_target_creation_closes_target_after_delayed_response() {
    let fake = start_server(ServerBehavior {
        delay_method: Some("Target.createTarget"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/cancel-create")
            .await
    });
    fake.delayed_method_seen.notified().await;
    creating.abort();
    let _ = creating.await;
    fake.release_delayed_method.notify_one();

    wait_for_method(&fake.commands, "Target.closeTarget").await;
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_created_target_attach_closes_target_after_delayed_response() {
    let fake = start_server(ServerBehavior {
        delay_method: Some("Target.attachToTarget"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let creating_session = session.clone();
    let creating = tokio::spawn(async move {
        creating_session
            .new_page("https://example.test/cancel-attach")
            .await
    });
    fake.delayed_method_seen.notified().await;
    creating.abort();
    let _ = creating.await;
    fake.release_delayed_method.notify_one();

    wait_for_method(&fake.commands, "Target.closeTarget").await;
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_borrowed_target_attach_detaches_after_delayed_response() {
    let fake = start_server(ServerBehavior {
        delay_method: Some("Target.attachToTarget"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let attaching_session = session.clone();
    let attaching = tokio::spawn(async move { attaching_session.attach_page("target-1").await });
    fake.delayed_method_seen.notified().await;
    attaching.abort();
    let _ = attaching.await;
    fake.release_delayed_method.notify_one();

    wait_for_method(&fake.commands, "Target.detachFromTarget").await;
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

fn route_commands_for(commands: &[Value], session_id: &str) -> Vec<String> {
    commands
        .iter()
        .filter(|command| command.get("sessionId").and_then(Value::as_str) == Some(session_id))
        .map(|command| command["method"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn attach_existing_recursively_configures_paused_oopifs_before_return() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        emit_nested_oopif: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();

    let page = session.attach_page("target-1").await.unwrap();
    let captured = fake.commands.lock().clone();
    for session_id in ["page-session-1", "oopif-session-1", "oopif-session-2"] {
        let auto_attach = captured
            .iter()
            .find(|command| {
                command["method"] == "Target.setAutoAttach"
                    && command.get("sessionId").and_then(Value::as_str) == Some(session_id)
            })
            .unwrap_or_else(|| panic!("missing auto-attach for {session_id}"));
        assert_eq!(auto_attach["params"]["waitForDebuggerOnStart"], true);
    }
    for session_id in ["oopif-session-1", "oopif-session-2"] {
        let route = route_commands_for(&captured, session_id);
        let expected = [
            "Page.enable",
            "Page.getFrameTree",
            "Emulation.setLocaleOverride",
            "Emulation.setTimezoneOverride",
            "Network.setUserAgentOverride",
            "Emulation.setGeolocationOverride",
            "Network.enable",
            "Network.setExtraHTTPHeaders",
            "Security.enable",
            "Security.setIgnoreCertificateErrors",
            "Target.setAutoAttach",
            "Runtime.runIfWaitingForDebugger",
            "Page.getFrameTree",
        ];
        let mut cursor = 0;
        for method in expected {
            cursor += route[cursor..]
                .iter()
                .position(|candidate| candidate == method)
                .unwrap_or_else(|| panic!("missing ordered {method} for {session_id}: {route:?}"));
        }
        assert!(!route
            .iter()
            .any(|method| method == "Emulation.setDeviceMetricsOverride"));
    }
    assert!(captured.iter().any(|command| {
        command["method"] == "Network.setExtraHTTPHeaders"
            && command.get("sessionId").and_then(Value::as_str) == Some("oopif-session-2")
    }));
    drop(captured);

    assert_eq!(page.target_id(), "target-1");
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn oopif_route_failure_rolls_back_detaches_and_fails_initial_attach_without_publish() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        fail_oopif_method: Some("Network.setExtraHTTPHeaders"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let mut events = session.subscribe_events().await.unwrap();

    assert!(session.attach_page("target-1").await.is_err());
    wait_for_method(&fake.commands, "Target.detachFromTarget").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), events.next())
            .await
            .is_err()
    );
    let captured = fake.commands.lock().clone();
    let route = route_commands_for(&captured, "oopif-session-1");
    let failed = route
        .iter()
        .position(|method| method == "Network.setExtraHTTPHeaders")
        .unwrap();
    let rollback_end = route
        .iter()
        .rposition(|method| method == "Emulation.setLocaleOverride")
        .unwrap();
    assert!(failed < rollback_end);
    assert!(!route
        .iter()
        .any(|method| method == "Runtime.runIfWaitingForDebugger"));
    let oopif_detach = captured.iter().position(|command| {
        command["method"] == "Target.detachFromTarget"
            && command["params"]["sessionId"] == "oopif-session-1"
    });
    assert!(
        oopif_detach.is_some(),
        "OOPIF must be detached: {captured:?}"
    );
    drop(captured);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_initial_oopif_configuration_rolls_back_before_detaching_both_routes() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        hold_oopif_method: Some("Network.setExtraHTTPHeaders"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let attaching_session = session.clone();
    let attaching = tokio::spawn(async move { attaching_session.attach_page("target-1").await });
    fake.held_method_seen.notified().await;
    attaching.abort();
    let _ = attaching.await;

    wait_for_method(&fake.commands, "Emulation.clearGeolocationOverride").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let detached = {
                let captured = fake.commands.lock();
                let oopif = captured.iter().any(|command| {
                    command["method"] == "Target.detachFromTarget"
                        && command["params"]["sessionId"] == "oopif-session-1"
                });
                let main = captured.iter().any(|command| {
                    command["method"] == "Target.detachFromTarget"
                        && command["params"]["sessionId"] == "page-session-1"
                });
                oopif && main
            };
            if detached {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn no_route_configuration_preserves_non_pausing_oopif_wire_behavior() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();
    wait_for_method(&fake.commands, "Target.setAutoAttach").await;

    let captured = fake.commands.lock().clone();
    let main_auto_attach = captured
        .iter()
        .find(|command| {
            command["method"] == "Target.setAutoAttach"
                && command.get("sessionId").and_then(Value::as_str) == Some("page-session-1")
        })
        .unwrap();
    assert_eq!(main_auto_attach["params"]["waitForDebuggerOnStart"], false);
    let oopif = route_commands_for(&captured, "oopif-session-1");
    assert!(!oopif.iter().any(|method| matches!(
        method.as_str(),
        "Emulation.setLocaleOverride"
            | "Network.setUserAgentOverride"
            | "Network.setExtraHTTPHeaders"
            | "Security.setIgnoreCertificateErrors"
            | "Runtime.runIfWaitingForDebugger"
    )));
    drop(captured);

    assert_eq!(page.target_id(), "target-1");
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn future_oopif_replays_existing_runtime_and_network_managers_after_route_config() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        emit_oopif_after_main_autoattach: 2,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();
    let _page_events = page.subscribe_events().await.unwrap();
    let _network_events = page.subscribe_network_events().await.unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let ready = {
                let commands = fake.commands.lock();
                let resumed = commands.iter().any(|command| {
                    command["method"] == "Runtime.runIfWaitingForDebugger"
                        && command.get("sessionId").and_then(Value::as_str)
                            == Some("oopif-session-1")
                });
                let fenced = commands
                    .iter()
                    .filter(|command| {
                        command["method"] == "Page.getFrameTree"
                            && command.get("sessionId").and_then(Value::as_str)
                                == Some("oopif-session-1")
                    })
                    .count()
                    >= 2;
                resumed && fenced
            };
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let captured = fake.commands.lock().clone();
    let route = route_commands_for(&captured, "oopif-session-1");
    let configured = route
        .iter()
        .position(|method| method == "Network.setExtraHTTPHeaders")
        .unwrap();
    let runtime_enabled = route
        .iter()
        .position(|method| method == "Runtime.enable")
        .unwrap();
    let network_replayed = route
        .iter()
        .enumerate()
        .skip(runtime_enabled + 1)
        .find_map(|(index, method)| (method == "Network.enable").then_some(index))
        .unwrap();
    let nested_autoattach = route
        .iter()
        .position(|method| method == "Target.setAutoAttach")
        .unwrap();
    let resumed = route
        .iter()
        .position(|method| method == "Runtime.runIfWaitingForDebugger")
        .unwrap();
    let fenced = route
        .iter()
        .rposition(|method| method == "Page.getFrameTree")
        .unwrap();
    assert!(configured < runtime_enabled);
    assert!(runtime_enabled < network_replayed);
    assert!(network_replayed < nested_autoattach);
    assert!(nested_autoattach < resumed);
    assert!(resumed < fenced);
    drop(captured);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

fn assert_full_route_cleanup(commands: &[Value], session_id: &str, include_viewport: bool) {
    let routed = commands
        .iter()
        .filter(|command| command.get("sessionId").and_then(Value::as_str) == Some(session_id))
        .collect::<Vec<_>>();
    let start = routed
        .iter()
        .rposition(|command| {
            command["method"] == "Security.setIgnoreCertificateErrors"
                && command["params"]["ignore"] == false
        })
        .unwrap_or_else(|| panic!("missing HTTPS reset for {session_id}: {routed:?}"));
    let expected = if include_viewport {
        vec![
            "Security.setIgnoreCertificateErrors",
            "Security.disable",
            "Network.setExtraHTTPHeaders",
            "Network.disable",
            "Emulation.clearGeolocationOverride",
            "Network.setUserAgentOverride",
            "Emulation.setTimezoneOverride",
            "Emulation.setLocaleOverride",
            "Emulation.clearDeviceMetricsOverride",
        ]
    } else {
        vec![
            "Security.setIgnoreCertificateErrors",
            "Security.disable",
            "Network.setExtraHTTPHeaders",
            "Network.disable",
            "Emulation.clearGeolocationOverride",
            "Network.setUserAgentOverride",
            "Emulation.setTimezoneOverride",
            "Emulation.setLocaleOverride",
        ]
    };
    assert_eq!(
        routed[start..]
            .iter()
            .take(expected.len())
            .map(|command| command["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected,
        "route cleanup must reverse successful application for {session_id}"
    );
    let user_agent_reset = routed[start..]
        .iter()
        .find(|command| command["method"] == "Network.setUserAgentOverride")
        .expect("UA reset command");
    assert_eq!(user_agent_reset["params"]["userAgent"], "BrowserKit Test");
    assert!(user_agent_reset["params"].get("acceptLanguage").is_none());
    assert_eq!(
        routed[start + 2]["params"]["headers"],
        json!({}),
        "only browserkit-owned extra headers are reset"
    );
}

#[tokio::test]
async fn successful_attached_page_close_rolls_back_main_route_before_detach() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    let report = page.close().await;
    assert!(report.is_complete(), "{report:?}");
    let captured = fake.commands.lock().clone();
    assert_full_route_cleanup(&captured, "page-session-1", true);
    let reset = captured
        .iter()
        .position(|command| {
            command["method"] == "Emulation.clearDeviceMetricsOverride"
                && command.get("sessionId").and_then(Value::as_str) == Some("page-session-1")
        })
        .unwrap();
    let detach = captured
        .iter()
        .position(|command| command["method"] == "Target.detachFromTarget")
        .unwrap();
    assert!(
        reset < detach,
        "route rollback must precede attached-page detach"
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn successful_nested_oopif_routes_are_retained_and_cleaned_on_page_close() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        emit_nested_oopif: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    assert!(page.close().await.is_complete());
    let captured = fake.commands.lock().clone();
    assert_full_route_cleanup(&captured, "page-session-1", true);
    assert_full_route_cleanup(&captured, "oopif-session-1", false);
    assert_full_route_cleanup(&captured, "oopif-session-2", false);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn empty_route_options_emit_zero_route_cleanup_commands() {
    let fake = start_server(ServerBehavior::default()).await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    assert!(page.close().await.is_complete());
    let cleanup_methods = [
        "Emulation.clearDeviceMetricsOverride",
        "Emulation.clearGeolocationOverride",
        "Emulation.setLocaleOverride",
        "Emulation.setTimezoneOverride",
        "Network.setUserAgentOverride",
        "Network.setExtraHTTPHeaders",
        "Network.disable",
        "Security.setIgnoreCertificateErrors",
        "Security.disable",
    ];
    let captured = fake.commands.lock().clone();
    assert!(!captured
        .iter()
        .any(|command| { cleanup_methods.contains(&command["method"].as_str().unwrap()) }));
    drop(captured);

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn attached_close_reports_route_and_detach_failures_from_one_close_task() {
    let fake = start_server(ServerBehavior {
        fail_cleanup_methods: &["Network.disable"],
        fail_detach: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    let report = page.close().await;
    assert!(!report.is_complete());
    assert!(report.failures().iter().any(|failure| {
        failure.resource() == "route:page-session-1"
            && failure
                .message()
                .contains("injected cleanup Network.disable failure")
    }));
    assert!(report.failures().iter().any(|failure| {
        failure.resource() == "page:target-1"
            && failure.message().contains("injected detach failure")
    }));
    let captured = fake.commands.lock().clone();
    let network_failure = captured
        .iter()
        .position(|command| {
            command["method"] == "Network.disable"
                && command.get("sessionId").and_then(Value::as_str) == Some("page-session-1")
        })
        .unwrap();
    let detach = captured
        .iter()
        .position(|command| command["method"] == "Target.detachFromTarget")
        .unwrap();
    assert!(network_failure < detach);

    let _ = session.close().await;
    let _ = runtime.close().await;
    fake.server.await.unwrap();
}

#[tokio::test]
async fn created_page_close_continues_to_close_target_after_route_rollback_failure() {
    let fake = start_server(ServerBehavior {
        fail_cleanup_methods: &["Security.disable"],
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.new_page("https://example.test/").await.unwrap();

    let report = page.close().await;
    assert!(!report.is_complete());
    assert!(report
        .failures()
        .iter()
        .any(|failure| failure.resource() == "route:page-session-1"));
    assert!(report
        .closed_resources()
        .iter()
        .any(|resource| resource == "page:target-1"));
    let captured = fake.commands.lock().clone();
    let rollback = captured
        .iter()
        .position(|command| {
            command["method"] == "Security.disable"
                && command.get("sessionId").and_then(Value::as_str) == Some("page-session-1")
        })
        .unwrap();
    let close_target = captured
        .iter()
        .position(|command| command["method"] == "Target.closeTarget")
        .unwrap();
    assert!(rollback < close_target);

    let _ = session.close().await;
    let _ = runtime.close().await;
    fake.server.await.unwrap();
}

#[tokio::test]
async fn nested_route_cleanup_aggregates_every_failure_without_stopping_other_routes() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        emit_nested_oopif: true,
        fail_cleanup_methods: &["Security.disable", "Network.disable"],
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    let report = page.close().await;
    for session_id in ["page-session-1", "oopif-session-1", "oopif-session-2"] {
        let resource = format!("route:{session_id}");
        let failure = report
            .failures()
            .iter()
            .find(|failure| failure.resource() == resource)
            .unwrap_or_else(|| panic!("missing cleanup failure for {resource}: {report:?}"));
        assert!(failure.message().contains("Security.disable"));
        assert!(failure.message().contains("Network.disable"));
    }
    assert_eq!(
        fake.commands
            .lock()
            .iter()
            .filter(|command| command["method"] == "Network.disable")
            .count(),
        3
    );

    let _ = session.close().await;
    let _ = runtime.close().await;
    fake.server.await.unwrap();
}

#[tokio::test]
async fn cancelled_and_concurrent_page_close_share_one_route_cleanup_report() {
    let fake = start_server(ServerBehavior {
        delay_method: Some("Emulation.clearDeviceMetricsOverride"),
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();

    let first_page = page.clone();
    let first = tokio::spawn(async move { first_page.close().await });
    fake.delayed_method_seen.notified().await;
    let second_page = page.clone();
    let second = tokio::spawn(async move { second_page.close().await });
    first.abort();
    let _ = first.await;
    fake.release_delayed_method.notify_one();

    let report = second.await.unwrap();
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(page.close().await, report);
    assert_eq!(
        fake.commands
            .lock()
            .iter()
            .filter(|command| command["method"] == "Emulation.clearDeviceMetricsOverride")
            .count(),
        1
    );

    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn oopif_detach_racing_page_close_cleans_each_route_exactly_once() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        emit_oopif_detach_on_runtime_enable: true,
        fail_cleanup_methods: &["Network.disable"],
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let page = session.attach_page("target-1").await.unwrap();
    let _events = page.subscribe_events().await.unwrap();

    let report = page.close().await;
    for session_id in ["page-session-1", "oopif-session-1"] {
        let resource = format!("route:{session_id}");
        assert_eq!(
            report
                .failures()
                .iter()
                .filter(|failure| failure.resource() == resource)
                .count(),
            1,
            "detach/close race must preserve one cleanup outcome for {resource}: {report:?}"
        );
    }
    let captured = fake.commands.lock().clone();
    for session_id in ["page-session-1", "oopif-session-1"] {
        assert_eq!(
            captured
                .iter()
                .filter(|command| {
                    command["method"] == "Network.disable"
                        && command.get("sessionId").and_then(Value::as_str) == Some(session_id)
                })
                .count(),
            1,
            "route cleanup dispatched more than once for {session_id}"
        );
    }
    drop(captured);

    let _ = session.close().await;
    let _ = runtime.close().await;
    fake.server.await.unwrap();
}

#[tokio::test]
async fn session_root_close_drains_routes_before_disposing_context() {
    let fake = start_server(ServerBehavior {
        emit_oopif: true,
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let _page = session.attach_page("target-1").await.unwrap();

    let report = session.close().await;
    assert!(report.is_complete(), "{report:?}");
    let captured = fake.commands.lock().clone();
    assert_full_route_cleanup(&captured, "page-session-1", true);
    assert_full_route_cleanup(&captured, "oopif-session-1", false);
    let last_route_cleanup = captured
        .iter()
        .rposition(|command| {
            matches!(
                command["method"].as_str(),
                Some("Emulation.clearDeviceMetricsOverride" | "Emulation.setLocaleOverride")
            )
        })
        .unwrap();
    let dispose = captured
        .iter()
        .position(|command| command["method"] == "Target.disposeBrowserContext")
        .unwrap();
    assert!(last_route_cleanup < dispose);

    assert!(runtime.close().await.is_complete());
    fake.server.await.unwrap();
}

#[tokio::test]
async fn runtime_root_close_uses_session_close_route_cleanup_report() {
    let fake = start_server(ServerBehavior {
        fail_cleanup_methods: &["Security.disable"],
        ..ServerBehavior::default()
    })
    .await;
    let runtime = BrowserRuntime::connect(fake.endpoint.clone())
        .await
        .unwrap();
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(full_route_context()))
        .await
        .unwrap();
    let _page = session.attach_page("target-1").await.unwrap();

    let report = runtime.close().await;
    assert!(!report.is_complete());
    assert!(report.failures().iter().any(|failure| {
        failure.resource() == "route:page-session-1"
            && failure.message().contains("Security.disable")
    }));
    let captured = fake.commands.lock().clone();
    let rollback = captured
        .iter()
        .position(|command| command["method"] == "Security.disable")
        .unwrap();
    let dispose = captured
        .iter()
        .position(|command| command["method"] == "Target.disposeBrowserContext")
        .unwrap();
    assert!(
        rollback < dispose,
        "root close must continue after route failure"
    );
    drop(captured);

    fake.server.await.unwrap();
}
