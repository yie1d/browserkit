use base64::Engine;
use cdpkit::network::methods::{GetRequestPostData, GetResponseBody};

use super::{RequestIdentity, Terminal};
use crate::runtime::{BrowserError, OperationPhase, Page};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyReadOptions {
    max_bytes: usize,
}
impl BodyReadOptions {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }
}
impl Default for BodyReadOptions {
    fn default() -> Self {
        Self::new(1024 * 1024)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyAvailability {
    Available(Vec<u8>),
    TooLarge {
        limit: usize,
        known_size: Option<u64>,
        transport_may_have_received_full_body: bool,
    },
    Unavailable {
        reason: String,
    },
    NotFinished,
    Evicted,
}

pub(crate) async fn response(
    page: &Page,
    id: &RequestIdentity,
    options: BodyReadOptions,
) -> Result<BodyAvailability, BrowserError> {
    let manager = page.network_manager().ok_or_else(|| {
        BrowserError::operation("read response body", OperationPhase::Preparation)
            .with_message("network observation has not been initialized")
    })?;
    manager.prune_retained_state(id.routed_session_id());
    let Some((_, _, terminal)) = manager.record(id) else {
        return Ok(BodyAvailability::Evicted);
    };
    match terminal {
        Some(Terminal::Finished) => {}
        Some(Terminal::Failed) => {
            return Ok(BodyAvailability::Unavailable {
                reason: "response body is unavailable for failed or redirected requests".into(),
            })
        }
        Some(Terminal::Redirected(next)) => {
            return Ok(BodyAvailability::Unavailable {
                reason: format!(
                    "response body is unavailable after redirect to hop {}",
                    next.redirect_ordinal()
                ),
            })
        }
        None => return Ok(BodyAvailability::NotFinished),
    }
    // encodedDataLength and Content-Length are diagnostic hints only. They do
    // not equal GetResponseBody's decoded bytes for HEAD, 304, compressed, or
    // otherwise transformed responses, so they must never pre-reject a read.
    let Some(session) = manager.route_session(id, true).await else {
        return Ok(BodyAvailability::Evicted);
    };
    let result = match GetResponseBody::new(id.request_id.clone())
        .send(&session)
        .await
    {
        Ok(v) => v,
        Err(error) => return Ok(unavailable(error)),
    };
    Ok(decode_body(result.body, result.base64_encoded, options))
}

pub(crate) async fn request(
    page: &Page,
    id: &RequestIdentity,
    options: BodyReadOptions,
) -> Result<BodyAvailability, BrowserError> {
    let manager = page.network_manager().ok_or_else(|| {
        BrowserError::operation("read request body", OperationPhase::Preparation)
            .with_message("network observation has not been initialized")
    })?;
    manager.prune_retained_state(id.routed_session_id());
    let Some((request, _, _)) = manager.record(id) else {
        return Ok(BodyAvailability::Evicted);
    };
    let Some(request) = request else {
        return Ok(BodyAvailability::Unavailable {
            reason: "request metadata has not arrived".into(),
        });
    };
    if !request.has_post_data {
        return Ok(BodyAvailability::Unavailable {
            reason: "request has no post data".into(),
        });
    }
    if request
        .event_post_data
        .as_ref()
        .is_some_and(|v| v.len() > options.max_bytes)
    {
        return Ok(too_large(
            options,
            request.event_post_data.as_ref().map(|v| v.len() as u64),
        ));
    }
    let Some(session) = manager.route_session(id, false).await else {
        return Ok(BodyAvailability::Evicted);
    };
    match GetRequestPostData::new(id.request_id.clone())
        .send(&session)
        .await
    {
        Ok(result) if result.post_data.len() <= options.max_bytes => {
            Ok(BodyAvailability::Available(result.post_data.into_bytes()))
        }
        Ok(result) => Ok(too_large(options, Some(result.post_data.len() as u64))),
        Err(error) => Ok(unavailable(error)),
    }
}
fn too_large(options: BodyReadOptions, known_size: Option<u64>) -> BodyAvailability {
    BodyAvailability::TooLarge {
        limit: options.max_bytes,
        known_size,
        transport_may_have_received_full_body: true,
    }
}
fn unavailable(error: cdpkit::CdpError) -> BodyAvailability {
    let reason = error.to_string();
    let normalized = reason.to_ascii_lowercase();
    if normalized.contains("no resource with given identifier")
        || normalized.contains("no data found")
    {
        return BodyAvailability::Evicted;
    }
    BodyAvailability::Unavailable { reason }
}
fn decode_body(body: String, base64_encoded: bool, options: BodyReadOptions) -> BodyAvailability {
    if base64_encoded {
        let encoded_len = body.len();
        if encoded_len > safe_encoded_max(options.max_bytes) {
            return too_large(options, decoded_upper_bound(encoded_len).map(|n| n as u64));
        }
        return match base64::engine::general_purpose::STANDARD.decode(body.as_bytes()) {
            Ok(bytes) if bytes.len() <= options.max_bytes => BodyAvailability::Available(bytes),
            Ok(bytes) => too_large(options, Some(bytes.len() as u64)),
            Err(error) => BodyAvailability::Unavailable {
                reason: format!("Chrome returned invalid base64 response body: {error}"),
            },
        };
    }
    if body.len() > options.max_bytes {
        too_large(options, Some(body.len() as u64))
    } else {
        BodyAvailability::Available(body.into_bytes())
    }
}
fn safe_encoded_max(max_bytes: usize) -> usize {
    max_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .unwrap_or(usize::MAX)
}
fn decoded_upper_bound(encoded: usize) -> Option<usize> {
    encoded.checked_add(3)?.checked_div(4)?.checked_mul(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_bound_is_checked_before_decode() {
        assert_eq!(decoded_upper_bound(8), Some(6));
        assert_eq!(safe_encoded_max(3), 4);
        assert_eq!(safe_encoded_max(usize::MAX), usize::MAX);
    }

    #[test]
    fn decode_body_decodes_base64_and_enforces_decoded_limit() {
        assert_eq!(
            decode_body("aGVsbG8=".to_owned(), true, BodyReadOptions::new(5)),
            BodyAvailability::Available(b"hello".to_vec())
        );
        assert!(matches!(
            decode_body("aGVsbG8=".to_owned(), true, BodyReadOptions::new(4)),
            BodyAvailability::TooLarge {
                known_size: Some(5),
                ..
            }
        ));
        assert!(matches!(
            decode_body("%%%".to_owned(), true, BodyReadOptions::new(10)),
            BodyAvailability::Unavailable { .. }
        ));
    }

    #[test]
    fn too_large_is_honest_about_protocol_transport() {
        assert!(matches!(
            too_large(BodyReadOptions::new(10), Some(11)),
            BodyAvailability::TooLarge {
                transport_may_have_received_full_body: true,
                ..
            }
        ));
    }
}
