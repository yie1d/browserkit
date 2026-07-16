// CLI entry point: clap command parsing + daemon client wiring
//
// Legacy v1 workspace resolution priority:
//   1. --ws / -w flag (explicit)
//   2. BK_WS environment variable
//   3. Daemon default workspace (ws.default)
//   4. Auto-detect when only one workspace exists
//   5. Error with helpful message

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
  attach      Attach existing browser tab to session
  close       Close tab
  tabs        List tabs in session
  wait        Wait for page condition
  evaluate    Execute JavaScript
  screenshot  Take a screenshot
  find        Find elements by CSS selector
  search      Search text in page
  html        Get page HTML
  console     Show console log buffer
  pdf         Generate PDF of current target
  session     Session management (close/list/cookies)
  status      Connection status

Legacy (v1, will be removed in Phase 3):
  url/title/fetch/ws/tab/browser/daemon/storage/dialog/debug

Removed aliases:
  goto -> use navigate    info -> use snapshot
  eval -> use evaluate    shot -> use screenshot
  back/forward/reload -> use navigate --back/--forward/--reload

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

// ── Top-level CLI ──────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "bk", about = "Persistent browser runtime CLI for AI agents", long_about = "Persistent browser runtime CLI for AI agents.\n\nAll output is JSON. Commands communicate with the local browserkit daemon over TCP.", version)]
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

    /// Attach an existing browser tab to the current session
    #[command(about = "Attach existing browser tab")]
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

    // ══════════════════════════════════════════════════════════════
    // V1 LEGACY COMMANDS (preserved, removed in Phase 3)
    // ══════════════════════════════════════════════════════════════

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
    /// Generate PDF of current target
    Pdf {
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Fetch HTML from URL (one-shot)
    #[command(hide = true)]
    Fetch {
        url: String,
    },

    // ── Management ────────────────────────────────────────────────
    /// Workspace management
    #[command(hide = true)]
    Ws {
        #[command(subcommand)]
        action: WsAction,
    },
    /// Tab management
    #[command(hide = true)]
    Tab {
        #[command(subcommand)]
        action: TabAction,
    },
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
    /// Storage management
    #[command(hide = true)]
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },
    /// Dialog management
    #[command(hide = true)]
    Dialog {
        #[command(subcommand)]
        action: DialogAction,
    },
    /// Debug tools
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    /// Generate shell completions
    #[command(hide = true)]
    Completions {
        shell: clap_complete::Shell,
    },
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
pub enum WsAction {
    /// Create a new workspace
    New {
        /// Browser host to connect to
        #[arg(long)]
        host: Option<String>,
        /// Workspace label
        #[arg(short, long)]
        label: Option<String>,
        /// Show browser window (override config headless = true)
        #[arg(long)]
        no_headless: bool,
        /// Create in attached mode (share user's browser context, no isolation)
        #[arg(long)]
        attached: bool,
        /// URL/title/target_id pattern to filter tabs (only with --attached)
        #[arg(short, long)]
        pattern: Option<String>,
    },
    /// Attach existing browser tabs into a new attached workspace
    Attach {
        /// URL/title/target_id pattern to filter tabs
        #[arg(short, long)]
        pattern: Option<String>,
        /// Browser host (must already be connected)
        #[arg(long)]
        host: Option<String>,
        /// Workspace label
        #[arg(short, long)]
        label: Option<String>,
    },
    /// List all workspaces
    List,
    /// Show workspace details
    Info {
        /// Workspace ID (optional, uses default if omitted)
        wid: Option<String>,
    },
    /// Close a workspace
    Close {
        /// Workspace ID
        wid: String,
    },
    /// Set default workspace
    Use {
        /// Workspace ID
        wid: String,
    },
    /// Show current default workspace
    Default,
}

#[derive(Subcommand)]
pub enum TabAction {
    /// Create a new tab
    New {
        /// Initial URL (default: about:blank)
        url: Option<String>,
    },
    /// Attach an existing browser tab by URL/title/target_id pattern
    Attach {
        /// Substring to match against URL, title, or target_id prefix
        pattern: String,
    },
    /// List tabs in workspace
    List,
    /// Switch active tab
    Switch {
        /// Tab alias (t1, t2, ...) or tid/prefix
        tid: String,
    },
    /// Close a tab
    Close {
        /// Tab alias (t1, t2, ...) or tid/prefix
        tid: String,
    },
}


#[derive(Subcommand)]
pub enum StorageAction {
    /// Cookie operations
    Cookies {
        #[command(subcommand)]
        action: CookieAction,
    },
    /// LocalStorage operations
    Local {
        #[command(subcommand)]
        action: LocalAction,
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
pub enum CookieAction {
    /// Get all cookies
    Get,
    /// Set cookies (JSON array)
    Set {
        /// Cookie JSON
        json: String,
    },
    /// Clear all cookies
    Clear,
}

#[derive(Subcommand)]
pub enum LocalAction {
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
pub enum DebugAction {
    /// Monitor network requests (streaming)
    Monitor,
    /// Navigate and record HAR
    Har {
        /// URL to navigate to
        url: String,
    },
    /// Block requests matching pattern
    Block {
        /// URL pattern to block
        pattern: String,
    },
    /// Unblock requests
    Unblock {
        /// Pattern to unblock (all if omitted)
        pattern: Option<String>,
    },
    /// Send raw CDP command
    Cdp {
        /// CDP method name
        method: String,
        /// JSON params (optional)
        params: Option<String>,
    },
    /// Listen to CDP events (streaming)
    Events {
        /// Event filter pattern
        #[arg(long)]
        filter: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum DialogAction {
    /// List pending dialogs in current workspace
    List,
    /// Accept (confirm) a pending dialog
    Accept {
        /// Tab ID (required if multiple pending dialogs)
        #[arg(long)]
        tid: Option<String>,
        /// Text to enter for prompt dialogs
        #[arg(long)]
        text: Option<String>,
    },
    /// Dismiss (cancel) a pending dialog
    Dismiss {
        /// Tab ID (required if multiple pending dialogs)
        #[arg(long)]
        tid: Option<String>,
    },
    /// View or set the dialog handling policy for this workspace
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
    if let Command::Daemon { action: DaemonAction::Start } = &cli.command {
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
    if let Command::Daemon { action: _action @ (DaemonAction::Stop | DaemonAction::Status) } = &cli.command {
        match DaemonClient::connect_only().await {
            Ok(mut client) => {
                let result = dispatch(&cli, &mut client).await;
                if let Err(msg) = result {
                    eprintln!("error: {}", msg);
                    std::process::exit(1);
                }
                // After daemon.stop, wait for the daemon process to actually exit
                // by polling until the port is no longer reachable.
                if let Command::Daemon { action: DaemonAction::Stop } = &cli.command {
                    drop(client); // close our connection first
                    wait_for_daemon_exit().await;
                }
            }
            Err(_) => {
                // No daemon running — report cleanly and exit 0
                println!("{}", serde_json::json!({"ok": true, "data": {"status": "daemon not running"}}));
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

// ── Workspace resolution (v1 legacy) ──────────────────────────

/// Resolve workspace ID for legacy v1 commands.
/// Priority: BK_WS env → daemon default → single-ws auto-detect → error
async fn resolve_workspace(client: &mut DaemonClient) -> Result<String, String> {
    // 1. BK_WS environment variable
    if let Ok(wid) = std::env::var("BK_WS") {
        if !wid.is_empty() {
            return Ok(wid);
        }
    }

    // 2. Query daemon for default workspace
    let resp = send_cmd(client, "ws.default", json!({})).await?;
    if let Some(data) = &resp.data {
        if let Some(wid) = data.get("wid").and_then(|v| v.as_str()) {
            return Ok(wid.to_string());
        }
    }

    // 3. Auto-detect: if only one workspace exists, use it
    let resp = send_cmd(client, "ws.list", json!({})).await?;
    if let Some(data) = &resp.data {
        if let Some(arr) = data.as_array() {
            if arr.len() == 1 {
                if let Some(wid) = arr[0].get("wid").and_then(|v| v.as_str()) {
                    return Ok(wid.to_string());
                }
            }
        }
    }

    Err("no workspace specified. Set BK_WS env or run `bk ws use <wid>`".into())
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
    if let Some(u) = url { params["url"] = json!(u); }
    if back { params["back"] = json!(true); }
    if forward { params["forward"] = json!(true); }
    if reload { params["reload"] = json!(true); }
    if let Some(s) = &cli.session { params["session"] = json!(s); }
    if let Some(t) = &cli.target { params["target"] = json!(t); }
    if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
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
    if let Some(o) = output { params["output"] = json!(o); }
    if let Some(s) = selector { params["selector"] = json!(s); }
    if let Some(s) = &cli.session { params["session"] = json!(s); }
    if let Some(t) = &cli.target { params["target"] = json!(t); }
    params
}

fn add_session_target_params(params: &mut serde_json::Value, cli: &Cli) {
    if let Some(s) = &cli.session { params["session"] = json!(s); }
    if let Some(t) = &cli.target { params["target"] = json!(t); }
}

async fn dispatch(cli: &Cli, client: &mut DaemonClient) -> Result<(), String> {
    match &cli.command {
        // ══════════════════════════════════════════════════════════
        // V2 PRIMARY COMMANDS
        // ══════════════════════════════════════════════════════════

        Command::Connect => {
            let mut params = json!({});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            let resp = send_cmd(client, "connect", params).await?;
            print_response(&resp);
        }

        Command::Snapshot { full, no_page_text, wait } => {
            let mut params = json!({"full": full, "no_page_text": no_page_text, "wait": wait});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
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
            if let Some(k) = kind { params["kind"] = json!(k); }
            if let Some(r) = element_ref { params["ref"] = json!(r); }
            if !set.is_empty() {
                use browserkit::page::interaction::parse_fill_set_target;

                let mut fields = Vec::with_capacity(set.len());
                for item in set {
                    let field = parse_fill_set_target(item)?;
                    let ref_id = match field.target {
                        browserkit::page::element_ref::ElementTarget::Ref(ref_id) => ref_id,
                        browserkit::page::element_ref::ElementTarget::Index(_) => {
                            return Err(
                                "bk act fill only accepts ref targets in --set (use ref:<id>=<value>)"
                                    .into(),
                            )
                        }
                        browserkit::page::element_ref::ElementTarget::Selector(_) => {
                            return Err(
                                "bk act fill does not accept selector targets in --set".into(),
                            )
                        }
                    };
                    fields.push(json!({"ref": ref_id, "value": field.value}));
                }
                params["fields"] = json!(fields);
            }
            if let Some(t) = text { params["text"] = json!(t); }
            if let Some(v) = value { params["value"] = json!(v); }
            if *append { params["append"] = json!(true); }
            if !keys.is_empty() { params["keys"] = json!(keys); }
            if let Some(cx) = x { params["x"] = json!(cx); }
            if let Some(cy) = y { params["y"] = json!(cy); }
            if let Some(dir) = direction { params["direction"] = json!(dir); }
            if let Some(a) = amount { params["amount"] = json!(a); }
            if let Some(sel) = selector { params["selector"] = json!(sel); }
            if !files.is_empty() { params["files"] = json!(files); }
            if let Some(from_ref) = from_ref { params["from_ref"] = json!(from_ref); }
            if let Some(from_selector) = from_selector { params["from_selector"] = json!(from_selector); }
            if let Some(to_ref) = to_ref { params["to_ref"] = json!(to_ref); }
            if let Some(to_selector) = to_selector { params["to_selector"] = json!(to_selector); }
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
            if cli.no_state_diff { params["no_state_diff"] = json!(true); }
            let resp = send_cmd(client, "act", params).await?;
            print_response(&resp);
        }

        Command::Navigate { url, back, forward, reload } => {
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
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
            let resp = send_cmd(client, "open", params).await?;
            print_response(&resp);
        }

        Command::CloseV2 => {
            let mut params = json!({});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            let resp = send_cmd(client, "close", params).await?;
            print_response(&resp);
        }

        Command::Tabs => {
            let mut params = json!({});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            let resp = send_cmd(client, "tabs", params).await?;
            print_response(&resp);
        }

        Command::Attach { pattern } => {
            let mut params = json!({});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            if let Some(p) = pattern { params["pattern"] = json!(p); }
            let resp = send_cmd(client, "attach", params).await?;
            print_response(&resp);
        }

        Command::Evaluate { expression, file } => {
            let js_expr = if let Some(path) = file {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| format!("failed to read JS file: {}", e))?;
                content
            } else if let Some(e) = expression {
                e.clone()
            } else {
                return Err("evaluate requires either an expression or --file".into());
            };
            let mut params = json!({"expression": js_expr});
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
            let resp = send_cmd(client, "evaluate", params).await?;
            print_response(&resp);
        }

        Command::ScreenshotV2 { output, full_page, selector, labels } => {
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

        Command::WaitV2 { selector, text, text_gone, url, idle, r#fn, time } => {
            let mut params = json!({});
            if let Some(s) = selector { params["selector"] = json!(s); }
            if let Some(t) = text { params["text"] = json!(t); }
            if let Some(tg) = text_gone { params["text_gone"] = json!(tg); }
            if let Some(u) = url { params["url"] = json!(u); }
            if *idle { params["load_state"] = json!("networkidle"); }
            if let Some(f) = r#fn { params["fn"] = json!(f); }
            if let Some(t) = time { params["time"] = json!(t); }
            if let Some(s) = &cli.session { params["session"] = json!(s); }
            if let Some(t) = &cli.target { params["target"] = json!(t); }
            if let Some(to) = cli.timeout { params["timeout"] = json!(to); }
            if params.get("timeout").is_none() { params["timeout"] = json!(30000u64); }
            let resp = send_cmd(client, "wait", params).await?;
            print_response(&resp);
        }

        Command::Session { action } => match action {
            SessionAction::Close => {
                let mut params = json!({});
                if let Some(s) = &cli.session { params["session"] = json!(s); }
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
                    if let Some(s) = &cli.session { params["session"] = json!(s); }
                    let resp = send_cmd(client, "session.cookies.get", params).await?;
                    print_response(&resp);
                }
                CookiesAction::Set { file } => {
                    let content = std::fs::read_to_string(file)
                        .map_err(|e| format!("failed to read cookies file: {}", e))?;
                    let cookies: serde_json::Value = serde_json::from_str(&content)
                        .map_err(|e| format!("invalid cookie JSON: {}", e))?;
                    let mut params = json!({"cookies": cookies});
                    if let Some(s) = &cli.session { params["session"] = json!(s); }
                    let resp = send_cmd(client, "session.cookies.set", params).await?;
                    print_response(&resp);
                }
                CookiesAction::Clear => {
                    let mut params = json!({});
                    if let Some(s) = &cli.session { params["session"] = json!(s); }
                    let resp = send_cmd(client, "session.cookies.clear", params).await?;
                    print_response(&resp);
                }
            },
        },

        Command::StatusV2 => {
            dispatch_status(client).await?;
        }

        // ══════════════════════════════════════════════════════════
        // V1 LEGACY COMMANDS
        // ══════════════════════════════════════════════════════════

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
                let resp = send_cmd(client, "browser.connect", json!({"host": host})).await?;
                print_response(&resp);
            }
            BrowserAction::Discover { path } => {
                let mut params = json!({});
                if let Some(p) = path { params["path"] = json!(p); }
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

        // ── Workspace ──────────────────────────────────────
        Command::Ws { action } => match action {
            WsAction::New { host, label, no_headless, attached, pattern } => {
                let mut params = json!({});
                if let Some(h) = host { params["host"] = json!(h); }
                if let Some(l) = label { params["label"] = json!(l); }
                if *no_headless { params["headless"] = json!(false); }
                if *attached { params["attached"] = json!(true); }
                if let Some(p) = pattern { params["pattern"] = json!(p); }
                let resp = send_cmd(client, "ws.new", params).await?;
                print_response(&resp);
            }
            WsAction::Attach { pattern, host, label } => {
                let mut params = json!({});
                if let Some(p) = pattern { params["pattern"] = json!(p); }
                if let Some(h) = host { params["host"] = json!(h); }
                if let Some(l) = label { params["label"] = json!(l); }
                let resp = send_cmd(client, "ws.attach", params).await?;
                print_response(&resp);
            }
            WsAction::List => {
                let resp = send_cmd(client, "ws.list", json!({})).await?;
                print_response(&resp);
            }
            WsAction::Info { wid } => {
                let wid = match wid {
                    Some(w) => w.clone(),
                    None => resolve_workspace(client).await?,
                };
                let resp = send_cmd(client, "ws.info", json!({"wid": wid})).await?;
                print_response(&resp);
            }
            WsAction::Close { wid } => {
                let resp = send_cmd(client, "ws.close", json!({"wid": wid})).await?;
                print_response(&resp);
            }
            WsAction::Use { wid } => {
                let resp = send_cmd(client, "ws.use", json!({"wid": wid})).await?;
                print_response(&resp);
            }
            WsAction::Default => {
                let resp = send_cmd(client, "ws.default", json!({})).await?;
                print_response(&resp);
            }
        },

        // ── Tab ────────────────────────────────────────────
        Command::Tab { action } => {
            let wid = resolve_workspace(client).await?;
            match action {
                TabAction::New { url } => {
                    let mut params = json!({"wid": wid});
                    if let Some(u) = url { params["url"] = json!(u); }
                    let resp = send_cmd(client, "tab.new", params).await?;
                    print_response(&resp);
                }
                TabAction::Attach { pattern } => {
                    let resp = send_cmd(client, "tab.attach", json!({"wid": wid, "pattern": pattern})).await?;
                    print_response(&resp);
                }
                TabAction::List => {
                    let resp = send_cmd(client, "tab.list", json!({"wid": wid})).await?;
                    print_response(&resp);
                }
                TabAction::Switch { tid } => {
                    let resp = send_cmd(client, "tab.switch", json!({"wid": wid, "tid": tid})).await?;
                    print_response(&resp);
                }
                TabAction::Close { tid } => {
                    let resp = send_cmd(client, "tab.close", json!({"wid": wid, "tid": tid})).await?;
                    print_response(&resp);
                }
            }
        },

        // ── Page State (top-level, v1 legacy) ────────────────────────
        Command::Find { selector, attributes, max, include_text } => {
            let mut params = json!({"selector": selector, "include_text": include_text});
            if let Some(attrs) = attributes {
                let attr_list: Vec<&str> = attrs.split(',').map(|s| s.trim()).collect();
                params["attributes"] = json!(attr_list);
            }
            if let Some(m) = max { params["max"] = json!(m); }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "find", params).await?;
            print_response(&resp);
        }

        Command::Search { text, regex, scope, context, max } => {
            let mut params = json!({"text": text, "regex": regex});
            if let Some(s) = scope { params["scope"] = json!(s); }
            if let Some(c) = context { params["context"] = json!(c); }
            if let Some(m) = max { params["max"] = json!(m); }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "search", params).await?;
            print_response(&resp);
        }

        Command::Html { selector } => {
            let mut params = json!({});
            if let Some(s) = selector { params["selector"] = json!(s); }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "html", params).await?;
            print_response(&resp);
        }

        Command::Console { level, limit } => {
            let mut params = json!({"level": level});
            if let Some(n) = limit { params["limit"] = json!(n); }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "console", params).await?;
            print_response(&resp);
        }

        Command::Pdf { output } => {
            let mut params = json!({});
            if let Some(o) = output { params["output"] = json!(o); }
            add_session_target_params(&mut params, cli);
            let resp = send_cmd(client, "pdf", params).await?;
            handle_binary_response(&resp, output.as_deref(), "page.pdf");
        }


        Command::Fetch { url } => {
            let resp = send_cmd(client, "ws.new", json!({})).await?;
            if !resp.ok {
                print_response(&resp);
                return Ok(());
            }
            let wid = resp.data.as_ref()
                .and_then(|d| d.get("wid"))
                .and_then(|v| v.as_str())
                .ok_or("failed to get wid from ws.new response")?
                .to_string();
            let _ = send_cmd(client, "nav.goto", json!({"wid": wid, "url": url})).await?;
            let resp = send_cmd(client, "page.html", json!({"wid": wid})).await?;
            print_response(&resp);
            let _ = send_cmd(client, "ws.close", json!({"wid": wid})).await;
        }


        // ── Management (Storage) ──────────────────────────
        Command::Storage { action } => {
            let wid = resolve_workspace(client).await?;
            match action {
                StorageAction::Cookies { action: ca } => match ca {
                    CookieAction::Get => {
                        let resp = send_cmd(client, "storage.cookies.get", json!({"wid": wid})).await?;
                        print_response(&resp);
                    }
                    CookieAction::Set { json: j } => {
                        let cookies: serde_json::Value = serde_json::from_str(j)
                            .map_err(|e| format!("invalid cookie JSON: {}", e))?;
                        let resp = send_cmd(client, "storage.cookies.set", json!({"wid": wid, "cookies": cookies})).await?;
                        print_response(&resp);
                    }
                    CookieAction::Clear => {
                        let resp = send_cmd(client, "storage.cookies.clear", json!({"wid": wid})).await?;
                        print_response(&resp);
                    }
                },
                StorageAction::Local { action: la } => match la {
                    LocalAction::Get { key } => {
                        let resp = send_cmd(client, "storage.local.get", json!({"wid": wid, "key": key})).await?;
                        print_response(&resp);
                    }
                    LocalAction::Set { key, value } => {
                        let resp = send_cmd(client, "storage.local.set", json!({"wid": wid, "key": key, "value": value})).await?;
                        print_response(&resp);
                    }
                },
                StorageAction::Export => {
                    let resp = send_cmd(client, "storage.export", json!({"wid": wid})).await?;
                    print_response(&resp);
                }
                StorageAction::Import { file } => {
                    let content = std::fs::read_to_string(file)
                        .map_err(|e| format!("failed to read storage file: {}", e))?;
                    let state: serde_json::Value = serde_json::from_str(&content)
                        .map_err(|e| format!("invalid storage JSON: {}", e))?;
                    let resp = send_cmd(client, "storage.import", json!({"wid": wid, "state": state})).await?;
                    print_response(&resp);
                }
            }
        },

        // ── Management (Dialog) ───────────────────────────
        Command::Dialog { action } => {
            let wid = resolve_workspace(client).await?;
            match action {
                DialogAction::List => {
                    let resp = send_cmd(client, "dialog.list", json!({"wid": wid})).await?;
                    print_response(&resp);
                }
                DialogAction::Accept { tid, text } => {
                    let mut params = json!({"wid": wid});
                    if let Some(t) = tid { params["tid"] = json!(t); }
                    if let Some(txt) = text { params["text"] = json!(txt); }
                    let resp = send_cmd(client, "dialog.accept", params).await?;
                    print_response(&resp);
                }
                DialogAction::Dismiss { tid } => {
                    let mut params = json!({"wid": wid});
                    if let Some(t) = tid { params["tid"] = json!(t); }
                    let resp = send_cmd(client, "dialog.dismiss", params).await?;
                    print_response(&resp);
                }
                DialogAction::Policy { policy } => {
                    let mut params = json!({"wid": wid});
                    if let Some(p) = policy { params["policy"] = json!(p); }
                    let resp = send_cmd(client, "dialog.policy", params).await?;
                    print_response(&resp);
                }
            }
        },

        // ── Management (Debug) ────────────────────────────
        Command::Debug { action } => {
            let wid = resolve_workspace(client).await?;
            match action {
                DebugAction::Monitor => {
                    let resp = send_cmd(client, "network.monitor", json!({"wid": wid})).await?;
                    print_response(&resp);
                    run_streaming(client).await;
                }
                DebugAction::Har { url } => {
                    let resp = send_cmd(client, "network.har", json!({"wid": wid, "url": url})).await?;
                    print_response(&resp);
                    run_streaming(client).await;
                }
                DebugAction::Block { pattern } => {
                    let resp = send_cmd(client, "network.block", json!({"wid": wid, "pattern": pattern})).await?;
                    print_response(&resp);
                }
                DebugAction::Unblock { pattern } => {
                    let mut params = json!({"wid": wid});
                    if let Some(p) = pattern { params["pattern"] = json!(p); }
                    let resp = send_cmd(client, "network.unblock", params).await?;
                    print_response(&resp);
                }
                DebugAction::Cdp { method, params } => {
                    let cdp_params = match params {
                        Some(p) => serde_json::from_str(p)
                            .map_err(|e| format!("invalid CDP params JSON: {}", e))?,
                        None => json!({}),
                    };
                    let resp = send_cmd(client, "cdp.send", json!({"wid": wid, "method": method, "params": cdp_params})).await?;
                    print_response(&resp);
                }
                DebugAction::Events { filter } => {
                    let mut params = json!({"wid": wid});
                    if let Some(f) = filter { params["filter"] = json!(f); }
                    let resp = send_cmd(client, "cdp.events", params).await?;
                    print_response(&resp);
                    run_streaming(client).await;
                }
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

/// Read streaming responses and print each one as JSON.
async fn run_streaming(client: &mut DaemonClient) {
    let _ = client
        .read_streaming(|resp| {
            print_response(&resp);
            true
        })
        .await;
}

/// Handle binary (base64) responses: save to file or print info.
fn handle_binary_response(
    resp: &Response,
    output: Option<&str>,
    _default_name: &str,
) {
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
    if let (Some(path), Some(data)) = (output, resp.data.as_ref().and_then(|d| d.get("data")).and_then(|v| v.as_str())) {
        match base64_decode_and_save(data, path) {
            Ok(()) => println!("{}", serde_json::json!({"ok": true, "data": {"file": path}})),
            Err(e) => println!("{}", serde_json::json!({"ok": false, "error": format!("save failed: {}", e)})),
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

// ── Status command ─────────────────────────────────────────────

/// Implement `bk status` (v1 legacy): show daemon + browsers + workspaces overview as JSON.
async fn dispatch_status(client: &mut DaemonClient) -> Result<(), String> {
    let daemon_resp = send_cmd(client, "daemon.status", json!({})).await?;
    let browser_resp = send_cmd(client, "browser.list", json!({})).await?;
    let ws_resp = send_cmd(client, "ws.list", json!({})).await?;
    let default_resp = send_cmd(client, "ws.default", json!({})).await?;

    let result = json!({
        "ok": true,
        "data": {
            "daemon": daemon_resp.data,
            "browsers": browser_resp.data,
            "workspaces": ws_resp.data,
            "default_workspace": default_resp.data,
        }
    });
    println!("{}", serde_json::to_string(&result).unwrap_or_default());

    Ok(())
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
    fn cli_parses_snapshot() {
        let cli = try_parse(&["bk", "snapshot"]).unwrap();
        assert!(matches!(cli.command, Command::Snapshot { .. }));
    }

    #[test]
    fn cli_parses_snapshot_full() {
        let cli = try_parse(&["bk", "snapshot", "--full"]).unwrap();
        if let Command::Snapshot { full, .. } = cli.command {
            assert!(full);
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_act_click() {
        let cli = try_parse(&["bk", "act", "click", "--ref", "42"]).unwrap();
        if let Command::Act { kind, element_ref, .. } = &cli.command {
            assert_eq!(kind.as_deref(), Some("click"));
            assert_eq!(*element_ref, Some(42));
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_act_type() {
        let cli = try_parse(&["bk", "act", "type", "--ref", "55", "--text", "hello"]).unwrap();
        if let Command::Act { kind, element_ref, text, .. } = &cli.command {
            assert_eq!(kind.as_deref(), Some("type"));
            assert_eq!(*element_ref, Some(55));
            assert_eq!(text.as_deref(), Some("hello"));
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_act_press() {
        let cli = try_parse(&["bk", "act", "press", "--keys", "Enter"]).unwrap();
        if let Command::Act { kind, keys, .. } = &cli.command {
            assert_eq!(kind.as_deref(), Some("press"));
            assert_eq!(keys, &vec!["Enter".to_string()]);
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_act_scroll_hover_and_focus() {
        let scroll = try_parse(&[
            "bk", "act", "scroll", "--direction", "down", "--amount", "250",
        ]).unwrap();
        assert!(matches!(
            scroll.command,
            Command::Act { ref kind, ref direction, amount: Some(250.0), .. }
                if kind.as_deref() == Some("scroll") && direction.as_deref() == Some("down")
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
        let select = try_parse(&["bk", "act", "select", "--ref", "42", "--value", "green"]).unwrap();
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
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_navigate_back() {
        let cli = try_parse(&["bk", "navigate", "--back"]).unwrap();
        if let Command::Navigate { back, .. } = &cli.command {
            assert!(*back);
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_open() {
        let cli = try_parse(&["bk", "open", "https://x.com"]).unwrap();
        if let Command::OpenV2 { url } = &cli.command {
            assert_eq!(url, "https://x.com");
        } else { panic!("wrong variant"); }
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
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_screenshot() {
        let cli = try_parse(&["bk", "screenshot", "--full-page"]).unwrap();
        if let Command::ScreenshotV2 { full_page, .. } = &cli.command {
            assert!(*full_page);
        } else { panic!("wrong variant"); }
    }

    #[test]
    fn cli_parses_screenshot_selector_and_labels() {
        let cli = try_parse(&["bk", "screenshot", "--selector", "#app", "--labels"]).unwrap();
        if let Command::ScreenshotV2 { selector, labels, .. } = &cli.command {
            assert_eq!(selector.as_deref(), Some("#app"));
            assert!(*labels);
        } else { panic!("wrong variant"); }
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
        } else { panic!("wrong variant"); }
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
        assert!(try_parse(&["bk", "pdf", "--output", "page.pdf"]).is_ok());
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
