use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use browserkit::runtime::{
    ActionCompletion, BrowserCookie, BrowserRuntime, Capability, CapabilityAvailability,
    CapabilityReason, CapabilityScope, ContextOptions, CookiePartitionKey, CookieSameSite,
    DefaultSessionOptions, DownloadPathCapability, DownloadTerminal, Geolocation, HttpHeaders,
    IsolatedSessionOptions, LaunchOptions, LoadState, NavigationOptions, NavigationResult,
    OperationPhase, Page, PageEvent, PermissionName, PermissionOverride, PermissionSetting,
    ProxyOptions, TargetRouteOptions, UserAgentOverride, Viewport, WaitOptions,
};

use cdpkit::target::methods::{GetBrowserContexts, GetTargets};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const CHROME_TEST_TIMEOUT: Duration = Duration::from_secs(15);

type ResponseFactory = Arc<dyn Fn(&str) -> FixtureResponse + Send + Sync>;

#[derive(Debug, Clone)]
struct RecordedRequest {
    request_target: String,
    headers: BTreeMap<String, String>,
}

struct FixtureResponse {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl FixtureResponse {
    fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: vec![("Content-Type", "text/html; charset=utf-8")],
            body: body.into(),
        }
    }

    fn download(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            headers: vec![
                ("Content-Type", "application/octet-stream"),
                ("Content-Disposition", "attachment; filename=task12.bin"),
            ],
            body: body.into(),
        }
    }
}

struct LoopbackFixture {
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl LoopbackFixture {
    async fn start(factory: ResponseFactory) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let factory = Arc::clone(&factory);
                tokio::spawn(async move {
                    let _ = serve_http_connection(stream, requests, factory).await;
                });
            }
        });
        Self {
            port,
            requests,
            task,
        }
    }

    fn origin(&self, host: &str) -> String {
        format!("http://{host}:{}", self.port)
    }

    async fn request_for(&self, suffix: &str) -> RecordedRequest {
        tokio::time::timeout(CHROME_TEST_TIMEOUT, async {
            loop {
                if let Some(request) = self
                    .requests
                    .lock()
                    .iter()
                    .find(|request| request.request_target.ends_with(suffix))
                    .cloned()
                {
                    return request;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fixture did not receive request ending in {suffix}"))
    }

    fn abort(self) {
        self.task.abort();
    }
}

async fn serve_http_connection(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    factory: ResponseFactory,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let request = String::from_utf8_lossy(&bytes);
    let mut lines = request.split("\r\n");
    let request_target = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    requests.lock().push(RecordedRequest {
        request_target: request_target.clone(),
        headers,
    });

    let response = factory(&request_target);
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in response.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

fn chrome_args(ports: &[u16]) -> Vec<String> {
    let allowed = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "--disable-background-networking".to_owned(),
        "--disable-component-update".to_owned(),
        "--disable-default-apps".to_owned(),
        "--disable-sync".to_owned(),
        format!("--explicitly-allowed-ports={allowed}"),
    ]
}

fn launch_options(profile: &TempDir, ports: &[u16]) -> LaunchOptions {
    chrome_args(ports).into_iter().fold(
        LaunchOptions::default()
            .headless(true)
            .user_data_dir(profile.path()),
        LaunchOptions::arg,
    )
}

fn configured_route(width: u32, height: u32) -> ContextOptions {
    let route = TargetRouteOptions::default()
        .viewport(Viewport::new(width, height).expect("valid viewport"))
        .locale("fr-CA")
        .expect("valid locale")
        .timezone("America/Toronto")
        .expect("valid timezone")
        .user_agent(
            UserAgentOverride::new("BrowserKit-Task12/1.0")
                .expect("valid user agent")
                .accept_language("fr-CA,fr")
                .expect("valid ordered browser language tags")
                .platform("Task12")
                .expect("valid platform"),
        )
        .geolocation(
            Geolocation::new(45.5017, -73.5673)
                .expect("valid coordinates")
                .accuracy(3.0)
                .expect("valid accuracy"),
        )
        .http_headers(HttpHeaders::new([("x-browserkit-task", "task12")]).unwrap());
    ContextOptions::default().target_route(route)
}

fn configured_context(origin: &str, width: u32, height: u32) -> ContextOptions {
    configured_route(width, height).permission(
        PermissionOverride::new(PermissionName::Geolocation, PermissionSetting::Allow)
            .origin(origin)
            .expect("valid permission origin"),
    )
}

async fn goto_loaded_document(page: &Page, url: impl Into<String>) -> NavigationResult {
    let url = url.into();
    let previous_epoch = page
        .main_frame()
        .await
        .expect("resolve pre-navigation main frame")
        .document_epoch();
    let navigation = page
        .goto(
            NavigationOptions::new(url.clone())
                .wait_until(LoadState::Load)
                .timeout(CHROME_TEST_TIMEOUT),
        )
        .await
        .expect("target document committed and loaded");
    assert_eq!(navigation.requested_url(), Some(url.as_str()));
    assert_eq!(navigation.final_url(), url);
    assert!(
        navigation.loader_id().is_some(),
        "target document fence must observe a cross-document loader"
    );
    let committed_epoch = page
        .main_frame()
        .await
        .expect("resolve committed main frame")
        .document_epoch();
    assert!(
        committed_epoch > previous_epoch,
        "target document commit did not advance the epoch: {previous_epoch:?} -> {committed_epoch:?}"
    );
    navigation
}

fn assert_available(runtime: &BrowserRuntime, scope: CapabilityScope, capability: Capability) {
    assert_eq!(
        runtime
            .capabilities()
            .status(scope, capability)
            .availability(),
        CapabilityAvailability::Available,
        "unexpected capability status for {scope:?}/{capability:?}"
    );
}

async fn browser_facts(page: &browserkit::runtime::Page) -> Value {
    page.evaluate(
        r#"(async () => {
            const position = await new Promise((resolve, reject) =>
                navigator.geolocation.getCurrentPosition(
                    value => resolve({
                        latitude: value.coords.latitude,
                        longitude: value.coords.longitude,
                        accuracy: value.coords.accuracy
                    }),
                    error => reject(new Error(`${error.code}:${error.message}`))
                )
            );
            return {
                width: innerWidth,
                height: innerHeight,
                userAgent: navigator.userAgent,
                language: navigator.language,
                languages: navigator.languages,
                timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
                position
            };
        })()"#,
    )
    .await
    .expect("evaluate configured browser facts")
}

#[tokio::test]
#[ignore = "requires installed Chrome; uses only a private profile and loopback HTTP"]
async fn chrome_launched_isolated_context_applies_first_request_and_js_facts() {
    let fixture = LoopbackFixture::start(Arc::new(|_| {
        FixtureResponse::html("<html><body>configured</body></html>")
    }))
    .await;
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-launched-")
        .tempdir()
        .unwrap();
    let runtime = BrowserRuntime::launch(launch_options(&profile, &[fixture.port]))
        .await
        .expect("launch private Chrome");

    for capability in [
        Capability::RequestRouting,
        Capability::PermissionOverrides,
        Capability::IgnoreHttpsErrors,
        Capability::DownloadObservation,
    ] {
        assert_available(&runtime, CapabilityScope::DefaultContext, capability);
        assert_available(&runtime, CapabilityScope::IsolatedContext, capability);
    }
    let default_proxy = runtime
        .capabilities()
        .status(CapabilityScope::DefaultContext, Capability::Proxy);
    assert_eq!(
        default_proxy.availability(),
        CapabilityAvailability::Conditional
    );
    assert_eq!(
        default_proxy.reason(),
        Some(CapabilityReason::RequiresBrowserLaunchConfiguration)
    );
    assert_eq!(default_proxy.scope(), CapabilityScope::BrowserLaunch);
    assert_available(
        &runtime,
        CapabilityScope::IsolatedContext,
        Capability::Proxy,
    );

    let default = runtime
        .default_session_with(DefaultSessionOptions::default().context(
            ContextOptions::default().target_route(
                TargetRouteOptions::default().viewport(Viewport::new(700, 500).unwrap()),
            ),
        ))
        .await
        .expect("launched default context supports route configuration");
    assert_eq!(
        default.capabilities(),
        runtime
            .capabilities()
            .for_scope(CapabilityScope::DefaultContext)
    );
    let default_page = default.new_page("about:blank").await.unwrap();
    goto_loaded_document(&default_page, "about:blank").await;
    let default_size: Value = default_page
        .evaluate("({width: innerWidth, height: innerHeight})")
        .await
        .unwrap();
    assert_eq!(default_size, json!({"width": 700, "height": 500}));
    assert!(default_page.close().await.is_complete());

    let origin = fixture.origin("127.0.0.1");
    let isolated = runtime
        .isolated_session(
            IsolatedSessionOptions::default().context(configured_context(&origin, 912, 678)),
        )
        .await
        .expect("configured isolated context");
    assert_eq!(
        isolated.capabilities(),
        runtime
            .capabilities()
            .for_scope(CapabilityScope::IsolatedContext)
    );
    let configured_url = format!("{origin}/configured");
    let page = isolated
        .new_page("about:blank")
        .await
        .expect("create configured page without a business request");
    let navigation = goto_loaded_document(&page, configured_url.clone()).await;
    assert_eq!(navigation.final_url(), configured_url);
    let request = fixture.request_for("/configured").await;
    assert_eq!(
        request.headers.get("user-agent").map(String::as_str),
        Some("BrowserKit-Task12/1.0")
    );
    assert_eq!(
        request.headers.get("accept-language").map(String::as_str),
        Some("fr-CA,fr;q=0.9")
    );
    assert_eq!(
        request.headers.get("x-browserkit-task").map(String::as_str),
        Some("task12")
    );

    let facts = browser_facts(&page).await;
    assert_eq!(facts["width"], 912);
    assert_eq!(facts["height"], 678);
    assert_eq!(facts["userAgent"], "BrowserKit-Task12/1.0");
    assert_eq!(facts["language"], "fr-CA");
    assert_eq!(facts["languages"], json!(["fr-CA", "fr"]));
    assert_eq!(facts["timezone"], "America/Toronto");
    assert!((facts["position"]["latitude"].as_f64().unwrap() - 45.5017).abs() < 0.0001);
    assert!((facts["position"]["longitude"].as_f64().unwrap() + 73.5673).abs() < 0.0001);
    assert_eq!(facts["position"]["accuracy"], 3.0);

    assert!(page.close().await.is_complete());
    assert!(isolated.close().await.is_complete());
    assert!(default.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fixture.abort();
}

#[tokio::test]
#[ignore = "requires installed Chrome; uses private profile, loopback origins, and forced OOPIFs"]
async fn chrome_nested_future_oopif_routes_publish_configured_facts_without_viewport_override() {
    let nested = LoopbackFixture::start(Arc::new(|_| {
        FixtureResponse::html("<html><body data-kind='nested'>nested</body></html>")
    }))
    .await;
    let nested_url = format!("{}/nested", nested.origin("nested.test"));
    let child = LoopbackFixture::start(Arc::new(move |_| {
        FixtureResponse::html(format!(
            "<html><body data-kind='child'>child<iframe src={nested_url:?}></iframe></body></html>"
        ))
    }))
    .await;
    let parent = LoopbackFixture::start(Arc::new(|_| {
        FixtureResponse::html("<html><body data-kind='parent'>parent</body></html>")
    }))
    .await;
    let ports = [parent.port, child.port, nested.port];
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-oopif-")
        .tempdir()
        .unwrap();
    let options = chrome_args(&ports).into_iter().fold(
        LaunchOptions::default()
            .headless(true)
            .user_data_dir(profile.path())
            .arg("--site-per-process")
            .arg("--host-resolver-rules=MAP *.test 127.0.0.1"),
        LaunchOptions::arg,
    );
    let runtime = BrowserRuntime::launch(options)
        .await
        .expect("launch private Chrome");
    let parent_origin = parent.origin("parent.test");
    let session = runtime
        .isolated_session(IsolatedSessionOptions::default().context(configured_route(1000, 740)))
        .await
        .expect("configured isolated context");
    let page = session.new_page("about:blank").await.expect("open page");
    goto_loaded_document(&page, format!("{parent_origin}/parent")).await;
    let mut route_events = page
        .subscribe_events()
        .await
        .expect("subscribe before creating future OOPIF routes");

    let child_url = format!("{}/child", child.origin("child.test"));
    let add_child = format!(
        "(() => {{ const frame = document.createElement('iframe'); frame.src = {child_url:?}; document.body.append(frame); return true; }})()"
    );
    assert!(page.evaluate::<bool>(add_child).await.unwrap());

    let route_deadline = Instant::now() + CHROME_TEST_TIMEOUT;
    let mut routed_frames = BTreeSet::new();
    while routed_frames.len() < 2 {
        let envelope = tokio::time::timeout_at(route_deadline, route_events.next())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "deadline waiting for child and nested OOPIF route commits; observed routes: {routed_frames:?}"
                )
            })
            .expect("page event stream closed before nested OOPIF routes committed")
            .unwrap_or_else(|error| panic!("page event stream failed before nested OOPIF routes committed: {error}"));
        match envelope.into_event() {
            PageEvent::FrameRouteChanged { frame_id, .. } => {
                routed_frames.insert(frame_id.as_str().to_owned());
            }
            PageEvent::FrameDetached { frame_id } if routed_frames.contains(frame_id.as_str()) => {
                panic!("routed OOPIF frame {frame_id} detached before facts were evaluated");
            }
            _ => {}
        }
    }

    let frames = page
        .frames()
        .await
        .expect("list active frames after route commit events");
    let active_frame_ids = frames
        .iter()
        .map(|frame| frame.id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(active_frame_ids.len(), 3, "expected three active frames");
    assert!(
        routed_frames.is_subset(&active_frame_ids),
        "route events must identify active frames: routes={routed_frames:?}, active={active_frame_ids:?}"
    );

    let mut frame_facts = BTreeMap::new();
    for frame in frames {
        let frame_id = frame.id().as_str().to_owned();
        let evaluation_deadline = Instant::now() + CHROME_TEST_TIMEOUT;
        let value = tokio::time::timeout_at(
            evaluation_deadline,
            frame.evaluate::<Value>(
                "({kind: document.body && document.body.dataset.kind, width: innerWidth, userAgent: navigator.userAgent, language: navigator.language, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone})",
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("deadline evaluating committed active frame {frame_id}"))
        .unwrap_or_else(|error| {
            panic!("evaluate committed active frame {frame_id} failed without retry: {error}")
        });
        let kind = value["kind"]
            .as_str()
            .unwrap_or_else(|| panic!("active frame {frame_id} did not publish a document kind"))
            .to_owned();
        assert!(
            frame_facts.insert(kind.clone(), value).is_none(),
            "duplicate frame facts for {kind}"
        );
    }
    assert_eq!(
        frame_facts.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from(["child".to_owned(), "nested".to_owned(), "parent".to_owned()])
    );

    assert_eq!(frame_facts["parent"]["width"], 1000);
    for kind in ["parent", "child", "nested"] {
        assert_eq!(frame_facts[kind]["userAgent"], "BrowserKit-Task12/1.0");
        assert_eq!(frame_facts[kind]["language"], "fr-CA");
        assert_eq!(frame_facts[kind]["timezone"], "America/Toronto");
    }
    assert_ne!(frame_facts["child"]["width"], 1000);
    assert_ne!(frame_facts["nested"]["width"], 1000);

    assert!(page.close().await.is_complete());
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    parent.abort();
    child.abort();
    nested.abort();
}

#[tokio::test]
#[ignore = "requires installed Chrome; proxy and destination are both loopback-contained"]
async fn chrome_isolated_context_proxy_observes_first_request_without_public_network() {
    let proxy = LoopbackFixture::start(Arc::new(|_| {
        FixtureResponse::html("<html><body>proxied</body></html>")
    }))
    .await;
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-proxy-")
        .tempdir()
        .unwrap();
    let runtime = BrowserRuntime::launch(launch_options(&profile, &[proxy.port]))
        .await
        .expect("launch private Chrome");
    assert_available(
        &runtime,
        CapabilityScope::IsolatedContext,
        Capability::Proxy,
    );
    let session = runtime
        .isolated_session(
            IsolatedSessionOptions::default().proxy(
                ProxyOptions::new(format!("http://127.0.0.1:{}", proxy.port))
                    .expect("valid loopback proxy"),
            ),
        )
        .await
        .expect("create proxied BrowserContext");
    let proxy_url = "http://proxy-target.invalid/task12-proxy";
    let page = session
        .new_page("about:blank")
        .await
        .expect("create proxied page without a business request");
    let navigation = goto_loaded_document(&page, proxy_url).await;
    assert_eq!(navigation.final_url(), proxy_url);
    let request = proxy.request_for("/task12-proxy").await;
    assert_eq!(
        request.request_target,
        "http://proxy-target.invalid/task12-proxy"
    );
    let body: String = page
        .evaluate("document.body.textContent")
        .await
        .expect("evaluate proxy response");
    assert_eq!(body, "proxied");

    assert!(page.close().await.is_complete());
    assert!(session.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    proxy.abort();
}

struct AttachedChrome {
    child: Child,
    _profile: TempDir,
    endpoint: String,
}

impl AttachedChrome {
    async fn start(ports: &[u16]) -> Self {
        let profile = tempfile::Builder::new()
            .prefix("browserkit-task12-attached-")
            .tempdir()
            .unwrap();
        let executable = chrome_executable();
        let mut command = Command::new(executable);
        command
            .arg("--headless=new")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args(chrome_args(ports))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("start dedicated attached Chrome");
        let endpoint = wait_for_devtools_endpoint(&mut child, profile.path()).await;
        Self {
            child,
            _profile: profile,
            endpoint,
        }
    }

    async fn stop(mut self) {
        match self.child.kill().await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(error) => panic!("stop dedicated attached Chrome: {error}"),
        }
        let _ = self.child.wait().await;
    }
}

fn chrome_executable() -> PathBuf {
    if let Some(path) = env::var_os("BROWSERKIT_CHROME_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return path;
        }
        panic!("BROWSERKIT_CHROME_PATH is not a file: {}", path.display());
    }

    let mut candidates = Vec::new();
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = env::var_os(variable) {
            let root = PathBuf::from(root);
            candidates.push(root.join("Google/Chrome/Application/chrome.exe"));
            candidates.push(root.join("Chromium/Application/chrome.exe"));
            candidates.push(root.join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    candidates.extend(
        [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .expect("installed Chrome/Chromium; or set BROWSERKIT_CHROME_PATH")
}

async fn wait_for_devtools_endpoint(child: &mut Child, profile: &Path) -> String {
    let deadline = Instant::now() + CHROME_TEST_TIMEOUT;
    let port_file = profile.join("DevToolsActivePort");
    loop {
        if let Some(status) = child.try_wait().expect("poll dedicated Chrome") {
            panic!("dedicated Chrome exited before DevToolsActivePort: {status}");
        }
        if let Ok(contents) = tokio::fs::read_to_string(&port_file).await {
            let mut lines = contents.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if port.parse::<u16>().is_ok() && path.starts_with('/') {
                    return format!("ws://127.0.0.1:{port}{path}");
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "dedicated Chrome did not publish DevToolsActivePort"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn browser_footprint(runtime: &BrowserRuntime) -> (BTreeSet<String>, BTreeSet<String>) {
    let contexts = GetBrowserContexts::new()
        .send(runtime.cdp())
        .await
        .unwrap()
        .browser_context_ids
        .into_iter()
        .map(|context| context.as_str().to_owned())
        .collect();
    let page_targets = GetTargets::new()
        .send(runtime.cdp())
        .await
        .unwrap()
        .target_infos
        .into_iter()
        .filter(|target| target.type_ == "page")
        .map(|target| target.target_id.as_str().to_owned())
        .collect();
    (contexts, page_targets)
}

async fn wait_for_stable_browser_footprint(
    runtime: &BrowserRuntime,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let deadline = Instant::now() + CHROME_TEST_TIMEOUT;
    let mut footprint = browser_footprint(runtime).await;
    let mut stable_since = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let current = browser_footprint(runtime).await;
        if current == footprint {
            if stable_since.elapsed() >= Duration::from_millis(100) {
                return current;
            }
        } else {
            footprint = current;
            stable_since = Instant::now();
        }
        assert!(
            Instant::now() < deadline,
            "dedicated Chrome browser footprint did not stabilize"
        );
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome; starts and owns a dedicated attached-mode process"]
async fn chrome_attached_private_process_preflights_default_and_supports_isolated() {
    let fixture = LoopbackFixture::start(Arc::new(|_| {
        FixtureResponse::html("<html><body>attached isolated</body></html>")
    }))
    .await;
    let mut chrome = AttachedChrome::start(&[fixture.port]).await;
    let runtime = BrowserRuntime::connect(chrome.endpoint.clone())
        .await
        .expect("attach only to dedicated Chrome");

    let footprint_before = wait_for_stable_browser_footprint(&runtime).await;
    let mutable_default = ContextOptions::default()
        .target_route(TargetRouteOptions::default().viewport(Viewport::new(640, 480).unwrap()));
    let error = runtime
        .default_session_with(DefaultSessionOptions::default().context(mutable_default))
        .await
        .expect_err("attached default mutation must fail in preflight");
    assert_eq!(error.phase(), OperationPhase::Preparation);
    assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
    assert_eq!(
        error.capability_status().unwrap().capability(),
        Capability::RequestRouting
    );
    let footprint_after = browser_footprint(&runtime).await;
    assert_eq!(footprint_after, footprint_before);

    assert_eq!(
        runtime
            .capabilities()
            .status(CapabilityScope::DefaultContext, Capability::RequestRouting)
            .availability(),
        CapabilityAvailability::Unavailable
    );
    assert_available(
        &runtime,
        CapabilityScope::IsolatedContext,
        Capability::RequestRouting,
    );
    let origin = fixture.origin("127.0.0.1");
    let isolated = runtime
        .isolated_session(IsolatedSessionOptions::default().context(
            ContextOptions::default().target_route(
                TargetRouteOptions::default().viewport(Viewport::new(811, 611).unwrap()),
            ),
        ))
        .await
        .expect("attached runtime supports isolated mutable context");
    let page = isolated.new_page("about:blank").await.unwrap();
    goto_loaded_document(&page, format!("{origin}/attached")).await;
    let width: u32 = page.evaluate("innerWidth").await.unwrap();
    assert_eq!(width, 811);
    assert!(page.close().await.is_complete());
    assert!(isolated.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    assert_eq!(
        chrome.child.try_wait().unwrap(),
        None,
        "attach close killed Chrome"
    );

    chrome.stop().await;
    fixture.abort();
}

fn prepare_private_default_download_profile(profile: &TempDir) -> PathBuf {
    let download_dir = profile.path().join("downloads");
    std::fs::create_dir_all(&download_dir).unwrap();
    let default_dir = profile.path().join("Default");
    std::fs::create_dir_all(&default_dir).unwrap();
    let preferences = json!({
        "download": {
            "default_directory": download_dir,
            "prompt_for_download": false
        }
    });
    std::fs::write(
        default_dir.join("Preferences"),
        serde_json::to_vec(&preferences).unwrap(),
    )
    .unwrap();
    download_dir
}

async fn trigger_download(
    page: &browserkit::runtime::Page,
    url: &str,
) -> browserkit::runtime::Download {
    let script = format!(
        "(() => {{ const link = document.createElement('a'); link.href = {url:?}; link.download = 'task12.bin'; document.body.append(link); link.click(); return true; }})()"
    );
    page.expect_download(WaitOptions::default(), async {
        page.evaluate::<bool>(script).await.map(|_| ())
    })
    .await
    .expect("observe download")
}

#[tokio::test]
#[ignore = "requires current installed Chrome; all downloads stay in private temporary directories"]
async fn chrome_partitioned_cookie_round_trip_and_download_capabilities_match_behavior() {
    let fixture = LoopbackFixture::start(Arc::new(|target| {
        if target.contains("/download/") {
            FixtureResponse::download(b"browserkit-task12-download".to_vec())
        } else {
            FixtureResponse::html("<html><body>download fixture</body></html>")
        }
    }))
    .await;
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-storage-download-")
        .tempdir()
        .unwrap();
    let private_default_downloads = prepare_private_default_download_profile(&profile);
    let runtime = BrowserRuntime::launch(launch_options(&profile, &[fixture.port]))
        .await
        .expect("launch private Chrome");

    for scope in [
        CapabilityScope::DefaultContext,
        CapabilityScope::IsolatedContext,
    ] {
        assert_available(&runtime, scope, Capability::DownloadObservation);
        assert_eq!(
            runtime
                .capabilities()
                .status(scope, Capability::PartitionedCookies)
                .availability(),
            CapabilityAvailability::Conditional
        );
    }
    assert_eq!(
        runtime
            .capabilities()
            .status(
                CapabilityScope::DefaultContext,
                Capability::ManagedDownloadPath
            )
            .availability(),
        CapabilityAvailability::Unavailable
    );
    assert_available(
        &runtime,
        CapabilityScope::IsolatedContext,
        Capability::ManagedDownloadPath,
    );

    let isolated = runtime
        .isolated_session(IsolatedSessionOptions::default())
        .await
        .unwrap();
    let partition_key = CookiePartitionKey {
        top_level_site: "https://top-level.test".to_owned(),
        has_cross_site_ancestor: true,
    };
    isolated
        .set_cookie(
            BrowserCookie::new("task12-partitioned", "round-trip")
                .url("https://third-party.test/")
                .secure(true)
                .same_site(CookieSameSite::None)
                .partition_key(partition_key.clone()),
        )
        .await
        .expect("current Chrome accepts partitioned cookie without fallback");
    let cookies = isolated.cookies().await.unwrap();
    let cookie = cookies
        .iter()
        .find(|cookie| cookie.name() == "task12-partitioned")
        .expect("partitioned cookie returned by Chrome");
    assert_eq!(cookie.value(), "round-trip");
    assert_eq!(cookie.partition_key_value(), Some(&partition_key));

    let origin = fixture.origin("127.0.0.1");
    let default = runtime.default_session().await.unwrap();
    let default_page = default.new_page("about:blank").await.unwrap();
    goto_loaded_document(&default_page, format!("{origin}/page/default")).await;
    let default_download =
        trigger_download(&default_page, &format!("{origin}/download/default")).await;
    assert_eq!(
        default_download.path_capability(),
        DownloadPathCapability::Unavailable
    );
    match default_download.wait().await.unwrap() {
        DownloadTerminal::Completed { path, .. } => assert_eq!(path, None),
        other => panic!("default download did not complete: {other:?}"),
    }
    assert!(private_default_downloads.join("task12.bin").is_file());

    let isolated_page = isolated.new_page("about:blank").await.unwrap();
    goto_loaded_document(&isolated_page, format!("{origin}/page/isolated")).await;
    let isolated_download =
        trigger_download(&isolated_page, &format!("{origin}/download/isolated")).await;
    assert_eq!(
        isolated_download.path_capability(),
        DownloadPathCapability::Available
    );
    match isolated_download.wait().await.unwrap() {
        DownloadTerminal::Completed {
            path: Some(path), ..
        } => {
            assert!(path.is_file(), "managed download path must exist");
            assert_eq!(std::fs::read(path).unwrap(), b"browserkit-task12-download");
        }
        other => panic!("isolated download path unavailable: {other:?}"),
    }

    assert!(default_page.close().await.is_complete());
    assert!(isolated_page.close().await.is_complete());
    assert!(default.close().await.is_complete());
    assert!(isolated.close().await.is_complete());
    assert!(runtime.close().await.is_complete());
    fixture.abort();
}

#[tokio::test]
#[ignore = "requires installed Chrome; uses only a private profile and loopback HTTP"]
async fn chrome_launched_isolated_route_and_network_observation_resume_dedicated_worker() {
    let fixture = LoopbackFixture::start(Arc::new(|target| {
        let path = target.split('?').next().unwrap_or(target);
        match path {
            "/task12-worker.js" => FixtureResponse {
                status: "200 OK",
                headers: vec![("Content-Type", "application/javascript; charset=utf-8")],
                body: br#"
                    self.postMessage({kind: 'marker', value: 'task12-worker-started'});
                    fetch('/task12-worker-fetch?source=dedicated-worker')
                        .then(response => {
                            if (!response.ok) throw new Error(`HTTP ${response.status}`);
                            return response.text();
                        })
                        .then(body => {
                            self.postMessage({kind: 'fetched', body});
                            self.close();
                        })
                        .catch(error => self.postMessage({kind: 'error', message: String(error)}));
                "#
                .to_vec(),
            },
            "/task12-worker-fetch" => FixtureResponse {
                status: "200 OK",
                headers: vec![("Content-Type", "text/plain; charset=utf-8")],
                body: b"task12-worker-fetch-ok".to_vec(),
            },
            _ => FixtureResponse::html(
                "<html><body><button id='after-worker' onclick=\"document.body.dataset.clicked='yes'\">after worker</button></body></html>",
            ),
        }
    }))
    .await;
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-worker-")
        .tempdir()
        .unwrap();
    let runtime = BrowserRuntime::launch(launch_options(&profile, &[fixture.port]))
        .await
        .expect("launch private Chrome");
    let route = TargetRouteOptions::default()
        .user_agent(
            UserAgentOverride::new("BrowserKit-Task12-Worker/1.0")
                .expect("valid worker test user agent"),
        )
        .http_headers(
            HttpHeaders::new([("x-browserkit-task12-worker", "configured")])
                .expect("valid worker test header"),
        );
    let session = runtime
        .isolated_session(
            IsolatedSessionOptions::default()
                .context(ContextOptions::default().target_route(route))
                .network_observation(
                    browserkit::runtime::NetworkObservationOptions::default()
                        .retained_state_max_bytes(2 * 1024 * 1024)
                        .retained_state_ttl(CHROME_TEST_TIMEOUT),
                ),
        )
        .await
        .expect("create routed isolated context with network observation");
    let origin = fixture.origin("127.0.0.1");
    let page = session.new_page("about:blank").await.expect("open page");
    goto_loaded_document(&page, format!("{origin}/task12-worker-page")).await;

    let worker_fetch_url = format!("{origin}/task12-worker-fetch?source=dedicated-worker");
    let worker_messages = Arc::new(Mutex::new(None));
    let observed_messages = Arc::clone(&worker_messages);
    let worker_page = page.clone();
    let worker_request = page
        .expect_network(
            browserkit::runtime::NetworkPredicate::new()
                .url(browserkit::runtime::TextMatcher::exact(
                    worker_fetch_url.clone(),
                    true,
                ))
                .method("GET")
                .status(200)
                .custom(|snapshot| {
                    snapshot.terminal
                        == Some(browserkit::runtime::NetworkRequestTerminal::Finished)
                }),
            WaitOptions::default().timeout(CHROME_TEST_TIMEOUT),
            async move {
                let messages: Vec<Value> = worker_page
                    .evaluate(
                        r#"new Promise((resolve, reject) => {
                            const messages = [];
                            const worker = new Worker('/task12-worker.js');
                            worker.onerror = event => reject(new Error(event.message || 'worker error'));
                            worker.onmessage = event => {
                                messages.push(event.data);
                                if (event.data.kind === 'error') {
                                    reject(new Error(event.data.message));
                                } else if (event.data.kind === 'fetched') {
                                    resolve(messages);
                                }
                            };
                        })"#,
                    )
                    .await?;
                *observed_messages.lock() = Some(messages);
                Ok(())
            },
        )
        .await
        .expect("dedicated worker resumed, posted markers, and completed observed fetch");

    assert_eq!(
        worker_request
            .request
            .as_ref()
            .map(|request| request.url.as_str()),
        Some(worker_fetch_url.as_str())
    );
    assert_eq!(
        worker_request.terminal,
        Some(browserkit::runtime::NetworkRequestTerminal::Finished)
    );
    assert_eq!(
        worker_messages.lock().as_ref(),
        Some(&vec![
            json!({"kind": "marker", "value": "task12-worker-started"}),
            json!({"kind": "fetched", "body": "task12-worker-fetch-ok"}),
        ])
    );
    assert!(
        page.terminal_route_error().is_none(),
        "dedicated worker must resume and detach without terminalizing the Page: {:?}",
        page.terminal_route_error()
    );

    let page_still_evaluates: bool = page
        .evaluate("document.querySelector('#after-worker') instanceof HTMLButtonElement")
        .await
        .expect("Page evaluate remains admitted after worker detach");
    assert!(page_still_evaluates);
    page.locator("#after-worker")
        .click()
        .await
        .expect("Page action remains admitted after worker detach");
    let click_observed: String = page
        .evaluate("document.body.dataset.clicked")
        .await
        .expect("evaluate action result after worker detach");
    assert_eq!(click_observed, "yes");

    let page_close = page.close().await;
    assert!(
        page_close.failures().is_empty(),
        "page cleanup failures: {:?}",
        page_close.failures()
    );
    let session_close = session.close().await;
    assert!(
        session_close.failures().is_empty(),
        "session cleanup failures: {:?}",
        session_close.failures()
    );
    let runtime_close = runtime.close().await;
    assert!(
        runtime_close.failures().is_empty(),
        "runtime cleanup failures: {:?}",
        runtime_close.failures()
    );
    fixture.abort();
}
