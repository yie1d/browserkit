use browserkit::runtime::{
    AccessibilityArtifact, ArtifactBytes, ArtifactClip, ArtifactDimensions, ArtifactMetadata,
    AuthImportMode, AuthStateImport, AuthenticationState, BodyAvailability, BodyReadOptions,
    BrowserCookie, BrowserError, BrowserRuntime, BrowserSession, CookieDeletion, CookieSameSite,
    DefaultSessionOptions, DiagnosticBundle, DiagnosticBundleOptions, DiagnosticCollector,
    DiagnosticCollectorOptions, DiagnosticEvents, Dialog, DialogType, DocumentLoadState, Download,
    ElementSnapshot, Evaluation, EvaluationArgument, EventEnvelope, EventStreamCloseReason,
    FileChooser, Frame, FrameSnapshotView, HtmlArtifact, HtmlOptions, IsolatedSessionOptions,
    JavaScriptError, LoadState, Locator, LocatorCondition, LocatorMatch, LocatorQuery,
    NavigationExpectation, NavigationOptions, NavigationResult, NetworkEvent, NetworkEventStream,
    NetworkIdleOptions, NetworkObservationOptions, NetworkPredicate, NetworkRequestSnapshot,
    OriginStorageState, Page, PageEvent, PageSnapshot, PdfOptions, RemoteValue, RemoteValueHandle,
    RequestIdentity, RoleQuery, RuntimeEvent, RuntimeEventStream, ScreenshotFormat,
    ScreenshotOptions, SessionEvent, SnapshotOptions, SnapshotTruncation, StackFrame, StorageEntry,
    TestIdQuery, TextMatcher, TypedEventStream, WaitOptions,
};

#[allow(dead_code)]
async fn popup_api(page: &Page) -> Result<Page, BrowserError> {
    page.expect_popup(WaitOptions::default(), async { Ok(()) })
        .await
}

#[allow(dead_code)]
async fn dialog_api(page: &Page) -> Result<(), BrowserError> {
    let dialog = page
        .expect_dialog(WaitOptions::default(), async { Ok(()) })
        .await?;
    let _ = (dialog.message(), dialog.dialog_type(), dialog.frame_id());
    dialog.accept(Some("answer")).await
}

#[allow(dead_code)]
async fn file_chooser_api(page: &Page) -> Result<(), BrowserError> {
    let chooser = page
        .expect_file_chooser(WaitOptions::default(), async { Ok(()) })
        .await?;
    let _ = (
        chooser.frame_id(),
        chooser.backend_node_id(),
        chooser.allows_multiple(),
    );
    chooser.set_files(["C:/fixtures/a.txt"]).await
}
#[allow(dead_code)]
async fn download_api(page: &Page) -> Result<(), BrowserError> {
    let download = page
        .expect_download(WaitOptions::default(), async { Ok(()) })
        .await?;
    let _ = (
        download.guid(),
        download.url(),
        download.suggested_filename(),
        download.path_capability(),
    );
    download.wait().await?;
    Ok(())
}
use static_assertions::assert_impl_all;
use std::fmt::Debug;

use browserkit::runtime::{
    BrowserMetadata, Capability, CapabilityAvailability, CapabilityReason, CapabilityScope,
    CapabilitySet, CapabilityStatus, ConfigurationFailure, ContextOptions, Geolocation,
    HeadlessMode, HttpHeaders, LaunchOptions, PermissionName, PermissionOverride,
    PermissionSetting, ProxyOptions, RuntimeCapabilities, TargetRouteOptions, UserAgentOverride,
    VersionKnowledge, Viewport,
};

assert_impl_all!(Locator: Clone, Send, Sync);
assert_impl_all!(PageSnapshot: Clone, Send, Sync);
assert_impl_all!(FrameSnapshotView: Clone, Send, Sync);
assert_impl_all!(ElementSnapshot: Clone, Send, Sync);
assert_impl_all!(SnapshotOptions: Clone, Send, Sync);
assert_impl_all!(SnapshotTruncation: Clone, Send, Sync);
assert_impl_all!(NavigationOptions: Clone, Send, Sync);
assert_impl_all!(NavigationResult: Clone, Send, Sync);
assert_impl_all!(NavigationExpectation: Clone, Send, Sync);
assert_impl_all!(WaitOptions: Clone, Send, Sync);
assert_impl_all!(RuntimeEvent: Clone, Send, Sync);
assert_impl_all!(SessionEvent: Clone, Send, Sync);
assert_impl_all!(PageEvent: Clone, Send, Sync);
assert_impl_all!(EventEnvelope<PageEvent>: Clone, Send, Sync);
assert_impl_all!(TypedEventStream<PageEvent>: Send);
assert_impl_all!(RuntimeEventStream: Send);
assert_impl_all!(JavaScriptError: Clone, Send, Sync);
assert_impl_all!(StackFrame: Clone, Send, Sync);
assert_impl_all!(Dialog: Send, Sync);
assert_impl_all!(DialogType: Clone, Send, Sync);
assert_impl_all!(FileChooser: Send, Sync);
assert_impl_all!(Download: Clone, Send, Sync);
assert_impl_all!(NetworkEvent: Clone, Send, Sync);
assert_impl_all!(NetworkEventStream: Send);
assert_impl_all!(NetworkPredicate: Clone, Send, Sync);
assert_impl_all!(NetworkRequestSnapshot: Clone, Send, Sync);
assert_impl_all!(NetworkObservationOptions: Clone, Copy, Send, Sync);
assert_impl_all!(RequestIdentity: Clone, Send, Sync);
assert_impl_all!(BodyReadOptions: Clone, Send, Sync);
assert_impl_all!(BodyAvailability: Clone, Send, Sync);
assert_impl_all!(NetworkIdleOptions: Clone, Send, Sync);
assert_impl_all!(Evaluation: Clone, Send, Sync);
assert_impl_all!(EvaluationArgument: Clone, Send, Sync);
assert_impl_all!(RemoteValue: Clone, Send, Sync);
assert_impl_all!(RemoteValueHandle: Send, Sync);
assert_impl_all!(ArtifactBytes: Clone, Send, Sync);
assert_impl_all!(ArtifactClip: Clone, Copy, Send, Sync);
assert_impl_all!(ArtifactDimensions: Clone, Copy, Send, Sync);
assert_impl_all!(ArtifactMetadata: Clone, Send, Sync);
assert_impl_all!(ScreenshotOptions: Clone, Send, Sync);
assert_impl_all!(PdfOptions: Clone, Send, Sync);
assert_impl_all!(HtmlOptions: Clone, Send, Sync);
assert_impl_all!(HtmlArtifact: Clone, Send, Sync);
assert_impl_all!(AccessibilityArtifact: Clone, Send, Sync);
assert_impl_all!(DiagnosticCollector: Send);
assert_impl_all!(DiagnosticEvents: Clone, Send, Sync);
assert_impl_all!(DiagnosticBundle: Clone, Send, Sync);

#[test]
fn public_query_and_matcher_types_are_composable() {
    let role = LocatorQuery::Role(RoleQuery::new("button").with_name(TextMatcher::Exact {
        value: "Save".to_owned(),
        case_sensitive: false,
    }));
    let test_id = LocatorQuery::TestId(TestIdQuery::new("save-profile"));

    assert_eq!(role.as_role().unwrap().role(), "button");
    assert_eq!(test_id.as_test_id().unwrap().value(), "save-profile");
    assert_eq!(LocatorMatch::default(), LocatorMatch::Strict);
}

#[test]
fn snapshot_contract_is_bounded_and_available_at_every_scope() {
    let options = SnapshotOptions::default()
        .with_max_bytes(64 * 1024)
        .with_max_elements(250);
    assert_eq!(options.max_bytes(), 64 * 1024);
    assert_eq!(options.max_elements(), 250);

    fn compile_contract(page: &Page, frame: &Frame, locator: &Locator) {
        let page_future = page.snapshot(SnapshotOptions::default());
        let frame_future = frame.snapshot(SnapshotOptions::default());
        let region_future = locator.snapshot(SnapshotOptions::default());
        drop((page_future, frame_future, region_future));
    }

    let _ = compile_contract as fn(&Page, &Frame, &Locator);
    let _ = DocumentLoadState::Complete;
}

#[test]
fn page_and_frame_expose_the_same_scoped_locator_shape() {
    fn compile_contract(page: &Page, frame: &Frame) {
        let page_locator = page
            .locator("main")
            .locator(LocatorQuery::text(TextMatcher::contains("Ready", false)))
            .first();
        let frame_locator = frame
            .locator(LocatorQuery::xpath("//form"))
            .locator(LocatorQuery::placeholder(TextMatcher::exact(
                "Email", false,
            )))
            .nth(1);

        assert_eq!(page_locator.match_policy(), LocatorMatch::First);
        assert_eq!(frame_locator.match_policy(), LocatorMatch::Nth(1));
        assert_eq!(page_locator.queries().len(), 2);
        assert_eq!(frame_locator.queries().len(), 2);
    }

    let _ = compile_contract as fn(&Page, &Frame);
}

#[test]
fn action_contract_is_available_without_service_or_target_wrappers() {
    fn compile_contract(page: &Page, frame: &Frame, source: &Locator, target: &Locator) {
        let locator_actions = async {
            source.click().await?;
            source.double_click().await?;
            source.fill("replacement").await?;
            source.type_text("typed").await?;
            source.press("Control+A").await?;
            source.select(["open", "closed"]).await?;
            source.check().await?;
            source.uncheck().await?;
            source.hover().await?;
            source.focus().await?;
            source.blur().await?;
            source.scroll(0.0, 240.0).await?;
            source.scroll_into_view().await?;
            source.drag_to(target).await?;
            source.set_input_files(["C:/fixtures/a.txt"]).await?;
            Ok::<_, browserkit::runtime::BrowserError>(())
        };

        let scoped_primitives = async {
            page.type_text("page text").await?;
            page.press("Enter").await?;
            page.move_pointer(10.0, 20.0).await?;
            page.click_at(10.0, 20.0).await?;
            page.scroll(0.0, 100.0).await?;

            frame.type_text("frame text").await?;
            frame.press("Escape").await?;
            frame.move_pointer(5.0, 6.0).await?;
            frame.click_at(5.0, 6.0).await?;
            frame.scroll(0.0, -50.0).await?;
            Ok::<_, browserkit::runtime::BrowserError>(())
        };

        drop((locator_actions, scoped_primitives));
    }

    let _ = compile_contract as fn(&Page, &Frame, &Locator, &Locator);
}

#[test]
fn navigation_wait_and_expectation_api_stays_rust_native() {
    fn compile_contract(page: &Page, frame: &Frame, locator: &Locator) {
        drop(page.goto("https://example.test"));
        drop(page.goto(
            NavigationOptions::new("https://example.test").wait_until(LoadState::DomContentLoaded),
        ));
        drop(page.reload());
        drop(page.go_back());
        drop(page.go_forward());
        drop(page.wait_for_load_state(LoadState::Load, WaitOptions::default()));
        drop(page.wait_for_url(
            TextMatcher::contains("/orders", false),
            WaitOptions::default(),
        ));
        drop(page.wait_for_title(TextMatcher::exact("Orders", true), WaitOptions::default()));
        drop(frame.wait_for_dom_stability(WaitOptions::default()));
        drop(locator.wait(LocatorCondition::Visible, WaitOptions::default()));
        drop(locator.wait(
            LocatorCondition::Text(TextMatcher::contains("Ready", false)),
            WaitOptions::default(),
        ));
        drop(locator.wait(
            LocatorCondition::Attribute {
                name: "data-state".into(),
                value: Some(TextMatcher::exact("ready", true)),
            },
            WaitOptions::default(),
        ));
        drop(locator.wait(LocatorCondition::Count(1), WaitOptions::default()));
        let action = async { locator.click().await };
        drop(page.expect_navigation(
            NavigationExpectation::default().wait_until(LoadState::DomContentLoaded),
            action,
        ));
    }

    let _ = compile_contract as fn(&Page, &Frame, &Locator);
}

#[test]
fn typed_event_subscriptions_are_available_at_each_runtime_scope() {
    fn compile_contract(runtime: &BrowserRuntime, session: &BrowserSession, page: &Page) {
        let subscriptions = async {
            let runtime_events: RuntimeEventStream = runtime.subscribe_events().await?;
            let session_events = session.subscribe_events().await?;
            let page_events = page.subscribe_events().await?;
            Ok::<_, browserkit::runtime::BrowserError>((
                runtime_events,
                session_events,
                page_events,
            ))
        };
        drop(subscriptions);
    }

    let _ = compile_contract as fn(&BrowserRuntime, &BrowserSession, &Page);
    let reason = EventStreamCloseReason::Disconnected;
    assert_eq!(reason, EventStreamCloseReason::Disconnected);
}

#[test]
fn network_observation_is_page_and_frame_scoped_without_resource_filtering() {
    fn compile_contract(page: &Page, frame: &Frame, request: &RequestIdentity) {
        let contract = async {
            let page_events: NetworkEventStream = page.subscribe_network_events().await?;
            let frame_events: NetworkEventStream = frame.subscribe_network_events().await?;
            page.wait_for_network(
                NetworkPredicate::new()
                    .url(TextMatcher::contains("/orders", false))
                    .method("POST")
                    .resource_type("Fetch")
                    .status(200)
                    .request_header("content-type", TextMatcher::contains("json", false))
                    .response_header("x-request-id", TextMatcher::contains("req-", true)),
                WaitOptions::default(),
            )
            .await?;
            page.wait_for_network_idle(NetworkIdleOptions::default())
                .await?;
            let _ = page
                .read_response_body(request, BodyReadOptions::new(1024 * 1024))
                .await?;
            let _ = page
                .read_request_body(request, BodyReadOptions::new(64 * 1024))
                .await?;
            Ok::<_, BrowserError>((page_events, frame_events))
        };
        drop(contract);
        let action = async { Ok(()) };
        drop(page.expect_network(
            NetworkPredicate::new().url(TextMatcher::contains("/api/", false)),
            WaitOptions::default(),
            action,
        ));
    }

    let _ = compile_contract as fn(&Page, &Frame, &RequestIdentity);
}

#[test]
fn network_retention_is_explicit_session_configuration() {
    let policy = NetworkObservationOptions::default()
        .retained_state_max_bytes(8 * 1024 * 1024)
        .retained_state_ttl(std::time::Duration::from_secs(10));
    let options = DefaultSessionOptions::default().network_observation(policy);
    assert_eq!(options.network_observation_options(), policy);
    assert_eq!(
        NetworkObservationOptions::DEFAULT_RETAINED_STATE_MAX_BYTES,
        16 * 1024 * 1024
    );
    assert_eq!(
        NetworkObservationOptions::DEFAULT_RETAINED_STATE_TTL,
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn evaluation_is_scoped_and_does_not_require_string_interpolation() {
    fn compile_contract(page: &Page, frame: &Frame, handle: &RemoteValueHandle) {
        let plain = async {
            let answer: i64 = page.evaluate("globalThis.appAnswer").await?;
            let value = frame
                .evaluate_value(
                    Evaluation::function("function(left, right) { return left + right; }")
                        .argument(EvaluationArgument::json(20)?)
                        .argument(EvaluationArgument::json(22)?),
                )
                .await?;
            Ok::<_, BrowserError>((answer, value))
        };
        let advanced = async {
            let object = page
                .evaluate_handle("({ nested: { answer: 42 }, add(value) { return this.nested.answer + value; } })")
                .await?;
            let nested = object.property("nested").await?;
            let json = nested.json_value().await?;
            let called = object
                .call(
                    "function(value) { return this.add(value); }",
                    [EvaluationArgument::json(8)?],
                )
                .await?;
            let _ = (object.type_name(), object.subtype(), object.description());
            called.release().await?;
            nested.release().await?;
            object.release().await?;
            Ok::<_, BrowserError>(json)
        };
        drop((plain, advanced, handle));
    }

    let _ = compile_contract as fn(&Page, &Frame, &RemoteValueHandle);
    let values = [
        RemoteValue::Undefined,
        RemoteValue::Null,
        RemoteValue::NaN,
        RemoteValue::Infinity,
        RemoteValue::NegativeInfinity,
        RemoteValue::NegativeZero,
        RemoteValue::BigInt("9007199254740993".into()),
    ];
    assert_eq!(values.len(), 7);
}

#[test]
fn storage_and_auth_state_are_explicit_scoped_and_secret_safe() {
    fn compile_contract(session: &BrowserSession, page: &Page, frame: &Frame) {
        let cookie = BrowserCookie::new("session", "secret")
            .url("https://example.test/")
            .path("/")
            .http_only(true)
            .secure(true)
            .same_site(CookieSameSite::Lax);
        let contract = async {
            let _cookies = session.cookies().await?;
            session.set_cookie(cookie.clone()).await?;
            session
                .delete_cookie(CookieDeletion::new("session").url("https://example.test/"))
                .await?;

            let local = page.local_storage();
            local.set("token", "secret").await?;
            let _ = local.get("token").await?;
            let _: Vec<StorageEntry> = local.list().await?;
            local.remove("token").await?;

            let per_document = frame.session_storage();
            per_document.clear().await?;

            let state = session
                .export_auth_state(std::slice::from_ref(page))
                .await?;
            session
                .import_auth_state(
                    &state,
                    AuthStateImport::new(AuthImportMode::Merge).page(page.clone()),
                )
                .await?;
            Ok::<_, BrowserError>(())
        };
        drop(contract);

        assert!(!format!("{cookie:?}").contains("secret"));
        let entry = StorageEntry::new("token", "secret");
        assert!(!format!("{entry:?}").contains("secret"));
        let _empty_state = AuthenticationState::new();
        let state = AuthenticationState::from_parts(
            vec![cookie],
            vec![OriginStorageState::new("https://example.test", vec![entry])],
        );
        let encoded = serde_json::to_string(&state).unwrap();
        assert!(encoded.contains("version"));
        assert!(!format!("{state:?}").contains("secret"));
    }

    let _ = compile_contract as fn(&BrowserSession, &Page, &Frame);
}

#[test]
fn artifacts_are_typed_retained_byte_bounded_and_available_at_their_natural_scope() {
    let metadata = ArtifactMetadata {
        encoded_bytes: 4,
        css_clip: Some(ArtifactClip {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }),
        full_page: false,
    };
    assert_eq!(metadata.encoded_bytes, 4);

    fn compile_contract(page: &Page, frame: &Frame, locator: &Locator) {
        let screenshot = ScreenshotOptions::default()
            .format(ScreenshotFormat::Webp)
            .quality(80)
            .max_bytes(8 * 1024 * 1024);
        drop(page.screenshot(screenshot.clone()));
        drop(page.screenshot(screenshot.clone().full_page(true)));
        drop(frame.screenshot(screenshot.clone()));
        drop(locator.screenshot(screenshot));
        drop(page.pdf(PdfOptions::default().print_background(true)));
        drop(page.html(HtmlOptions::default().max_bytes(2 * 1024 * 1024)));
        drop(frame.html(HtmlOptions::default()));
        drop(page.accessibility_artifact(SnapshotOptions::default()));
    }

    fn artifact_bytes_contract(artifact: &ArtifactBytes, path: &std::path::Path) {
        let _: &[u8] = artifact.as_bytes();
        let _: &str = artifact.mime_type();
        let _: Option<ArtifactDimensions> = artifact.dimensions();
        let _: &ArtifactMetadata = artifact.metadata();
        drop(artifact.save(path));
    }

    fn consume_artifact_bytes(artifact: ArtifactBytes) -> Vec<u8> {
        artifact.into_bytes()
    }

    let _ = compile_contract as fn(&Page, &Frame, &Locator);
    let _ = artifact_bytes_contract as fn(&ArtifactBytes, &std::path::Path);
    let _ = consume_artifact_bytes as fn(ArtifactBytes) -> Vec<u8>;
}

#[test]
fn diagnostics_require_an_explicit_bounded_collection_window() {
    fn compile_contract(page: &Page) {
        let collect = async {
            let collector = page
                .start_diagnostic_collector(
                    DiagnosticCollectorOptions::default()
                        .max_events(240)
                        .max_bytes(512 * 1024)
                        .max_duration(std::time::Duration::from_secs(3)),
                )
                .await?;
            let events = collector.finish().await;
            let bundle = page
                .diagnostic_bundle(
                    DiagnosticBundleOptions::default().include_screenshot(true),
                    events,
                )
                .await?;
            Ok::<_, BrowserError>(bundle)
        };
        drop(collect);
    }

    let _ = compile_contract as fn(&Page);
}

assert_impl_all!(Capability: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(CapabilityAvailability: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(CapabilityReason: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(CapabilityScope: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(CapabilityStatus: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(CapabilitySet: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(BrowserMetadata: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(VersionKnowledge: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(HeadlessMode: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(RuntimeCapabilities: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(ConfigurationFailure: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(ContextOptions: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(TargetRouteOptions: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(Viewport: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(Geolocation: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(UserAgentOverride: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(HttpHeaders: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(PermissionName: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(PermissionSetting: Clone, Copy, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(PermissionOverride: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(ProxyOptions: Clone, Debug, Eq, std::hash::Hash, Send, Sync);
assert_impl_all!(LaunchOptions: Clone, Debug, Send, Sync);

#[test]
fn runtime_foundation_queries_are_synchronous_and_immutable() {
    fn compile_contract(runtime: &BrowserRuntime) {
        let capabilities: &RuntimeCapabilities = runtime.capabilities();
        let metadata: &BrowserMetadata = capabilities.metadata();
        let set: &CapabilitySet = capabilities.for_scope(CapabilityScope::DefaultContext);
        let status: &CapabilityStatus = set.status(Capability::DownloadObservation);
        let _: CapabilityScope = status.scope();
        let _ = (metadata, status.availability(), status.reason());
    }
    let _ = compile_contract as fn(&BrowserRuntime);
}

#[test]
fn context_configuration_is_immutable_session_configuration() {
    let route = TargetRouteOptions::default()
        .locale("en-US")
        .unwrap()
        .timezone("Europe/London")
        .unwrap()
        .viewport(Viewport::new(1280, 720).unwrap());
    let context = ContextOptions::default().target_route(route);
    let proxy = ProxyOptions::new("http://proxy.test:8080").unwrap();

    let default = DefaultSessionOptions::default().context(context.clone());
    assert_eq!(default.context_options(), &context);

    let isolated = IsolatedSessionOptions::default()
        .context(context.clone())
        .proxy(proxy.clone());
    assert_eq!(isolated.context_options(), &context);
    assert_eq!(isolated.proxy_options(), Some(&proxy));

    fn session_snapshot(session: &BrowserSession) {
        let _: &ContextOptions = session.context_options();
        let _: &CapabilitySet = session.capabilities();
    }
    let _ = session_snapshot as fn(&BrowserSession);
}

#[test]
fn capability_failures_are_structured_preflight_errors() {
    fn compile_contract(error: &BrowserError) {
        let _: Option<&CapabilityStatus> = error.capability_status();
    }
    let _ = compile_contract as fn(&BrowserError);
    let _typed_default_mismatch = ConfigurationFailure::ImmutableDefaultSessionOptions;
    let _typed_unsupported = ConfigurationFailure::UnsupportedCapability {
        capability: Capability::Pdf,
        reason: CapabilityReason::HeadlessBrowserRequired,
    };
    let _launch_scope = CapabilityScope::BrowserLaunch;
    let _context_creation_scope = CapabilityScope::BrowserContextCreation;
}
