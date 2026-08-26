use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use cdpkit::page::methods::HandleJavaScriptDialog;

use super::{ActionCompletion, BrowserError, OperationPhase, Page, WaitFailure, WaitOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogType {
    Alert,
    Confirm,
    Prompt,
    BeforeUnload,
    Unknown(String),
}

impl DialogType {
    pub(crate) fn from_protocol(value: impl Into<String>) -> Self {
        let value = value.into();
        match value.as_str() {
            "alert" => Self::Alert,
            "confirm" => Self::Confirm,
            "prompt" => Self::Prompt,
            "beforeunload" => Self::BeforeUnload,
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DialogOpenedFact {
    pub(crate) epoch: u64,
    pub(crate) routed_session: cdpkit::Session,
    pub(crate) frame_id: String,
    pub(crate) message: String,
    pub(crate) dialog_type: DialogType,
    pub(crate) default_prompt: Option<String>,
}

#[derive(Debug, Clone)]
struct CurrentDialog {
    epoch: u64,
    claimed: bool,
}

pub(crate) struct DialogCoordinator {
    next_epoch: AtomicU64,
    current: parking_lot::Mutex<HashMap<String, CurrentDialog>>,
    opened: tokio::sync::broadcast::Sender<DialogOpenedFact>,
}

impl DialogCoordinator {
    pub(crate) fn new() -> Self {
        let (opened, _) = tokio::sync::broadcast::channel(16);
        Self {
            next_epoch: AtomicU64::new(1),
            current: parking_lot::Mutex::new(HashMap::new()),
            opened,
        }
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<DialogOpenedFact> {
        self.opened.subscribe()
    }

    pub(crate) fn open(&self, mut fact: DialogOpenedFact) -> DialogOpenedFact {
        let epoch = self.open_epoch(fact.routed_session.id());
        fact.epoch = epoch;
        let _ = self.opened.send(fact.clone());
        fact
    }

    fn open_epoch(&self, routed_session_id: impl Into<String>) -> u64 {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        let routed_session_id = routed_session_id.into();
        self.current.lock().insert(
            routed_session_id.clone(),
            CurrentDialog {
                epoch,
                claimed: false,
            },
        );
        epoch
    }

    pub(crate) fn claim(&self, epoch: u64) -> bool {
        let mut current = self.current.lock();
        let Some(dialog) = current.values_mut().find(|dialog| dialog.epoch == epoch) else {
            return false;
        };
        if dialog.claimed {
            return false;
        }
        dialog.claimed = true;
        true
    }

    #[cfg(test)]
    fn close(&self, epoch: u64) {
        let mut current = self.current.lock();
        if let Some(route) = current
            .iter()
            .find_map(|(route, dialog)| (dialog.epoch == epoch).then(|| route.clone()))
        {
            current.remove(&route);
        }
    }

    pub(crate) fn close_current(&self) {
        self.current.lock().clear();
    }

    pub(crate) fn close_route(&self, routed_session_id: &str) {
        self.current.lock().remove(routed_session_id);
    }
}

pub(crate) struct DialogActionRegistry {
    next_id: AtomicU64,
    state: Arc<parking_lot::Mutex<DialogActionRegistryState>>,
}

#[derive(Default)]
struct DialogActionRegistryState {
    closing: bool,
    active: HashMap<u64, tokio_util::sync::CancellationToken>,
}

pub(crate) struct DialogActionLease {
    id: u64,
    state: Arc<parking_lot::Mutex<DialogActionRegistryState>>,
}

impl DialogActionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            state: Arc::new(parking_lot::Mutex::new(DialogActionRegistryState::default())),
        }
    }

    pub(crate) fn register(&self) -> (DialogActionLease, tokio_util::sync::CancellationToken) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut state = self.state.lock();
        if state.closing {
            cancellation.cancel();
        }
        state.active.insert(id, cancellation.clone());
        (
            DialogActionLease {
                id,
                state: Arc::clone(&self.state),
            },
            cancellation,
        )
    }

    pub(crate) fn cancel_all(&self) {
        let mut state = self.state.lock();
        state.closing = true;
        for cancellation in state.active.values() {
            cancellation.cancel();
        }
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.state.lock().active.len()
    }
}

impl Drop for DialogActionLease {
    fn drop(&mut self) {
        self.state.lock().active.remove(&self.id);
    }
}

pub struct Dialog {
    page: Page,
    message: String,
    dialog_type: DialogType,
    epoch: u64,
    routed_session: cdpkit::Session,
    frame_id: String,
    default_prompt: Option<String>,
    handled: Arc<AtomicBool>,
    action_result:
        tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<Result<(), BrowserError>>>>,
}

impl Dialog {
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn dialog_type(&self) -> &DialogType {
        &self.dialog_type
    }
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }
    pub fn default_prompt(&self) -> Option<&str> {
        self.default_prompt.as_deref()
    }
    pub async fn accept(&self, prompt_text: Option<&str>) -> Result<(), BrowserError> {
        self.handle(true, prompt_text).await
    }
    pub async fn dismiss(&self) -> Result<(), BrowserError> {
        self.handle(false, None).await
    }
    async fn completed_action_result(&self) -> Result<Option<()>, BrowserError> {
        let mut receiver = self.action_result.lock().await;
        let Some(action) = receiver.as_mut() else {
            return Ok(Some(()));
        };
        match action.try_recv() {
            Ok(result) => {
                receiver.take();
                result.map(Some)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                receiver.take();
                Err(
                    BrowserError::operation("complete dialog action", OperationPhase::Confirmation)
                        .with_action_completion(ActionCompletion::Unknown)
                        .with_message("dialog action task ended without a result"),
                )
            }
        }
    }
    async fn await_action_result(&self) -> Result<(), BrowserError> {
        if let Some(receiver) = self.action_result.lock().await.take() {
            receiver.await.map_err(|_| {
                BrowserError::operation("complete dialog action", OperationPhase::Confirmation)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message("dialog action task ended without a result")
            })??;
        }
        Ok(())
    }
    async fn handle(&self, accept: bool, prompt_text: Option<&str>) -> Result<(), BrowserError> {
        self.completed_action_result().await?;
        if !claim_dialog_handle(&self.handled) {
            return Err(BrowserError::operation(
                "handle JavaScript dialog",
                OperationPhase::Preparation,
            )
            .with_message("dialog handle was already used or is no longer current"));
        }
        if !self.page.dialogs().claim(self.epoch) {
            // An externally closed/replaced dialog releases a blocking action.
            // Preserve the original action failure before reporting staleness.
            self.await_action_result().await?;
            return Err(BrowserError::operation(
                "handle JavaScript dialog",
                OperationPhase::Preparation,
            )
            .with_message("dialog handle was already used or is no longer current"));
        }
        let _operation = self.page.admit_operation("handle JavaScript dialog")?;
        let mut command = HandleJavaScriptDialog::new(accept);
        if let Some(text) = prompt_text {
            command = command.with_prompt_text(text);
        }
        command.send(&self.routed_session).await.map_err(|error| {
            BrowserError::cdp_operation("handle JavaScript dialog", OperationPhase::Dispatch, error)
                .with_action_completion(ActionCompletion::Unknown)
                .with_message("dialog is already closed, replaced, or its target is unavailable")
        })?;
        self.await_action_result().await
    }
}
fn claim_dialog_handle(handled: &AtomicBool) -> bool {
    !handled.swap(true, Ordering::AcqRel)
}
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn only_the_current_dialog_epoch_can_be_claimed() {
        let dialogs = DialogCoordinator::new();
        let first = dialogs.open_epoch("route");
        let second = dialogs.open_epoch("route");

        assert!(!dialogs.claim(first));
        assert!(dialogs.claim(second));
        assert!(!dialogs.claim(second));
    }

    #[test]
    fn independent_routes_can_each_have_a_current_dialog() {
        let dialogs = DialogCoordinator::new();
        let first = dialogs.open_epoch("first-route");
        let second = dialogs.open_epoch("second-route");

        assert!(dialogs.claim(first));
        assert!(dialogs.claim(second));
    }

    #[test]
    fn closing_a_dialog_invalidates_its_epoch_without_invalidating_a_replacement() {
        let dialogs = DialogCoordinator::new();
        let first = dialogs.open_epoch("route");
        let second = dialogs.open_epoch("route");

        dialogs.close(first);
        assert!(dialogs.claim(second));
    }

    #[tokio::test]
    async fn page_close_cancellation_releases_an_in_flight_dialog_action() {
        let actions = DialogActionRegistry::new();
        let (lease, cancellation) = actions.register();
        let task = tokio::spawn(async move {
            let _lease = lease;
            cancellation.cancelled().await;
        });

        actions.cancel_all();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("dialog action remained live during page close")
            .unwrap();
        assert_eq!(actions.active_count(), 0);
    }

    #[test]
    fn registration_after_page_close_is_already_cancelled() {
        let actions = DialogActionRegistry::new();
        actions.cancel_all();

        let (_lease, cancellation) = actions.register();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn a_late_close_from_an_old_route_cannot_clear_a_new_route_dialog() {
        let dialogs = DialogCoordinator::new();
        dialogs.open_epoch("route-a");
        let current = dialogs.open_epoch("route-b");

        dialogs.close_route("route-a");

        assert!(dialogs.claim(current));
    }

    #[test]
    fn dialog_handle_is_one_shot() {
        let h = AtomicBool::new(false);
        assert!(claim_dialog_handle(&h));
        assert!(!claim_dialog_handle(&h));
    }
    #[tokio::test]
    #[ignore = "requires installed Chrome"]
    async fn live_chrome_dialog_popup_and_file_chooser() {
        use crate::runtime::{BrowserRuntime, LaunchOptions, WaitOptions};
        use cdpkit::runtime::methods::Evaluate;
        use std::time::Duration;
        let runtime = BrowserRuntime::launch(LaunchOptions::default().headless(true))
            .await
            .unwrap();
        let session = runtime.default_session().await.unwrap();
        let page = session
            .new_page("data:text/html,<button id=p onclick=window.open('about:blank')>popup</button><input id=f type=file><script>window.ready=true</script>")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = page.cdp_session().clone();
        let dialog = page
            .expect_dialog(
                WaitOptions::default().timeout(Duration::from_secs(3)),
                async move {
                    Evaluate::new("alert('hello')")
                        .send(&s)
                        .await
                        .map(|_| ())
                        .map_err(BrowserError::from)
                },
            )
            .await
            .unwrap();
        assert_eq!(dialog.message(), "hello");
        dialog.accept(None).await.unwrap();
        let popup_trigger = page.locator("#p");
        let popup = page
            .expect_popup(
                WaitOptions::default().timeout(Duration::from_secs(3)),
                async move { popup_trigger.click().await },
            )
            .await
            .unwrap();
        assert_ne!(popup.target_id(), page.target_id());
        assert!(popup.close().await.is_complete());

        // A caller may cancel after Chrome has emitted TargetCreated but before
        // expect_popup consumes it. The Page-owned expectation must still claim
        // and close that popup instead of leaving an unowned tab behind.
        let baseline_targets = session
            .pages()
            .await
            .unwrap()
            .into_iter()
            .map(|page| page.target_id().to_owned())
            .collect::<std::collections::HashSet<_>>();
        let cancelled_page = page.clone();
        let cancelled = tokio::spawn(async move {
            let trigger = cancelled_page.locator("#p");
            cancelled_page
                .expect_popup(
                    WaitOptions::default().timeout(Duration::from_secs(3)),
                    async move {
                        trigger.click().await?;
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        Ok(())
                    },
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancelled.abort();
        let _ = cancelled.await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let remaining_targets = session
            .pages()
            .await
            .unwrap()
            .into_iter()
            .map(|page| page.target_id().to_owned())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(remaining_targets, baseline_targets);

        let chooser_trigger = page.locator("#f");
        let chooser = page
            .expect_file_chooser(
                WaitOptions::default().timeout(Duration::from_secs(3)),
                async move { chooser_trigger.click().await },
            )
            .await
            .unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        chooser.set_files([file.path()]).await.unwrap();
        let _ = runtime.close().await;
    }
}

pub(crate) async fn expect_dialog<F>(
    page: &Page,
    options: WaitOptions,
    action: F,
) -> Result<Dialog, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>> + Send + 'static,
{
    let operation = page.admit_operation("expect JavaScript dialog")?;
    page.locator_frame_store(&operation)
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?;
    let mut opened = page.dialogs().subscribe();
    let started = Instant::now();
    let (sender, mut action_result) = tokio::sync::oneshot::channel();
    let (lease, cancellation) = page.side_effect_actions().register();
    let action_cancellation = cancellation.clone();
    tokio::spawn(async move {
        let _lease = lease;
        let _operation = operation;
        let result = tokio::select! {
            biased;
            _ = action_cancellation.cancelled() => Err(
                BrowserError::operation("complete dialog action", OperationPhase::Cleanup)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message("dialog action was cancelled because its page or expectation closed"),
            ),
            result = action => result,
        };
        let _ = sender.send(result);
    });
    let mut action_completed = false;
    let result = tokio::time::timeout(options.timeout_value(), async {
        tokio::select! {
            event = opened.recv() => event.map_err(|error| {
                BrowserError::operation("expect JavaScript dialog", OperationPhase::Confirmation)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message(format!("dialog event source closed: {error}"))
            }),
            result = &mut action_result => {
                result.map_err(|_| {
                    BrowserError::operation("complete dialog action", OperationPhase::Confirmation)
                        .with_action_completion(ActionCompletion::Unknown)
                })??;
                action_completed = true;
                opened.recv().await.map_err(|error| {
                    BrowserError::operation("expect JavaScript dialog", OperationPhase::Confirmation)
                        .with_action_completion(ActionCompletion::Completed)
                        .with_message(format!("dialog event source closed: {error}"))
                })
            }
        }
    })
    .await;
    let event = match result {
        Ok(result) => result?,
        Err(_) => {
            cancellation.cancel();
            return Err(BrowserError::operation(
                "expect JavaScript dialog",
                OperationPhase::Confirmation,
            )
            .with_action_completion(if action_completed {
                ActionCompletion::Completed
            } else {
                ActionCompletion::Unknown
            })
            .with_wait_failure(WaitFailure::new(
                "JavaScript dialog opening",
                page.target_id(),
                started.elapsed(),
                None,
            )));
        }
    };
    Ok(Dialog {
        page: page.clone(),
        message: event.message,
        dialog_type: event.dialog_type,
        epoch: event.epoch,
        routed_session: event.routed_session,
        frame_id: event.frame_id,
        default_prompt: event.default_prompt,
        handled: Arc::new(AtomicBool::new(false)),
        action_result: tokio::sync::Mutex::new(if action_completed {
            None
        } else {
            Some(action_result)
        }),
    })
}
