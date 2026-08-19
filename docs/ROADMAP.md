# browserkit Roadmap

## Current phase: lifecycle spine

- `BrowserRuntime`, `BrowserSession`, `Page`, and `Frame` are the public SDK identities.
- Runtime instances may attach to an existing CDP endpoint or launch a private browser profile. Only the latter owns process termination.
- Default and isolated BrowserContexts share one session API while retaining explicit cleanup ownership.
- Frame trees are bootstrapped from `Page.getFrameTree`, reduced from Page-scoped attach and connection-scoped detach events, and preserve IDs across out-of-process routing changes.
- Explicit close produces `CloseReport`; Drop performs no protocol I/O. The runtime retains ownership of SDK-created targets and isolated BrowserContexts so root close can clean resources whose child handles were dropped.
- Explicitly closing the default session is terminal for that runtime.
- Rust 1.88 checks and the repository contract validate the public surface.

## Event discipline

Every generated event subscription uses `Event::subscribe(&sender).await` with the sender matching the CDP delivery scope: target/page-scoped events use `&session`; browser/connection-scoped events use `&cdp`. Subscription registration is awaited before enabling its domain or triggering an action. Each subscriber has an independent unbounded queue; finite operations define their own bounds.

The explicit CDP escape hatches are `runtime.cdp()`, `page.cdp_session()`, and `frame.cdp_session()`.

## Later phases

Locators, interactions, expectations, waiting, network and download support, and dialog handling remain outside the current lifecycle SDK phase. The historical `bk` CLI and daemon keep their existing capabilities and are migrated separately.
