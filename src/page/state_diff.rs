// state_diff: lightweight before/after page state comparison for act responses.
//
// After each act operation, compare URL/title/element count before and after
// to produce a state_diff object telling the agent what changed without a full snapshot.

use serde::Serialize;
use serde_json::json;

use super::INTERACTIVE_SELECTOR;

/// A lightweight snapshot of page state for diff computation.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub url: String,
    pub title: String,
    pub element_count: usize,
}

/// URL change record.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UrlChange {
    pub from: String,
    pub to: String,
}

/// Title change record.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TitleChange {
    pub from: String,
    pub to: String,
}

/// The diff between pre-action and post-action page state.
#[derive(Debug, Clone)]
pub struct StateDiff {
    pub url_changed: Option<UrlChange>,
    pub title_changed: Option<TitleChange>,
    pub elements_added: i64,
    pub elements_removed: i64,
}

/// Compare two state snapshots and produce a diff.
pub fn compute_state_diff(before: &StateSnapshot, after: &StateSnapshot) -> StateDiff {
    let url_changed = if before.url != after.url {
        Some(UrlChange {
            from: before.url.clone(),
            to: after.url.clone(),
        })
    } else {
        None
    };

    let title_changed = if before.title != after.title {
        Some(TitleChange {
            from: before.title.clone(),
            to: after.title.clone(),
        })
    } else {
        None
    };

    let count_diff = after.element_count as i64 - before.element_count as i64;
    let (elements_added, elements_removed) = if count_diff >= 0 {
        (count_diff, 0)
    } else {
        (0, -count_diff)
    };

    StateDiff {
        url_changed,
        title_changed,
        elements_added,
        elements_removed,
    }
}

impl StateDiff {
    /// Serialize to JSON value for inclusion in act responses.
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "url_changed": self.url_changed.as_ref().map(|c| json!({"from": c.from, "to": c.to})),
            "title_changed": self.title_changed.as_ref().map(|c| json!({"from": c.from, "to": c.to})),
            "elements_added": self.elements_added,
            "elements_removed": self.elements_removed,
        })
    }
}

/// Capture a lightweight state snapshot from a CDP session.
///
/// Uses a single `Runtime.evaluate` call to get URL, title, and interactive element count.
/// The selector matches the same `INTERACTIVE_SELECTOR` used by element discovery.
pub async fn capture_state_snapshot(
    session: &(impl cdpkit::Sender + Sync),
) -> Result<StateSnapshot, crate::error::BkError> {
    // Build JS that queries all interactive elements using the same selector as discovery.
    let js = format!(
        r#"JSON.stringify({{url: window.location.href, title: document.title, count: document.querySelectorAll({selector}).length}})"#,
        selector = serde_json::to_string(INTERACTIVE_SELECTOR)
            .unwrap_or_else(|_| format!("\"{}\"", INTERACTIVE_SELECTOR))
    );

    let result = cdpkit::runtime::methods::Evaluate::new(js)
        .send(session)
        .await
        .map_err(|e| crate::error::BkError::JsError(format!("state snapshot eval failed: {e}")))?;

    // Check for exceptions
    if let Some(details) = result.exception_details {
        return Err(crate::error::BkError::JsError(format!(
            "state snapshot exception: {}",
            super::exception_message(&details)
        )));
    }

    let json_str = result
        .result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| {
            crate::error::BkError::JsError("state snapshot returned non-string".into())
        })?;

    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| crate::error::BkError::JsError(format!("state snapshot parse error: {e}")))?;

    Ok(StateSnapshot {
        url: parsed["url"].as_str().unwrap_or("").to_string(),
        title: parsed["title"].as_str().unwrap_or("").to_string(),
        element_count: parsed["count"].as_u64().unwrap_or(0) as usize,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_snapshot_captures_fields() {
        let snap = StateSnapshot {
            url: "https://example.com".into(),
            title: "Example".into(),
            element_count: 42,
        };
        assert_eq!(snap.url, "https://example.com");
        assert_eq!(snap.title, "Example");
        assert_eq!(snap.element_count, 42);
    }

    #[test]
    fn compute_diff_no_change() {
        let before = StateSnapshot {
            url: "https://a.com".into(),
            title: "A".into(),
            element_count: 10,
        };
        let after = StateSnapshot {
            url: "https://a.com".into(),
            title: "A".into(),
            element_count: 10,
        };
        let diff = compute_state_diff(&before, &after);
        assert!(diff.url_changed.is_none());
        assert!(diff.title_changed.is_none());
        assert_eq!(diff.elements_added, 0);
        assert_eq!(diff.elements_removed, 0);
    }

    #[test]
    fn compute_diff_url_changed() {
        let before = StateSnapshot {
            url: "https://a.com/login".into(),
            title: "Login".into(),
            element_count: 5,
        };
        let after = StateSnapshot {
            url: "https://a.com/dashboard".into(),
            title: "Dashboard".into(),
            element_count: 15,
        };
        let diff = compute_state_diff(&before, &after);
        let url_change = diff.url_changed.unwrap();
        assert_eq!(url_change.from, "https://a.com/login");
        assert_eq!(url_change.to, "https://a.com/dashboard");
        let title_change = diff.title_changed.unwrap();
        assert_eq!(title_change.from, "Login");
        assert_eq!(title_change.to, "Dashboard");
        assert_eq!(diff.elements_added, 10);
        assert_eq!(diff.elements_removed, 0);
    }

    #[test]
    fn compute_diff_elements_removed() {
        let before = StateSnapshot {
            url: "https://a.com".into(),
            title: "A".into(),
            element_count: 20,
        };
        let after = StateSnapshot {
            url: "https://a.com".into(),
            title: "A".into(),
            element_count: 12,
        };
        let diff = compute_state_diff(&before, &after);
        assert_eq!(diff.elements_added, 0);
        assert_eq!(diff.elements_removed, 8);
    }

    #[test]
    fn compute_diff_only_url_changes() {
        let before = StateSnapshot {
            url: "https://a.com/page1".into(),
            title: "A".into(),
            element_count: 10,
        };
        let after = StateSnapshot {
            url: "https://a.com/page2".into(),
            title: "A".into(),
            element_count: 10,
        };
        let diff = compute_state_diff(&before, &after);
        assert!(diff.url_changed.is_some());
        assert!(diff.title_changed.is_none());
        assert_eq!(diff.elements_added, 0);
        assert_eq!(diff.elements_removed, 0);
    }

    #[test]
    fn compute_diff_only_title_changes() {
        let before = StateSnapshot {
            url: "https://a.com".into(),
            title: "Loading...".into(),
            element_count: 10,
        };
        let after = StateSnapshot {
            url: "https://a.com".into(),
            title: "Done".into(),
            element_count: 10,
        };
        let diff = compute_state_diff(&before, &after);
        assert!(diff.url_changed.is_none());
        assert!(diff.title_changed.is_some());
        assert_eq!(diff.title_changed.unwrap().from, "Loading...");
    }

    #[test]
    fn state_diff_to_json_with_changes() {
        let diff = StateDiff {
            url_changed: Some(UrlChange {
                from: "https://a.com".into(),
                to: "https://b.com".into(),
            }),
            title_changed: None,
            elements_added: 3,
            elements_removed: 1,
        };
        let json = diff.to_json();
        assert_eq!(json["url_changed"]["from"], "https://a.com");
        assert_eq!(json["url_changed"]["to"], "https://b.com");
        assert!(json["title_changed"].is_null());
        assert_eq!(json["elements_added"], 3);
        assert_eq!(json["elements_removed"], 1);
    }

    #[test]
    fn state_diff_to_json_no_changes() {
        let diff = StateDiff {
            url_changed: None,
            title_changed: None,
            elements_added: 0,
            elements_removed: 0,
        };
        let json = diff.to_json();
        // Even with no changes, return the full structure (agent expects consistent shape)
        assert!(json["url_changed"].is_null());
        assert!(json["title_changed"].is_null());
        assert_eq!(json["elements_added"], 0);
        assert_eq!(json["elements_removed"], 0);
    }

    #[test]
    fn state_diff_to_json_both_changed() {
        let diff = StateDiff {
            url_changed: Some(UrlChange {
                from: "https://x.com/login".into(),
                to: "https://x.com/home".into(),
            }),
            title_changed: Some(TitleChange {
                from: "Login".into(),
                to: "Home".into(),
            }),
            elements_added: 5,
            elements_removed: 2,
        };
        let json = diff.to_json();
        assert_eq!(json["url_changed"]["from"], "https://x.com/login");
        assert_eq!(json["url_changed"]["to"], "https://x.com/home");
        assert_eq!(json["title_changed"]["from"], "Login");
        assert_eq!(json["title_changed"]["to"], "Home");
        assert_eq!(json["elements_added"], 5);
        assert_eq!(json["elements_removed"], 2);
    }

    #[test]
    fn state_diff_zero_element_count() {
        let before = StateSnapshot {
            url: "about:blank".into(),
            title: "".into(),
            element_count: 0,
        };
        let after = StateSnapshot {
            url: "https://a.com".into(),
            title: "A".into(),
            element_count: 7,
        };
        let diff = compute_state_diff(&before, &after);
        assert_eq!(diff.elements_added, 7);
        assert_eq!(diff.elements_removed, 0);
    }
}
