// CLI entry point: clap command parsing + daemon client wiring
//
// Workspace resolution priority:
//   1. --ws / -w flag (explicit)
//   2. BK_WS environment variable (scripts / MCP)
//   3. Daemon default workspace (ws.default)
//   4. Auto-detect when only one workspace exists
//   5. Error with helpful message

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::json;

use browserkit::client::{build_request, DaemonClient};
use browserkit::daemon;
use browserkit::daemon::protocol::Response;

// ── Output format ──────────────────────────────────────────────

#[derive(Clone, Debug, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Tsv,
}

// ── Top-level CLI ──────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "bk", about = "Browser automation CLI")]
pub struct Cli {
    /// Target workspace ID (or set BK_WS env var)
    #[arg(short = 'w', long = "ws", global = true, env = "BK_WS")]
    pub workspace: Option<String>,

    /// Output format
    #[arg(long, default_value = "text", global = true)]
    pub format: OutputFormat,

    #[command(subcommand)]
    pub command: Command,
}

// ── Command enum ───────────────────────────────────────────────

#[derive(Subcommand)]
pub enum Command {
    // ── Management ─────────────────────────────────────────
    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Browser management
    Browser {
        #[command(subcommand)]
        action: BrowserAction,
    },
    /// Workspace management
    Ws {
        #[command(subcommand)]
        action: WsAction,
    },
    /// Tab management (uses --ws for workspace)
    Tab {
        #[command(subcommand)]
        action: TabAction,
    },
    /// Navigation commands
    Nav {
        #[command(subcommand)]
        action: NavAction,
    },
    /// Page inspection
    Page {
        #[command(subcommand)]
        action: PageAction,
    },
    /// JavaScript execution
    Js {
        #[command(subcommand)]
        action: JsAction,
    },

    /// Storage management
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },
    /// Debug tools (network monitoring, raw CDP)
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    // ── Top-level shortcuts ────────────────────────────────

    /// Show daemon + browser + workspace overview
    Status,

    /// Navigate to URL
    Goto {
        /// Target URL
        url: String,
    },
    /// Click element by index or coordinates
    Click {
        /// Element index from page state
        #[arg(short, long)]
        index: Option<usize>,
        /// X coordinate
        #[arg(short, long)]
        x: Option<f64>,
        /// Y coordinate
        #[arg(short, long)]
        y: Option<f64>,
    },
    /// Type text into element
    Type {
        /// Element index
        #[arg(short, long)]
        index: usize,
        /// Text to type
        text: String,
    },
    /// Scroll page
    Scroll {
        /// Direction: up or down (default: down)
        direction: Option<String>,
    },
    /// Select dropdown option
    Select {
        /// Element index
        #[arg(short, long)]
        index: usize,
        /// Option value
        value: String,
    },
    /// Hover over element
    Hover {
        /// Element index
        #[arg(short, long)]
        index: usize,
    },
    /// Focus element
    Focus {
        /// Element index
        #[arg(short, long)]
        index: usize,
    },
    /// Execute JS expression (async-capable, maps to js.await)
    Eval {
        /// JavaScript expression
        expr: String,
    },
    /// Get page HTML
    Html {
        /// CSS selector for element HTML
        #[arg(short, long)]
        selector: Option<String>,
    },
    /// Reload current page
    Reload,
    /// Screenshot (supports one-shot with URL)
    Shot {
        /// URL for one-shot mode (auto-creates and closes workspace)
        url: Option<String>,
        /// Output file path
        #[arg(short, long)]
        output: Option<String>,
        /// Capture full scrollable page
        #[arg(long)]
        full_page: bool,
        /// CSS selector for element screenshot
        #[arg(short, long)]
        selector: Option<String>,
    },
    /// Generate PDF (supports one-shot with URL)
    Pdf {
        /// URL for one-shot mode
        url: Option<String>,
        /// Output file path
        #[arg(short, long)]
        output: Option<String>,
    },

    // ── One-shot commands ──────────────────────────────────

    /// Open URL in new workspace (keeps workspace alive)
    Open {
        /// URL to open
        url: String,
        /// Show browser window (override config headless = true)
        #[arg(long)]
        no_headless: bool,
    },
    /// Fetch HTML from URL (one-shot: auto-creates and closes workspace)
    Fetch {
        /// URL to fetch
        url: String,
    },

    // ── Aliases ────────────────────────────────────────────

    /// Create new workspace (alias for ws new)
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
    },
    /// List workspaces (alias for ws list)
    Ls,
    /// Close workspace (alias for ws close)
    Rm {
        /// Workspace ID
        wid: String,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
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
}

#[derive(Subcommand)]
pub enum TabAction {
    /// Create a new tab
    New {
        /// Initial URL (default: about:blank)
        url: Option<String>,
    },
    /// List tabs in workspace
    List,
    /// Switch active tab
    Switch {
        /// Tab ID
        tid: String,
    },
    /// Close a tab
    Close {
        /// Tab ID
        tid: String,
    },
}

#[derive(Subcommand)]
pub enum NavAction {
    /// Navigate to URL
    Goto {
        /// Target URL
        url: String,
    },
    /// Go back
    Back,
    /// Go forward
    Forward,
    /// Reload page
    Reload,
    /// Get current URL
    Url,
    /// Get page title
    Title,
    /// Wait for page load
    Wait,
}

#[derive(Subcommand)]
pub enum PageAction {
    /// Get interactive elements
    State {
        /// Include viewport screenshot
        #[arg(long)]
        screenshot: bool,
    },
    /// Search text in page
    Search {
        /// Text to search
        text: String,
    },
}

#[derive(Subcommand)]
pub enum JsAction {
    /// Execute JS synchronously (no await)
    Eval {
        /// JavaScript expression
        expr: String,
    },
    /// Execute JS from file
    File {
        /// Path to JS file
        path: String,
        /// Await promises in the file (default: true)
        #[arg(long, default_value = "true")]
        r#await: bool,
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

// ── Main ───────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
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

    // All other commands need a daemon connection
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("browserkit=info".parse().unwrap()),
        )
        .init();

    match daemon::start_daemon().await {
        Ok(_server) => {
            println!("daemon started on port {}", _server.port);
            // Wait for Ctrl+C to keep the daemon running in foreground
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen for Ctrl+C");
            println!("\nshutting down...");
            daemon::stop_daemon_cleanup();
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    }
}

// ── Workspace resolution ───────────────────────────────────────

/// Resolve workspace ID using the priority chain:
/// --ws flag → BK_WS env → daemon default → single-ws auto-detect → error
async fn resolve_workspace(
    cli_ws: &Option<String>,
    client: &mut DaemonClient,
) -> Result<String, String> {
    // 1. Explicit --ws flag (already includes BK_WS env via clap's `env` attr)
    if let Some(wid) = cli_ws {
        return Ok(wid.clone());
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

    Err("no workspace specified. Use --ws <wid>, set BK_WS env, or run `bk ws use <wid>`".into())
}

// ── Command dispatch ───────────────────────────────────────────

/// Macro to reduce boilerplate for commands that follow the pattern:
/// resolve workspace → send daemon command → print response.
macro_rules! ws_cmd {
    ($cli:expr, $client:expr, $fmt:expr, $cmd:expr, { $($key:expr => $val:expr),* $(,)? }) => {{
        let wid = resolve_workspace(&$cli.workspace, $client).await?;
        #[allow(unused_mut)]
        let mut params = json!({"wid": wid});
        $( params[$key] = json!($val); )*
        let resp = send_cmd($client, $cmd, params).await?;
        print_response(&resp, $fmt);
    }};
}

async fn dispatch(cli: &Cli, client: &mut DaemonClient) -> Result<(), String> {
    let fmt = &cli.format;

    match &cli.command {
        // ── Daemon ─────────────────────────────────────────
        Command::Daemon { action } => match action {
            DaemonAction::Start => unreachable!(),
            DaemonAction::Stop => {
                let resp = send_cmd(client, "daemon.stop", json!({})).await?;
                print_response(&resp, fmt);
            }
            DaemonAction::Status => {
                let resp = send_cmd(client, "daemon.status", json!({})).await?;
                print_response(&resp, fmt);
            }
        },

        // ── Browser ────────────────────────────────────────
        Command::Browser { action } => match action {
            BrowserAction::Connect { host } => {
                let resp = send_cmd(client, "browser.connect", json!({"host": host})).await?;
                print_response(&resp, fmt);
            }
            BrowserAction::List => {
                let resp = send_cmd(client, "browser.list", json!({})).await?;
                print_response(&resp, fmt);
            }
            BrowserAction::Disconnect { host } => {
                let resp = send_cmd(client, "browser.disconnect", json!({"host": host})).await?;
                print_response(&resp, fmt);
            }
        },

        // ── Workspace ──────────────────────────────────────
        Command::Ws { action } => match action {
            WsAction::New { host, label, no_headless } => {
                let mut params = json!({});
                if let Some(h) = host { params["host"] = json!(h); }
                if let Some(l) = label { params["label"] = json!(l); }
                if *no_headless { params["headless"] = json!(false); }
                let resp = send_cmd(client, "ws.new", params).await?;
                print_response(&resp, fmt);
            }
            WsAction::List => {
                let resp = send_cmd(client, "ws.list", json!({})).await?;
                print_response(&resp, fmt);
            }
            WsAction::Info { wid } => {
                let wid = match wid {
                    Some(w) => w.clone(),
                    None => resolve_workspace(&cli.workspace, client).await?,
                };
                let resp = send_cmd(client, "ws.info", json!({"wid": wid})).await?;
                print_response(&resp, fmt);
            }
            WsAction::Close { wid } => {
                let resp = send_cmd(client, "ws.close", json!({"wid": wid})).await?;
                print_response(&resp, fmt);
            }
            WsAction::Use { wid } => {
                let resp = send_cmd(client, "ws.use", json!({"wid": wid})).await?;
                print_response(&resp, fmt);
            }
        },

        // ── Tab ────────────────────────────────────────────
        Command::Tab { action } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            match action {
                TabAction::New { url } => {
                    let mut params = json!({"wid": wid});
                    if let Some(u) = url { params["url"] = json!(u); }
                    let resp = send_cmd(client, "tab.new", params).await?;
                    print_response(&resp, fmt);
                }
                TabAction::List => {
                    let resp = send_cmd(client, "tab.list", json!({"wid": wid})).await?;
                    print_response(&resp, fmt);
                }
                TabAction::Switch { tid } => {
                    let resp = send_cmd(client, "tab.switch", json!({"wid": wid, "tid": tid})).await?;
                    print_response(&resp, fmt);
                }
                TabAction::Close { tid } => {
                    let resp = send_cmd(client, "tab.close", json!({"wid": wid, "tid": tid})).await?;
                    print_response(&resp, fmt);
                }
            }
        },

        // ── Nav ────────────────────────────────────────────
        Command::Nav { action } => {
            match action {
                NavAction::Goto { url } => {
                    ws_cmd!(cli, client, fmt, "nav.goto", { "url" => url });
                }
                NavAction::Back => { ws_cmd!(cli, client, fmt, "nav.back", {}); }
                NavAction::Forward => { ws_cmd!(cli, client, fmt, "nav.forward", {}); }
                NavAction::Reload => { ws_cmd!(cli, client, fmt, "nav.reload", {}); }
                NavAction::Url => { ws_cmd!(cli, client, fmt, "nav.url", {}); }
                NavAction::Title => { ws_cmd!(cli, client, fmt, "nav.title", {}); }
                NavAction::Wait => { ws_cmd!(cli, client, fmt, "nav.wait", {}); }
            }
        },

        // ── Page ───────────────────────────────────────────
        Command::Page { action } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            match action {
                PageAction::State { screenshot } => {
                    let resp = send_cmd(client, "page.state", json!({"wid": wid, "screenshot": screenshot})).await?;
                    print_response(&resp, fmt);
                }
                PageAction::Search { text } => {
                    let resp = send_cmd(client, "page.search", json!({"wid": wid, "text": text})).await?;
                    print_response(&resp, fmt);
                }
            }
        },

        // ── JS ─────────────────────────────────────────────
        Command::Js { action } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            match action {
                JsAction::Eval { expr } => {
                    let resp = send_cmd(client, "js.eval", json!({"wid": wid, "expr": expr, "await": false})).await?;
                    print_response(&resp, fmt);
                }
                JsAction::File { path, r#await } => {
                    let content = std::fs::read_to_string(path)
                        .map_err(|e| format!("failed to read JS file: {}", e))?;
                    // Guard against sending excessively large files over TCP
                    const MAX_JS_FILE_SIZE: usize = 5 * 1024 * 1024; // 5 MB
                    if content.len() > MAX_JS_FILE_SIZE {
                        return Err(format!(
                            "JS file too large ({} bytes, max {} bytes)",
                            content.len(),
                            MAX_JS_FILE_SIZE
                        ));
                    }
                    let resp = send_cmd(client, "js.eval", json!({"wid": wid, "expr": content, "await": r#await})).await?;
                    print_response(&resp, fmt);
                }
            }
        },

        // ── Storage ────────────────────────────────────────
        Command::Storage { action } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            match action {
                StorageAction::Cookies { action: ca } => match ca {
                    CookieAction::Get => {
                        let resp = send_cmd(client, "storage.cookies.get", json!({"wid": wid})).await?;
                        print_response(&resp, fmt);
                    }
                    CookieAction::Set { json: j } => {
                        let cookies: serde_json::Value = serde_json::from_str(j)
                            .map_err(|e| format!("invalid cookie JSON: {}", e))?;
                        let resp = send_cmd(client, "storage.cookies.set", json!({"wid": wid, "cookies": cookies})).await?;
                        print_response(&resp, fmt);
                    }
                    CookieAction::Clear => {
                        let resp = send_cmd(client, "storage.cookies.clear", json!({"wid": wid})).await?;
                        print_response(&resp, fmt);
                    }
                },
                StorageAction::Local { action: la } => match la {
                    LocalAction::Get { key } => {
                        let resp = send_cmd(client, "storage.local.get", json!({"wid": wid, "key": key})).await?;
                        print_response(&resp, fmt);
                    }
                    LocalAction::Set { key, value } => {
                        let resp = send_cmd(client, "storage.local.set", json!({"wid": wid, "key": key, "value": value})).await?;
                        print_response(&resp, fmt);
                    }
                },
                StorageAction::Export => {
                    let resp = send_cmd(client, "storage.export", json!({"wid": wid})).await?;
                    print_response(&resp, fmt);
                }
                StorageAction::Import { file } => {
                    let content = std::fs::read_to_string(file)
                        .map_err(|e| format!("failed to read storage file: {}", e))?;
                    let state: serde_json::Value = serde_json::from_str(&content)
                        .map_err(|e| format!("invalid storage JSON: {}", e))?;
                    let resp = send_cmd(client, "storage.import", json!({"wid": wid, "state": state})).await?;
                    print_response(&resp, fmt);
                }
            }
        },

        // ── Debug (network + CDP) ──────────────────────────
        Command::Debug { action } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            match action {
                DebugAction::Monitor => {
                    let resp = send_cmd(client, "network.monitor", json!({"wid": wid})).await?;
                    print_response(&resp, fmt);
                    run_streaming(client, fmt).await;
                }
                DebugAction::Har { url } => {
                    let resp = send_cmd(client, "network.har", json!({"wid": wid, "url": url})).await?;
                    print_response(&resp, fmt);
                    run_streaming(client, fmt).await;
                }
                DebugAction::Block { pattern } => {
                    let resp = send_cmd(client, "network.block", json!({"wid": wid, "pattern": pattern})).await?;
                    print_response(&resp, fmt);
                }
                DebugAction::Unblock { pattern } => {
                    let mut params = json!({"wid": wid});
                    if let Some(p) = pattern { params["pattern"] = json!(p); }
                    let resp = send_cmd(client, "network.unblock", params).await?;
                    print_response(&resp, fmt);
                }
                DebugAction::Cdp { method, params } => {
                    let cdp_params = match params {
                        Some(p) => serde_json::from_str(p)
                            .map_err(|e| format!("invalid CDP params JSON: {}", e))?,
                        None => json!({}),
                    };
                    let resp = send_cmd(client, "cdp.send", json!({"wid": wid, "method": method, "params": cdp_params})).await?;
                    print_response(&resp, fmt);
                }
                DebugAction::Events { filter } => {
                    let mut params = json!({"wid": wid});
                    if let Some(f) = filter { params["filter"] = json!(f); }
                    let resp = send_cmd(client, "cdp.events", params).await?;
                    print_response(&resp, fmt);
                    run_streaming(client, fmt).await;
                }
            }
        },

        // ── Top-level shortcuts ────────────────────────────

        Command::Status => {
            dispatch_status(client, fmt).await?;
        }

        Command::Goto { url } => {
            ws_cmd!(cli, client, fmt, "nav.goto", { "url" => url });
        }

        Command::Click { index, x, y } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            let mut params = json!({"wid": wid});
            if let Some(i) = index { params["index"] = json!(i); }
            if let Some(cx) = x { params["x"] = json!(cx); }
            if let Some(cy) = y { params["y"] = json!(cy); }
            let resp = send_cmd(client, "act.click", params).await?;
            print_response(&resp, fmt);
        }

        Command::Type { index, text } => {
            ws_cmd!(cli, client, fmt, "act.type", { "index" => index, "text" => text });
        }

        Command::Scroll { direction } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            let dir = direction.as_deref().unwrap_or("down");
            let resp = send_cmd(client, "act.scroll", json!({"wid": wid, "direction": dir})).await?;
            print_response(&resp, fmt);
        }

        Command::Select { index, value } => {
            ws_cmd!(cli, client, fmt, "act.select", { "index" => index, "value" => value });
        }

        Command::Hover { index } => {
            ws_cmd!(cli, client, fmt, "act.hover", { "index" => index });
        }

        Command::Focus { index } => {
            ws_cmd!(cli, client, fmt, "act.focus", { "index" => index });
        }

        Command::Eval { expr } => {
            ws_cmd!(cli, client, fmt, "js.eval", { "expr" => expr, "await" => true });
        }

        Command::Html { selector } => {
            let wid = resolve_workspace(&cli.workspace, client).await?;
            let mut params = json!({"wid": wid});
            if let Some(s) = selector { params["selector"] = json!(s); }
            let resp = send_cmd(client, "page.html", params).await?;
            print_response(&resp, fmt);
        }

        Command::Reload => {
            ws_cmd!(cli, client, fmt, "nav.reload", {});
        }

        Command::Shot { url, output, full_page, selector } => {
            if let Some(target_url) = url {
                // One-shot mode: create ws → goto → screenshot → close ws
                dispatch_oneshot_shot(client, fmt, target_url, output, full_page, selector).await?;
            } else {
                let wid = resolve_workspace(&cli.workspace, client).await?;
                let mut params = json!({"wid": wid, "full_page": full_page});
                if let Some(s) = selector { params["selector"] = json!(s); }
                if let Some(o) = output { params["output"] = json!(o); }
                let resp = send_cmd(client, "page.screenshot", params).await?;
                handle_binary_response(&resp, fmt, output.as_deref(), "screenshot.png");
            }
        }

        Command::Pdf { url, output } => {
            if let Some(target_url) = url {
                dispatch_oneshot_pdf(client, fmt, target_url, output).await?;
            } else {
                let wid = resolve_workspace(&cli.workspace, client).await?;
                let mut params = json!({"wid": wid});
                if let Some(o) = output { params["output"] = json!(o); }
                let resp = send_cmd(client, "page.pdf", params).await?;
                handle_binary_response(&resp, fmt, output.as_deref(), "page.pdf");
            }
        }

        // ── One-shot commands ──────────────────────────────

        Command::Open { url, no_headless } => {
            // Create workspace + navigate, keep workspace alive
            let mut ws_params = json!({});
            if *no_headless { ws_params["headless"] = json!(false); }
            let resp = send_cmd(client, "ws.new", ws_params).await?;
            if !resp.ok {
                print_response(&resp, fmt);
                return Ok(());
            }
            let wid = resp.data.as_ref()
                .and_then(|d| d.get("wid"))
                .and_then(|v| v.as_str())
                .ok_or("failed to get wid from ws.new response")?
                .to_string();
            // Explicitly set as default (open is a human-terminal convenience command)
            let use_resp = send_cmd(client, "ws.use", json!({"wid": wid})).await?;
            if !use_resp.ok {
                eprintln!("warning: failed to set default workspace: {}", use_resp.error.unwrap_or_default());
            }
            let resp = send_cmd(client, "nav.goto", json!({"wid": wid, "url": url})).await?;
            print_response(&resp, fmt);
        }

        Command::Fetch { url } => {
            // One-shot: create ws → goto → html → close ws
            let resp = send_cmd(client, "ws.new", json!({})).await?;
            if !resp.ok {
                print_response(&resp, fmt);
                return Ok(());
            }
            let wid = resp.data.as_ref()
                .and_then(|d| d.get("wid"))
                .and_then(|v| v.as_str())
                .ok_or("failed to get wid from ws.new response")?
                .to_string();
            let _ = send_cmd(client, "nav.goto", json!({"wid": wid, "url": url})).await?;
            let resp = send_cmd(client, "page.html", json!({"wid": wid})).await?;
            print_response(&resp, fmt);
            let _ = send_cmd(client, "ws.close", json!({"wid": wid})).await;
        }

        // ── Aliases ────────────────────────────────────────

        Command::New { host, label, no_headless } => {
            let mut params = json!({});
            if let Some(h) = host { params["host"] = json!(h); }
            if let Some(l) = label { params["label"] = json!(l); }
            if *no_headless { params["headless"] = json!(false); }
            let resp = send_cmd(client, "ws.new", params).await?;
            print_response(&resp, fmt);
        }

        Command::Ls => {
            let resp = send_cmd(client, "ws.list", json!({})).await?;
            print_response(&resp, fmt);
        }

        Command::Rm { wid } => {
            let resp = send_cmd(client, "ws.close", json!({"wid": wid})).await?;
            print_response(&resp, fmt);
        }

        Command::Completions { .. } => unreachable!(),
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
fn print_response(resp: &Response, fmt: &OutputFormat) {
    match fmt {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
        }
        OutputFormat::Tsv => {
            if resp.ok {
                if let Some(data) = &resp.data {
                    print_tsv_output(data);
                }
            } else if let Some(err) = &resp.error {
                eprintln!("error\t{}", err);
            }
        }
        OutputFormat::Text => {
            if resp.ok {
                if let Some(data) = &resp.data {
                    print_text_output(data);
                }
            } else if let Some(err) = &resp.error {
                eprintln!("error: {}", err);
            }
        }
    }
}

/// Format a JSON value as human-readable text output.
fn print_text_output(data: &serde_json::Value) {
    match data {
        serde_json::Value::String(s) => println!("{}", s),
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Some(obj) = item.as_object() {
                    let parts: Vec<String> = obj
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, format_value(v)))
                        .collect();
                    println!("{}", parts.join("  "));
                } else {
                    println!("{}", format_value(item));
                }
            }
        }
        serde_json::Value::Object(obj) => {
            // Skip printing raw base64 data fields in text mode
            for (k, v) in obj {
                if k == "data" && v.as_str().is_some_and(|s| s.len() > 200) {
                    continue; // skip large base64 blobs
                }
                println!("{}={}", k, format_value(v));
            }
        }
        _ => println!("{}", data),
    }
}

/// Format a single JSON value for text display.
fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Format JSON data as TSV (tab-separated values) for pipe-friendly output.
///
/// Arrays of objects: first line is header (keys), subsequent lines are values.
/// Single objects: one key\tvalue per line.
/// Scalars: printed as-is.
fn print_tsv_output(data: &serde_json::Value) {
    match data {
        serde_json::Value::Array(arr) if !arr.is_empty() => {
            // Collect all keys from the first object for the header
            if let Some(first) = arr[0].as_object() {
                let keys: Vec<&String> = first.keys().collect();
                println!("{}", keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join("\t"));
                for item in arr {
                    if let Some(obj) = item.as_object() {
                        let vals: Vec<String> = keys
                            .iter()
                            .map(|k| format_value(obj.get(*k).unwrap_or(&serde_json::Value::Null)))
                            .collect();
                        println!("{}", vals.join("\t"));
                    }
                }
            } else {
                // Array of non-objects
                for item in arr {
                    println!("{}", format_value(item));
                }
            }
        }
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                if k == "data" && v.as_str().is_some_and(|s| s.len() > 200) {
                    continue;
                }
                println!("{}\t{}", k, format_value(v));
            }
        }
        _ => println!("{}", format_value(data)),
    }
}

/// Read streaming responses and print each one.
async fn run_streaming(client: &mut DaemonClient, fmt: &OutputFormat) {
    let fmt = fmt.clone();
    let _ = client
        .read_streaming(|resp| {
            print_response(&resp, &fmt);
            true
        })
        .await;
}

/// Handle binary (base64) responses: save to file or print info.
fn handle_binary_response(
    resp: &Response,
    fmt: &OutputFormat,
    output: Option<&str>,
    _default_name: &str,
) {
    if !resp.ok {
        print_response(resp, fmt);
        return;
    }

    // If already saved by daemon (output param was in the request)
    if let Some(data) = &resp.data {
        if data.get("file").is_some() {
            print_response(resp, fmt);
            return;
        }
    }

    // Save base64 data to file if output specified but wasn't in request
    if let (Some(path), Some(data)) = (output, resp.data.as_ref().and_then(|d| d.get("data")).and_then(|v| v.as_str())) {
        match base64_decode_and_save(data, path) {
            Ok(()) => println!("saved to {}", path),
            Err(e) => eprintln!("error saving file: {}", e),
        }
    } else {
        print_response(resp, fmt);
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

/// Implement `bk status`: show daemon + browsers + workspaces overview.
async fn dispatch_status(client: &mut DaemonClient, fmt: &OutputFormat) -> Result<(), String> {
    let daemon_resp = send_cmd(client, "daemon.status", json!({})).await?;
    let browser_resp = send_cmd(client, "browser.list", json!({})).await?;
    let ws_resp = send_cmd(client, "ws.list", json!({})).await?;
    let default_resp = send_cmd(client, "ws.default", json!({})).await?;

    match fmt {
        OutputFormat::Json => {
            let result = json!({
                "daemon": daemon_resp.data,
                "browsers": browser_resp.data,
                "workspaces": ws_resp.data,
                "default_workspace": default_resp.data,
            });
            println!("{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        }
        OutputFormat::Tsv => {
            // TSV: print each section as tab-separated data
            if let Some(d) = &daemon_resp.data {
                print_tsv_output(d);
            }
            if let Some(d) = &browser_resp.data {
                print_tsv_output(d);
            }
            if let Some(d) = &ws_resp.data {
                print_tsv_output(d);
            }
        }
        OutputFormat::Text => {
            // Daemon info
            if let Some(d) = &daemon_resp.data {
                println!(
                    "daemon    running (port {}, pid {}, uptime {}s, requests {})",
                    d.get("port").and_then(|v| v.as_u64()).unwrap_or(0),
                    d.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                    d.get("uptime_seconds").and_then(|v| v.as_u64()).unwrap_or(0),
                    d.get("request_count").and_then(|v| v.as_u64()).unwrap_or(0),
                );
            }

            // Browsers
            if let Some(arr) = browser_resp.data.as_ref().and_then(|d| d.as_array()) {
                println!("browsers  {}", arr.len());
                for b in arr {
                    let host = b.get("host").and_then(|v| v.as_str()).unwrap_or("?");
                    let managed = b.get("managed").and_then(|v| v.as_bool()).unwrap_or(false);
                    let tag = if managed { "managed" } else { "unmanaged" };
                    println!("  {}  ({})", host, tag);
                }
            }

            // Workspaces
            let default_wid = default_resp
                .data
                .as_ref()
                .and_then(|d| d.get("wid"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if let Some(arr) = ws_resp.data.as_ref().and_then(|d| d.as_array()) {
                println!("workspaces  {}", arr.len());
                for ws in arr {
                    let wid = ws.get("wid").and_then(|v| v.as_str()).unwrap_or("?");
                    let label = ws.get("label").and_then(|v| v.as_str()).unwrap_or("");
                    let tabs = ws.get("tabs").and_then(|v| v.as_u64()).unwrap_or(0);
                    let marker = if wid == default_wid { "*" } else { " " };
                    if label.is_empty() {
                        println!("{} {}  ({} tabs)", marker, wid, tabs);
                    } else {
                        println!("{} {}  {}  ({} tabs)", marker, wid, label, tabs);
                    }
                }
            }
        }
    }

    Ok(())
}

// ── One-shot helpers ───────────────────────────────────────────

/// One-shot screenshot: create ws → goto → screenshot → close ws.
async fn dispatch_oneshot_shot(
    client: &mut DaemonClient,
    fmt: &OutputFormat,
    url: &str,
    output: &Option<String>,
    full_page: &bool,
    selector: &Option<String>,
) -> Result<(), String> {
    let resp = send_cmd(client, "ws.new", json!({})).await?;
    if !resp.ok {
        print_response(&resp, fmt);
        return Ok(());
    }
    let wid = resp.data.as_ref()
        .and_then(|d| d.get("wid"))
        .and_then(|v| v.as_str())
        .ok_or("failed to get wid")?
        .to_string();

    let _ = send_cmd(client, "nav.goto", json!({"wid": wid, "url": url})).await?;

    let mut params = json!({"wid": wid, "full_page": full_page});
    if let Some(s) = selector { params["selector"] = json!(s); }
    if let Some(o) = output { params["output"] = json!(o); }
    let resp = send_cmd(client, "page.screenshot", params).await?;
    handle_binary_response(&resp, fmt, output.as_deref(), "screenshot.png");

    let _ = send_cmd(client, "ws.close", json!({"wid": wid})).await;
    Ok(())
}

/// One-shot PDF: create ws → goto → pdf → close ws.
async fn dispatch_oneshot_pdf(
    client: &mut DaemonClient,
    fmt: &OutputFormat,
    url: &str,
    output: &Option<String>,
) -> Result<(), String> {
    let resp = send_cmd(client, "ws.new", json!({})).await?;
    if !resp.ok {
        print_response(&resp, fmt);
        return Ok(());
    }
    let wid = resp.data.as_ref()
        .and_then(|d| d.get("wid"))
        .and_then(|v| v.as_str())
        .ok_or("failed to get wid")?
        .to_string();

    let _ = send_cmd(client, "nav.goto", json!({"wid": wid, "url": url})).await?;

    let mut params = json!({"wid": wid});
    if let Some(o) = output { params["output"] = json!(o); }
    let resp = send_cmd(client, "page.pdf", params).await?;
    handle_binary_response(&resp, fmt, output.as_deref(), "page.pdf");

    let _ = send_cmd(client, "ws.close", json!({"wid": wid})).await;
    Ok(())
}
