use std::future::Future;
use std::time::Duration;

use cdpkit::target::events::TargetDestroyed;
use cdpkit::target::methods::{CloseTarget, GetTargets};
use cdpkit::CDP;
use futures::StreamExt;
use tokio::time::Instant;

use crate::runtime::OwnershipCleanupError;

const TARGET_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIRMATION_RESERVE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
struct TargetCloseDeadline {
    confirm_at: Instant,
    expires_at: Instant,
}

impl TargetCloseDeadline {
    fn after(timeout: Duration) -> Self {
        let now = Instant::now();
        let reserve = CONFIRMATION_RESERVE
            .min(timeout / 4)
            .max(Duration::from_millis(1));
        let expires_at = now + timeout;
        Self {
            confirm_at: expires_at.checked_sub(reserve).unwrap_or(now),
            expires_at,
        }
    }
}

/// Closes one SDK-created target and does not report success until Chrome's
/// target inventory confirms completion. One absolute deadline covers event
/// subscription, command dispatch, event waiting, and the fallback query.
pub(crate) async fn close_created_target_and_wait(
    cdp: &CDP,
    target_id: String,
) -> Result<(), OwnershipCleanupError> {
    close_created_target_and_wait_until(
        cdp,
        target_id,
        TargetCloseDeadline::after(TARGET_CLOSE_TIMEOUT),
    )
    .await
}

async fn close_created_target_and_wait_until(
    cdp: &CDP,
    target_id: String,
    deadline: TargetCloseDeadline,
) -> Result<(), OwnershipCleanupError> {
    let destroyed = async {
        until(
            deadline.expires_at,
            "subscribe to Target.targetDestroyed",
            TargetDestroyed::subscribe(cdp),
        )
        .await?
        .map_err(OwnershipCleanupError::from)
    };
    let close = async {
        let close = until(
            deadline.expires_at,
            "dispatch Target.closeTarget",
            CloseTarget::new(target_id.clone()).send(cdp),
        )
        .await?;
        match close {
            Ok(response) => {
                #[allow(deprecated)]
                let success = response.success;
                Ok(TargetCloseDispatch::Acknowledged(success))
            }
            Err(error) if error_is_missing_target(&error) => Ok(TargetCloseDispatch::MissingTarget),
            Err(error) => Err(OwnershipCleanupError::from(error)),
        }
    };
    close_created_target_and_wait_until_with_confirmation(
        &target_id,
        deadline,
        destroyed,
        close,
        confirm_target_absent(cdp, &target_id, deadline.expires_at),
    )
    .await
}

#[derive(Debug, Clone, Copy)]
enum TargetCloseDispatch {
    Acknowledged(bool),
    MissingTarget,
}

async fn close_created_target_and_wait_until_with_confirmation<
    S,
    E,
    Destroyed,
    Close,
    Confirmation,
>(
    target_id: &str,
    deadline: TargetCloseDeadline,
    destroyed: Destroyed,
    close: Close,
    confirmation: Confirmation,
) -> Result<(), OwnershipCleanupError>
where
    S: futures::Stream<Item = Result<TargetDestroyed, E>> + Unpin,
    Destroyed: Future<Output = Result<S, OwnershipCleanupError>>,
    Close: Future<Output = Result<TargetCloseDispatch, OwnershipCleanupError>>,
    Confirmation: Future<Output = Result<(), OwnershipCleanupError>>,
{
    let mut destroyed = destroyed.await?;

    match close.await? {
        TargetCloseDispatch::Acknowledged(true) => {
            return wait_for_destroyed_or_confirm_absent(
                &mut destroyed,
                target_id,
                deadline,
                || confirmation,
            )
            .await;
        }
        TargetCloseDispatch::Acknowledged(false) => {
            // A false acknowledgement is not completion evidence. Always
            // confirm inventory absence, even if an event raced with the ack.
        }
        TargetCloseDispatch::MissingTarget => {
            // Chrome may destroy the target before replying. Drain an already
            // queued matching event without consuming the fallback budget.
            while let Ok(Some(item)) =
                tokio::time::timeout_at(Instant::now(), destroyed.next()).await
            {
                match item {
                    Ok(event) if event.target_id == target_id => return Ok(()),
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    confirmation.await
}

async fn wait_for_destroyed_or_confirm_absent<S, E, Confirm, ConfirmFuture>(
    destroyed: &mut S,
    target_id: &str,
    deadline: TargetCloseDeadline,
    confirm_absent: Confirm,
) -> Result<(), OwnershipCleanupError>
where
    S: futures::Stream<Item = Result<TargetDestroyed, E>> + Unpin,
    Confirm: FnOnce() -> ConfirmFuture,
    ConfirmFuture: Future<Output = Result<(), OwnershipCleanupError>>,
{
    loop {
        match tokio::time::timeout_at(deadline.confirm_at, destroyed.next()).await {
            Ok(Some(Ok(event))) if event.target_id == target_id => return Ok(()),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return confirm_absent().await,
        }
    }
}

async fn confirm_target_absent(
    cdp: &CDP,
    target_id: &str,
    deadline: Instant,
) -> Result<(), OwnershipCleanupError> {
    let targets = until(
        deadline,
        "confirm Target.getTargets",
        GetTargets::new().send(cdp),
    )
    .await?
    .map_err(OwnershipCleanupError::from)?;
    if targets
        .target_infos
        .iter()
        .any(|target| target.target_id == target_id)
    {
        Err(OwnershipCleanupError::TargetStillPresent {
            target_id: target_id.to_owned(),
        })
    } else {
        Ok(())
    }
}

async fn until<T>(
    deadline: Instant,
    stage: &'static str,
    future: impl Future<Output = T>,
) -> Result<T, OwnershipCleanupError> {
    tokio::time::timeout_at(deadline, future)
        .await
        .map_err(|_| OwnershipCleanupError::DeadlineExceeded { stage })
}

fn error_is_missing_target(error: &cdpkit::CdpError) -> bool {
    matches!(
        error,
        cdpkit::CdpError::Protocol { code: -32000, message, .. }
            if matches!(message.as_str(), "No target with given id" | "No target with given id found")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::Message;

    #[derive(Clone, Copy)]
    enum Behavior {
        DelayedDestroyed,
        EventBeforeAck,
        FalseAbsent,
    }

    async fn server(
        behavior: Behavior,
    ) -> (
        CDP,
        std::sync::Arc<Notify>,
        std::sync::Arc<Notify>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acked = std::sync::Arc::new(Notify::new());
        let release = std::sync::Arc::new(Notify::new());
        let server_acked = std::sync::Arc::clone(&acked);
        let server_release = std::sync::Arc::clone(&release);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut methods = Vec::new();
            while let Some(Ok(message)) = read.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let command: Value = serde_json::from_str(&text).unwrap();
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                match method.as_str() {
                    "Target.closeTarget" => match behavior {
                        Behavior::DelayedDestroyed => {
                            write
                                .send(Message::Text(
                                    json!({"id":id,"result":{"success":true}})
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                            server_acked.notify_one();
                            server_release.notified().await;
                            write.send(Message::Text(json!({"method":"Target.targetDestroyed","params":{"targetId":"target-1"}}).to_string().into())).await.unwrap();
                        }
                        Behavior::EventBeforeAck => {
                            write.send(Message::Text(json!({"method":"Target.targetDestroyed","params":{"targetId":"target-1"}}).to_string().into())).await.unwrap();
                            write
                                .send(Message::Text(
                                    json!({"id":id,"result":{"success":true}})
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                        }
                        Behavior::FalseAbsent => {
                            write
                                .send(Message::Text(
                                    json!({"id":id,"result":{"success":false}})
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                        }
                    },
                    "Target.getTargets" => {
                        write
                            .send(Message::Text(
                                json!({"id":id,"result":{"targetInfos":[]}})
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    other => panic!("unexpected command: {other}"),
                }
            }
            methods
        });
        let cdp = CDP::connect_ws_with_timeout(&format!("ws://{address}"), Duration::from_secs(1))
            .await
            .unwrap();
        (cdp, acked, release, task)
    }

    async fn finish(cdp: CDP, task: tokio::task::JoinHandle<Vec<String>>) -> Vec<String> {
        cdp.close();
        cdp.closed().await;
        task.await.unwrap()
    }

    #[tokio::test]
    async fn ack_then_delayed_destroyed_does_not_return_early() {
        let (cdp, acked, release, server) = server(Behavior::DelayedDestroyed).await;
        let close_cdp = cdp.clone();
        let closing = tokio::spawn(async move {
            close_created_target_and_wait_until(
                &close_cdp,
                "target-1".into(),
                TargetCloseDeadline::after(Duration::from_secs(1)),
            )
            .await
        });
        acked.notified().await;
        assert!(!closing.is_finished());
        release.notify_one();
        closing.await.unwrap().unwrap();
        assert_eq!(finish(cdp, server).await, vec!["Target.closeTarget"]);
    }

    #[tokio::test]
    async fn destroyed_event_before_ack_is_observed() {
        let (cdp, _, _, server) = server(Behavior::EventBeforeAck).await;
        close_created_target_and_wait_until(
            &cdp,
            "target-1".into(),
            TargetCloseDeadline::after(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert_eq!(finish(cdp, server).await, vec!["Target.closeTarget"]);
    }

    #[tokio::test(start_paused = true)]
    async fn event_deadline_uses_fake_dispatch_and_inventory_confirmation() {
        let deadline = TargetCloseDeadline::after(Duration::from_millis(80));
        let close_dispatches = std::sync::atomic::AtomicUsize::new(0);
        let confirmations = std::sync::atomic::AtomicUsize::new(0);
        let waiting = close_created_target_and_wait_until_with_confirmation(
            "target-1",
            deadline,
            async { Ok(futures::stream::pending::<Result<TargetDestroyed, ()>>()) },
            async {
                close_dispatches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(TargetCloseDispatch::Acknowledged(true))
            },
            async {
                confirmations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(OwnershipCleanupError::TargetStillPresent {
                    target_id: "target-1".into(),
                })
            },
        );
        tokio::pin!(waiting);

        assert!(matches!(
            futures::poll!(&mut waiting),
            std::task::Poll::Pending
        ));
        assert_eq!(
            close_dispatches.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(confirmations.load(std::sync::atomic::Ordering::SeqCst), 0);

        tokio::time::advance(
            deadline
                .confirm_at
                .saturating_duration_since(Instant::now()),
        )
        .await;
        let error = waiting.await.unwrap_err();

        assert_eq!(
            error,
            OwnershipCleanupError::TargetStillPresent {
                target_id: "target-1".into()
            }
        );
        assert_eq!(confirmations.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closed_event_stream_confirms_absence_before_success() {
        let mut stream = futures::stream::empty::<Result<TargetDestroyed, ()>>();
        let confirmations = std::sync::atomic::AtomicUsize::new(0);
        wait_for_destroyed_or_confirm_absent(
            &mut stream,
            "target-1",
            TargetCloseDeadline::after(Duration::from_secs(1)),
            || async {
                confirmations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(confirmations.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn false_ack_sends_one_typed_close_and_one_inventory_command() {
        let (cdp, _, _, server) = server(Behavior::FalseAbsent).await;
        close_created_target_and_wait_until(
            &cdp,
            "target-1".into(),
            TargetCloseDeadline::after(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert_eq!(
            finish(cdp, server).await,
            vec!["Target.closeTarget", "Target.getTargets"]
        );
    }
}
