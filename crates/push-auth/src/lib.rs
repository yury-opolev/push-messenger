//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

mod db;

pub use db::{MIGRATOR, migrate_database};
