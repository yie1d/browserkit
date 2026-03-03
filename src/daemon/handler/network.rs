// Network handlers: monitor, har, block, unblock

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tracing::info;

use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::BkError;
use super::common::{handler, now_ts, resolve_context, touch_workspace};

handler!(handle_network_monitor, do_network_monitor(req, state));

async fn do_network_monitor(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "network.monitor")?;
    ctx.cdp.send(cdpkit::network::methods::Enable::new(), Some(&ctx.cdp_session_id)).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "network monitoring enabled");
    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "status": "monitoring",
        "message": "Network.enable activated. Events are being collected on the CDP session."
    })))
}

handler!(handle_network_har, do_network_har(req, state));

/// NOTE: This is a stub implementation. HAR entry collection (subscribing to
/// Network events and recording request/response pairs) is not yet implemented.
/// The response always contains an empty `entries` array.
async fn do_network_har(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "network.har")?;
    let url = req.params.get("url").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("network.har requires 'url' param".into()))?;

    ctx.cdp.send(cdpkit::network::methods::Enable::new(), Some(&ctx.cdp_session_id)).await?;
    let nav_url = crate::page::navigation::goto(&ctx.cdp, &ctx.cdp_session_id, url).await?;

    if let Some(mut ws) = state.workspaces.get_mut(&ctx.wid) {
        if let Some(tab) = ws.tabs.get_mut(&ctx.tid) {
            tab.url = nav_url.clone();
        }
        ws.last_active = now_ts();
    }

    info!(wid = %ctx.wid, url = %url, "HAR navigation completed (stub: entry collection not implemented)");

    Ok(Response::ok(json!({
        "wid": ctx.wid,
        "url": nav_url,
        "status": "recorded",
        "note": "HAR entry collection not yet implemented. entries is always empty.",
        "log": {
            "version": "1.2",
            "creator": { "name": "browserkit", "version": "0.1.0" },
            "pages": [{ "startedDateTime": chrono_now_iso(), "id": "page_1", "title": url }],
            "entries": []
        }
    })))
}

fn chrono_now_iso() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    unix_to_iso8601(now.as_secs(), now.subsec_millis())
}

fn unix_to_iso8601(secs: u64, millis: u32) -> String {
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;

    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, m, d, hours, minutes, seconds, millis)
}

handler!(handle_network_block, do_network_block(req, state));

async fn do_network_block(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "network.block")?;
    let pattern = req.params.get("pattern").and_then(|v| v.as_str())
        .ok_or_else(|| BkError::InvalidRequest("network.block requires 'pattern' param".into()))?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(vec![pattern.to_string()]);
    ctx.cdp.send(cmd, Some(&ctx.cdp_session_id)).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, pattern = %pattern, "network requests blocked");
    Ok(Response::ok(json!({ "wid": ctx.wid, "pattern": pattern, "status": "blocked" })))
}

handler!(handle_network_unblock, do_network_unblock(req, state));

async fn do_network_unblock(req: &Request, state: &Arc<DaemonState>) -> Result<Response, BkError> {
    let ctx = resolve_context(req, state, "network.unblock")?;
    #[allow(deprecated)]
    let cmd = cdpkit::network::methods::SetBlockedUrLs::new().with_urls(Vec::<String>::new());
    ctx.cdp.send(cmd, Some(&ctx.cdp_session_id)).await?;
    touch_workspace(state, &ctx.wid);
    info!(wid = %ctx.wid, "network request blocking removed");
    Ok(Response::ok(json!({ "wid": ctx.wid, "status": "unblocked" })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_is_1970_01_01() { assert_eq!(unix_to_iso8601(0, 0), "1970-01-01T00:00:00.000Z"); }
    #[test]
    fn unix_epoch_with_millis() { assert_eq!(unix_to_iso8601(0, 123), "1970-01-01T00:00:00.123Z"); }
    #[test]
    fn known_date_2024_01_01() { assert_eq!(unix_to_iso8601(1704067200, 0), "2024-01-01T00:00:00.000Z"); }
    #[test]
    fn leap_year_feb_29() { assert_eq!(unix_to_iso8601(1709208000, 0), "2024-02-29T12:00:00.000Z"); }
    #[test]
    fn year_end_dec_31() { assert_eq!(unix_to_iso8601(1704067199, 999), "2023-12-31T23:59:59.999Z"); }
    #[test]
    fn y2k_date() { assert_eq!(unix_to_iso8601(946684800, 0), "2000-01-01T00:00:00.000Z"); }
    #[test]
    fn far_future_2100() { assert_eq!(unix_to_iso8601(4102444800, 0), "2100-01-01T00:00:00.000Z"); }
    #[test]
    fn chrono_now_iso_format_is_valid() {
        let iso = chrono_now_iso();
        assert_eq!(iso.len(), 24);
        assert!(iso.ends_with('Z'));
    }
}
