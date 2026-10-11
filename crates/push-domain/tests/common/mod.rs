//! Shared test harness: a `Messaging` service over the in-memory store, a manual clock and
//! a recording notifier.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{DeviceId, InMemoryStore, ManualClock, Messaging, RecordingNotifier, UserId};

pub type Svc = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

/// A verified user and one of their devices.
#[derive(Debug, Clone, Copy)]
pub struct Person {
    pub user: UserId,
    pub device: DeviceId,
}

pub struct World {
    pub svc: Svc,
    pub clock: Arc<ManualClock>,
    pub notifier: Arc<RecordingNotifier>,
}

impl World {
    pub fn new() -> Self {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let svc = Messaging::new(
            InMemoryStore::new(),
            Arc::clone(&clock),
            Arc::clone(&notifier),
        );
        Self {
            svc,
            clock,
            notifier,
        }
    }

    /// Registers a verified user with one device.
    pub async fn person(&self) -> Person {
        let person = Person {
            user: UserId::new(),
            device: DeviceId::new(),
        };
        self.svc.register_user(person.user).await.unwrap();
        self.svc
            .register_device(person.device, person.user)
            .await
            .unwrap();
        person
    }

    /// Registers another device for an existing person.
    pub async fn add_device(&self, person: &Person) -> DeviceId {
        let device = DeviceId::new();
        self.svc.register_device(device, person.user).await.unwrap();
        device
    }

    /// Makes two people accepted contacts.
    pub async fn befriend(&self, a: &Person, b: &Person) {
        self.svc.contact_request(a.user, b.user).await.unwrap();
        self.svc.contact_accept(b.user, a.user).await.unwrap();
    }
}
