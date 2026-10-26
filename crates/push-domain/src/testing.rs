//! Set-up shared by the unit tests that drive [`Messaging`] over the in-memory store.

use std::sync::Arc;

use crate::clock::SystemClock;
use crate::ids::UserId;
use crate::memory::InMemoryStore;
use crate::notify::RecordingNotifier;
use crate::service::Messaging;

pub(crate) type TestService = Messaging<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

/// A service over an empty in-memory store, and the notifier recording its events.
pub(crate) fn service() -> (TestService, Arc<RecordingNotifier>) {
    let notifier = Arc::new(RecordingNotifier::default());
    let service = Messaging::new(InMemoryStore::new(), SystemClock, Arc::clone(&notifier));
    (service, notifier)
}

/// Registers a new verified user.
pub(crate) async fn user(service: &TestService) -> UserId {
    let user = UserId::new();
    service.register_user(user).await.unwrap();
    user
}

/// Makes `a` and `b` accepted contacts.
pub(crate) async fn befriend(service: &TestService, a: UserId, b: UserId) {
    service.contact_request(a, b).await.unwrap();
    service.contact_accept(b, a).await.unwrap();
}
