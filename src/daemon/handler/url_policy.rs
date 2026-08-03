use crate::daemon::protocol::Response;
use crate::error::ErrorCode;

pub(super) fn validate_and_normalize_url(url: &str) -> Result<String, Response> {
    let normalized = url.trim();
    if normalized.is_empty() {
        return Err(invalid_url("URL must not be empty"));
    }

    if normalized.chars().any(char::is_control) {
        return Err(invalid_url("URL must not contain control characters"));
    }
    let (scheme, remainder) = normalized
        .split_once(':')
        .ok_or_else(|| invalid_url("URL must include a scheme"))?;
    if !valid_scheme(scheme) {
        return Err(invalid_url("URL has an invalid scheme"));
    }
    let scheme = scheme.to_ascii_lowercase();
    let allowed = match scheme.as_str() {
        "http" | "https" => valid_web_remainder(remainder),
        "file" => valid_file_remainder(remainder),
        "about" => remainder.eq_ignore_ascii_case("blank"),
        _ => false,
    };

    if !allowed {
        return Err(invalid_url(format!(
            "URL scheme or form not allowed: {}",
            scheme
        )));
    }

    Ok(normalized.to_string())
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
}

fn valid_web_remainder(remainder: &str) -> bool {
    let Some(authority_and_path) = remainder.strip_prefix("//") else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    !authority.is_empty() && !authority.chars().any(char::is_whitespace)
}

fn valid_file_remainder(remainder: &str) -> bool {
    let Some(location) = remainder.strip_prefix("//") else {
        return false;
    };
    !matches!(location, "" | "/") && !location.chars().any(char::is_whitespace)
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
        for url in ["", "   ", "https://", "file://"] {
            assert!(validate_and_normalize_url(url).is_err(), "{url:?}");
        }
    }
}
