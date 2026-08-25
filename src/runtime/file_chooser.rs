use super::{ActionCompletion, BrowserError, OperationPhase, Page, WaitFailure, WaitOptions};
use cdpkit::dom::methods::SetFileInputFiles;
use std::{future::Future, path::PathBuf, sync::Arc, time::Instant};

#[derive(Default)]
pub(crate) struct FileChooserInterceptionState {
    next_generation: u64,
    active: Option<u64>,
    enabled_sessions: std::collections::HashMap<String, cdpkit::Session>,
}
impl FileChooserInterceptionState {
    pub(crate) fn begin(&mut self) -> Option<u64> {
        if self.active.is_some() {
            return None;
        }
        self.next_generation = self.next_generation.saturating_add(1);
        self.active = Some(self.next_generation);
        self.active
    }
    pub(crate) fn finish(&mut self, generation: u64) -> bool {
        if self.active == Some(generation) {
            self.active = None;
            self.enabled_sessions.clear();
            true
        } else {
            false
        }
    }
    pub(crate) fn active_generation(&self) -> Option<u64> {
        self.active
    }
    pub(crate) fn track_enabled(&mut self, session: cdpkit::Session) {
        self.enabled_sessions
            .insert(session.id().to_owned(), session);
    }
    pub(crate) fn enabled_sessions(&self) -> Vec<cdpkit::Session> {
        self.enabled_sessions.values().cloned().collect()
    }
    pub(crate) fn close_locally(&mut self) {
        self.active = None;
        self.enabled_sessions.clear();
    }
    pub(crate) fn remove_enabled(&mut self, session_id: &str) {
        self.enabled_sessions.remove(session_id);
    }
}

#[derive(Clone)]
pub(crate) struct FileChooserOpenedFact {
    pub(crate) routed_session: cdpkit::Session,
    pub(crate) frame_id: String,
    pub(crate) backend_node_id: Option<i64>,
    pub(crate) multiple: bool,
}
pub struct FileChooser {
    page: Page,
    routed_session: cdpkit::Session,
    frame_id: String,
    backend_node_id: i64,
    multiple: bool,
    action_result:
        tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<FileChooserActionOutcome>>>,
}
#[derive(Debug)]
enum FileChooserActionOutcome {
    Completed,
    LocallyAborted,
    Failed(BrowserError),
}
impl FileChooser {
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }
    pub fn backend_node_id(&self) -> i64 {
        self.backend_node_id
    }
    pub fn allows_multiple(&self) -> bool {
        self.multiple
    }
    pub async fn set_files<I, P>(&self, files: I) -> Result<(), BrowserError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let files = files
            .into_iter()
            .map(|p| p.into().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if files.is_empty() || (!self.multiple && files.len() > 1) {
            return Err(BrowserError::operation(
                "set file chooser files",
                OperationPhase::Preparation,
            )
            .with_message("invalid file selection"));
        }
        let _op = self.page.admit_operation("set file chooser files")?;
        SetFileInputFiles::new(files)
            .with_backend_node_id(self.backend_node_id)
            .send(&self.routed_session)
            .await
            .map_err(|e| {
                BrowserError::cdp_operation("set file chooser files", OperationPhase::Dispatch, e)
                    .with_action_completion(ActionCompletion::Unknown)
            })?;
        if let Some(receiver) = self.action_result.lock().await.take() {
            match receiver.await.map_err(|_| {
                BrowserError::operation(
                    "complete file chooser action",
                    OperationPhase::Confirmation,
                )
                .with_action_completion(ActionCompletion::Unknown)
                .with_message("file chooser action task ended without a result")
            })? {
                FileChooserActionOutcome::Completed => {}
                FileChooserActionOutcome::Failed(error) => return Err(error),
                FileChooserActionOutcome::LocallyAborted => {
                    return Err(BrowserError::operation(
                        "complete file chooser action",
                        OperationPhase::Confirmation,
                    )
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message("file chooser action was aborted by its expectation"));
                }
            }
        }
        Ok(())
    }
}
struct Guard {
    cleanup: Option<super::PendingOwnershipGuard>,
}
impl Guard {
    async fn disable(mut self) -> Result<(), BrowserError> {
        let cleanup = self.cleanup.take().expect("file chooser cleanup is armed");
        cleanup.cleanup().await.map_err(|error| {
            BrowserError::operation("disable file chooser interception", OperationPhase::Cleanup)
                .with_action_completion(ActionCompletion::Completed)
                .with_message(error.to_string())
        })
    }
}
pub(crate) async fn expect_file_chooser<F>(
    page: &Page,
    options: WaitOptions,
    action: F,
) -> Result<FileChooser, BrowserError>
where
    F: Future<Output = Result<(), BrowserError>> + Send + 'static,
{
    let operation = page
        .admit_operation("expect file chooser")
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?;
    let store = page
        .locator_frame_store(&operation)
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?
        .clone();
    let mut events = store.subscribe_file_choosers();
    let generation = store
        .begin_file_chooser_interception()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::NotStarted))?;
    let cleanup_store = Arc::clone(&store);
    let cleanup = page.track_locator_cleanup(
        format!("file-chooser:{}:{generation}", page.target_id()),
        move || async move {
            cleanup_store
                .end_file_chooser_interception(generation)
                .await
                .map_err(|error| super::OwnershipCleanupError::from(error.to_string()))
        },
    );
    let guard = Guard {
        cleanup: Some(cleanup),
    };
    let started = Instant::now();
    let (sender, mut action_result) = tokio::sync::oneshot::channel();
    let (lease, cancellation) = page.side_effect_actions().register();
    let action_cancellation = cancellation.clone();
    let local_abort = tokio_util::sync::CancellationToken::new();
    let task_local_abort = local_abort.clone();
    tokio::spawn(async move {
        let _lease = lease;
        let _operation = operation;
        let result = tokio::select! {
            biased;
            _ = action_cancellation.cancelled() => FileChooserActionOutcome::Failed(
                BrowserError::operation("complete file chooser action", OperationPhase::Cleanup)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message("file chooser action was cancelled because its page closed"),
            ),
            result = action => match result {
                Ok(()) => FileChooserActionOutcome::Completed,
                Err(error) => FileChooserActionOutcome::Failed(error),
            },
            _ = task_local_abort.cancelled() => FileChooserActionOutcome::LocallyAborted,
        };
        let _ = sender.send(result);
    });
    let mut action_completed = false;
    let result = tokio::time::timeout(options.timeout_value(), async {
        tokio::select! {
            event = events.recv() => event.map_err(|error| {
                BrowserError::operation("expect file chooser", OperationPhase::Confirmation)
                    .with_action_completion(ActionCompletion::Unknown)
                    .with_message(format!("file chooser event source closed: {error}"))
            }),
            result = &mut action_result => {
                match result.map_err(|_| {
                    BrowserError::operation("complete file chooser action", OperationPhase::Confirmation)
                        .with_action_completion(ActionCompletion::Unknown)
                })? {
                    FileChooserActionOutcome::Completed => {}
                    FileChooserActionOutcome::Failed(error) => return Err(error),
                    FileChooserActionOutcome::LocallyAborted => {
                        return Err(BrowserError::operation("complete file chooser action", OperationPhase::Confirmation)
                            .with_action_completion(ActionCompletion::Unknown)
                            .with_message("file chooser action was aborted by its expectation"));
                    }
                }
                action_completed = true;
                events.recv().await.map_err(|error| {
                    BrowserError::operation("expect file chooser", OperationPhase::Confirmation)
                        .with_action_completion(ActionCompletion::Completed)
                        .with_message(format!("file chooser event source closed: {error}"))
                })
            }
        }
    }).await;
    let event = match result {
        Ok(result) => result?,
        Err(_) => {
            local_abort.cancel();
            return Err(BrowserError::operation(
                "expect file chooser",
                OperationPhase::Confirmation,
            )
            .with_action_completion(if action_completed {
                ActionCompletion::Completed
            } else {
                ActionCompletion::Unknown
            })
            .with_wait_failure(WaitFailure::new(
                "file chooser opened",
                page.target_id(),
                started.elapsed(),
                None,
            )));
        }
    };
    let Some(backend) = event.backend_node_id else {
        let completion =
            complete_or_cancel_action(action_completed, &local_abort, &mut action_result).await?;
        return Err(
            BrowserError::operation("expect file chooser", OperationPhase::Confirmation)
                .with_action_completion(completion)
                .with_message("chooser is not a page file input"),
        );
    };
    if let Err(error) = guard.disable().await {
        let completion =
            complete_or_cancel_action(action_completed, &local_abort, &mut action_result).await?;
        return Err(error.with_action_completion(completion));
    }
    Ok(FileChooser {
        page: page.clone(),
        routed_session: event.routed_session,
        frame_id: event.frame_id,
        backend_node_id: backend,
        multiple: event.multiple,
        action_result: tokio::sync::Mutex::new(if action_completed {
            None
        } else {
            Some(action_result)
        }),
    })
}

async fn complete_or_cancel_action(
    already_completed: bool,
    local_abort: &tokio_util::sync::CancellationToken,
    action_result: &mut tokio::sync::oneshot::Receiver<FileChooserActionOutcome>,
) -> Result<ActionCompletion, BrowserError> {
    if already_completed {
        return Ok(ActionCompletion::Completed);
    }
    local_abort.cancel();
    match action_result.await.map_err(|_| {
        BrowserError::operation("complete file chooser action", OperationPhase::Confirmation)
            .with_action_completion(ActionCompletion::Unknown)
            .with_message("file chooser action task ended without a result")
    })? {
        FileChooserActionOutcome::Completed => Ok(ActionCompletion::Completed),
        FileChooserActionOutcome::LocallyAborted => Ok(ActionCompletion::Unknown),
        FileChooserActionOutcome::Failed(error) => Err(error),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_cleanup_cannot_disable_a_new_file_chooser_generation() {
        let mut state = FileChooserInterceptionState::default();
        let first = state.begin().unwrap();
        assert!(state.finish(first));
        let second = state.begin().unwrap();

        assert!(!state.finish(first));
        assert_eq!(state.active_generation(), Some(second));
        assert!(state.finish(second));
        assert_eq!(state.active_generation(), None);
    }

    #[tokio::test]
    async fn draining_a_concurrently_completed_action_reports_completed() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        sender.send(FileChooserActionOutcome::Completed).unwrap();

        assert_eq!(
            complete_or_cancel_action(false, &cancellation, &mut receiver)
                .await
                .unwrap(),
            ActionCompletion::Completed
        );
    }

    #[tokio::test]
    async fn draining_an_action_error_preserves_that_error() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        sender
            .send(FileChooserActionOutcome::Failed(BrowserError::operation(
                "original chooser action",
                OperationPhase::Dispatch,
            )))
            .unwrap();

        let error = complete_or_cancel_action(false, &cancellation, &mut receiver)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("original chooser action"));
    }

    #[tokio::test]
    async fn local_abort_is_unknown_without_pretending_the_page_closed() {
        let local_abort = tokio_util::sync::CancellationToken::new();
        let task_abort = local_abort.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            task_abort.cancelled().await;
            let _ = sender.send(FileChooserActionOutcome::LocallyAborted);
        });

        assert_eq!(
            complete_or_cancel_action(false, &local_abort, &mut receiver)
                .await
                .unwrap(),
            ActionCompletion::Unknown
        );
    }
}
