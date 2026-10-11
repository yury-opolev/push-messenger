//! The doorbell. After a commit, the service tells the transport layer which users have
//! something new. It carries no message content, and losing a notification only delays
//! delivery: the next sync reads the log.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::ids::UserId;

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Best effort; must not fail the operation that triggered it.
    async fn notify_users(&self, users: &[UserId]);
}

/// Discards notifications.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn notify_users(&self, _users: &[UserId]) {}
}

/// Records notifications. For tests.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    calls: Mutex<Vec<Vec<UserId>>>,
}

impl RecordingNotifier {
    /// Returns and clears the notifications recorded so far, one list per call.
    pub fn take(&self) -> Vec<Vec<UserId>> {
        std::mem::take(&mut *self.calls.lock().expect("notifier lock poisoned"))
    }
}

#[async_trait]
impl Notifier for RecordingNotifier {
    async fn notify_users(&self, users: &[UserId]) {
        self.calls
            .lock()
            .expect("notifier lock poisoned")
            .push(users.to_vec());
    }
}

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn notify_users(&self, users: &[UserId]) {
        (**self).notify_users(users).await;
    }
}
