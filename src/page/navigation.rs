// Navigation: goto, reload, back, forward, url, title, wait

use std::sync::Arc;
use std::time::Duration;

use cdpkit::CDP;

use crate::error::BkError;

/// Default timeout for page load operations (goto, reload, back, forward, nav.wait).
pub const PAGE_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Navigate to a URL using CDP `Page.navigate`.
///
/// Returns the new URL after navigation. Checks `errorText` in the response
/// to detect navigation failures. Retries once on transient connection errors
/// (e.g. `net::ERR_CONNECTION_CLOSED`) which can occur when Chrome reuses a
/// stale connection from its pool.
pub async fn goto(cdp: &Arc<CDP>, session_id: &str, url: &str) -> Result<String, BkError> {
    let mut last_err = None;

    for attempt in 0..2 {
        if attempt > 0 {
            // Brief pause before retry to let Chrome recover
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let resp = cdp
            .send(
                cdpkit::page::methods::Navigate::new(url),
                Some(session_id),
            )
            .await?;

        if let Some(error_text) = &resp.error_text {
            // Retry on transient connection errors
            if error_text.contains("ERR_CONNECTION_CLOSED")
                || error_text.contains("ERR_CONNECTION_RESET")
                || error_text.contains("ERR_EMPTY_RESPONSE")
            {
                last_err = Some(BkError::NavigationFailed(error_text.clone()));
                continue;
            }
            return Err(BkError::NavigationFailed(error_text.clone()));
        }

        // Wait for the load lifecycle event after navigation
        wait_for_load(cdp, session_id, PAGE_LOAD_TIMEOUT).await?;
        return Ok(url.to_string());
    }

    Err(last_err.unwrap_or_else(|| BkError::NavigationFailed("navigation failed".into())))
}

/// Reload the current page using CDP `Page.reload`.
pub async fn reload(cdp: &Arc<CDP>, session_id: &str) -> Result<(), BkError> {
    cdp.send(
        cdpkit::page::methods::Reload::new(),
        Some(session_id),
    )
    .await?;

    wait_for_load(cdp, session_id, PAGE_LOAD_TIMEOUT).await?;

    Ok(())
}

/// Navigate back in history using `Page.getNavigationHistory` + `Page.navigateToHistoryEntry`.
pub async fn back(cdp: &Arc<CDP>, session_id: &str) -> Result<(), BkError> {
    let history = cdp
        .send(
            cdpkit::page::methods::GetNavigationHistory::new(),
            Some(session_id),
        )
        .await?;

    // Safely check bounds: current_index is i64 from CDP, subtraction won't overflow
    // in practice but we guard against it and negative indices explicitly.
    if history.current_index <= 0 {
        return Err(BkError::NavigationFailed("no previous history entry".into()));
    }
    let new_index = (history.current_index - 1) as usize;
    if new_index >= history.entries.len() {
        return Err(BkError::NavigationFailed("no previous history entry".into()));
    }

    let entry_id = history.entries[new_index].id;
    cdp.send(
        cdpkit::page::methods::NavigateToHistoryEntry::new(entry_id),
        Some(session_id),
    )
    .await?;

    wait_for_load(cdp, session_id, PAGE_LOAD_TIMEOUT).await?;

    Ok(())
}

/// Navigate forward in history using `Page.getNavigationHistory` + `Page.navigateToHistoryEntry`.
pub async fn forward(cdp: &Arc<CDP>, session_id: &str) -> Result<(), BkError> {
    let history = cdp
        .send(
            cdpkit::page::methods::GetNavigationHistory::new(),
            Some(session_id),
        )
        .await?;

    // Guard against negative current_index (CDP returns i64)
    if history.current_index < 0 {
        return Err(BkError::NavigationFailed("no next history entry".into()));
    }

    let new_index = history.current_index + 1;
    if (new_index as usize) >= history.entries.len() {
        return Err(BkError::NavigationFailed("no next history entry".into()));
    }

    let entry_id = history.entries[new_index as usize].id;
    cdp.send(
        cdpkit::page::methods::NavigateToHistoryEntry::new(entry_id),
        Some(session_id),
    )
    .await?;

    wait_for_load(cdp, session_id, PAGE_LOAD_TIMEOUT).await?;

    Ok(())
}

/// Get the current page URL via `Runtime.evaluate("window.location.href")`.
pub async fn get_url(cdp: &Arc<CDP>, session_id: &str) -> Result<String, BkError> {
    let resp = cdp
        .send(
            cdpkit::runtime::methods::Evaluate::new("window.location.href")
                .with_return_by_value(true),
            Some(session_id),
        )
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(details.text.clone()));
    }

    resp.result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| BkError::Other("failed to get URL from evaluate result".into()))
}

/// Get the current page title via `Runtime.evaluate("document.title")`.
pub async fn get_title(cdp: &Arc<CDP>, session_id: &str) -> Result<String, BkError> {
    let resp = cdp
        .send(
            cdpkit::runtime::methods::Evaluate::new("document.title")
                .with_return_by_value(true),
            Some(session_id),
        )
        .await?;

    if let Some(details) = &resp.exception_details {
        return Err(BkError::JsError(details.text.clone()));
    }

    resp.result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| BkError::Other("failed to get title from evaluate result".into()))
}

/// Wait for the page `load` lifecycle event.
///
/// Uses polling via `Runtime.evaluate("document.readyState")` to detect when
/// the page has finished loading. This approach is session-aware and avoids
/// the issue of receiving lifecycle events from other sessions.
///
/// Uses exponential backoff (50ms → 100ms → 200ms → capped at 500ms) to
/// reduce CDP traffic while keeping latency low for fast-loading pages.
///
/// Falls back to a 30-second timeout if the page never reaches "complete".
pub async fn wait_for_load(
    cdp: &Arc<CDP>,
    session_id: &str,
    timeout: Duration,
) -> Result<(), BkError> {
    let start = tokio::time::Instant::now();
    let mut poll_interval = Duration::from_millis(50);
    let max_interval = Duration::from_millis(500);

    loop {
        if start.elapsed() >= timeout {
            return Err(BkError::Timeout("waiting for page load".into()));
        }

        let resp = cdp
            .send(
                cdpkit::runtime::methods::Evaluate::new("document.readyState")
                    .with_return_by_value(true),
                Some(session_id),
            )
            .await;

        match resp {
            Ok(r) => {
                if let Some(val) = r.result.value.as_ref().and_then(|v| v.as_str()) {
                    if val == "complete" {
                        return Ok(());
                    }
                }
            }
            Err(_) => {
                // Session might not be ready yet, keep polling
            }
        }

        tokio::time::sleep(poll_interval).await;
        // Exponential backoff capped at max_interval
        poll_interval = (poll_interval * 2).min(max_interval);
    }
}
