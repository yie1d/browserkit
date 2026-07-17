// CLI entry point: clap command parsing + daemon client wiring

use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

// ── Custom grouped help text ──────────────────────────────────

const HELP_TEXT: &str = "\
Persistent browser runtime CLI for AI agents. All output is JSON.

bk is the thin CLI client for the local browserkit daemon.

Usage: bk [OPTIONS] <COMMAND>

Primary:
  setup       One-time Chrome remote debugging setup
  connect     Connect to browser (idempotent)
  snapshot    Get page state (elements + text + viewport)
  act         Execute interaction (click/type/fill/press/scroll/hover/focus/select/options/upload/drag)
  navigate    Navigate to URL or back/forward/reload
  open        Open URL in new tab
  attach      Attach existing browser tab to default session
  close       Close tab
  tabs        List tabs in session
  wait        Wait for page condition
  evaluate    Execute JavaScript
  screenshot  Take a screenshot
  find        Find elements by CSS selector
  search      Search text in page
  html        Get page HTML
  console     Show console log buffer
  network     Observe XHR/fetch responses
  download    Trigger and wait for a download
  pdf         Generate PDF of current target
  session     Session management (close/list/cookies/storage)
  status      Connection status
  browser     Browser connection management
  daemon      Daemon process management
  dialog      Dialog management
  debug       Request blocking and raw CDP

Options:
      --session <NAME>    Target session (or BK_SESSION env var)
      --target <ID>       Target tab (targetId)
      --timeout <MS>      Timeout in milliseconds [default: 30000]
      --no-state-diff     Skip state_diff in act responses
      --focus             Bring tab to foreground
  -h, --help              Print help
      --version           Print version

Run `bk <COMMAND> --help` for detailed usage and examples.";

use browserkit::client::{build_request, DaemonClient};
use browserkit::daemon;
use browserkit::daemon::protocol::Response;
use browserkit::error::ErrorCode;

// ── Top-level CLI ──────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "bk",
    about = "Persistent browser runtime CLI for AI agents",
    long_about = "Persistent browser runtime CLI for AI agents.\n\nAll output is JSON. Commands communicate with the local browserkit daemon over TCP.",
    version
)]
pub struct Cli {
    /// Target session name (or set BK_SESSION env var)
    #[arg(long = "session", global = true, env = "BK_SESSION")]
    pub session: Option<String>,

    /// Target tab (targetId)
    #[arg(long = "target", global = true)]
    pub target: Option<String>,

    /// Timeout in milliseconds
    #[arg(long = "timeout", global = true)]
    pub timeout: Option<u64>,

    /// Skip state_diff in act responses
    #[arg(long = "no-state-diff", global = true)]
    pub no_state_diff: bool,

    /// Bring tab to foreground
    #[arg(long = "focus", global = true)]
    pub focus: bool,

    #[command(subcommand)]
    pub command: Command,
}

// ── Command enum ───────────────────────────────────────────────

#[derive(Subcommand)]
pub enum Command {
    // ══════════════════════════════════════════════════════════════
    // V2 PRIMARY COMMANDS
    // ══════════════════════════════════════════════════════════════
    /// Set up Chrome remote debugging (interactive, one-time, no daemon needed)
    #[command(about = "Set up Chrome remote debugging (interactive, one-time)")]
    Setup,

    /// Connect to browser (idempotent)
    #[command(about = "Connect to browser")]
    Connect,

    /// Get page state (elements + text + viewport)
    #[command(about = "Get page snapshot")]
    Snapshot {
        /// Include all elements (no truncation)
        #[arg(long)]
        full: bool,
        /// Exclude page text from output
        #[arg(long)]
        no_page_text: bool,
        /// Wait strategy: dom-stable, networkidle, none
        #[arg(long, default_value = "dom-stable")]
        wait: String,
        /// Deterministic content token budget (16..100000)
        #[arg(long)]
        max_tokens: Option<usize>,
    },

    /// Execute interaction (click/type/fill/press/scroll/hover/focus/select/options/upload/drag)
    #[command(about = "Execute interaction")]
    Act {
        /// Action kind (click, type, fill, press, scroll, hover, focus, select, options, upload, drag)
        kind: Option<String>,
        /// Element ref (backendNodeId)
        #[arg(long = "ref")]
        element_ref: Option<i64>,
        /// Field assignment for fill action (ref:<id>=<value>)
        #[arg(long = "set")]
        set: Vec<String>,
        /// Text for type action
        #[arg(long)]
        text: Option<String>,
        /// Value for select action
        #[arg(long)]
        value: Option<String>,
        /// Append mode for type (default: replace)
        #[arg(long)]
        append: bool,
        /// Keys for press action
        #[arg(long, num_args = 1..)]
        keys: Vec<String>,
        /// X coordinate for click
        #[arg(long)]
        x: Option<f64>,
        /// Y coordinate for click
        #[arg(long)]
        y: Option<f64>,
        /// Scroll direction
        #[arg(long)]
        direction: Option<String>,
        /// Scroll amount in pixels
        #[arg(long)]
        amount: Option<f64>,
        /// CSS selector for scroll target
        #[arg(long)]
        selector: Option<String>,
        /// Files for upload action
        files: Vec<String>,
        /// Source element ref for drag
        #[arg(long)]
        from_ref: Option<i64>,
        /// Source element selector for drag
        #[arg(long)]
        from_selector: Option<String>,
        /// Destination element ref for drag
        #[arg(long)]
        to_ref: Option<i64>,
        /// Destination element selector for drag
        #[arg(long)]
        to_selector: Option<String>,
    },

    /// Navigate to URL or back/forward/reload
    #[command(about = "Navigate")]
    Navigate {
        /// Target URL
        url: Option<String>,
        /// Go back
        #[arg(long)]
        back: bool,
        /// Go forward
        #[arg(long)]
        forward: bool,
        /// Reload page
        #[arg(long)]
        reload: bool,
    },

    /// Open URL in new tab
    #[command(about = "Open URL in new tab", name = "open")]
    OpenV2 {
        /// URL to open
        url: String,
    },

    /// Close tab
    #[command(about = "Close tab", name = "close")]
    CloseV2,

    /// List tabs in session
    #[command(about = "List tabs")]
    Tabs,

    /// Attach an existing browser tab to the default session
    #[command(
        about = "Attach existing browser tab to default session",
        long_about = "Attach an existing user browser tab to the default session. Named isolated sessions must use 'bk open' instead."
    )]
    Attach {
        /// URL, title, or target ID substring; omit when global --target is present.
        pattern: Option<String>,
    },

    /// Evaluate JavaScript expression
    #[command(about = "Evaluate JavaScript")]
    Evaluate {
        /// JavaScript expression (omit when using --file)
        expression: Option<String>,
        /// Execute JS from file path
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Append a string result to a local file without printing the result
        #[arg(long, value_name = "FILE")]
        append_to: Option<String>,
    },

    /// Take screenshot
    #[command(about = "Take screenshot", name = "screenshot")]
    ScreenshotV2 {
        /// Output file path
        #[arg(long)]
        output: Option<String>,
        /// Capture full scrollable page
        #[arg(long)]
        full_page: bool,
        /// CSS selector for element screenshot
        #[arg(long)]
        selector: Option<String>,
        /// Overlay element labels before capture
        #[arg(long)]
        labels: bool,
    },

    /// Wait for condition
    #[command(about = "Wait for condition", name = "wait")]
    WaitV2 {
        /// CSS selector to wait for
        #[arg(long)]
        selector: Option<String>,
        /// Wait for text to appear
        #[arg(long)]
        text: Option<String>,
        /// Wait for text to disappear
        #[arg(long)]
        text_gone: Option<String>,
        /// Wait for URL to match
        #[arg(long)]
        url: Option<String>,
        /// Wait for network idle
        #[arg(long)]
        idle: bool,
        /// Wait for JS expression to return truthy
        #[arg(long, value_name = "EXPR")]
        r#fn: Option<String>,
        /// Fixed delay in milliseconds
        #[arg(long)]
        time: Option<u64>,
    },

    /// Session management
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    /// Connection status
    #[command(about = "Show connection status", name = "status")]
    StatusV2,

    /// Find elements by CSS selector
    Find {
        selector: String,
        #[arg(long)]
        attributes: Option<String>,
        #[arg(long)]
        max: Option<usize>,
        #[arg(long)]
        include_text: bool,
    },
    /// Search text in page
    Search {
        text: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        context: Option<usize>,
        #[arg(long)]
        max: Option<usize>,
    },
    /// Get page HTML
    Html {
        #[arg(short, long)]
        selector: Option<String>,
    },
    /// Show console log buffer
    Console {
        #[arg(long, default_value = "all")]
        level: String,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Observe network responses
    Network {
        #[command(subcommand)]
        action: NetworkAction,
    },
    /// Trigger a download by clicking an element and wait for completion
    Download {
        /// Element ref for the download trigger
        #[arg(long = "ref")]
        element_ref: i64,
        /// Existing directory where Chrome should save the download
        #[arg(long, value_name = "DIR")]
        output_dir: String,
    },
    /// Generate PDF of current target
    Pdf {
        #[arg(short, long)]
        output: Option<String>,
    },

    // ── Management ────────────────────────────────────────────────
    /// Browser management
    Browser {
        #[command(subcommand)]
        action: BrowserAction,
    },
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Dialog management
    Dialog {
        #[command(subcommand)]
        action: DialogAction,
    },
    /// Debug tools
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    /// Generate shell completions
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },
}

// ── V2 Session subcommands ────────────────────────────────────────

#[derive(Subcommand)]
pub enum SessionAction {
    /// Close session
    Close,
    /// List all sessions
    List,
    /// Cookie operations
    Cookies {
        #[command(subcommand)]
        action: CookiesAction,
    },
    /// Storage operations
    Storage {
        #[command(subcommand)]
        action: SessionStorageAction,
    },
}

#[derive(Subcommand)]
pub enum CookiesAction {
    /// Get cookies
    Get,
    /// Set cookies from JSON file
    Set {
        #[arg(long)]
        file: String,
    },
    /// Clear all cookies
    Clear,
}

#[derive(Subcommand)]
pub enum SessionStorageAction {
    /// LocalStorage operations
    Local {
        #[command(subcommand)]
        action: SessionLocalStorageAction,
    },
    /// Export all storage state
    Export,
    /// Import storage state from file
    Import {
        /// Path to state JSON file
        file: String,
    },
}

#[derive(Subcommand)]
pub enum SessionLocalStorageAction {
    /// Get localStorage value
    Get {
        /// Key
        key: String,
    },
    /// Set localStorage value
    Set {
        /// Key
        key: String,
        /// Value
        value: String,
    },
}

#[derive(Subcommand)]
pub enum NetworkAction {
    /// Observe a bounded number of XHR/fetch responses
    Watch {
        /// URL substring to match
        #[arg(long)]
        pattern: String,
        /// Maximum number of matching responses to collect
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
}

// ── Subcommand enums ───────────────────────────────────────────

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start the daemon
    Start,
    /// Stop the daemon
    Stop,
    /// Show daemon status
    Status,
}

#[derive(Subcommand)]
pub enum BrowserAction {
    /// Connect to an existing browser
    Connect {
        /// CDP endpoint host (e.g. localhost:9222)
        host: String,
    },
    /// Auto-discover user's Chrome via DevToolsActivePort
    Discover {
        /// Custom path to DevToolsActivePort file
        #[arg(long)]
        path: Option<String>,
    },
    /// List connected browsers
    List,
    /// Disconnect from a browser
    Disconnect {
        /// CDP endpoint host
        host: String,
    },
}

#[derive(Subcommand)]
pub enum DebugAction {
    /// Block requests matching pattern
    Block {
        /// URL pattern to block
        pattern: String,
    },
    /// Unblock requests
    Unblock,
    /// Send raw CDP command
    Cdp {
        /// CDP method name
        method: String,
        /// JSON params (optional)
        params: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum DialogAction {
    /// List pending dialogs in current session
    List,
    /// Accept (confirm) a pending dialog
    Accept {
        /// Text to enter for prompt dialogs
        #[arg(long)]
        text: Option<String>,
    },
    /// Dismiss (cancel) a pending dialog
    Dismiss,
    /// View or set the dialog handling policy for this session
    Policy {
        /// Policy to set: manual, accept, dismiss (omit to view current)
        policy: Option<String>,
    },
}

// ── Main ───────────────────────────────────────────────────────

fn main() {
    // Clap derive generates deep recursion for large enum variants in debug builds.
    // Spawn the async runtime on a thread with a larger stack to prevent overflow.
    let thread = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to create tokio runtime")
                .block_on(async_main());
        })
        .expect("failed to spawn main thread");
    thread.join().unwrap();
}

async fn async_main() {
    // Intercept top-level --help / -h before clap parsing.
    // We show our grouped help when -h/--help is present and no subcommand is detected.
    // Subcommand help (e.g. `bk navigate --help`) is handled by clap normally.
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has_help = args.iter().any(|a| a == "-h" || a == "--help");
        if has_help {
            // Check if any argument looks like a subcommand (not starting with '-')
            // and is not a value for --session or --target or --timeout
            let has_subcommand = {
                let mut skip_next = false;
                let mut found = false;
                for arg in &args {
                    if skip_next {
                        skip_next = false;
                        continue;
                    }
                    if arg == "--session" || arg == "--target" || arg == "--timeout" {
                        skip_next = true;
                        continue;
                    }
                    if arg.starts_with('-') {
                        continue;
                    }
                    // Positional argument = subcommand
                    found = true;
                    break;
                }
                found
            };
            if !has_subcommand {
                println!("{}", HELP_TEXT);
                std::process::exit(0);
            }
        }
    }

    let cli = Cli::parse();

    // daemon start is special — runs the server in foreground
    if let Command::Daemon {
        action: DaemonAction::Start,
    } = &cli.command
    {
        run_daemon_start().await;
        return;
    }

    // Shell completions (no daemon needed)
    if let Command::Completions { shell } = &cli.command {
        let mut cmd = Cli::command();
        clap_complete::generate(*shell, &mut cmd, "bk", &mut std::io::stdout());
        return;
    }

    // Setup is pure CLI-side, no daemon needed
    if let Command::Setup = &cli.command {
        run_setup().await;
        return;
    }

    // daemon stop / daemon status: connect-only, never auto-start a daemon
    if let Command::Daemon {
        action: _action @ (DaemonAction::Stop | DaemonAction::Status),
    } = &cli.command
    {
        match DaemonClient::connect_only().await {
            Ok(mut client) => {
                let result = dispatch(&cli, &mut client).await;
                if let Err(msg) = result {
                    eprintln!("error: {}", msg);
                    std::process::exit(1);
                }
                // After daemon.stop, wait for the daemon process to actually exit
                // by polling until the port is no longer reachable.
                if let Command::Daemon {
                    action: DaemonAction::Stop,
                } = &cli.command
                {
                    drop(client); // close our connection first
                    wait_for_daemon_exit().await;
                }
            }
            Err(_) => {
                // No daemon running — report cleanly and exit 0
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "data": {"status": "daemon not running"}})
                );
            }
        }
        return;
    }

    // All other commands need a daemon connection (auto-starts if needed)
    let mut client = match DaemonClient::connect_or_start().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let result = dispatch(&cli, &mut client).await;
    match result {
        Ok(()) => {}
        Err(msg) => {
            eprintln!("error: {}", msg);
            std::process::exit(1);
        }
    }
}

/// Run daemon in foreground (blocking).
async fn run_daemon_start() {
    // Write daemon logs to ~/.bk/daemon.log (append mode).
    // Since the daemon is typically spawned with stdio redirected to null,
    // file-based logging is the only way to observe runtime behavior.
    let log_dir = daemon::bk_home();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))
        .expect("failed to open daemon.log for writing");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("browserkit=debug".parse().unwrap()),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    match daemon::start_daemon().await {
        Ok(result) => {
            println!("daemon started on port {}", result.server.port);
            // Wait for either Ctrl+C or a shutdown signal (from daemon.stop handler)
            let mut shutdown_rx = result.shutdown_rx;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = shutdown_rx.changed() => {}
            }
            println!("\nshutting down...");
            daemon::stop_daemon_cleanup();
            // Force exit to ensure all background tasks (persist, cleanup, restore)
            // are terminated and the process doesn't hang.
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

/// Interactive setup: detect browser and guide user through enabling remote debugging.
async fn run_setup() {
    use browserkit::browser::finder::*;

    // Step 1: Detect browser
    let browser = detect_installed_browser();
    let (browser_name, inspect_url) = match &browser {
        BrowserDetection::Chrome(p) => {
            eprintln!("Checking Chrome... found at {}", p.display());
            ("Chrome", "chrome://inspect/#remote-debugging")
        }
        BrowserDetection::Edge(p) => {
            eprintln!("Checking Edge... found at {}", p.display());
            ("Edge", "edge://inspect/#remote-debugging")
        }
        BrowserDetection::None => {
            let resp = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "BROWSER_NOT_INSTALLED",
                    "message": "neither Chrome nor Edge found",
                    "suggestion": "install Google Chrome from https://www.google.com/chrome",
                    "recoverable": false
                }
            });
            println!("{}", serde_json::to_string(&resp).unwrap());
            return;
        }
    };

    // Step 2: Check if already configured (DevToolsActivePort exists)
    if find_devtools_port().is_some() {
        eprintln!("Checking remote debugging... already enabled!");
        let resp = build_setup_success_json(browser_name);
        println!("{}", serde_json::to_string(&resp).unwrap());
        return;
    }

    // Step 3: Guide user
    eprintln!("Checking remote debugging... not enabled\n");
    eprintln!(
        "Remote debugging lets bk connect to your {} browser.",
        browser_name
    );
    eprintln!("You only need to do this once.\n");
    eprintln!("Steps:");
    eprintln!("  1. Open {} (if not already open)", browser_name);
    eprintln!("  2. In the address bar, type: {}", inspect_url);
    eprintln!("  3. Enable remote debugging (check the box or toggle)");
    eprintln!("  4. Come back here and press Enter\n");
    eprintln!("Waiting... [Press Enter when done]");

    // Wait for user input
    let mut _input = String::new();
    if std::io::stdin().read_line(&mut _input).is_err() {
        eprintln!("error: failed to read from stdin");
        return;
    }

    // Step 4: Poll for DevToolsActivePort (up to 30 attempts, 1s each)
    eprintln!("Checking connection...");
    for _ in 0..30 {
        if find_devtools_port().is_some() {
            eprintln!("Connected to {}!", browser_name);
            let resp = build_setup_success_json(browser_name);
            println!("{}", serde_json::to_string(&resp).unwrap());
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // Timeout
    let resp = serde_json::json!({
        "ok": false,
        "error": {
            "code": "REMOTE_DEBUG_NOT_ENABLED",
            "message": "could not detect remote debugging after 30s",
            "suggestion": format!("open {} and enable remote debugging, then retry bk setup", inspect_url),
            "recoverable": true
        }
    });
    println!("{}", serde_json::to_string(&resp).unwrap());
}

// ── Command dispatch ───────────────────────────────────────────

fn build_navigate_params(
    url: Option<&String>,
    back: bool,
    forward: bool,
    reload: bool,
    cli: &Cli,
) -> serde_json::Value {
    let mut params = json!({});
    if let Some(u) = url {
        params["url"] = json!(u);
    }
    if back {
        params["back"] = json!(true);
    }
    if forward {
        params["forward"] = json!(true);
    }
    if reload {
        params["reload"] = json!(true);
    }
    if let Some(s) = &cli.session {
        params["session"] = json!(s);
    }
    if let Some(t) = &cli.target {
        params["target"] = json!(t);
    }
    if let Some(to) = cli.timeout {
        params["timeout"] = json!(to);
    }
    params
}

fn build_snapshot_params(
    full: bool,
    no_page_text: bool,
    wait: &str,
    max_tokens: Option<usize>,
    cli: &Cli,
) -> serde_json::Value {
    let mut params = json!({"full": full, "no_page_text": no_page_text, "wait": wait});
    add_session_target_params(&mut params, cli);
    if let Some(timeout) = cli.timeout {
        params["timeout"] = json!(timeout);
    }
    if let Some(max_tokens) = max_tokens {
        params["max_tokens"] = json!(max_tokens);
    }
    params
}

fn build_network_watch_params(pattern: &str, count: usize, cli: &Cli) -> serde_json::Value {
    let mut params = json!({"pattern": pattern, "count": count});
    add_session_target_params(&mut params, cli);
    if let Some(timeout) = cli.timeout {
        params["timeout"] = json!(timeout);
    }
    params
}

fn build_download_params(
    element_ref: i64,
    output_dir: &str,
    cli: &Cli,
) -> Result<serde_json::Value, Response> {
    let output_dir = canonical_directory(output_dir)?;
    let mut params = json!({
        "ref": element_ref,
        "output_dir": output_dir.to_string_lossy(),
    });
    add_session_target_params(&mut params, cli);
    if let Some(timeout) = cli.timeout {
        params["timeout"] = json!(timeout);
    }
    Ok(params)
}

fn build_evaluate_params(expression: &str, cli: &Cli) -> serde_json::Value {
    let mut params = json!({"expression": expression});
    add_session_target_params(&mut params, cli);
    if let Some(timeout) = cli.timeout {
        params["timeout"] = json!(timeout);
    }
    params
}

fn build_screenshot_params(
    output: Option<&str>,
    full_page: bool,
    selector: Option<&str>,
    labels: bool,
    cli: &Cli,
) -> serde_json::Value {
    let mut params = json!({
        "full_page": full_page,
        "labels": labels,
    });
    if let Some(o) = output {
        params["output"] = json!(o);
    }
    if let Some(s) = selector {
        params["selector"] = json!(s);
    }
    if let Some(s) = &cli.session {
        params["session"] = json!(s);
    }
    if let Some(t) = &cli.target {
        params["target"] = json!(t);
    }
    params
}

fn add_session_target_params(params: &mut serde_json::Value, cli: &Cli) {
    if let Some(s) = &cli.session {
        params["session"] = json!(s);
    }
    if let Some(t) = &cli.target {
        params["target"] = json!(t);
    }
}

fn add_session_param(params: &mut serde_json::Value, cli: &Cli) {
    if let Some(session) = &cli.session {
        params["session"] = json!(session);
    }
}

async fn dispatch(cli: &Cli, client: &mut DaemonClient) -> Result<(), String> {
    match &cli.command {
        // ══════════════════════════════════════════════════════════
        // V2 PRIMARY COMMANDS
        // ══════════════════════════════════════════════════════════
        Command::Connect => {
            let mut params = json!({});
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            let resp = send_cmd(client, "connect", params).await?;
            print_response(&resp);
        }

        Command::Snapshot {
            full,
            no_page_text,
            wait,
            max_tokens,
        } => {
            let params = build_snapshot_params(*full, *no_page_text, wait, *max_tokens, cli);
            let resp = send_cmd(client, "snapshot", params).await?;
            print_response(&resp);
        }

        Command::Act {
            kind,
            element_ref,
            set,
            text,
            value,
            append,
            keys,
            x,
            y,
            direction,
            amount,
            selector,
            files,
            from_ref,
            from_selector,
            to_ref,
            to_selector,
        } => {
            if !set.is_empty() && kind.as_deref() != Some("fill") {
                return Err("--set is only supported with 'bk act fill'".into());
            }

            let mut params = json!({});
            if let Some(k) = kind {
                params["kind"] = json!(k);
            }
            if let Some(r) = element_ref {
                params["ref"] = json!(r);
            }
            if !set.is_empty() {
                use browserkit::page::interaction::parse_fill_set_target;

                let mut fields = Vec::with_capacity(set.len());
                for item in set {
                    let field = parse_fill_set_target(item)?;
                    let ref_id = match field.target {
                        browserkit::page::element_ref::ElementTarget::Ref(ref_id) => ref_id,
                        browserkit::page::element_ref::ElementTarget::Index(_) => return Err(
                            "bk act fill only accepts ref targets in --set (use ref:<id>=<value>)"
                                .into(),
                        ),
                        browserkit::page::element_ref::ElementTarget::Selector(_) => {
                            return Err(
                                "bk act fill does not accept selector targets in --set".into()
                            )
                        }
                    };
                    fields.push(json!({"ref": ref_id, "value": field.value}));
                }
                params["fields"] = json!(fields);
            }
            if let Some(t) = text {
                params["text"] = json!(t);
            }
            if let Some(v) = value {
                params["value"] = json!(v);
            }
            if *append {
                params["append"] = json!(true);
            }
            if !keys.is_empty() {
                params["keys"] = json!(keys);
            }
            if let Some(cx) = x {
                params["x"] = json!(cx);
            }
            if let Some(cy) = y {
                params["y"] = json!(cy);
            }
            if let Some(dir) = direction {
                params["direction"] = json!(dir);
            }
            if let Some(a) = amount {
                params["amount"] = json!(a);
            }
            if let Some(sel) = selector {
                params["selector"] = json!(sel);
            }
            if !files.is_empty() {
                params["files"] = json!(files);
            }
            if let Some(from_ref) = from_ref {
                params["from_ref"] = json!(from_ref);
            }
            if let Some(from_selector) = from_selector {
                params["from_selector"] = json!(from_selector);
            }
            if let Some(to_ref) = to_ref {
                params["to_ref"] = json!(to_ref);
            }
            if let Some(to_selector) = to_selector {
                params["to_selector"] = json!(to_selector);
            }
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            if let Some(t) = &cli.target {
                params["target"] = json!(t);
            }
            if let Some(to) = cli.timeout {
                params["timeout"] = json!(to);
            }
            if cli.no_state_diff {
                params["no_state_diff"] = json!(true);
            }
            let resp = send_cmd(client, "act", params).await?;
            print_response(&resp);
        }

        Command::Navigate {
            url,
            back,
            forward,
            reload,
        } => {
            let resp = send_cmd(
                client,
                "navigate",
                build_navigate_params(url.as_ref(), *back, *forward, *reload, cli),
            )
            .await?;
            print_response(&resp);
        }

        Command::OpenV2 { url } => {
            let mut params = json!({"url": url});
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            if let Some(to) = cli.timeout {
                params["timeout"] = json!(to);
            }
            let resp = send_cmd(client, "open", params).await?;
            print_response(&resp);
        }

        Command::CloseV2 => {
            let mut params = json!({});
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            if let Some(t) = &cli.target {
                params["target"] = json!(t);
            }
            let resp = send_cmd(client, "close", params).await?;
            print_response(&resp);
        }

        Command::Tabs => {
            let mut params = json!({});
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            let resp = send_cmd(client, "tabs", params).await?;
            print_response(&resp);
        }

        Command::Attach { pattern } => {
            let mut params = json!({});
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            if let Some(t) = &cli.target {
                params["target"] = json!(t);
            }
            if let Some(p) = pattern {
                params["pattern"] = json!(p);
            }
            let resp = send_cmd(client, "attach", params).await?;
            print_response(&resp);
        }

        Command::Evaluate {
            expression,
            file,
            append_to,
        } => {
            let js_expr = if let Some(path) = file {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read JS file: {}", e))?;
                content
            } else if let Some(e) = expression {
                e.clone()
            } else {
                return Err("evaluate requires either an expression or --file".into());
            };
            let params = build_evaluate_params(&js_expr, cli);
            let resp = send_cmd(client, "evaluate", params).await?;
            if let Some(path) = append_to {
                print_response(&append_evaluate_result(&resp, path));
            } else {
                print_response(&resp);
            }
        }

        Command::Network { action } => match action {
            NetworkAction::Watch { pattern, count } => {
                let params = build_network_watch_params(pattern, *count, cli);
                let resp = send_cmd(client, "network.watch", params).await?;
                print_response(&resp);
            }
        },

        Command::Download {
            element_ref,
            output_dir,
        } => {
            let params = match build_download_params(*element_ref, output_dir, cli) {
                Ok(params) => params,
                Err(response) => {
                    print_response(&response);
                    return Ok(());
                }
            };
            let resp = send_cmd(client, "download", params).await?;
            print_response(&resp);
        }

        Command::ScreenshotV2 {
            output,
            full_page,
            selector,
            labels,
        } => {
            let resp = send_cmd(
                client,
                "screenshot",
                build_screenshot_params(
                    output.as_deref(),
                    *full_page,
                    selector.as_deref(),
                    *labels,
                    cli,
                ),
            )
            .await?;
            handle_binary_response(&resp, output.as_deref(), "screenshot.png");
        }

        Command::WaitV2 {
            selector,
            text,
            text_gone,
            url,
            idle,
            r#fn,
            time,
        } => {
            let mut params = json!({});
            if let Some(s) = selector {
                params["selector"] = json!(s);
            }
            if let Some(t) = text {
                params["text"] = json!(t);
            }
            if let Some(tg) = text_gone {
                params["text_gone"] = json!(tg);
            }
            if let Some(u) = url {
                params["url"] = json!(u);
            }
            if *idle {
                params["load_state"] = json!("networkidle");
            }
            if let Some(f) = r#fn {
                params["fn"] = json!(f);
            }
            if let Some(t) = time {
                params["time"] = json!(t);
            }
            if let Some(s) = &cli.session {
                params["session"] = json!(s);
            }
            if let Some(t) = &cli.target {
                params["target"] = json!(t);
            }
            if let Some(to) = cli.timeout {
                params["timeout"] = json!(to);
            }
            if params.get("timeout").is_none() {
                params["timeout"] = json!(30000u64);
            }
            let resp = send_cmd(client, "wait", params).await?;
            print_response(&resp);
        }

        Command::Session { action } => match action {
            SessionAction::Close => {
                let mut params = json!({});
                if let Some(s) = &cli.session {
                    params["session"] = json!(s);
                }
                let resp = send_cmd(client, "session.close", params).await?;
                print_response(&resp);
            }
            SessionAction::List => {
                let resp = send_cmd(client, "session.list", json!({})).await?;
                print_response(&resp);
            }
            SessionAction::Cookies { action: ca } => match ca {
                CookiesAction::Get => {
                    let mut params = json!({});
                    if let Some(s) = &cli.session {
                        params["session"] = json!(s);
                    }
                    let resp = send_cmd(client, "session.cookies.get", params).await?;
                    print_response(&resp);
                }
                CookiesAction::Set { file } => {
                    let content = std::fs::read_to_string(file)
                        .map_err(|e| format!("failed to read cookies file: {}", e))?;
                    let cookies: serde_json::Value = serde_json::from_str(&content)
                        .map_err(|e| format!("invalid cookie JSON: {}", e))?;
                    let mut params = json!({"cookies": cookies});
                    if let Some(s) = &cli.session {
                        params["session"] = json!(s);
                    }
                    let resp = send_cmd(client, "session.cookies.set", params).await?;
                    print_response(&resp);
                }
                CookiesAction::Clear => {
                    let mut params = json!({});
                    if let Some(s) = &cli.session {
                        params["session"] = json!(s);
                    }
                    let resp = send_cmd(client, "session.cookies.clear", params).await?;
                    print_response(&resp);
                }
            },
            SessionAction::Storage { action: sa } => match sa {
                SessionStorageAction::Local { action: la } => match la {
                    SessionLocalStorageAction::Get { key } => {
                        let mut params = json!({"key": key});
                        add_session_target_params(&mut params, cli);
                        let resp = send_cmd(client, "session.storage.local.get", params).await?;
                        print_response(&resp);
                    }
                    SessionLocalStorageAction::Set { key, value } => {
                        let mut params = json!({"key": key, "value": value});
                        add_session_target_params(&mut params, cli);
                        let resp = send_cmd(client, "session.storage.local.set", params).await?;
                        print_response(&resp);
                    }
                },
                SessionStorageAction::Export => {
                    let mut params = json!({});
                    add_session_target_params(&mut params, cli);
                    let resp = send_cmd(client, "session.storage.export", params).await?;
                    print_response(&resp);
                }
                SessionStorageAction::Import { file } => {
                    let content = std::fs::read_to_string(file)
                        .map_err(|e| format!("failed to read storage file: {}", e))?;
                    let state: serde_json::Value = serde_json::from_str(&content)
                        .map_err(|e| format!("invalid storage JSON: {}", e))?;
                    let mut params = json!({"state": state});
                    add_session_target_params(&mut params, cli);
                    let resp = send_cmd(client, "session.storage.import", params).await?;
                    print_response(&resp);
                }
            },
        },

        Command::StatusV2 => {
            let resp = send_cmd(client, "daemon.status", json!({})).await?;
            print_response(&resp);
        }

        // ── Daemon ─────────────────────────────────────────
        Command::Daemon { action } => match action {
            DaemonAction::Start => unreachable!(),
            DaemonAction::Stop => {
                let resp = send_cmd(client, "daemon.stop", json!({})).await?;
                print_response(&resp);
            }
            DaemonAction::Status => {
                let resp = send_cmd(client, "daemon.status", json!({})).await?;
                print_response(&resp);
            }
        },

        // ── Browser ────────────────────────────────────────
        Command::Browser { action } => match action {
            BrowserAction::Connect { host } => {
                let mut params = json!({"host": host});
                add_session_param(&mut params, cli);
                let resp = send_cmd(client, "browser.connect", params).await?;
                print_response(&resp);
            }
            BrowserAction::Discover { path } => {
                let mut params = json!({});
                if let Some(p) = path {
                    params["path"] = json!(p);
                }
                add_session_param(&mut params, cli);
                let resp = send_cmd(client, "browser.discover", params).await?;
                print_response(&resp);
            }
            BrowserAction::List => {
                let resp = send_cmd(client, "browser.list", json!({})).await?;
                print_response(&resp);
            }
            BrowserAction::Disconnect { host } => {
                let resp = send_cmd(client, "browser.disconnect", json!({"host": host})).await?;
                print_response(&resp);
            }
        },

        // ── Session-native page inspection ──────────────────────────
        Command::Find {
            selector,
            attributes,
            max,
            include_text,
        } => {
            let mut params = json!({"selector": selector, "include_text": include_text});
            if let Some(attrs) = attributes {
                let attr_list: Vec<&str> = attrs.split(',').map(|s| s.trim()).collect();
                params["attributes"] = json!(attr_list);
            }
            if let Some(m) = max {
                params["max"] = json!(m);
            }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "find", params).await?;
            print_response(&resp);
        }

        Command::Search {
            text,
            regex,
            scope,
            context,
            max,
        } => {
            let mut params = json!({"text": text, "regex": regex});
            if let Some(s) = scope {
                params["scope"] = json!(s);
            }
            if let Some(c) = context {
                params["context"] = json!(c);
            }
            if let Some(m) = max {
                params["max"] = json!(m);
            }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "search", params).await?;
            print_response(&resp);
        }

        Command::Html { selector } => {
            let mut params = json!({});
            if let Some(s) = selector {
                params["selector"] = json!(s);
            }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "html", params).await?;
            print_response(&resp);
        }

        Command::Console { level, limit } => {
            let mut params = json!({"level": level});
            if let Some(n) = limit {
                params["limit"] = json!(n);
            }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "console", params).await?;
            print_response(&resp);
        }

        Command::Pdf { output } => {
            let mut params = json!({});
            if let Some(o) = output {
                params["output"] = json!(o);
            }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "pdf", params).await?;
            handle_binary_response(&resp, output.as_deref(), concat!("page", ".pdf"));
        }

        // ── Management (Dialog) ───────────────────────────
        Command::Dialog { action } => match action {
            DialogAction::List => {
                let mut params = json!({});
                add_session_target_params(&mut params, cli);
                let resp = send_cmd(client, "dialog.list", params).await?;
                print_response(&resp);
            }
            DialogAction::Accept { text } => {
                let mut params = json!({});
                add_session_target_params(&mut params, cli);
                if let Some(txt) = text {
                    params["text"] = json!(txt);
                }
                let resp = send_cmd(client, "dialog.accept", params).await?;
                print_response(&resp);
            }
            DialogAction::Dismiss => {
                let mut params = json!({});
                add_session_target_params(&mut params, cli);
                let resp = send_cmd(client, "dialog.dismiss", params).await?;
                print_response(&resp);
            }
            DialogAction::Policy { policy } => {
                let mut params = json!({});
                add_session_target_params(&mut params, cli);
                if let Some(p) = policy {
                    params["policy"] = json!(p);
                }
                let resp = send_cmd(client, "dialog.policy", params).await?;
                print_response(&resp);
            }
        },

        // ── Management (Debug) ────────────────────────────
        Command::Debug { action } => match action {
            DebugAction::Block { pattern } => {
                let mut params = json!({"pattern": pattern});
                add_session_target_params(&mut params, cli);
                let resp = send_cmd(client, "debug.block", params).await?;
                print_response(&resp);
            }
            DebugAction::Unblock => {
                let mut params = json!({});
                add_session_target_params(&mut params, cli);
                let resp = send_cmd(client, "debug.unblock", params).await?;
                print_response(&resp);
            }
            DebugAction::Cdp { method, params } => {
                let cdp_params = match params {
                    Some(p) => serde_json::from_str(p)
                        .map_err(|e| format!("invalid CDP params JSON: {}", e))?,
                    None => json!({}),
                };
                let mut req_params = json!({"method": method, "params": cdp_params});
                add_session_target_params(&mut req_params, cli);
                let resp = send_cmd(client, "debug.cdp", req_params).await?;
                print_response(&resp);
            }
        },

        Command::Completions { .. } => unreachable!(),
        Command::Setup => unreachable!(),
    }

    Ok(())
}

// ── Helper functions ───────────────────────────────────────────

/// Send a command to the daemon and return the response.
async fn send_cmd(
    client: &mut DaemonClient,
    cmd: &str,
    params: serde_json::Value,
) -> Result<Response, String> {
    let req = build_request(cmd, params);
    client
        .send_request(&req)
        .await
        .map_err(|e| format!("daemon communication error: {}", e))
}

/// Print a response according to the output format.
/// Print a response as JSON to stdout.
fn print_response(resp: &Response) {
    println!("{}", serde_json::to_string(resp).unwrap_or_default());
}

fn canonical_directory(path: &str) -> Result<std::path::PathBuf, Response> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("failed to resolve output directory '{}': {error}", path),
            Some("choose an existing download directory".into()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("output directory is not a directory: {path}"),
            Some("choose an existing download directory".into()),
        ));
    }
    Ok(canonical)
}

#[derive(Debug)]
struct AppendWriteFailure {
    error: std::io::Error,
    bytes_written: usize,
}

fn write_single_append<W: std::io::Write>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<usize, AppendWriteFailure> {
    let bytes_written = writer.write(bytes).map_err(|error| AppendWriteFailure {
        error,
        bytes_written: 0,
    })?;
    if bytes_written != bytes.len() {
        return Err(AppendWriteFailure {
            error: std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!(
                    "single append wrote {bytes_written} of {} bytes",
                    bytes.len()
                ),
            ),
            bytes_written,
        });
    }
    writer.flush().map_err(|error| AppendWriteFailure {
        error,
        bytes_written,
    })?;
    Ok(bytes_written)
}

fn append_write_failure_response(
    destination: &std::path::Path,
    requested_bytes: usize,
    failure: &AppendWriteFailure,
) -> Response {
    let partial_write = failure.bytes_written > 0 && failure.bytes_written < requested_bytes;
    let retry_safe = failure.bytes_written == 0;
    let suggestion = if retry_safe {
        "fix the destination and retry the append"
    } else {
        "do not retry automatically; inspect the destination to avoid duplicate bytes"
    };
    let mut response = Response::error_detail(
        ErrorCode::FileWriteFailed,
        format!(
            "failed to append to '{}': {}",
            destination.display(),
            failure.error
        ),
        Some(suggestion.into()),
    );
    if let Some(serde_json::Value::Object(error)) = response.error.as_mut() {
        error.insert("bytes_written".into(), json!(failure.bytes_written));
        error.insert("partial_write".into(), json!(partial_write));
        error.insert("retry_safe".into(), json!(retry_safe));
    }
    response
}

fn append_evaluate_result(resp: &Response, path: &str) -> Response {
    if !resp.ok {
        return resp.clone();
    }
    let Some(result) = resp
        .data
        .as_ref()
        .and_then(|data| data.get("result"))
        .and_then(serde_json::Value::as_str)
    else {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            "evaluate --append-to requires the expression to return a string".into(),
            Some("return a string, or use JSON.stringify(value) before appending".into()),
        );
    };

    let requested = std::path::PathBuf::from(path);
    let absolute = if requested.is_absolute() {
        requested
    } else {
        match std::env::current_dir() {
            Ok(current_dir) => current_dir.join(requested),
            Err(error) => {
                return Response::error_detail(
                    ErrorCode::FileWriteFailed,
                    format!("failed to resolve current directory: {error}"),
                    None,
                )
            }
        }
    };
    if absolute.is_dir() {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            format!("append destination is a directory: {}", absolute.display()),
            Some("choose a file path for --append-to".into()),
        );
    }
    if absolute
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            format!(
                "append destination must not be a symbolic link: {}",
                absolute.display()
            ),
            Some("choose a regular file path for --append-to".into()),
        );
    }
    let Some(filename) = absolute.file_name() else {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            format!(
                "append destination is not a file path: {}",
                absolute.display()
            ),
            None,
        );
    };
    let Some(parent) = absolute.parent() else {
        return Response::error_detail(
            ErrorCode::InvalidArgument,
            format!(
                "append destination has no parent directory: {}",
                absolute.display()
            ),
            None,
        );
    };
    let parent = match std::fs::canonicalize(parent) {
        Ok(parent) if parent.is_dir() => parent,
        Ok(parent) => {
            return Response::error_detail(
                ErrorCode::FileWriteFailed,
                format!("append parent is not a directory: {}", parent.display()),
                None,
            )
        }
        Err(error) => {
            return Response::error_detail(
                ErrorCode::FileWriteFailed,
                format!(
                    "failed to resolve append parent '{}': {error}",
                    parent.display()
                ),
                None,
            )
        }
    };
    let destination = parent.join(filename);
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&destination)
    {
        Ok(file) => file,
        Err(error) => {
            return append_write_failure_response(
                &destination,
                result.len(),
                &AppendWriteFailure {
                    error,
                    bytes_written: 0,
                },
            )
        }
    };
    if let Err(failure) = write_single_append(&mut file, result.as_bytes()) {
        return append_write_failure_response(&destination, result.len(), &failure);
    }

    let file = std::fs::canonicalize(&destination).unwrap_or(destination);
    Response::ok(json!({
        "file": file,
        "bytes_appended": result.len(),
        "result_type": "string",
    }))
}

/// Handle binary (base64) responses: save to file or print info.
fn handle_binary_response(resp: &Response, output: Option<&str>, _default_name: &str) {
    if !resp.ok {
        print_response(resp);
        return;
    }

    // If already saved by daemon (output param was in the request)
    if let Some(data) = &resp.data {
        if data.get("file").is_some() {
            print_response(resp);
            return;
        }
    }

    // Save base64 data to file if output specified but wasn't in request
    if let (Some(path), Some(data)) = (
        output,
        resp.data
            .as_ref()
            .and_then(|d| d.get("data"))
            .and_then(|v| v.as_str()),
    ) {
        match base64_decode_and_save(data, path) {
            Ok(()) => println!(
                "{}",
                serde_json::json!({"ok": true, "data": {"file": path}})
            ),
            Err(e) => println!(
                "{}",
                serde_json::json!({"ok": false, "error": format!("save failed: {}", e)})
            ),
        }
    } else {
        print_response(resp);
    }
}

/// Decode base64 data and write to file.
fn base64_decode_and_save(data: &str, path: &str) -> Result<(), String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("base64 decode error: {}", e))?;
    std::fs::write(path, bytes).map_err(|e| format!("write error: {}", e))
}

// ── Daemon exit wait ──────────────────────────────────────────

/// After sending `daemon.stop`, poll the port until the daemon process exits
/// (port becomes unreachable). Gives up after 5 seconds with a warning.
async fn wait_for_daemon_exit() {
    let port = match browserkit::daemon::read_port_file() {
        Some(p) => p,
        None => return, // port file already gone, daemon exited
    };

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let poll_interval = std::time::Duration::from_millis(50);

    loop {
        if tokio::time::Instant::now() >= deadline {
            eprintln!("warning: daemon did not exit within 5s, may need manual cleanup");
            return;
        }
        // Check if port is still reachable
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port)),
        )
        .await
        {
            Ok(Ok(_)) => {
                // Still alive, wait and retry
                tokio::time::sleep(poll_interval).await;
            }
            _ => {
                // Connection refused or timeout — daemon is gone
                return;
            }
        }
    }
}

// ── CLI Argument Validation Tests ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Helper: attempt to parse CLI args, return whether it succeeded.
    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // ── V2 commands ──────────────────────────────────────────────

    #[test]
    fn cli_parses_connect() {
        let cli = try_parse(&["bk", "connect"]).unwrap();
        assert!(matches!(cli.command, Command::Connect));
    }

    #[test]
    fn cli_parses_connect_with_session() {
        let cli = try_parse(&["bk", "--session", "agent-a", "connect"]).unwrap();
        assert_eq!(cli.session, Some("agent-a".into()));
    }

    #[test]
    fn browser_admin_connection_params_include_global_session() {
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "browser",
            "connect",
            "remote.example:9222",
        ])
        .unwrap();
        let mut params = json!({"host": "remote.example:9222"});

        add_session_param(&mut params, &cli);

        assert_eq!(params["session"], "agent-a");
    }

    #[test]
    fn attach_help_names_default_session_constraint() {
        assert!(HELP_TEXT.contains("Attach existing browser tab to default session"));
        let command = Cli::command();
        let attach = command.find_subcommand("attach").expect("attach command");
        assert!(attach
            .get_long_about()
            .expect("attach long help")
            .to_string()
            .contains("default session"));
    }

    #[test]
    fn cli_parses_snapshot() {
        let cli = try_parse(&["bk", "snapshot"]).unwrap();
        assert!(matches!(cli.command, Command::Snapshot { .. }));
    }

    #[test]
    fn cli_parses_snapshot_full() {
        let cli = try_parse(&["bk", "snapshot", "--full"]).unwrap();
        if let Command::Snapshot { full, .. } = cli.command {
            assert!(full);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_snapshot_token_budget() {
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "--target",
            "TAB1",
            "--timeout",
            "5000",
            "snapshot",
            "--max-tokens",
            "512",
            "--wait",
            "none",
        ])
        .unwrap();
        let Command::Snapshot {
            full,
            no_page_text,
            wait,
            max_tokens,
        } = &cli.command
        else {
            panic!("wrong variant");
        };
        let params = build_snapshot_params(*full, *no_page_text, wait, *max_tokens, &cli);
        assert_eq!(params["max_tokens"], 512);
        assert_eq!(params["session"], "agent-a");
        assert_eq!(params["target"], "TAB1");
        assert_eq!(params["timeout"], 5000);
        assert_eq!(params["wait"], "none");
    }

    #[test]
    fn cli_parses_act_click() {
        let cli = try_parse(&["bk", "act", "click", "--ref", "42"]).unwrap();
        if let Command::Act {
            kind, element_ref, ..
        } = &cli.command
        {
            assert_eq!(kind.as_deref(), Some("click"));
            assert_eq!(*element_ref, Some(42));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_act_type() {
        let cli = try_parse(&["bk", "act", "type", "--ref", "55", "--text", "hello"]).unwrap();
        if let Command::Act {
            kind,
            element_ref,
            text,
            ..
        } = &cli.command
        {
            assert_eq!(kind.as_deref(), Some("type"));
            assert_eq!(*element_ref, Some(55));
            assert_eq!(text.as_deref(), Some("hello"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_act_press() {
        let cli = try_parse(&["bk", "act", "press", "--keys", "Enter"]).unwrap();
        if let Command::Act { kind, keys, .. } = &cli.command {
            assert_eq!(kind.as_deref(), Some("press"));
            assert_eq!(keys, &vec!["Enter".to_string()]);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_act_scroll_hover_and_focus() {
        let scroll = try_parse(&[
            "bk",
            "act",
            "scroll",
            "--direction",
            "down",
            "--amount",
            "250",
        ])
        .unwrap();
        assert!(matches!(
            scroll.command,
            Command::Act { ref kind, ref direction, amount: Some(amount), .. }
                if kind.as_deref() == Some("scroll")
                    && direction.as_deref() == Some("down")
                    && amount == 250.0
        ));

        let hover = try_parse(&["bk", "act", "hover", "--ref", "42"]).unwrap();
        assert!(matches!(
            hover.command,
            Command::Act { ref kind, element_ref: Some(42), .. }
                if kind.as_deref() == Some("hover")
        ));

        let focus = try_parse(&["bk", "act", "focus", "--ref", "43"]).unwrap();
        assert!(matches!(
            focus.command,
            Command::Act { ref kind, element_ref: Some(43), .. }
                if kind.as_deref() == Some("focus")
        ));
    }

    #[test]
    fn cli_parses_act_select_and_options() {
        let select =
            try_parse(&["bk", "act", "select", "--ref", "42", "--value", "green"]).unwrap();
        assert!(matches!(
            select.command,
            Command::Act { ref kind, ref value, .. }
                if kind.as_deref() == Some("select") && value.as_deref() == Some("green")
        ));

        let options = try_parse(&["bk", "act", "options", "--ref", "42"]).unwrap();
        assert!(matches!(
            options.command,
            Command::Act { ref kind, element_ref: Some(42), .. }
                if kind.as_deref() == Some("options")
        ));
    }

    #[test]
    fn cli_parses_act_fill_sets() {
        let cli = try_parse(&[
            "bk",
            "act",
            "fill",
            "--set",
            "ref:42=alpha",
            "--set",
            "ref:55=beta",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Act {
                ref kind,
                ref set,
                ..
            } if kind.as_deref() == Some("fill") && set.len() == 2
        ));
    }

    #[test]
    fn cli_parses_act_upload_and_drag() {
        let upload = try_parse(&["bk", "act", "upload", "--ref", "42", "a.txt", "b.txt"]).unwrap();
        assert!(matches!(
            upload.command,
            Command::Act {
                ref kind,
                element_ref: Some(42),
                ref files,
                ..
            } if kind.as_deref() == Some("upload") && files == &["a.txt", "b.txt"]
        ));

        let drag = try_parse(&[
            "bk",
            "act",
            "drag",
            "--from-ref",
            "10",
            "--to-selector",
            "#drop",
        ])
        .unwrap();
        assert!(matches!(
            drag.command,
            Command::Act {
                ref kind,
                from_ref: Some(10),
                ref to_selector,
                ..
            } if kind.as_deref() == Some("drag") && to_selector.as_deref() == Some("#drop")
        ));
    }

    #[test]
    fn cli_parses_act_scroll_selector() {
        let cli = try_parse(&["bk", "act", "scroll", "--selector", "#main"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Act {
                ref kind,
                ref selector,
                element_ref: None,
                ..
            } if kind.as_deref() == Some("scroll") && selector.as_deref() == Some("#main")
        ));
    }

    #[test]
    fn cli_parses_navigate_url() {
        let cli = try_parse(&["bk", "navigate", "https://example.com"]).unwrap();
        if let Command::Navigate { url, .. } = &cli.command {
            assert_eq!(url.as_deref(), Some("https://example.com"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_navigate_back() {
        let cli = try_parse(&["bk", "navigate", "--back"]).unwrap();
        if let Command::Navigate { back, .. } = &cli.command {
            assert!(*back);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_open() {
        let cli = try_parse(&["bk", "open", "https://x.com"]).unwrap();
        if let Command::OpenV2 { url } = &cli.command {
            assert_eq!(url, "https://x.com");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_close() {
        let cli = try_parse(&["bk", "close"]).unwrap();
        assert!(matches!(cli.command, Command::CloseV2));
    }

    #[test]
    fn cli_parses_tabs() {
        let cli = try_parse(&["bk", "tabs"]).unwrap();
        assert!(matches!(cli.command, Command::Tabs));
    }

    #[test]
    fn cli_parses_attach_target_and_pattern() {
        let target = try_parse(&["bk", "attach", "--target", "ABC123"]).unwrap();
        assert!(matches!(target.command, Command::Attach { pattern: None }));

        let pattern = try_parse(&["bk", "attach", "github.com"]).unwrap();
        assert!(matches!(
            pattern.command,
            Command::Attach { pattern: Some(ref p) } if p == "github.com"
        ));
    }

    #[test]
    fn cli_parses_evaluate() {
        let cli = try_parse(&["bk", "evaluate", "document.title"]).unwrap();
        if let Command::Evaluate { expression, .. } = &cli.command {
            assert_eq!(expression.as_deref(), Some("document.title"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_evaluate_append_to() {
        let expression = try_parse(&[
            "bk",
            "evaluate",
            "JSON.stringify({ok: true})",
            "--append-to",
            "results.jsonl",
        ]);
        let file = try_parse(&[
            "bk",
            "evaluate",
            "--file",
            "extract.js",
            "--append-to",
            "results.jsonl",
        ]);

        assert!(expression.is_ok());
        assert!(file.is_ok());
    }

    #[test]
    fn evaluate_append_writes_exact_string_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.txt");
        let path_arg = path.to_string_lossy();

        let first = append_evaluate_result(&Response::ok(json!({"result": "first"})), &path_arg);
        let second = append_evaluate_result(&Response::ok(json!({"result": "second"})), &path_arg);

        assert!(first.ok);
        assert!(second.ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "firstsecond");
        assert_eq!(second.data.unwrap()["bytes_appended"], 6);
    }

    #[test]
    fn evaluate_append_rejects_non_string_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-created.txt");
        let non_string = append_evaluate_result(
            &Response::ok(json!({"result": [1, 2, 3]})),
            &file.to_string_lossy(),
        );
        let directory = append_evaluate_result(
            &Response::ok(json!({"result": "text"})),
            &dir.path().to_string_lossy(),
        );

        assert_eq!(non_string.error.unwrap()["code"], "INVALID_ARGUMENT");
        assert!(!file.exists());
        assert_eq!(directory.error.unwrap()["code"], "INVALID_ARGUMENT");
    }

    #[test]
    fn evaluate_append_reports_write_failure_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("results.txt");
        let response = append_evaluate_result(
            &Response::ok(json!({"result": "text"})),
            &path.to_string_lossy(),
        );

        assert_eq!(response.error.unwrap()["code"], "FILE_WRITE_FAILED");
    }

    #[test]
    fn evaluate_append_path_is_not_sent_to_daemon() {
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "--target",
            "TAB1",
            "--timeout",
            "5000",
            "evaluate",
            "extract()",
            "--append-to",
            "results.txt",
        ])
        .unwrap();

        let params = build_evaluate_params("extract()", &cli);
        assert_eq!(params["expression"], "extract()");
        assert_eq!(params["session"], "agent-a");
        assert_eq!(params["target"], "TAB1");
        assert_eq!(params["timeout"], 5000);
        assert!(params.get("append_to").is_none());
    }

    #[test]
    fn cli_parses_bounded_network_watch() {
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "--target",
            "TAB1",
            "--timeout",
            "5000",
            "network",
            "watch",
            "--pattern",
            "/api/orders",
            "--count",
            "3",
        ])
        .expect("network watch should be a public CLI command");

        let Command::Network {
            action: NetworkAction::Watch { pattern, count },
        } = &cli.command
        else {
            panic!("wrong variant");
        };
        let params = build_network_watch_params(pattern, *count, &cli);
        assert_eq!(params["session"], "agent-a");
        assert_eq!(params["target"], "TAB1");
        assert_eq!(params["timeout"], 5000);
        assert_eq!(params["pattern"], "/api/orders");
        assert_eq!(params["count"], 3);
    }

    #[test]
    fn cli_parses_download_lifecycle_command() {
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "--target",
            "TAB1",
            "--timeout",
            "15000",
            "download",
            "--ref",
            "42",
            "--output-dir",
            "downloads",
        ]);

        let cli = cli.expect("download should be a public CLI command");
        assert!(matches!(
            cli.command,
            Command::Download {
                element_ref: 42,
                ref output_dir,
            } if output_dir == "downloads"
        ));
    }

    #[test]
    fn download_request_shape_uses_canonical_output_directory() {
        let output_dir = tempfile::tempdir().unwrap();
        let output_arg = output_dir.path().to_string_lossy().into_owned();
        let cli = try_parse(&[
            "bk",
            "--session",
            "agent-a",
            "--target",
            "TAB1",
            "--timeout",
            "15000",
            "download",
            "--ref",
            "42",
            "--output-dir",
            &output_arg,
        ])
        .unwrap();

        let params = build_download_params(42, &output_arg, &cli).unwrap();
        assert_eq!(params["ref"], 42);
        assert_eq!(
            params["output_dir"],
            json!(std::fs::canonicalize(output_dir.path())
                .unwrap()
                .to_string_lossy())
        );
        assert_eq!(params["session"], "agent-a");
        assert_eq!(params["target"], "TAB1");
        assert_eq!(params["timeout"], 15000);
    }

    #[test]
    fn download_directory_errors_are_structured_responses() {
        let cli = try_parse(&[
            "bk",
            "download",
            "--ref",
            "42",
            "--output-dir",
            "missing-download-directory",
        ])
        .unwrap();

        let response = build_download_params(42, "missing-download-directory", &cli)
            .expect_err("missing directory must be rejected locally");
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "INVALID_ARGUMENT");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed to resolve output directory"));
    }

    struct PartialAppendWriter;

    impl std::io::Write for PartialAppendWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len().min(3))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn evaluate_append_partial_write_reports_non_retryable_progress() {
        let failure = write_single_append(&mut PartialAppendWriter, b"abcdef")
            .expect_err("a partial single write must fail");
        assert_eq!(failure.bytes_written, 3);

        let response =
            append_write_failure_response(std::path::Path::new("results.txt"), 6, &failure);
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["error"]["bytes_written"], 3);
        assert_eq!(value["error"]["partial_write"], true);
        assert_eq!(value["error"]["retry_safe"], false);
        assert!(value["error"]["suggestion"]
            .as_str()
            .unwrap()
            .contains("do not retry automatically"));
    }

    #[test]
    fn cli_parses_screenshot() {
        let cli = try_parse(&["bk", "screenshot", "--full-page"]).unwrap();
        if let Command::ScreenshotV2 { full_page, .. } = &cli.command {
            assert!(*full_page);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_screenshot_selector_and_labels() {
        let cli = try_parse(&["bk", "screenshot", "--selector", "#app", "--labels"]).unwrap();
        if let Command::ScreenshotV2 {
            selector, labels, ..
        } = &cli.command
        {
            assert_eq!(selector.as_deref(), Some("#app"));
            assert!(*labels);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_session_close() {
        let cli = try_parse(&["bk", "session", "close"]).unwrap();
        assert!(matches!(cli.command, Command::Session { .. }));
    }

    #[test]
    fn cli_parses_session_list() {
        let cli = try_parse(&["bk", "session", "list"]).unwrap();
        if let Command::Session { action } = &cli.command {
            assert!(matches!(action, SessionAction::List));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_session_storage_local_get() {
        let cli = try_parse(&["bk", "session", "storage", "local", "get", "token"]).unwrap();
        if let Command::Session { action } = &cli.command {
            assert!(matches!(
                action,
                SessionAction::Storage {
                    action: SessionStorageAction::Local {
                        action: SessionLocalStorageAction::Get { key }
                    }
                } if key == "token"
            ));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn cli_parses_status() {
        let cli = try_parse(&["bk", "status"]).unwrap();
        assert!(matches!(cli.command, Command::StatusV2));
    }

    #[test]
    fn cli_global_session_param() {
        let cli = try_parse(&["bk", "--session", "my-session", "snapshot"]).unwrap();
        assert_eq!(cli.session, Some("my-session".into()));
    }

    #[test]
    fn cli_global_target_param() {
        let cli = try_parse(&["bk", "--target", "TAB123", "snapshot"]).unwrap();
        assert_eq!(cli.target, Some("TAB123".into()));
    }

    #[test]
    fn cli_global_timeout_param() {
        let cli = try_parse(&["bk", "--timeout", "60000", "act", "click", "--ref", "5"]).unwrap();
        assert_eq!(cli.timeout, Some(60000));
    }

    #[test]
    fn cli_global_no_state_diff_param() {
        let cli = try_parse(&["bk", "--no-state-diff", "act", "click", "--ref", "5"]).unwrap();
        assert!(cli.no_state_diff);
    }

    // ── Removed aliases ───────────────────────────────────────────

    #[test]
    fn cli_rejects_removed_goto_alias() {
        let result = try_parse(&["bk", "goto", "https://a.com"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_removed_info_alias() {
        let result = try_parse(&["bk", "info"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_removed_eval_alias() {
        let result = try_parse(&["bk", "eval", "document.title"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_removed_shot_alias() {
        let result = try_parse(&["bk", "shot"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_removed_navigation_aliases() {
        for args in [
            &["bk", "back"][..],
            &["bk", "forward"][..],
            &["bk", "reload"][..],
        ] {
            let result = try_parse(args);
            assert!(result.is_err(), "{args:?} should be removed");
        }
    }

    #[test]
    fn cli_rejects_removed_workspace_aliases() {
        for args in [
            &["bk", "new"][..],
            &["bk", "ls"][..],
            &["bk", "rm", "ws1"][..],
        ] {
            let result = try_parse(args);
            assert!(result.is_err(), "{args:?} should be removed");
        }
    }

    #[test]
    fn cli_rejects_removed_url_title_aliases() {
        for args in [&["bk", "url"][..], &["bk", "title"][..]] {
            let result = try_parse(args);
            assert!(result.is_err(), "{args:?} should be removed");
        }
    }

    fn assert_cli_commands_removed(cases: &[&[&str]]) {
        for args in cases {
            assert!(try_parse(args).is_err(), "{args:?} should be removed");
        }
    }

    #[test]
    fn cli_rejects_removed_scroll_hover_focus_commands() {
        assert_cli_commands_removed(&[
            &["bk", "scroll", "down"][..],
            &["bk", "hover", "--ref", "42"][..],
            &["bk", "focus", "--ref", "43"][..],
        ]);
    }

    #[test]
    fn cli_rejects_removed_select_and_options_commands() {
        assert_cli_commands_removed(&[
            &["bk", "select", "--ref", "42", "green"][..],
            &["bk", "options", "--ref", "42"][..],
        ]);
    }

    #[test]
    fn cli_rejects_removed_fill_command() {
        assert_cli_commands_removed(&[&["bk", "fill", "--set", "ref:42=value"][..]]);
    }

    #[test]
    fn cli_rejects_removed_upload_and_drag_commands() {
        assert_cli_commands_removed(&[
            &["bk", "upload", "--ref", "42", "a.txt"][..],
            &["bk", "drag", "--from-ref", "10", "--to-ref", "20"][..],
        ]);
    }

    #[test]
    fn cli_rejects_removed_keys_command() {
        assert_cli_commands_removed(&[
            &["bk", "keys", "Enter"][..],
            &["bk", "keys", "Control+a"][..],
        ]);
    }

    #[test]
    fn cli_rejects_removed_click_and_type_commands() {
        assert_cli_commands_removed(&[
            &["bk", "click", "--ref", "42"][..],
            &["bk", "type", "--ref", "42", "hello"][..],
        ]);
    }

    #[test]
    fn top_level_help_omits_removed_action_alias_guidance() {
        for removed in [
            "click -> use act click",
            "type -> use act type",
            "keys -> use act press --keys",
            "scroll -> use act scroll",
            "hover -> use act hover",
            "focus -> use act focus",
            "fill -> use act fill",
            "select -> use act select",
            "options -> use act options",
            "upload -> use act upload",
            "drag -> use act drag",
        ] {
            assert!(!HELP_TEXT.contains(removed), "{removed}");
        }
    }

    #[test]
    fn top_level_help_primary_includes_attach() {
        assert!(
            HELP_TEXT.contains("  attach"),
            "custom primary help should list attach"
        );
    }

    #[test]
    fn cli_rejects_removed_streaming_debug_commands() {
        assert_cli_commands_removed(&[
            &["bk", "debug", "monitor"][..],
            &["bk", "debug", "har", "https://example.com"][..],
            &["bk", "debug", "events"][..],
        ]);
    }

    #[test]
    fn cli_rejects_all_removed_workspace_surfaces() {
        for args in [
            &["bk", "ws", "list"][..],
            &["bk", "tab", "list"][..],
            &["bk", "fetch", "https://example.com"][..],
            &["bk", "storage", "export"][..],
            &["bk", "debug", "monitor"][..],
            &["bk", "debug", "har", "https://example.com"][..],
            &["bk", "debug", "events"][..],
            &["bk", "--ws", "abc", "snapshot"][..],
        ] {
            assert!(try_parse(args).is_err(), "removed command parsed: {args:?}");
        }
    }

    // ── Removed flags ────────────────────────────────────────────

    #[test]
    fn cli_no_format_flag() {
        let result = try_parse(&["bk", "--format", "text", "snapshot"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_no_ws_flag() {
        let result = try_parse(&["bk", "--ws", "abc", "snapshot"]);
        assert!(result.is_err());
    }

    #[test]
    fn press_with_single_key_succeeds() {
        let result = try_parse(&["bk", "act", "press", "--keys", "Enter"]);
        assert!(result.is_ok());
    }

    #[test]
    fn back_is_removed() {
        let result = try_parse(&["bk", "back"]);
        assert!(result.is_err());
    }

    #[test]
    fn forward_is_removed() {
        let result = try_parse(&["bk", "forward"]);
        assert!(result.is_err());
    }

    #[test]
    fn setup_succeeds() {
        let result = try_parse(&["bk", "setup"]);
        assert!(result.is_ok());
        if let Ok(cli) = result {
            assert!(matches!(cli.command, Command::Setup));
        }
    }

    #[test]
    fn find_requires_selector() {
        let result = try_parse(&["bk", "find"]);
        assert!(result.is_err());
    }

    #[test]
    fn find_with_selector_succeeds() {
        let result = try_parse(&["bk", "find", "a[href]"]);
        assert!(result.is_ok());
    }

    #[test]
    fn pdf_no_longer_accepts_a_url() {
        assert!(try_parse(&["bk", "pdf", "https://example.com"]).is_err());
        assert!(try_parse(&["bk", "pdf", "--output", concat!("page", ".pdf")]).is_ok());
    }

    #[test]
    fn wait_with_selector_succeeds() {
        let result = try_parse(&["bk", "wait", "--selector", "#foo"]);
        assert!(result.is_ok());
    }

    #[test]
    fn wait_with_text_succeeds() {
        let result = try_parse(&["bk", "wait", "--text", "Loading"]);
        assert!(result.is_ok());
    }
}
