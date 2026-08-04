use crate::daemon::protocol::Response;
use crate::error::ErrorCode;
use url::Url;

pub(super) fn validate_and_normalize_url(url: &str) -> Result<String, Response> {
    let normalized = url.trim();
    if normalized.is_empty() {
        return Err(invalid_url("URL must not be empty"));
    }

    if normalized.chars().any(char::is_control) {
        return Err(invalid_url("URL must not contain control characters"));
    }
    let parsed =
        Url::parse(normalized).map_err(|error| invalid_url(format!("invalid URL: {error}")))?;
    let allowed = match parsed.scheme() {
        "http" | "https" => parsed.host().is_some(),
        "file" => valid_file_url(normalized, &parsed),
        "about" => parsed.as_str().eq_ignore_ascii_case("about:blank"),
        _ => false,
    };

    if !allowed {
        return Err(invalid_url(format!(
            "URL scheme or form not allowed: {}",
            parsed.scheme()
        )));
    }

    Ok(parsed.to_string())
}

fn valid_file_url(original: &str, parsed: &Url) -> bool {
    if original.starts_with("file:////") || original.starts_with("file://C:/") {
        return false;
    }
    match parsed.host_str() {
        Some(host) => !host.is_empty() && parsed.path() != "/",
        None => original.starts_with("file:///") && parsed.path() != "/",
    }
}

fn invalid_url(message: impl Into<String>) -> Response {
    Response::error_detail(
        ErrorCode::InvalidArgument,
        message.into(),
        Some("use http:, https:, file:, or about:blank".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_web_loopback_private_and_file_urls() {
        for url in [
            "https://example.com/path?q=1",
            "http://127.0.0.1:9222/",
            "http://[::1]:3000/",
            "http://192.168.1.8/",
            "file:///C:/Users/test/report.html",
            "file://server/share/report.html",
            "about:blank",
        ] {
            assert_eq!(validate_and_normalize_url(url).unwrap(), url);
        }
    }

    #[test]
    fn trims_before_enforcing_policy() {
        assert_eq!(
            validate_and_normalize_url("  https://example.com/a  ").unwrap(),
            "https://example.com/a"
        );
        assert!(validate_and_normalize_url("  javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_active_content_internal_and_unknown_schemes() {
        for url in [
            "javascript:alert(1)",
            "data:text/plain,hello",
            "chrome://settings",
            "chrome-extension://abc/page.html",
            "devtools://devtools/bundled/inspector.html",
            "ftp://example.com/file",
            "custom:payload",
            "about:config",
        ] {
            let response = validate_and_normalize_url(url).unwrap_err();
            let json = serde_json::to_value(response).unwrap();
            assert_eq!(json["error"]["code"], "INVALID_ARGUMENT", "{url}");
        }
    }

    #[test]
    fn rejects_malformed_or_empty_urls() {
        for url in [
            "",
            "   ",
            "https://",
            "https://exa[mple.com/path",
            "file://",
            "file://C:/report.html",
            "file:////server/share/report.html",
        ] {
            assert!(validate_and_normalize_url(url).is_err(), "{url:?}");
        }
    }
}
