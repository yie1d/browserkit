use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use browserkit::runtime::{
    ActionCompletion, BrowserError, BrowserRuntime, Capability, CapabilityAvailability,
    CapabilityScope, CloseReport, ContextOptions, DefaultSessionOptions, IsolatedSessionOptions,
    LaunchOptions, LoadState, NavigationOptions, OperationPhase, Page,
};
use cdpkit::target::methods::{GetBrowserContexts, GetTargets};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

const TEST_DEADLINE: Duration = Duration::from_secs(15);
const TLS_HOST: &str = "tls.test";
const TLS_MARKER: &str = "browserkit-task12-local-tls";

struct TlsFixture {
    port: u16,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl TlsFixture {
    async fn start() -> Self {
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec![TLS_HOST.to_owned()])
                .expect("self-signed tls.test cert");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("valid self-signed server identity");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local TLS fixture");
        let port = listener.local_addr().expect("TLS fixture address").port();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => break,
                    Some(joined) = connections.join_next(), if !connections.is_empty() => {
                        if let Err(error) = joined {
                            assert!(error.is_cancelled(), "TLS connection task failed: {error}");
                        }
                    }
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.expect("accept local TLS connection");
                        let acceptor = acceptor.clone();
                        connections.spawn(async move {
                            // Certificate-rejecting control navigations can end the handshake early.
                            let Ok(stream) = acceptor.accept(stream).await else {
                                return;
                            };
                            serve_marker(stream).await.expect("serve TLS marker response");
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            port,
            cancel,
            task: Some(task),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("https://{TLS_HOST}:{}{path}", self.port)
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        let task = self.task.take().expect("TLS fixture task is present");
        tokio::time::timeout(TEST_DEADLINE, task)
            .await
            .expect("TLS fixture cancellation deadline")
            .expect("TLS fixture task");
    }
}

impl Drop for TlsFixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve_marker(
    mut stream: tokio_rustls::server::TlsStream<TcpStream>,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await?;
        if read == 0 || request.len() + read > 64 * 1024 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let body = format!("<html><body id=marker>{TLS_MARKER}</body></html>");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn chrome_args(port: u16) -> [String; 6] {
    [
        "--disable-background-networking".to_owned(),
        "--disable-component-update".to_owned(),
        "--disable-default-apps".to_owned(),
        "--disable-sync".to_owned(),
        format!("--explicitly-allowed-ports={port}"),
        format!("--host-resolver-rules=MAP {TLS_HOST} 127.0.0.1"),
    ]
}

fn launch_options(profile: &TempDir, port: u16) -> LaunchOptions {
    chrome_args(port).into_iter().fold(
        LaunchOptions::default()
            .headless(true)
            .user_data_dir(profile.path())
            .timeout(TEST_DEADLINE),
        LaunchOptions::arg,
    )
}

fn https_context(ignore: bool) -> ContextOptions {
    ContextOptions::default().ignore_https_errors(ignore)
}

fn assert_available(runtime: &BrowserRuntime, scope: CapabilityScope) {
    assert_eq!(
        runtime
            .capabilities()
            .status(scope, Capability::IgnoreHttpsErrors)
            .availability(),
        CapabilityAvailability::Available
    );
}

fn assert_clean(report: &CloseReport, resource: &str) {
    assert!(
        report.failures().is_empty(),
        "{resource} route/resource cleanup failures: {:?}",
        report.failures()
    );
    assert!(report.is_complete(), "{resource} close report was partial");
}

fn assert_certificate_navigation_failure(error: &BrowserError, completion: ActionCompletion) {
    assert_eq!(error.operation_name(), Some("navigate page"));
    assert_eq!(error.phase(), OperationPhase::Confirmation);
    assert_eq!(error.action_completed(), completion);
    assert!(
        error.to_string().contains("ERR_CERT_AUTHORITY_INVALID"),
        "unexpected navigation failure: {error}"
    );
    assert!(
        error.cleanup_failures().is_empty(),
        "navigation rollback reported cleanup failures: {:?}",
        error.cleanup_failures()
    );
}

async fn goto_tls_marker(page: &Page, url: &str) {
    let navigation = page
        .goto(
            NavigationOptions::new(url)
                .wait_until(LoadState::Load)
                .timeout(TEST_DEADLINE),
        )
        .await
        .expect("self-signed TLS navigation succeeds when enabled for this route");
    assert_eq!(navigation.final_url(), url);
    let marker: String = page
        .evaluate("document.body.textContent")
        .await
        .expect("read local TLS marker");
    assert_eq!(marker, TLS_MARKER);
}

async fn page_target_ids(runtime: &BrowserRuntime) -> BTreeSet<String> {
    GetTargets::new()
        .send(runtime.cdp())
        .await
        .expect("list Chrome targets")
        .target_infos
        .into_iter()
        .filter(|target| target.type_ == "page")
        .map(|target| target.target_id.as_str().to_owned())
        .collect()
}

async fn browser_footprint(runtime: &BrowserRuntime) -> (BTreeSet<String>, BTreeSet<String>) {
    let contexts = GetBrowserContexts::new()
        .send(runtime.cdp())
        .await
        .expect("list browser contexts")
        .browser_context_ids
        .into_iter()
        .map(|context| context.as_str().to_owned())
        .collect();
    (contexts, page_target_ids(runtime).await)
}

#[tokio::test]
#[ignore = "requires installed Chrome; uses a self-signed tls.test fixture, private profile, and loopback only"]
async fn chrome_local_tls_launched_default_and_isolated_scopes_do_not_leak() {
    tokio::time::timeout(Duration::from_secs(90), launched_tls_scenario())
        .await
        .expect("launched local TLS test deadline");
}

async fn launched_tls_scenario() {
    let fixture = TlsFixture::start().await;
    let tls_url = fixture.url("/launched");
    let profile = tempfile::Builder::new()
        .prefix("browserkit-task12-tls-launched-")
        .tempdir()
        .expect("private launched Chrome profile");
    let runtime = BrowserRuntime::launch(launch_options(&profile, fixture.port))
        .await
        .expect("launch private Chrome");
    assert_available(&runtime, CapabilityScope::DefaultContext);
    assert_available(&runtime, CapabilityScope::IsolatedContext);

    let control = runtime
        .isolated_session(IsolatedSessionOptions::default().context(https_context(false)))
        .await
        .expect("control isolated context");
    let targets_before = page_target_ids(&runtime).await;
    let error = control
        .new_page(tls_url.clone())
        .await
        .expect_err("control new_page must reject the self-signed certificate");
    assert_certificate_navigation_failure(&error, ActionCompletion::Completed);
    assert_eq!(
        page_target_ids(&runtime).await,
        targets_before,
        "failed new_page published or retained a page target"
    );
    assert_clean(&control.close().await, "control isolated session");

    let default = runtime
        .default_session_with(DefaultSessionOptions::default().context(https_context(true)))
        .await
        .expect("launched default context can ignore HTTPS errors");
    let default_page = default.new_page("about:blank").await.expect("default page");
    goto_tls_marker(&default_page, &tls_url).await;
    assert_clean(&default_page.close().await, "default TLS page");

    let trusted = runtime
        .isolated_session(IsolatedSessionOptions::default().context(https_context(true)))
        .await
        .expect("trusted isolated context");
    let trusted_page = trusted.new_page("about:blank").await.expect("trusted page");
    goto_tls_marker(&trusted_page, &tls_url).await;

    let strict = runtime
        .isolated_session(IsolatedSessionOptions::default().context(https_context(false)))
        .await
        .expect("strict isolated context");
    let strict_page = strict.new_page("about:blank").await.expect("strict page");
    let error = strict_page
        .goto(
            NavigationOptions::new(&tls_url)
                .wait_until(LoadState::Load)
                .timeout(TEST_DEADLINE),
        )
        .await
        .expect_err("trusted isolated route must not leak into strict isolated route");
    assert_certificate_navigation_failure(&error, ActionCompletion::Completed);
    assert_clean(&strict_page.close().await, "first strict page");

    assert_clean(
        &trusted_page.close().await,
        "trusted TLS page route rollback",
    );
    let strict_after_rollback = strict
        .new_page("about:blank")
        .await
        .expect("strict page after trusted route rollback");
    let error = strict_after_rollback
        .goto(
            NavigationOptions::new(&tls_url)
                .wait_until(LoadState::Load)
                .timeout(TEST_DEADLINE),
        )
        .await
        .expect_err("trusted route rollback must not enable a later strict route");
    assert_certificate_navigation_failure(&error, ActionCompletion::Completed);
    assert_clean(
        &strict_after_rollback.close().await,
        "strict page after rollback",
    );

    assert_clean(&strict.close().await, "strict isolated session");
    assert_clean(&trusted.close().await, "trusted isolated session");
    assert_clean(&default.close().await, "default TLS session");
    assert_clean(&runtime.close().await, "launched TLS runtime");
    fixture.stop().await;
}

struct AttachedChrome {
    child: Child,
    _profile: TempDir,
    endpoint: String,
}

impl AttachedChrome {
    async fn start(port: u16) -> Self {
        let profile = tempfile::Builder::new()
            .prefix("browserkit-task12-tls-attached-")
            .tempdir()
            .expect("private attached Chrome profile");
        let mut command = Command::new(chrome_executable());
        command
            .arg("--headless=new")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .args(chrome_args(port))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("start dedicated attached Chrome");
        let stderr = child
            .stderr
            .take()
            .expect("capture dedicated Chrome stderr");
        let endpoint = tokio::time::timeout(TEST_DEADLINE, async {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                tokio::select! {
                    status = child.wait() => panic!("dedicated Chrome exited before DevTools endpoint: {}", status.expect("wait dedicated Chrome")),
                    line = lines.next_line() => {
                        let line = line.expect("read dedicated Chrome stderr")
                            .expect("dedicated Chrome closed stderr before DevTools endpoint");
                        if let Some(endpoint) = line.strip_prefix("DevTools listening on ") {
                            break endpoint.to_owned();
                        }
                    }
                }
            }
        })
        .await
        .expect("dedicated Chrome DevTools endpoint deadline");
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
        tokio::time::timeout(TEST_DEADLINE, self.child.wait())
            .await
            .expect("dedicated Chrome termination deadline")
            .expect("wait for dedicated Chrome");
    }
}

fn chrome_executable() -> PathBuf {
    if let Some(path) = env::var_os("BROWSERKIT_CHROME_PATH") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "BROWSERKIT_CHROME_PATH is not a file: {}",
            path.display()
        );
        return path;
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

#[tokio::test]
#[ignore = "requires installed Chrome; attaches only to a dedicated private-profile process and loopback TLS"]
async fn chrome_local_tls_attached_default_preflight_and_isolated_scope() {
    tokio::time::timeout(Duration::from_secs(60), attached_tls_scenario())
        .await
        .expect("attached local TLS test deadline");
}

async fn attached_tls_scenario() {
    let fixture = TlsFixture::start().await;
    let tls_url = fixture.url("/attached");
    let mut chrome = AttachedChrome::start(fixture.port).await;
    let runtime = BrowserRuntime::connect(chrome.endpoint.clone())
        .await
        .expect("attach only to dedicated test Chrome");

    let status = runtime.capabilities().status(
        CapabilityScope::DefaultContext,
        Capability::IgnoreHttpsErrors,
    );
    assert_eq!(status.availability(), CapabilityAvailability::Unavailable);
    let footprint_before = browser_footprint(&runtime).await;
    let error = runtime
        .default_session_with(DefaultSessionOptions::default().context(https_context(true)))
        .await
        .expect_err("attached default HTTPS mutation must fail in preflight");
    let failure = error.capability_status().expect("typed capability failure");
    assert_eq!(failure.capability(), Capability::IgnoreHttpsErrors);
    assert_eq!(failure.availability(), CapabilityAvailability::Unavailable);
    assert_eq!(error.phase(), OperationPhase::Preparation);
    assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
    assert_eq!(
        browser_footprint(&runtime).await,
        footprint_before,
        "attached default preflight dispatched a context or target mutation"
    );

    assert_available(&runtime, CapabilityScope::IsolatedContext);
    let isolated = runtime
        .isolated_session(IsolatedSessionOptions::default().context(https_context(true)))
        .await
        .expect("attached isolated context supports ignoring HTTPS errors");
    let page = isolated
        .new_page("about:blank")
        .await
        .expect("attached TLS page");
    goto_tls_marker(&page, &tls_url).await;
    assert_clean(&page.close().await, "attached isolated TLS page");
    assert_clean(&isolated.close().await, "attached isolated TLS session");
    assert_clean(&runtime.close().await, "attached TLS runtime");
    assert_eq!(
        chrome.child.try_wait().expect("poll dedicated Chrome"),
        None,
        "attached runtime close terminated the external test child"
    );

    chrome.stop().await;
    fixture.stop().await;
}
