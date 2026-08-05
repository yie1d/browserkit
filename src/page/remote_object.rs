use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OBJECT_GROUP_ID: AtomicU64 = AtomicU64::new(1);

fn next_object_group_name(operation: &str) -> String {
    let id = NEXT_OBJECT_GROUP_ID.fetch_add(1, Ordering::Relaxed);
    format!("browserkit:{operation}:{id}")
}

/// A unique, operation-scoped, cancellation-safe CDP Runtime object group.
///
/// Chrome owns the objects referenced by `objectId`; dropping the Rust string
/// does not release them. Normal paths call [`release`](Self::release). If an
/// operation future is cancelled, `Drop` schedules the same cleanup on the
/// current Tokio runtime.
pub(crate) struct RemoteObjectScope {
    name: Option<String>,
    session: Option<cdpkit::OwnedSession>,
}

impl RemoteObjectScope {
    pub(crate) fn new(cdp: &cdpkit::CDP, session_id: &str, operation: &str) -> Self {
        Self {
            name: Some(next_object_group_name(operation)),
            session: Some(cdp.owned_session(session_id)),
        }
    }

    pub(crate) fn name(&self) -> &str {
        self.name
            .as_deref()
            .expect("remote object scope already released")
    }

    pub(crate) async fn release(mut self) {
        let Some(name) = self.name.as_deref() else {
            return;
        };
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if let Err(error) = cdpkit::runtime::methods::ReleaseObjectGroup::new(name)
            .send(session)
            .await
        {
            log_release_error(name, &error);
        }
        self.name.take();
        self.session.take();
    }
}

impl Drop for RemoteObjectScope {
    fn drop(&mut self) {
        let (Some(name), Some(session)) = (self.name.take(), self.session.take()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                object_group = %name,
                "cannot schedule remote object cleanup outside a Tokio runtime"
            );
            return;
        };
        runtime.spawn(release_object_group(name, session));
    }
}

async fn release_object_group(name: String, session: cdpkit::OwnedSession) {
    if let Err(error) = cdpkit::runtime::methods::ReleaseObjectGroup::new(&name)
        .send(&session)
        .await
    {
        log_release_error(&name, &error);
    }
}

fn log_release_error(name: &str, error: &cdpkit::CdpError) {
    // A navigation or closed connection also destroys the execution context
    // and its objects. Cleanup is best-effort and must not mask the operation's
    // original result.
    tracing::debug!(
        object_group = %name,
        error = %error,
        "failed to release CDP remote object group"
    );
}

#[cfg(test)]
mod tests {
    use super::next_object_group_name;

    #[test]
    fn object_groups_are_unique_and_operation_labeled() {
        let first = next_object_group_name("snapshot");
        let second = next_object_group_name("snapshot");

        assert_ne!(first, second);
        assert!(first.starts_with("browserkit:snapshot:"));
    }
}
