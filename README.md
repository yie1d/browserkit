# browserkit

browserkit is a client-neutral Rust runtime for Chrome DevTools Protocol (CDP) connections, built on [cdpkit](https://crates.io/crates/cdpkit). Its public lifecycle is `BrowserRuntime -> BrowserSession -> Page -> Frame`, with typed APIs for browser work on top of that ownership spine.

## Requirements

- Rust 1.88+
- A Chromium browser with a CDP endpoint, or a locally installed Chromium executable that browserkit can discover or that you supply to `LaunchOptions`

## Lifecycle SDK

`BrowserRuntime::connect` attaches to an existing browser and never owns its process. `BrowserRuntime::launch` starts a browser that the runtime owns. Launch uses a temporary private profile by default; `LaunchOptions::user_data_dir` selects an explicit profile directory. Both modes return the same runtime API.

`runtime.cdp()`, `page.cdp_session()`, and `frame.cdp_session()` are explicit protocol escape hatches. They retain the CDP connection and Session scope selected by the lifecycle SDK.

An attached page is detached by explicit close; a page created by the SDK is closed. An isolated session owns its BrowserContext, while the default session does not. Every explicit `close()` returns a `CloseReport`; inspect it instead of assuming cleanup completed. Close is cancellation-safe: once started, one cleanup task continues and later calls await the same report. Dropping a child handle performs no protocol I/O, but the root runtime retains remote ownership and cleans created targets and isolated BrowserContexts during `BrowserRuntime::close()`. Explicitly closing the default session is terminal for that runtime; later `default_session()` calls return an error.

See [runtime_connect.rs](examples/runtime_connect.rs) and [runtime_launch.rs](examples/runtime_launch.rs) for canonical, compile-checked programs.

## Scope

The Runtime SDK includes lifecycle and ownership, recursive Frame/OOPIF routing and document epochs; context configuration and capability preflight; locators, interactions, navigation, expectations and waits; evaluation, dialogs and file choosers; network observation and bodies; downloads, storage and authentication state; screenshots, PDF/HTML and snapshots; diagnostics and typed event streams. These APIs remain client-neutral: browserkit does not provide workflow orchestration policy. The historical `bk` CLI and daemon remain a separate entry point with their existing command surface.
