//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls on
//! behalf of connected devices (every client frame of spec §7.1), records the users and
//! devices the worker relays from auth-svc and, after every commit, rings the affected
//! users' Redis channels so their gateways sync them, and kicks revoked devices. It also
//! fans out live activity, reads the presence the gateways record in Redis and announces
//! its changes to contacts, keeps the template registry for auth-svc and seeds the
//! built-in templates, and, with calls enabled, runs the call engine (voice calls design,
//! docs/superpowers/specs/2026-11-03-voice-calls-design.md).

pub mod calls;
mod config;
mod convert;
mod grpc;
mod listeners;
mod metrics;
mod notify;
mod presence;
mod requests;
mod server;
mod templates;

pub use calls::{CallService, CallsApi, RedisCalls};
pub use config::{CallsConfig, Config, ConfigError, MigrateConfig};
pub use grpc::{AccountsApi, DirectoryApi, InternalToken, MAX_VISIBLE_USERS, MessagingApi};
pub use listeners::Listeners;
pub use metrics::{Metrics, SignalFrame};
pub use notify::RedisNotifier;
pub use presence::{
    FixedPresence, OfflinePresence, PresenceRead, PresenceStates, PresenceUnavailable,
    RedisPresence,
};
pub use server::{init_logging, migrate, serve, terminated};
pub use templates::{TemplatesApi, seed_builtins};
