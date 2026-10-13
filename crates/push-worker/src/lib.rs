//! push-worker: the worker of push-messenger (spec §2, §3.3, §9).
//!
//! It owns the `worker` schema in its own database (`push_worker`) and drains the other
//! services' outboxes into its job queue: it relays auth-svc's user and device events to
//! message-svc, sends email, takes delivery webhooks, and runs the cleanup jobs.

mod db;
pub mod queue;

pub use db::{MIGRATOR, migrate_database};
