use std::future::Future;
use std::time::Instant;

use futures::StreamExt;

use super::{
    ActionCompletion, BrowserError, OperationPhase, Page, SessionEvent, WaitFailure, WaitOptions,
};

pub(crate) async fn expect_popup<F>(
    opener: &Page,
    options: WaitOptions,
    action: F,
) -> Result<Page, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>> + Send + 'static,
{
    let operation = opener.admit_operation("expect popup")?;
    let session = opener.owner_session()?;
    // Registration completes before `action` is first polled. The Session's target
    // lifecycle reducer remains the sole Target.* watcher.
    let mut events = session.subscribe_events().await?;
    let opener_target_id = opener.target_id().to_owned();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let (lease, cancellation) = opener.side_effect_actions().register();
    tokio::spawn(async move {
        let result = async {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(popup_cancelled()),
                result = action => result?,
            }
            let started = Instant::now();
            let target_id = tokio::time::timeout(options.timeout_value(), async {
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Err(popup_cancelled()),
                        event = events.next() => event,
                    };
                    let Some(event) = event else {
                        return Err(BrowserError::operation(
                            "expect popup",
                            OperationPhase::Confirmation,
                        )
                        .with_action_completion(ActionCompletion::Completed)
                        .with_message("popup event source closed"));
                    };
                    let event = event.map_err(|error| {
                        BrowserError::operation("expect popup", OperationPhase::Confirmation)
                            .with_action_completion(ActionCompletion::Completed)
                            .with_message(error.to_string())
                    })?;
                    if let SessionEvent::PageTargetCreated(fact) = event.into_event() {
                        if popup_belongs_to(&fact, &opener_target_id) {
                            return Ok(fact.target_id);
                        }
                    }
                }
            })
            .await
            .map_err(|_| {
                BrowserError::operation("expect popup", OperationPhase::Confirmation)
                    .with_action_completion(ActionCompletion::Completed)
                    .with_wait_failure(WaitFailure::new(
                        "popup opened by page",
                        &opener_target_id,
                        started.elapsed(),
                        None,
                    ))
            })??;
            // The owned task survives cancellation of the caller. As soon as it
            // consumes the queued fact, every later await is covered by CloseTarget.
            let pending_target = session.track_pending_target(target_id.clone());
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(popup_cancelled()),
                result = session.attach_action_popup(target_id, pending_target) => {
                    result.map_err(|error| error.with_action_completion(ActionCompletion::Completed))
                }
            }
        }
        .await;
        // Release the page gate and action registration before closing an
        // undeliverable popup; Page::close waits for admitted operations.
        drop(operation);
        drop(lease);
        if let Err(Ok(page)) = sender.send(result) {
            let _ = page.close().await;
        }
    });
    receiver.await.map_err(|_| {
        BrowserError::operation("expect popup", OperationPhase::Confirmation)
            .with_action_completion(ActionCompletion::Unknown)
            .with_message("popup expectation task ended without a result")
    })?
}

fn popup_cancelled() -> BrowserError {
    BrowserError::operation("expect popup", OperationPhase::Cleanup)
        .with_action_completion(ActionCompletion::Unknown)
        .with_message("popup expectation was cancelled because its opener page closed")
}
fn popup_belongs_to(fact: &super::TargetFact, opener: &str) -> bool {
    fact.opener_target_id.as_deref() == Some(opener)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oopif_opener_frame_does_not_replace_target_correlation() {
        let f = crate::runtime::TargetFact {
            target_id: "popup".into(),
            browser_context_id: None,
            opener_target_id: Some("page".into()),
            opener_frame_id: Some("oopif".into()),
            url: String::new(),
            title: String::new(),
        };
        assert!(popup_belongs_to(&f, "page"));
        assert!(!popup_belongs_to(&f, "oopif"));
    }
}
