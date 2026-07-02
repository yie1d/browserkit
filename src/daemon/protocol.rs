// Request/Response protocol: newline-delimited JSON

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use crate::error::{BkError, ErrorCode};

/// A command request sent from client to daemon.
///
/// Transported as a single JSON line terminated by `\n`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// Authentication token (required for all requests when daemon has a token set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// A response sent from daemon to client.
///
/// Success: `{"ok":true,"data":{...}}`
/// Error (legacy):   `{"ok":false,"error":"<message>"}`
/// Error (v2):       `{"ok":false,"error":{"code":"...","message":"...","suggestion":"...","recoverable":bool}}`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

impl Response {
    /// Build a success response with the given data payload.
    #[must_use]
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Build a legacy error response with a plain string message.
    #[must_use]
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(serde_json::Value::String(msg.into())),
        }
    }

    /// Build a v2 structured error response with code, message, suggestion, and recoverable flag.
    #[must_use]
    pub fn error_detail(code: ErrorCode, message: String, suggestion: Option<String>) -> Self {
        let suggestion = suggestion.unwrap_or_else(|| code.suggestion().to_string());
        let recoverable = code.recoverable();
        Self {
            ok: false,
            data: None,
            error: Some(serde_json::json!({
                "code": code,
                "message": message,
                "suggestion": suggestion,
                "recoverable": recoverable,
            })),
        }
    }
}

impl From<BkError> for Response {
    fn from(e: BkError) -> Self {
        Response::err(e.to_string())
    }
}

/// Maximum allowed request line size (1 MB). Prevents DoS via unbounded reads.
const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;

/// Read a single [`Request`] from a newline-delimited JSON stream.
///
/// Returns `Ok(None)` when the stream reaches EOF (client disconnected).
/// Returns an error `Response` when the line cannot be parsed as a valid request.
/// Rejects lines exceeding [`MAX_REQUEST_LINE_BYTES`] to prevent memory exhaustion.
pub async fn read_request<R>(reader: &mut BufReader<R>) -> Result<Option<Request>, Response>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    let mut total = 0usize;

    // Read one byte at a time from the buffered reader until newline or limit.
    // BufReader makes this efficient — it reads from its internal buffer, not
    // from the underlying reader on each call.
    loop {
        let buf = reader.fill_buf().await.map_err(|e| {
            Response::err(format!("IO error: {e}"))
        })?;

        if buf.is_empty() {
            if total == 0 {
                return Ok(None); // EOF before any data
            }
            break; // EOF mid-line, try to parse what we have
        }

        // Find newline position within the buffered data
        let available = buf.len();
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            // Found newline — consume up to and including it
            let chunk = &buf[..=pos];
            total += chunk.len();
            if total > MAX_REQUEST_LINE_BYTES {
                reader.consume(pos + 1);
                return Err(Response::err(format!(
                    "request too large (>{} bytes, max {})",
                    MAX_REQUEST_LINE_BYTES, MAX_REQUEST_LINE_BYTES
                )));
            }
            // Safety: we're reading from a text protocol, invalid UTF-8 will be caught by JSON parse
            line.push_str(&String::from_utf8_lossy(chunk));
            reader.consume(pos + 1);
            break;
        } else {
            // No newline yet — consume all available bytes
            total += available;
            if total > MAX_REQUEST_LINE_BYTES {
                reader.consume(available);
                return Err(Response::err(format!(
                    "request too large (>{} bytes, max {})",
                    MAX_REQUEST_LINE_BYTES, MAX_REQUEST_LINE_BYTES
                )));
            }
            line.push_str(&String::from_utf8_lossy(buf));
            reader.consume(available);
        }
    }

    let req: Request = serde_json::from_str(line.trim()).map_err(|e| {
        Response::err(format!("invalid request: {e}"))
    })?;
    Ok(Some(req))
}

/// Write a single [`Response`] as a newline-delimited JSON line.
pub async fn write_response<W>(writer: &mut BufWriter<W>, resp: &Response) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_string(resp)
        .map_err(std::io::Error::other)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_ok_builds_success() {
        let r = Response::ok(json!({"status": "running"}));
        assert!(r.ok);
        assert_eq!(r.data, Some(json!({"status": "running"})));
        assert!(r.error.is_none());
    }

    #[test]
    fn response_err_builds_error() {
        let r = Response::err("something broke");
        assert!(!r.ok);
        assert!(r.data.is_none());
        assert_eq!(
            r.error,
            Some(serde_json::Value::String("something broke".into()))
        );
    }

    #[test]
    fn request_default_params_is_null() {
        let req: Request = serde_json::from_str(r#"{"cmd":"ping"}"#).unwrap();
        assert_eq!(req.cmd, "ping");
        assert_eq!(req.params, serde_json::Value::Null);
        assert_eq!(req.token, None);
    }

    #[test]
    fn request_with_token_deserializes() {
        let req: Request = serde_json::from_str(
            r#"{"cmd":"ping","params":{},"token":"abc123"}"#,
        )
        .unwrap();
        assert_eq!(req.cmd, "ping");
        assert_eq!(req.token, Some("abc123".into()));
    }

    #[test]
    fn request_without_token_field_defaults_to_none() {
        let req: Request = serde_json::from_str(r#"{"cmd":"ping","params":{}}"#).unwrap();
        assert_eq!(req.token, None);
    }

    #[test]
    fn request_token_none_is_not_serialized() {
        let req = Request {
            cmd: "ping".into(),
            params: json!({}),
            token: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("token"));
    }

    #[test]
    fn request_token_some_is_serialized() {
        let req = Request {
            cmd: "ping".into(),
            params: json!({}),
            token: Some("secret".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""token":"secret""#));
    }

    #[test]
    fn request_roundtrip() {
        let req = Request {
            cmd: "ws.new".into(),
            params: json!({"label": "test"}),
            token: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn response_ok_roundtrip() {
        let resp = Response::ok(json!({"wid": "a3f2"}));
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn response_err_roundtrip() {
        let resp = Response::err("not found");
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn response_ok_skips_none_fields() {
        let resp = Response::ok(json!(42));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn response_err_skips_none_fields() {
        let resp = Response::err("oops");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("data"));
    }

    #[test]
    fn from_bkerror_produces_error_response() {
        let e = BkError::WorkspaceNotFound("a3f2".into());
        let resp: Response = e.into();
        assert!(!resp.ok);
        assert_eq!(
            resp.error,
            Some(serde_json::Value::String("workspace not found: a3f2".into()))
        );
    }

    #[tokio::test]
    async fn read_request_parses_valid_json_line() {
        let input = b"{\"cmd\":\"ping\",\"params\":{}}\n";
        let mut reader = BufReader::new(&input[..]);
        let req = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(req.cmd, "ping");
        assert_eq!(req.params, json!({}));
    }

    #[tokio::test]
    async fn read_request_returns_none_on_eof() {
        let input = b"";
        let mut reader = BufReader::new(&input[..]);
        let result = read_request(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_request_returns_error_on_invalid_json() {
        let input = b"not json\n";
        let mut reader = BufReader::new(&input[..]);
        let err = read_request(&mut reader).await.unwrap_err();
        assert!(!err.ok);
        assert!(err.error.unwrap().as_str().unwrap().contains("invalid request"));
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_line() {
        // Create a line that exceeds MAX_REQUEST_LINE_BYTES (1MB) without a newline
        let oversized = vec![b'x'; MAX_REQUEST_LINE_BYTES + 100];
        let mut reader = BufReader::new(&oversized[..]);
        let err = read_request(&mut reader).await.unwrap_err();
        assert!(!err.ok);
        assert!(err.error.unwrap().as_str().unwrap().contains("request too large"));
    }

    #[tokio::test]
    async fn write_response_produces_json_line() {
        let resp = Response::ok(json!({"status": "running"}));
        let mut buf = Vec::new();
        {
            let mut writer = BufWriter::new(&mut buf);
            write_response(&mut writer, &resp).await.unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert!(output.ends_with('\n'));
        let parsed: Response = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed, resp);
    }

    // ── v2 error_detail tests ───────────────────────────────────────────

    #[test]
    fn response_error_detail_has_correct_structure() {
        let resp =
            Response::error_detail(ErrorCode::RefNotFound, "element ref 42 not found".into(), None);
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err["code"], "REF_NOT_FOUND");
        assert!(err["message"].as_str().unwrap().contains("42"));
        assert!(err["suggestion"].as_str().unwrap().contains("snapshot"));
        assert_eq!(err["recoverable"], true);
    }

    #[test]
    fn response_error_detail_custom_suggestion() {
        let resp = Response::error_detail(
            ErrorCode::Timeout,
            "timed out".into(),
            Some("try again with --timeout 60000".into()),
        );
        let err = resp.error.unwrap();
        assert_eq!(err["suggestion"], "try again with --timeout 60000");
    }

    #[test]
    fn response_error_detail_json_wire_format() {
        let resp = Response::error_detail(ErrorCode::NotConnected, "no connection".into(), None);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "NOT_CONNECTED");
        assert_eq!(json["error"]["recoverable"], true);
    }

    #[test]
    fn response_legacy_err_unchanged() {
        let resp = Response::err("something broke");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["error"], "something broke");
    }
}
