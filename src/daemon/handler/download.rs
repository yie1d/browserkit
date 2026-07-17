// Session-native download lifecycle handler.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use serde_json::json;

use super::common::resolve_session_target;
use crate::daemon::protocol::{Request, Response};
use crate::daemon::state::DaemonState;
use crate::error::ErrorCode;

static DOWNLOAD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug)]
struct DownloadParams {
    element_ref: i64,
    output_dir: PathBuf,
    timeout: u64,
}

fn invalid_argument(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::InvalidArgument, message.into(), None)
}

fn validate_download_params(params: &serde_json::Value) -> Result<DownloadParams, Response> {
    let element_ref = params
        .get("ref")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_argument("download 'ref' must be a positive integer"))?;
    let output_dir = params
        .get("output_dir")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| invalid_argument("download 'output_dir' must be a string"))?;
    if !output_dir.is_absolute() {
        return Err(invalid_argument(
            "download 'output_dir' must be an absolute path",
        ));
    }
    let output_dir = std::fs::canonicalize(&output_dir).map_err(|error| {
        invalid_argument(format!(
            "download output directory '{}' cannot be resolved: {error}",
            output_dir.display()
        ))
    })?;
    if !output_dir.is_dir() {
        return Err(invalid_argument(format!(
            "download output path is not a directory: {}",
            output_dir.display()
        )));
    }
    let timeout = match params.get("timeout") {
        None => 30000,
        Some(value) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid_argument("download 'timeout' must be a positive integer"))?,
    };

    Ok(DownloadParams {
        element_ref,
        output_dir,
        timeout,
    })
}

fn download_failed(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::DownloadFailed, message.into(), None)
}

fn resolve_download_path(
    output_dir: &Path,
    event_path: Option<&Path>,
    suggested_filename: &str,
) -> Result<Option<PathBuf>, Response> {
    let output_dir = std::fs::canonicalize(output_dir).map_err(|error| {
        download_failed(format!(
            "download output directory '{}' cannot be resolved: {error}",
            output_dir.display()
        ))
    })?;
    let candidate = if let Some(path) = event_path {
        if !path.is_absolute() {
            return Err(download_failed(format!(
                "Chrome returned a non-absolute download path: {}",
                path.display()
            )));
        }
        path.to_path_buf()
    } else {
        let Some(filename) = Path::new(suggested_filename).file_name() else {
            return Ok(None);
        };
        output_dir.join(filename)
    };

    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    if !canonical.starts_with(&output_dir) {
        return Err(download_failed(format!(
            "download path escaped output directory: {}",
            canonical.display()
        )));
    }
    Ok(Some(canonical))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadTerminal {
    Completed,
    Canceled,
}

fn download_start_matches(event_frame_id: &str, main_frame_id: &str) -> bool {
    event_frame_id == main_frame_id
}

fn classify_download_progress(
    event_guid: &str,
    download_guid: &str,
    state: &str,
) -> Option<DownloadTerminal> {
    if event_guid != download_guid {
        return None;
    }
    match state {
        "completed" => Some(DownloadTerminal::Completed),
        "canceled" => Some(DownloadTerminal::Canceled),
        _ => None,
    }
}

fn timeout_response(message: impl Into<String>) -> Response {
    Response::error_detail(ErrorCode::Timeout, message.into(), None)
}

fn download_behavior(
    behavior: &str,
    output_dir: Option<&Path>,
    browser_context_id: Option<&str>,
    events_enabled: bool,
) -> cdpkit::browser::methods::SetDownloadBehavior {
    let mut command = cdpkit::browser::methods::SetDownloadBehavior::new(behavior)
        .with_events_enabled(events_enabled);
    if let Some(output_dir) = output_dir {
        command = command.with_download_path(output_dir.to_string_lossy());
    }
    if let Some(browser_context_id) = browser_context_id {
        command = command.with_browser_context_id(browser_context_id.to_string());
    }
    command
}

async fn restore_download_behavior(
    ctx: &super::common::SessionTargetContext,
) -> Result<(), Response> {
    let reset = download_behavior("default", None, ctx.browser_context_id.as_deref(), false);
    match tokio::time::timeout(Duration::from_secs(5), reset.send(&*ctx.cdp)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Response::error_detail(
            ErrorCode::DaemonError,
            format!("failed to restore download behavior: {error}"),
            None,
        )),
        Err(_) => Err(Response::error_detail(
            ErrorCode::DaemonError,
            "timed out restoring download behavior".into(),
            None,
        )),
    }
}

async fn cancel_download(cdp: &cdpkit::CDP, guid: &str, browser_context_id: Option<&str>) {
    let mut command = cdpkit::browser::methods::CancelDownload::new(guid);
    if let Some(browser_context_id) = browser_context_id {
        command = command.with_browser_context_id(browser_context_id.to_string());
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), command.send(cdp)).await;
}

struct DownloadObservation<'a> {
    req: &'a Request,
    state: &'a Arc<DaemonState>,
    ctx: &'a super::common::SessionTargetContext,
    params: &'a DownloadParams,
    main_frame_id: &'a str,
    deadline: tokio::time::Instant,
}

async fn observe_download(
    observation: DownloadObservation<'_>,
    mut begin_events: cdpkit::EventStream<cdpkit::browser::events::DownloadWillBegin>,
    mut progress_events: cdpkit::EventStream<cdpkit::browser::events::DownloadProgress>,
) -> Response {
    let DownloadObservation {
        req,
        state,
        ctx,
        params,
        main_frame_id,
        deadline,
    } = observation;
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return timeout_response("download timed out before the trigger click");
    }
    let trigger = Request {
        cmd: "act".into(),
        params: json!({
            "kind": "click",
            "ref": params.element_ref,
            "session": ctx.session_name,
            "target": ctx.target_id,
            "timeout": remaining.as_millis().min(u128::from(u64::MAX)) as u64,
            "no_state_diff": true,
        }),
        token: req.token.clone(),
    };
    let trigger_response = super::act::handle_act(&trigger, state).await;
    if !trigger_response.ok {
        return trigger_response;
    }

    let started = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return timeout_response(format!(
                    "no download started within {}ms after clicking ref {}",
                    params.timeout, params.element_ref
                ));
            }
            event = begin_events.next() => match event {
                Some(event) if download_start_matches(&event.frame_id, main_frame_id) => break event,
                Some(_) => continue,
                None => {
                    return Response::error_detail(
                        ErrorCode::DaemonError,
                        "download start event stream closed".into(),
                        None,
                    );
                }
            }
        }
    };

    let mut progress_count = 0usize;
    let mut received_bytes = 0.0;
    let mut total_bytes = 0.0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                cancel_download(&ctx.cdp, &started.guid, ctx.browser_context_id.as_deref()).await;
                return timeout_response(format!(
                    "download '{}' timed out after {}ms at {}/{} bytes",
                    started.guid, params.timeout, received_bytes, total_bytes
                ));
            }
            event = progress_events.next() => match event {
                Some(event) if event.guid == started.guid => {
                    progress_count += 1;
                    received_bytes = event.received_bytes;
                    total_bytes = event.total_bytes;
                    match classify_download_progress(&event.guid, &started.guid, &event.state) {
                        Some(DownloadTerminal::Completed) => {
                            let path = match resolve_download_path(
                                &params.output_dir,
                                event.file_path.as_deref().map(Path::new),
                                &started.suggested_filename,
                            ) {
                                Ok(path) => path,
                                Err(response) => return response,
                            };
                            return Response::ok(json!({
                                "session": ctx.session_name,
                                "target": ctx.target_id,
                                "ref": params.element_ref,
                                "guid": started.guid,
                                "url": started.url,
                                "suggested_filename": started.suggested_filename,
                                "output_dir": params.output_dir,
                                "path": path,
                                "path_verified": path.is_some(),
                                "status": "completed",
                                "progress_events": progress_count,
                                "received_bytes": received_bytes,
                                "total_bytes": total_bytes,
                            }));
                        }
                        Some(DownloadTerminal::Canceled) => {
                            return download_failed(format!(
                                "download '{}' was canceled after receiving {}/{} bytes",
                                started.guid, received_bytes, total_bytes
                            ));
                        }
                        None => {}
                    }
                }
                Some(_) => continue,
                None => {
                    return Response::error_detail(
                        ErrorCode::DaemonError,
                        "download progress event stream closed".into(),
                        None,
                    );
                }
            }
        }
    }
}

pub async fn handle_download(req: &Request, state: &Arc<DaemonState>) -> Response {
    let params = match validate_download_params(&req.params) {
        Ok(params) => params,
        Err(response) => return response,
    };
    let ctx = match resolve_session_target(state, &req.params) {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(params.timeout);
    let _guard = match tokio::time::timeout_at(deadline, DOWNLOAD_LOCK.lock()).await {
        Ok(guard) => guard,
        Err(_) => return timeout_response("download timed out waiting for another download"),
    };

    let session = ctx.cdp.session(&ctx.cdp_session_id);
    let frame_tree = match tokio::time::timeout_at(
        deadline,
        cdpkit::page::methods::GetFrameTree::new().send(&session),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            return Response::error_detail(
                ErrorCode::DaemonError,
                format!("failed to resolve download frame: {error}"),
                None,
            )
        }
        Err(_) => return timeout_response("download timed out while resolving the page frame"),
    };

    let begin_events = cdpkit::browser::events::DownloadWillBegin::subscribe(&*ctx.cdp);
    let progress_events = cdpkit::browser::events::DownloadProgress::subscribe(&*ctx.cdp);
    let enable = download_behavior(
        "allow",
        Some(&params.output_dir),
        ctx.browser_context_id.as_deref(),
        true,
    );
    match tokio::time::timeout_at(deadline, enable.send(&*ctx.cdp)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = restore_download_behavior(&ctx).await;
            return Response::error_detail(
                ErrorCode::DaemonError,
                format!("failed to configure download behavior: {error}"),
                None,
            );
        }
        Err(_) => {
            if let Err(error) = restore_download_behavior(&ctx).await {
                return error;
            }
            return timeout_response("download timed out while configuring Chrome");
        }
    }

    let response = observe_download(
        DownloadObservation {
            req,
            state,
            ctx: &ctx,
            params: &params,
            main_frame_id: &frame_tree.frame_tree.frame.id,
            deadline,
        },
        begin_events,
        progress_events,
    )
    .await;

    match restore_download_behavior(&ctx).await {
        Ok(()) => response,
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: serde_json::Value) -> Request {
        Request {
            cmd: "download".into(),
            params,
            token: None,
        }
    }

    fn error_code(response: Response) -> serde_json::Value {
        serde_json::to_value(response).unwrap()["error"]["code"].clone()
    }

    #[tokio::test]
    async fn download_rejects_invalid_ref_and_timeout_before_session_lookup() {
        let state = Arc::new(DaemonState::new());
        let dir = tempfile::tempdir().unwrap();

        for params in [
            json!({"ref": 0, "output_dir": dir.path(), "timeout": 1000}),
            json!({"ref": 42, "output_dir": dir.path(), "timeout": 0}),
        ] {
            assert_eq!(
                error_code(handle_download(&request(params), &state).await),
                "INVALID_ARGUMENT"
            );
        }
    }

    #[tokio::test]
    async fn download_requires_an_existing_absolute_directory() {
        let state = Arc::new(DaemonState::new());
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();

        for output_dir in [
            serde_json::Value::String("relative/path".into()),
            serde_json::json!(file),
        ] {
            let params = json!({"ref": 42, "output_dir": output_dir, "timeout": 1000});
            assert_eq!(
                error_code(handle_download(&request(params), &state).await),
                "INVALID_ARGUMENT"
            );
        }
    }

    #[test]
    fn completed_download_path_must_stay_inside_output_directory() {
        let output = tempfile::tempdir().unwrap();
        let inside = output.path().join("report.json");
        std::fs::write(&inside, b"{}").unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("secret.txt");
        std::fs::write(&outside, b"secret").unwrap();

        assert_eq!(
            resolve_download_path(output.path(), Some(inside.as_path()), "ignored.json").unwrap(),
            Some(std::fs::canonicalize(&inside).unwrap())
        );
        let error = resolve_download_path(output.path(), Some(outside.as_path()), "ignored.txt")
            .expect_err("outside file must be rejected");
        assert_eq!(error_code(error), "DOWNLOAD_FAILED");
    }

    #[test]
    fn completed_download_fallback_uses_only_suggested_basename() {
        let output = tempfile::tempdir().unwrap();
        let inside = output.path().join("report.json");
        std::fs::write(&inside, b"{}").unwrap();

        assert_eq!(
            resolve_download_path(output.path(), None, "../report.json").unwrap(),
            Some(std::fs::canonicalize(&inside).unwrap())
        );
        assert_eq!(
            resolve_download_path(output.path(), None, "missing.json").unwrap(),
            None
        );
    }

    #[test]
    fn download_events_are_correlated_by_frame_and_guid() {
        assert!(download_start_matches("FRAME1", "FRAME1"));
        assert!(!download_start_matches("FRAME2", "FRAME1"));

        assert_eq!(
            classify_download_progress("GUID1", "GUID1", "inProgress"),
            None
        );
        assert_eq!(
            classify_download_progress("GUID1", "GUID1", "completed"),
            Some(DownloadTerminal::Completed)
        );
        assert_eq!(
            classify_download_progress("GUID1", "GUID1", "canceled"),
            Some(DownloadTerminal::Canceled)
        );
        assert_eq!(
            classify_download_progress("OTHER", "GUID1", "completed"),
            None
        );
    }
}
