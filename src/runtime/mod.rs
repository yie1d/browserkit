mod action;
mod artifact;
mod browser;
mod capability;
mod context_options;
mod diagnostic;
mod dialog;
mod download;
mod error;
mod evaluate;
mod event;
mod file_chooser;
mod frame;
mod geometry;
mod identity;
mod launch;
mod lifecycle;
mod locator;
mod navigation;
mod network;
mod page;
mod popup;
mod route;
mod session;
mod snapshot;
mod storage;
mod target_close;
mod wait;

pub use artifact::*;
pub use browser::*;
pub use capability::*;
pub use context_options::*;
pub use diagnostic::*;
pub use dialog::*;
pub use download::*;
pub use error::*;
pub use evaluate::*;
pub use event::*;
pub use file_chooser::*;
pub use frame::*;
pub use identity::*;
pub use launch::*;
pub use lifecycle::*;
pub use locator::*;
pub use navigation::*;
pub use network::*;
pub use page::*;
pub use session::*;
pub use snapshot::*;
pub use storage::*;
pub use wait::*;

#[cfg(test)]
pub(crate) fn test_browser_version_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "1.3",
        "product": "Chrome/123.0.6312.86",
        "revision": "@revision",
        "userAgent": "BrowserKit Test",
        "jsVersion": "12.3"
    })
}
